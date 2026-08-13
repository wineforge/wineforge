use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inspection {
    pub symlinks: Vec<SymlinkFinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymlinkFinding {
    pub path: PathBuf,
    pub target: PathBuf,
    pub resolved_target: PathBuf,
    pub escapes_prefix: bool,
    pub target_exists: bool,
}

#[derive(Debug, Error)]
pub enum InspectionError {
    #[error("prefix is not a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("failed to inspect {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Recursively inventories prefix symlinks without following them.
pub fn inspect_prefix(prefix: &Path) -> Result<Inspection, InspectionError> {
    let canonical_prefix = fs::canonicalize(prefix).map_err(|source| InspectionError::Io {
        path: prefix.to_owned(),
        source,
    })?;
    if !canonical_prefix.is_dir() {
        return Err(InspectionError::NotDirectory(prefix.to_owned()));
    }
    let mut symlinks = Vec::new();
    walk(prefix, &canonical_prefix, &mut symlinks)?;
    symlinks.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Inspection { symlinks })
}

fn walk(
    path: &Path,
    canonical_prefix: &Path,
    output: &mut Vec<SymlinkFinding>,
) -> Result<(), InspectionError> {
    let entries = fs::read_dir(path).map_err(|source| InspectionError::Io {
        path: path.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| InspectionError::Io {
            path: path.to_owned(),
            source,
        })?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child).map_err(|source| InspectionError::Io {
            path: child.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&child).map_err(|source| InspectionError::Io {
                path: child.clone(),
                source,
            })?;
            let unresolved = if target.is_absolute() {
                target.clone()
            } else {
                child.parent().unwrap_or(path).join(&target)
            };
            let canonical = fs::canonicalize(&unresolved);
            let target_exists = canonical.is_ok();
            let resolved_target = canonical.unwrap_or_else(|_| lexical_normalize(&unresolved));
            output.push(SymlinkFinding {
                path: child,
                target,
                escapes_prefix: !resolved_target.starts_with(canonical_prefix),
                resolved_target,
                target_exists,
            });
        } else if metadata.is_dir() {
            walk(&child, canonical_prefix, output)?;
        }
    }
    Ok(())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}
