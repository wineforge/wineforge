use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::{ApplicationProfile, MappingAccess};

/// A mapping observed in the prefix before applying a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentMapping {
    pub drive: char,
    pub host_path: PathBuf,
    pub access: MappingAccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MappingAction {
    Create {
        drive: char,
        host_path: PathBuf,
        access: MappingAccess,
    },
    Replace {
        drive: char,
        old_host_path: PathBuf,
        host_path: PathBuf,
        access: MappingAccess,
    },
    Remove {
        drive: char,
        old_host_path: PathBuf,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MappingPlan {
    pub actions: Vec<MappingAction>,
}

/// Computes an ordered desired-state plan without touching the filesystem.
///
/// The caller should validate the profile first. Duplicate current entries are collapsed by
/// drive letter; the final one wins. Drive C is never managed by this function.
pub fn plan_mappings(profile: &ApplicationProfile, current: &[CurrentMapping]) -> MappingPlan {
    let desired: BTreeMap<char, _> = profile
        .mappings
        .iter()
        .filter_map(|mapping| mapping.normalized_drive().map(|drive| (drive, mapping)))
        .collect();
    let current: BTreeMap<char, _> = current
        .iter()
        .filter(|mapping| !mapping.drive.eq_ignore_ascii_case(&'C'))
        .map(|mapping| (mapping.drive.to_ascii_uppercase(), mapping))
        .collect();
    let drives: BTreeSet<char> = desired.keys().chain(current.keys()).copied().collect();
    let mut actions = Vec::new();

    for drive in drives {
        match (desired.get(&drive), current.get(&drive)) {
            (Some(wanted), None) => actions.push(MappingAction::Create {
                drive,
                host_path: wanted.host_path.clone(),
                access: wanted.access,
            }),
            (None, Some(existing)) => actions.push(MappingAction::Remove {
                drive,
                old_host_path: existing.host_path.clone(),
            }),
            (Some(wanted), Some(existing))
                if wanted.host_path != existing.host_path || wanted.access != existing.access =>
            {
                actions.push(MappingAction::Replace {
                    drive,
                    old_host_path: existing.host_path.clone(),
                    host_path: wanted.host_path.clone(),
                    access: wanted.access,
                });
            }
            _ => {}
        }
    }
    MappingPlan { actions }
}
