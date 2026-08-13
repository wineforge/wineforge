use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::{MappingAccess, MappingAction, MappingPlan};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReceipt {
    pub backup_directory: PathBuf,
    pub changed_drives: Vec<char>,
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("prefix is not a directory: {0}")]
    InvalidPrefix(PathBuf),
    #[error("prefix is locked by another operation: {0}")]
    Locked(PathBuf),
    #[error("read-only mapping for drive {0} requires a platform isolation backend")]
    ReadOnlyUnsupported(char),
    #[error("unsafe drive in mapping plan: {0}")]
    UnsafeDrive(char),
    #[error("unsafe host path for drive {drive}: {path} ({reason})")]
    UnsafeHostPath {
        drive: char,
        path: PathBuf,
        reason: &'static str,
    },
    #[error("managed directory must not be a symlink: {0}")]
    UnsafeManagedDirectory(PathBuf),
    #[error("failed to update {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("verification failed for drive {drive}: expected {expected}, observed {observed:?}")]
    Verification {
        drive: char,
        expected: PathBuf,
        observed: Option<PathBuf>,
    },
}

struct PrefixLock(PathBuf);

impl Drop for PrefixLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Applies an already-reviewed plan. All mutations are generated beneath `dosdevices` and
/// replaced prefix entries are moved into a receipt-specific backup directory.
pub fn apply_mapping_plan(prefix: &Path, plan: &MappingPlan) -> Result<ApplyReceipt, ApplyError> {
    if !is_real_directory(prefix) {
        return Err(ApplyError::InvalidPrefix(prefix.to_owned()));
    }
    validate_actions(plan)?;
    let state = prefix.join(".wineforge");
    ensure_real_directory(&state)?;
    let lock_path = state.join("mapping.lock");
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path);
    let _lock = match lock {
        Ok(_) => PrefixLock(lock_path.clone()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(ApplyError::Locked(lock_path));
        }
        Err(source) => return Err(io_error(&lock_path, source)),
    };

    let dosdevices = prefix.join("dosdevices");
    ensure_real_directory(&dosdevices)?;
    let backup = state.join("backups").join(unique_id());
    fs::create_dir_all(&backup).map_err(|source| io_error(&backup, source))?;
    let mut changed_drives = Vec::new();

    for action in &plan.actions {
        let drive = action_drive(action);
        let destination = dosdevices.join(format!("{}:", drive.to_ascii_lowercase()));
        let result = match action {
            MappingAction::Remove { .. } => backup_existing(&destination, &backup, drive),
            MappingAction::Create { host_path, .. } | MappingAction::Replace { host_path, .. } => {
                backup_existing(&destination, &backup, drive)?;
                atomic_symlink(host_path, &destination)
            }
        };
        if let Err(error) = result {
            rollback_changed(&dosdevices, &backup, &changed_drives);
            rollback_drive(&destination, &backup, drive);
            return Err(error);
        }
        changed_drives.push(drive);
    }

    if let Err(error) = verify_mappings(prefix, plan) {
        rollback_changed(&dosdevices, &backup, &changed_drives);
        return Err(error);
    }
    Ok(ApplyReceipt {
        backup_directory: backup,
        changed_drives,
    })
}

/// Verifies that creates/replacements point exactly to the desired host path and removals are gone.
pub fn verify_mappings(prefix: &Path, plan: &MappingPlan) -> Result<(), ApplyError> {
    let dosdevices = prefix.join("dosdevices");
    for action in &plan.actions {
        let drive = action_drive(action);
        let destination = dosdevices.join(format!("{}:", drive.to_ascii_lowercase()));
        let expected = match action {
            MappingAction::Create { host_path, .. } | MappingAction::Replace { host_path, .. } => {
                Some(host_path)
            }
            MappingAction::Remove { .. } => None,
        };
        let observed = fs::read_link(&destination).ok();
        let valid = match expected {
            Some(expected) => observed.as_deref() == Some(expected.as_path()),
            None => fs::symlink_metadata(&destination)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        };
        if !valid {
            return Err(ApplyError::Verification {
                drive,
                expected: expected.cloned().unwrap_or_default(),
                observed,
            });
        }
    }
    Ok(())
}

