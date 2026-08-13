use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use wineforge_core::{ApplicationProfile, IsolationMode, MappingAccess};

pub(crate) fn command(
    profile: &ApplicationProfile,
    engine_root: &Path,
    executable: &Path,
) -> Result<Command> {
    if profile.isolation.mode == IsolationMode::Disabled {
        return Ok(Command::new(executable));
    }

    #[cfg(target_os = "macos")]
    {
        macos_command(profile, engine_root, executable)
    }

    #[cfg(target_os = "linux")]
    {
        linux_command(profile, engine_root, executable)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("required filesystem isolation is unsupported on this platform")
    }
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("invalid {label} directory: {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf> {
    let resolved = if path.components().count() == 1 {
        resolve_from_path(path)
            .with_context(|| format!("{label} was not found: {}", path.display()))?
    } else {
        path.to_owned()
    };
    let canonical = fs::canonicalize(&resolved)
        .with_context(|| format!("invalid {label}: {}", resolved.display()))?;
    if !canonical.is_file() {
        bail!("{label} is not a file: {}", canonical.display());
    }
    Ok(canonical)
}

fn canonical_mappings(
    profile: &ApplicationProfile,
    protected_paths: &[&Path],
) -> Result<Vec<(PathBuf, MappingAccess)>> {
    let mut mappings: Vec<(PathBuf, MappingAccess)> = Vec::new();
    for mapping in &profile.mappings {
        let path = canonical_directory(&mapping.host_path, "mapped host")?;
        for protected in protected_paths {
            if path.starts_with(protected) || protected.starts_with(&path) {
                bail!(
                    "mapped host path overlaps a protected runtime path: {} and {}",
                    path.display(),
                    protected.display()
                );
            }
        }
        for (existing, access) in &mappings {
            if mapping.access != *access
                && (path.starts_with(existing) || existing.starts_with(&path))
            {
                bail!(
                    "overlapping mappings request conflicting access: {} and {}",
                    existing.display(),
                    path.display()
                );
            }
        }
        mappings.push((path, mapping.access));
    }
    Ok(mappings)
}

fn resolve_from_path(executable: &Path) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|value| {
        env::split_paths(&value)
            .map(|directory| directory.join(executable))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(target_os = "macos")]
fn macos_command(
    profile: &ApplicationProfile,
    engine_root: &Path,
    executable: &Path,
) -> Result<Command> {
    use std::os::unix::fs::MetadataExt;

    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
    if !Path::new(SANDBOX_EXEC).is_file() {
        bail!("required macOS isolation backend is unavailable: {SANDBOX_EXEC}");
    }

    let prefix = canonical_directory(&profile.prefix, "prefix")?;
    let engine = canonical_directory(engine_root, "engine root")?;
    let executable = canonical_file(executable, "sandboxed executable")?;
    let uid = fs::metadata(&prefix)?.uid();

    let mut readable = vec![prefix.clone(), engine.clone(), executable.clone()];
    let mut writable = vec![prefix.clone()];
    let mut read_only = Vec::new();
    for (path, access) in canonical_mappings(profile, &[&prefix, &engine, &executable])? {
        readable.push(path.clone());
        if access == MappingAccess::ReadWrite {
            writable.push(path);
        } else {
            read_only.push(path);
        }
    }

    // Wine's server uses this shared parent on macOS. It is outside the protected
    // user-data roots below, but retaining the concrete path documents that fact.
    readable.push(PathBuf::from(format!("/private/tmp/.wine-{uid}")));
    writable.push(PathBuf::from(format!("/private/tmp/.wine-{uid}")));

    let policy = macos_policy(&readable, &writable, &read_only)?;
    let mut command = Command::new(SANDBOX_EXEC);
    command.arg("-p").arg(policy).arg(executable);
    Ok(command)
}

#[cfg(target_os = "macos")]
fn macos_policy(
    readable: &[PathBuf],
    writable: &[PathBuf],
    read_only: &[PathBuf],
) -> Result<String> {
    fn requirement(paths: &[PathBuf]) -> Result<String> {
        let entries = paths
            .iter()
            .map(|path| {
                path.to_str()
                    .context("macOS sandbox paths must be valid UTF-8")
                    .map(|path| format!("(subpath \"{}\")", seatbelt_escape(path)))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!("(require-any {})", entries.join(" ")))
    }

    let readable = requirement(readable)?;
    let writable = requirement(writable)?;
    let mut policy = String::from("(version 1)\n(allow default)\n");
    for protected_root in ["/Users", "/Applications", "/Volumes", "/Network"] {
        policy.push_str(&format!(
            "(deny file-read-data (require-all (subpath \"{protected_root}\") (require-not {readable})))\n\
             (deny file-write* (require-all (subpath \"{protected_root}\") (require-not {writable})))\n"
        ));
    }
    for path in read_only {
        let path = path
            .to_str()
            .context("macOS sandbox paths must be valid UTF-8")?;
        policy.push_str(&format!(
            "(deny file-write* (subpath \"{}\"))\n",
            seatbelt_escape(path)
        ));
    }
    Ok(policy)
}

#[cfg(target_os = "macos")]
fn seatbelt_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn linux_command(
    profile: &ApplicationProfile,
    engine_root: &Path,
    executable: &Path,
) -> Result<Command> {
    let bwrap = canonical_file(Path::new("bwrap"), "Bubblewrap executable")?;
    let prefix = canonical_directory(&profile.prefix, "prefix")?;
    let engine = canonical_directory(engine_root, "engine root")?;
    let executable = canonical_file(executable, "sandboxed executable")?;

    let mut command = Command::new(bwrap);
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-ipc",
    ]);
    for root in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt"] {
        if Path::new(root).exists() {
            command.arg("--ro-bind").arg(root).arg(root);
        }
    }
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    if Path::new("/tmp/.X11-unix").exists() {
        command.args(["--ro-bind", "/tmp/.X11-unix", "/tmp/.X11-unix"]);
    }
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        if runtime.is_dir() {
            command.arg("--ro-bind").arg(&runtime).arg(&runtime);
        }
    }
    command.arg("--ro-bind").arg(&engine).arg(&engine);
    if !executable.starts_with(&engine) {
        command.arg("--ro-bind").arg(&executable).arg(&executable);
    }
    command.arg("--bind").arg(&prefix).arg(&prefix);
    for (path, access) in canonical_mappings(profile, &[&prefix, &engine, &executable])? {
        command.arg(if access == MappingAccess::ReadOnly {
            "--ro-bind"
        } else {
            "--bind"
        });
        command.arg(&path).arg(&path);
    }
    command.arg("--chdir").arg(prefix.join("drive_c"));
    command.arg("--").arg(executable);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_policy_distinguishes_read_only_and_read_write_roots() {
        let policy = macos_policy(
            &[
                PathBuf::from("/Users/example/read only"),
                PathBuf::from("/Users/example/read-write"),
            ],
            &[PathBuf::from("/Users/example/read-write")],
            &[PathBuf::from("/Users/example/read only")],
        )
        .unwrap();
        assert!(policy.contains("(deny file-read-data"));
        assert!(policy.contains("(deny file-write*"));
        assert!(policy.contains("/Users/example/read only"));
        assert!(policy.contains("/Users/example/read-write"));
        assert!(policy.contains("/Applications"));
        assert!(policy.contains("/Volumes"));
        assert!(policy.contains("/Network"));
        assert!(policy.contains("(deny file-write* (subpath \"/Users/example/read only\"))"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_strings_are_escaped() {
        assert_eq!(seatbelt_escape("/tmp/a\\b\"c"), "/tmp/a\\\\b\\\"c");
    }
}
