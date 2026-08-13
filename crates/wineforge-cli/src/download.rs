use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use reqwest::redirect::{Action, Attempt, Policy};
use sha2::{Digest, Sha256};

use crate::recipe::Source;

const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub fn resolve(source: &Source, recipe_dir: &Path, cache: &Path) -> Result<PathBuf> {
    match source {
        Source::Repository { path, sha256, .. } => {
            let root = fs::canonicalize(recipe_dir).with_context(|| {
                format!(
                    "failed to resolve recipe directory {}",
                    recipe_dir.display()
                )
            })?;
            let resolved = fs::canonicalize(recipe_dir.join(path)).with_context(|| {
                format!("failed to resolve repository source {}", path.display())
            })?;
            if !resolved.starts_with(&root) || !resolved.is_file() {
                bail!("repository source escapes the recipe directory");
            }
            verify_file(&resolved, sha256)?;
            Ok(resolved)
        }
        Source::Remote { url, sha256, .. } => fetch_remote(url, sha256, cache),
    }
}

fn fetch_remote(url: &str, expected: &str, cache: &Path) -> Result<PathBuf> {
    ensure_cache(cache)?;
    let destination = cache.join(expected);
    if destination.exists() {
        verify_regular_file(&destination)?;
        verify_file(&destination, expected).context("cached source failed verification")?;
        return Ok(destination);
    }

    let temporary = cache.join(format!(".{expected}.{}.part", std::process::id()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| {
            format!(
                "failed to create download staging file {}",
                temporary.display()
            )
        })?;
    let result = (|| -> Result<()> {
        let client = Client::builder()
            .user_agent(concat!("wineforge/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::custom(https_redirect))
            .build()
            .context("failed to construct HTTPS client")?;
        let mut response = client.get(url).send().context("source download failed")?;
        if response.url().scheme() != "https" {
            bail!("source response downgraded from HTTPS");
        }
        response = response
            .error_for_status()
            .context("source server returned an error")?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_DOWNLOAD_BYTES)
        {
            bail!("source exceeds the 4 GiB download limit");
        }
        let mut limited = response.take(MAX_DOWNLOAD_BYTES + 1);
        let copied =
            io::copy(&mut limited, &mut output).context("failed while downloading source")?;
        if copied > MAX_DOWNLOAD_BYTES {
            bail!("source exceeds the 4 GiB download limit");
        }
        output.flush()?;
        output.sync_all()?;
        drop(output);
        verify_file(&temporary, expected)?;
        fs::rename(&temporary, &destination).with_context(|| {
            format!(
                "failed to commit source cache entry {}",
                destination.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(destination)
}

fn https_redirect(attempt: Attempt<'_>) -> Action {
    if attempt.previous().len() >= 10 {
        return attempt.error("too many redirects");
    }
    if attempt.url().scheme() != "https" {
        return attempt.error("redirect would leave HTTPS");
    }
    attempt.follow()
}

fn ensure_cache(cache: &Path) -> Result<()> {
    if !cache.exists() {
        fs::create_dir_all(cache)
            .with_context(|| format!("failed to create source cache {}", cache.display()))?;
    }
    let metadata = fs::symlink_metadata(cache)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("source cache must be a real directory: {}", cache.display());
    }
    Ok(())
}

fn verify_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("source must be a regular file: {}", path.display());
    }
    Ok(())
}

pub fn verify_file(path: &Path, expected: &str) -> Result<()> {
    verify_regular_file(path)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        bail!("source digest mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}
