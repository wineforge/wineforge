use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use wineforge_core::Translation as EngineTranslation;
use wineforge_core::{ApplicationProfile, EngineManifest};
use zip::ZipArchive;

use crate::chocolatey;
use crate::download;
use crate::recipe::{
    ChocolateyMode, InstallStep, InstallerType, LicenseAcceptance, Postcondition, Recipe, Source,
    windows_path_to_prefix,
};
use crate::{
    add_wine_environment, add_winetricks_engine_environment, create_app_instance, engine_wine,
    prepare_private_runtime, sandbox, sanitize_prefix, verify_host_exposure,
    verify_managed_instance, write_json,
};

#[derive(Debug)]
enum PlannedStep {
    RunInstaller {
        source: PathBuf,
        source_sha256: String,
        installer_type: InstallerType,
        arguments: Vec<String>,
        success_exit_codes: Vec<i32>,
        origin: String,
    },
    CreateDirectory {
        path: String,
    },
    ExtractArchive {
        source: PathBuf,
        source_sha256: String,
        destination: String,
        strip_components: u8,
    },
    Winetricks {
        verbs: Vec<String>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallationReceipt<'a> {
    schema_version: u32,
    recipe_id: &'a str,
    recipe_version: &'a str,
    engine_id: &'a str,
    sources: BTreeMap<String, String>,
    verified_postconditions: usize,
}

pub fn inspect(recipe: &Recipe, recipe_path: &Path) -> Result<()> {
    println!("recipe\t{}\t{}\t{}", recipe.id, recipe.version, recipe.name);
    println!(
        "license\t{}\t{:?}",
        recipe.license.spdx, recipe.license.acceptance
    );
    for source in &recipe.sources {
        match source {
            Source::Remote { id, url, sha256 } => println!("source\t{id}\tremote\t{sha256}\t{url}"),
            Source::Repository { id, path, sha256 } => println!(
                "source\t{id}\trepository\t{sha256}\t{}",
                recipe_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
                    .display()
            ),
        }
    }
    for (index, step) in recipe.install.iter().enumerate() {
        println!("install\t{}\t{}", index + 1, step.action_name());
    }
    for (index, check) in recipe.verify.iter().enumerate() {
        match check {
            Postcondition::FileExists { path, .. } => {
                println!("verify\t{}\tfile-exists\t{path}", index + 1)
            }
            Postcondition::RegistryValueEquals { .. } => {
                println!("verify\t{}\tregistry-value-equals", index + 1)
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    recipe: &Recipe,
    recipe_path: &Path,
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    winetricks_command: &Path,
    cache: &Path,
    accept_license: bool,
) -> Result<()> {
    if recipe.license.acceptance == LicenseAcceptance::Required && !accept_license {
        bail!(
            "recipe license acceptance is required; review license.noticeUrl and pass --accept-license"
        );
    }
    if recipe.application.executable != profile.executable
        || recipe.application.arguments != profile.arguments
    {
        bail!("recipe application executable and arguments must match the selected profile");
    }
    validate_host_variant(recipe, engine)?;
    if fs::symlink_metadata(&profile.prefix).is_ok() {
        bail!(
            "recipe installation requires a fresh app instance: {}",
            profile.prefix.display()
        );
    }

    let recipe_dir = recipe_path.parent().unwrap_or_else(|| Path::new("."));
    let sources = recipe.source_map();
    let mut source_digests = BTreeMap::new();
    let mut plan = Vec::new();
    for step in &recipe.install {
        match step {
            InstallStep::RunInstaller {
                source,
                installer_type,
                arguments,
                success_exit_codes,
            } => {
                let declared = sources
                    .get(source.as_str())
                    .context("validated source disappeared")?;
                let resolved = download::resolve(declared, recipe_dir, cache)?;
                source_digests.insert(source.clone(), declared.sha256().to_owned());
                plan.push(PlannedStep::RunInstaller {
                    source: resolved,
                    source_sha256: declared.sha256().to_owned(),
                    installer_type: *installer_type,
                    arguments: arguments.clone(),
                    success_exit_codes: success_exit_codes.clone(),
                    origin: source.clone(),
                });
            }
            InstallStep::ChocolateyPackage {
                source,
                package_id,
                package_version,
                mode,
            } => {
                if *mode != ChocolateyMode::Translate {
                    bail!("only Chocolatey mode = \"translate\" is supported");
                }
                let declared = sources
                    .get(source.as_str())
                    .context("validated source disappeared")?;
                let package = download::resolve(declared, recipe_dir, cache)?;
                let translated = chocolatey::translate(&package, package_id, package_version)?;
                source_digests.insert(source.clone(), declared.sha256().to_owned());
                let installer_id = format!("{source}:vendor-installer");
                let installer_source = Source::Remote {
                    id: "translated.installer".to_owned(),
                    url: translated.url,
                    sha256: translated.sha256.clone(),
                };
                let installer = download::resolve(&installer_source, recipe_dir, cache)?;
                source_digests.insert(installer_id.clone(), translated.sha256.clone());
                plan.push(PlannedStep::RunInstaller {
                    source: installer,
                    source_sha256: translated.sha256,
                    installer_type: translated.installer_type,
                    arguments: translated.arguments,
                    success_exit_codes: translated.success_exit_codes,
                    origin: installer_id,
                });
            }
            InstallStep::CreateDirectory { path } => {
                plan.push(PlannedStep::CreateDirectory { path: path.clone() });
            }
            InstallStep::ExtractArchive {
                source,
                destination,
                strip_components,
            } => {
                let declared = sources
                    .get(source.as_str())
                    .context("validated source disappeared")?;
                let resolved = download::resolve(declared, recipe_dir, cache)?;
                source_digests.insert(source.clone(), declared.sha256().to_owned());
                plan.push(PlannedStep::ExtractArchive {
                    source: resolved,
                    source_sha256: declared.sha256().to_owned(),
                    destination: destination.clone(),
                    strip_components: *strip_components,
                });
            }
            InstallStep::Winetricks { verbs } => {
                plan.push(PlannedStep::Winetricks {
                    verbs: verbs.clone(),
                });
            }
            other => bail!(
                "native execution of {} is not implemented yet",
                other.action_name()
            ),
        }
    }

    create_app_instance(profile, engine, engine_root)?;
    let result = execute_plan(
        recipe,
        profile,
        engine,
        engine_root,
        winetricks_command,
        &plan,
        &source_digests,
    );
    if result.is_err()
        && fs::symlink_metadata(&profile.prefix)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        fs::remove_dir_all(&profile.prefix).with_context(|| {
            format!(
                "installation failed and the fresh prefix could not be removed: {}",
                profile.prefix.display()
            )
        })?;
    }
    result
}

fn validate_host_variant(recipe: &Recipe, engine: &EngineManifest) -> Result<()> {
    #[cfg(target_os = "macos")]
    let platform = "macos";
    #[cfg(target_os = "linux")]
    let platform = "linux";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let platform = std::env::consts::OS;
    let architecture = std::env::consts::ARCH;
    let translation = match engine.translation {
        EngineTranslation::Native => "native",
        EngineTranslation::Rosetta2 => "rosetta2",
    };
    if !recipe.variants.iter().any(|variant| {
        variant.platform == platform
            && variant.host_architecture == architecture
            && variant.translation == translation
    }) {
        bail!("recipe has no variant for {platform}/{architecture}/{translation}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_plan(
    recipe: &Recipe,
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    winetricks_command: &Path,
    plan: &[PlannedStep],
    source_digests: &BTreeMap<String, String>,
) -> Result<()> {
    verify_managed_instance(profile)?;
    prepare_private_runtime(profile)?;
    let wine = engine_wine(engine, engine_root)?;
    for (index, step) in plan.iter().enumerate() {
        match step {
            PlannedStep::RunInstaller {
                source,
                source_sha256,
                installer_type,
                arguments,
                success_exit_codes,
                origin,
            } => run_installer(
                profile,
                engine,
                engine_root,
                &wine,
                index,
                source,
                source_sha256,
                *installer_type,
                arguments,
                success_exit_codes,
                origin,
            )?,
            PlannedStep::CreateDirectory { path } => {
                let destination = windows_path_to_prefix(&profile.prefix, path)?;
                fs::create_dir_all(&destination)
                    .with_context(|| format!("failed to create {}", destination.display()))?;
            }
            PlannedStep::ExtractArchive {
                source,
                source_sha256,
                destination,
                strip_components,
            } => {
                download::verify_file(source, source_sha256)?;
                let destination = windows_path_to_prefix(&profile.prefix, destination)?;
                extract_zip_archive(source, &destination, *strip_components)?;
            }
            PlannedStep::Winetricks { verbs } => {
                let mut command = sandbox::command(profile, engine_root, winetricks_command)?;
                command.arg("-q").args(verbs);
                command.current_dir(profile.prefix.join("drive_c"));
                add_winetricks_engine_environment(&mut command, &wine)?;
                add_wine_environment(&mut command, profile, engine);
                let status = command.status().context("failed to start Winetricks")?;
                if !status.success() {
                    bail!("Winetricks installation step exited with {status}");
                }
            }
        }
        sanitize_prefix(&profile.prefix)?;
        verify_host_exposure(profile)?;
    }
    verify_postconditions(recipe, profile)?;
    let receipt = InstallationReceipt {
        schema_version: 1,
        recipe_id: &recipe.id,
        recipe_version: &recipe.version,
        engine_id: &engine.id,
        sources: source_digests.clone(),
        verified_postconditions: recipe.verify.len(),
    };
    write_json(
        &profile
            .prefix
            .join(".wineforge")
            .join("installation-receipt.json"),
        &receipt,
    )?;
    println!("installed recipe {} {}", recipe.id, recipe.version);
    Ok(())
}

const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_EXTRACTED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

fn extract_zip_archive(source: &Path, destination: &Path, strip_components: u8) -> Result<()> {
    ensure_real_directory(destination)?;
    let file = File::open(source)
        .with_context(|| format!("failed to open ZIP archive {}", source.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("invalid ZIP archive {}", source.display()))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("ZIP archive contains too many entries");
    }

    let mut extracted_bytes = 0_u64;
    let mut destinations = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        validate_zip_entry_type(&entry)?;
        let relative = safe_zip_path(entry.name(), strip_components)?;
        let Some(relative) = relative else {
            continue;
        };
        if !destinations.insert(relative.clone()) {
            bail!(
                "ZIP archive contains a duplicate destination: {}",
                relative.display()
            );
        }
        let output = destination.join(&relative);
        if entry.is_dir() {
            ensure_real_directory(&output)?;
            continue;
        }

        let parent = output
            .parent()
            .context("ZIP output has no parent directory")?;
        ensure_real_directory(parent)?;
        if let Ok(metadata) = fs::symlink_metadata(&output) {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "ZIP output would replace a non-regular file: {}",
                    output.display()
                );
            }
        }
        let remaining = MAX_EXTRACTED_BYTES
            .checked_sub(extracted_bytes)
            .context("ZIP archive exceeds the extracted size limit")?;
        let temporary = parent.join(format!(
            ".{}.wineforge-{}-{index}.part",
            output
                .file_name()
                .and_then(|name| name.to_str())
                .context("ZIP output filename is not UTF-8")?,
            std::process::id()
        ));
        let mut staged = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "failed to create extraction staging file {}",
                    temporary.display()
                )
            })?;
        let copy_result = (|| -> Result<u64> {
            let copied = std::io::copy(&mut entry.by_ref().take(remaining + 1), &mut staged)?;
            if copied > remaining {
                bail!("ZIP archive exceeds the 8 GiB extracted size limit");
            }
            staged.flush()?;
            staged.sync_all()?;
            Ok(copied)
        })();
        drop(staged);
        let copied = match copy_result {
            Ok(copied) => copied,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        if let Err(error) = fs::rename(&temporary, &output) {
            let _ = fs::remove_file(&temporary);
            return Err(error)
                .with_context(|| format!("failed to commit extracted file {}", output.display()));
        }
        extracted_bytes += copied;
    }
    Ok(())
}