fn validate_actions(plan: &MappingPlan) -> Result<(), ApplyError> {
    for action in &plan.actions {
        let drive = action_drive(action);
        if !drive.is_ascii_alphabetic() || matches!(drive.to_ascii_uppercase(), 'A' | 'B' | 'C') {
            return Err(ApplyError::UnsafeDrive(drive));
        }
        let access = match action {
            MappingAction::Create { access, .. } | MappingAction::Replace { access, .. } => {
                Some(access)
            }
            MappingAction::Remove { .. } => None,
        };
        if access == Some(&MappingAccess::ReadOnly) {
            return Err(ApplyError::ReadOnlyUnsupported(drive));
        }
        if let MappingAction::Create { host_path, .. } | MappingAction::Replace { host_path, .. } =
            action
        {
            if !host_path.is_absolute() {
                return Err(ApplyError::UnsafeHostPath {
                    drive,
                    path: host_path.clone(),
                    reason: "path is not absolute",
                });
            }
            let canonical =
                fs::canonicalize(host_path).map_err(|source| io_error(host_path, source))?;
            if canonical.parent().is_none() {
                return Err(ApplyError::UnsafeHostPath {
                    drive,
                    path: host_path.clone(),
                    reason: "filesystem root mappings are forbidden",
                });
            }
            if !canonical.is_dir() {
                return Err(ApplyError::UnsafeHostPath {
                    drive,
                    path: host_path.clone(),
                    reason: "target is not a directory",
                });
            }
        }
    }
    Ok(())
}

fn action_drive(action: &MappingAction) -> char {
    match action {
        MappingAction::Create { drive, .. }
        | MappingAction::Replace { drive, .. }
        | MappingAction::Remove { drive, .. } => drive.to_ascii_uppercase(),
    }
}

fn backup_existing(destination: &Path, backup: &Path, drive: char) -> Result<(), ApplyError> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            let target = backup.join(format!("{}:", drive.to_ascii_lowercase()));
            fs::rename(destination, &target).map_err(|source| io_error(destination, source))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(destination, source)),
    }
}

fn rollback_changed(dosdevices: &Path, backup: &Path, changed: &[char]) {
    for drive in changed.iter().rev().copied() {
        let destination = dosdevices.join(format!("{}:", drive.to_ascii_lowercase()));
        rollback_drive(&destination, backup, drive);
    }
}

fn rollback_drive(destination: &Path, backup: &Path, drive: char) {
    if fs::symlink_metadata(destination).is_ok() {
        let _ = fs::remove_file(destination);
    }
    let saved = backup.join(format!("{}:", drive.to_ascii_lowercase()));
    if fs::symlink_metadata(&saved).is_ok() {
        let _ = fs::rename(saved, destination);
    }
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn ensure_real_directory(path: &Path) -> Result<(), ApplyError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ApplyError::UnsafeManagedDirectory(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| io_error(path, source))
        }
        Err(source) => Err(io_error(path, source)),
    }
}

#[cfg(unix)]
fn atomic_symlink(target: &Path, destination: &Path) -> Result<(), ApplyError> {
    use std::os::unix::fs::symlink;
    let temporary = destination.with_extension(format!("wineforge-{}", unique_id()));
    symlink(target, &temporary).map_err(|source| io_error(&temporary, source))?;
    if let Err(source) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(destination, source));
    }
    Ok(())
}

#[cfg(not(unix))]
fn atomic_symlink(_target: &Path, destination: &Path) -> Result<(), ApplyError> {
    Err(io_error(
        destination,
        io::Error::new(io::ErrorKind::Unsupported, "symlinks require a Unix host"),
    ))
}

fn unique_id() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{time}-{}-{sequence}", std::process::id())
}

fn io_error(path: &Path, source: io::Error) -> ApplyError {
    ApplyError::Io {
        path: path.to_owned(),
        source,
    }
}