fn safe_zip_path(name: &str, strip_components: u8) -> Result<Option<PathBuf>> {
    if name.contains('\0') || name.starts_with('/') || name.starts_with('\\') {
        bail!("unsafe absolute ZIP entry path: {name:?}");
    }
    let normalized = name.replace('\\', "/");
    let components: Vec<_> = normalized.trim_end_matches('/').split('/').collect();
    if components.iter().any(|component| {
        component.is_empty() || *component == "." || *component == ".." || component.contains(':')
    }) {
        bail!("unsafe ZIP entry path: {name:?}");
    }
    let stripped = components.get(usize::from(strip_components)..);
    let Some(stripped) = stripped.filter(|parts| !parts.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(stripped.iter().collect()))
}

fn validate_zip_entry_type(entry: &zip::read::ZipFile<'_>) -> Result<()> {
    if let Some(mode) = entry.unix_mode() {
        let file_type = mode & 0o170000;
        if file_type != 0 && file_type != 0o040000 && file_type != 0o100000 {
            bail!(
                "ZIP archive contains a link or special file: {:?}",
                entry.name()
            );
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "extraction path is not a real directory: {}",
                path.display()
            );
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .context("extraction directory has no parent")?;
    ensure_real_directory(parent)?;
    fs::create_dir(path)
        .with_context(|| format!("failed to create extraction directory {}", path.display()))
}

#[allow(clippy::too_many_arguments)]
fn run_installer(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    wine: &Path,
    index: usize,
    source: &Path,
    source_sha256: &str,
    installer_type: InstallerType,
    arguments: &[String],
    success_exit_codes: &[i32],
    origin: &str,
) -> Result<()> {
    download::verify_file(source, source_sha256)?;
    let stage = profile.prefix.join("drive_c/.wineforge-install");
    fs::create_dir_all(&stage)?;
    let extension = match installer_type {
        InstallerType::Exe => "exe",
        InstallerType::Msi => "msi",
    };
    let staged = stage.join(format!("step-{index}.{extension}"));
    if fs::symlink_metadata(&staged).is_ok() {
        bail!(
            "installer staging path already exists: {}",
            staged.display()
        );
    }
    fs::copy(source, &staged).context("failed to stage verified installer inside prefix")?;
    download::verify_file(&staged, source_sha256)?;
    let windows_path = format!("C:\\.wineforge-install\\step-{index}.{extension}");
    let result = (|| -> Result<()> {
        let mut command = sandbox::command(profile, engine_root, wine)?;
        match installer_type {
            InstallerType::Exe => {
                command.arg(&windows_path);
            }
            InstallerType::Msi => {
                command.args(["msiexec", "/i", &windows_path]);
            }
        }
        command.args(arguments);
        command.current_dir(profile.prefix.join("drive_c"));
        add_wine_environment(&mut command, profile, engine);
        let status = command
            .status()
            .with_context(|| format!("failed to start installer from {origin}"))?;
        let code = status
            .code()
            .context("installer terminated without an exit code")?;
        if !success_exit_codes.contains(&code) {
            bail!("installer from {origin} exited with unaccepted code {code}");
        }
        Ok(())
    })();
    let cleanup = fs::remove_file(&staged)
        .with_context(|| format!("failed to remove staged installer {}", staged.display()));
    if let Err(error) = result {
        let _ = cleanup;
        return Err(error);
    }
    cleanup?;
    if stage.read_dir()?.next().is_none() {
        fs::remove_dir(&stage)?;
    }
    Ok(())
}

fn verify_postconditions(recipe: &Recipe, profile: &ApplicationProfile) -> Result<()> {
    for condition in &recipe.verify {
        match condition {
            Postcondition::FileExists { path, sha256 } => {
                let resolved = windows_path_to_prefix(&profile.prefix, path)?;
                let metadata = fs::symlink_metadata(&resolved)
                    .with_context(|| format!("required installed file is missing: {path}"))?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!("required installed path is not a regular file: {path}");
                }
                if let Some(expected) = sha256 {
                    download::verify_file(&resolved, expected)?;
                }
            }
            Postcondition::RegistryValueEquals { .. } => {
                bail!("registry-value-equals verification is not implemented yet");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use zip::write::SimpleFileOptions;

    fn archive(entries: &[(&str, &[u8])]) -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(file.reopen().unwrap());
        for (name, contents) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
        file
    }

    #[test]
    fn zip_patch_safely_overlays_regular_files() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("application");
        let base = archive(&[("bin/app.exe", b"base"), ("data.txt", b"data")]);
        let patch = archive(&[("bin/app.exe", b"patched")]);

        extract_zip_archive(base.path(), &destination, 0).unwrap();
        extract_zip_archive(patch.path(), &destination, 0).unwrap();

        assert_eq!(
            fs::read(destination.join("bin/app.exe")).unwrap(),
            b"patched"
        );
        assert_eq!(fs::read(destination.join("data.txt")).unwrap(), b"data");
    }

    #[test]
    fn zip_strip_components_removes_the_declared_prefix() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("application");
        let source = archive(&[("release/bin/app.exe", b"application")]);

        extract_zip_archive(source.path(), &destination, 1).unwrap();

        assert_eq!(
            fs::read(destination.join("bin/app.exe")).unwrap(),
            b"application"
        );
        assert!(!destination.join("release").exists());
    }

    #[test]
    fn zip_traversal_is_rejected_without_writing_outside_destination() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("application");
        let source = archive(&[("../escaped.txt", b"escape")]);

        assert!(extract_zip_archive(source.path(), &destination, 0).is_err());
        assert!(!root.path().join("escaped.txt").exists());
    }

    #[test]
    fn zip_symlinks_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("application");
        let file = NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(file.reopen().unwrap());
        writer
            .add_symlink(
                "link",
                "../../outside",
                SimpleFileOptions::default().unix_permissions(0o777),
            )
            .unwrap();
        writer.finish().unwrap();

        assert!(extract_zip_archive(file.path(), &destination, 0).is_err());
        assert!(!destination.join("link").exists());
    }
}
