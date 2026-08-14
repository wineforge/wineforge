use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;
use wineforge_core::{
    ApplicationProfile, ApplyError, Artifact, ArtifactSource, CurrentMapping, EngineManifest,
    EngineSelection, Environment, HostMapping, License, MappingAccess, MappingAction, Platform,
    Sha256Digest, Translation, Validate, apply_mapping_plan, inspect_prefix, plan_mappings,
    verify_mappings,
};

fn profile(prefix: &Path) -> ApplicationProfile {
    ApplicationProfile {
        schema_version: 1,
        id: "sample-editor".into(),
        name: "Sample Editor".into(),
        prefix: prefix.to_owned(),
        executable: r"C:\Program Files\Sample\editor.exe".into(),
        arguments: vec!["--safe-mode".into(), "a value with spaces".into()],
        engines: BTreeMap::from([(
            Platform::MacosX86_64,
            EngineSelection {
                id: "verified-wine-10".into(),
            },
        )]),
        environment: Environment(BTreeMap::from([("WINEDEBUG".into(), "-all".into())])),
        mappings: vec![HostMapping {
            drive: "W".into(),
            host_path: prefix
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join("workspace"),
            access: MappingAccess::ReadWrite,
        }],
        isolation: wineforge_core::IsolationPolicy::default(),
    }
}

#[test]
fn profile_schema_rejects_unknown_fields_and_shell_form_arguments() {
    let unknown = r#"{
        "schema_version":1,"id":"sample","name":"Sample","prefix":"/tmp/sample",
        "executable":"sample.exe","arguments":[],"engines":{},"surprise":true
    }"#;
    assert!(serde_json::from_str::<ApplicationProfile>(unknown).is_err());

    let shell_form = r#"{
        "schema_version":1,"id":"sample","name":"Sample","prefix":"/tmp/sample",
        "executable":"sample.exe","arguments":"--one --two","engines":{}
    }"#;
    assert!(serde_json::from_str::<ApplicationProfile>(shell_form).is_err());
}

#[test]
fn profile_validation_rejects_root_reserved_duplicate_and_unsafe_environment() {
    let mut value = profile(Path::new("/"));
    value.environment.0.insert("BAD-NAME".into(), "ok".into());
    value
        .environment
        .0
        .insert("LD_PRELOAD".into(), "/tmp/module".into());
    value.mappings = vec![
        HostMapping {
            drive: "c".into(),
            host_path: PathBuf::from("/"),
            access: MappingAccess::ReadWrite,
        },
        HostMapping {
            drive: "C".into(),
            host_path: PathBuf::from("relative"),
            access: MappingAccess::ReadWrite,
        },
    ];
    let errors = value.validate().unwrap_err().to_string();
    assert!(errors.contains("filesystem root"));
    assert!(errors.contains("reserved"));
    assert!(errors.contains("duplicated"));
    assert!(errors.contains("invalid environment"));
    assert!(errors.contains("controlled by the launcher"));
    assert!(errors.contains("must be absolute"));
}

#[test]
fn profile_schema_rejects_winetricks_provisioning() {
    let profile = r#"{
        "schema_version":1,"id":"sample","name":"Sample","prefix":"/tmp/sample",
        "executable":"sample.exe","arguments":[],"engines":{},
        "winetricks":["corefonts"]
    }"#;
    assert!(serde_json::from_str::<ApplicationProfile>(profile).is_err());
}

#[test]
fn read_only_mapping_requires_isolation() {
    let temp = tempdir().unwrap();
    let mut value = profile(&temp.path().join("prefix"));
    value.mappings[0].access = MappingAccess::ReadOnly;
    value.isolation.mode = wineforge_core::IsolationMode::Disabled;

    let errors = value.validate().unwrap_err().to_string();
    assert!(errors.contains("read-only access requires operating-system isolation"));

    value.isolation.mode = wineforge_core::IsolationMode::Required;
    value.validate().unwrap();
}

#[test]
fn conflicting_nested_mapping_access_is_rejected() {
    let temp = tempdir().unwrap();
    let mut value = profile(&temp.path().join("prefix"));
    let parent = temp.path().join("workspace");
    value.mappings = vec![
        HostMapping {
            drive: "R".into(),
            host_path: parent.clone(),
            access: MappingAccess::ReadOnly,
        },
        HostMapping {
            drive: "W".into(),
            host_path: parent.join("writable"),
            access: MappingAccess::ReadWrite,
        },
    ];
    assert!(
        value
            .validate()
            .unwrap_err()
            .to_string()
            .contains("overlaps a mapping with conflicting access")
    );
}

#[test]
fn engine_schema_and_validation_are_strict() {
    let manifest = EngineManifest {
        schema_version: 1,
        id: "verified-wine-10".into(),
        platform: Platform::MacosX86_64,
        host_architecture: "x86_64".into(),
        translation: Translation::Rosetta2,
        artifact: Artifact {
            source: ArtifactSource::DirectDownload {
                url: "https://example.invalid/wine.tar.xz".parse().unwrap(),
            },
            sha256: Sha256Digest("a".repeat(64)),
        },
        wine_binary: "bin/wine".into(),
        environment: Environment::default(),
        license: License {
            name: "Example License".into(),
            url: "https://example.invalid/license".parse().unwrap(),
            acceptance_required: false,
        },
    };
    manifest.validate().unwrap();
    let mut invalid = manifest;
    invalid.artifact.sha256 = Sha256Digest("ABC".into());
    invalid.wine_binary = PathBuf::from("../wine");
    assert_eq!(invalid.validate().unwrap_err().0.len(), 2);

    let json = serde_json::to_string(&invalid).unwrap();
    let with_extra = json.strip_suffix('}').unwrap().to_owned() + ",\"extra\":1}";
    assert!(serde_json::from_str::<EngineManifest>(&with_extra).is_err());
}

#[test]
fn planner_is_deterministic_and_minimal() {
    let temp = tempdir().unwrap();
    let mut desired = profile(&temp.path().join("prefix"));
    desired.mappings.push(HostMapping {
        drive: "x".into(),
        host_path: temp.path().join("new-x"),
        access: MappingAccess::ReadWrite,
    });
    let current = vec![
        CurrentMapping {
            drive: 'W',
            host_path: temp.path().join("workspace"),
            access: MappingAccess::ReadWrite,
        },
        CurrentMapping {
            drive: 'X',
            host_path: temp.path().join("old-x"),
            access: MappingAccess::ReadWrite,
        },
        CurrentMapping {
            drive: 'Y',
            host_path: temp.path().join("old-y"),
            access: MappingAccess::ReadWrite,
        },
        CurrentMapping {
            drive: 'C',
            host_path: temp.path().join("drive-c"),
            access: MappingAccess::ReadWrite,
        },
    ];
    let plan = plan_mappings(&desired, &current);
    assert_eq!(
        plan.actions,
        vec![
            MappingAction::Replace {
                drive: 'X',
                old_host_path: temp.path().join("old-x"),
                host_path: temp.path().join("new-x"),
                access: MappingAccess::ReadWrite,
            },
            MappingAction::Remove {
                drive: 'Y',
                old_host_path: temp.path().join("old-y")
            },
        ]
    );
}

#[cfg(unix)]
#[test]
fn inspector_reports_internal_external_and_dangling_links_without_following() {
    use std::os::unix::fs::symlink;
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let outside = temp.path().join("outside");
    fs::create_dir_all(prefix.join("inside")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink("inside", prefix.join("internal")).unwrap();
    symlink(&outside, prefix.join("external")).unwrap();
    symlink("../missing", prefix.join("dangling")).unwrap();

    let report = inspect_prefix(&prefix).unwrap();
    assert_eq!(report.symlinks.len(), 3);
    let internal = report
        .symlinks
        .iter()
        .find(|item| item.path.ends_with("internal"))
        .unwrap();
    assert!(!internal.escapes_prefix && internal.target_exists);
    let external = report
        .symlinks
        .iter()
        .find(|item| item.path.ends_with("external"))
        .unwrap();
    assert!(external.escapes_prefix && external.target_exists);
    let dangling = report
        .symlinks
        .iter()
        .find(|item| item.path.ends_with("dangling"))
        .unwrap();
    assert!(dangling.escapes_prefix && !dangling.target_exists);
}

#[cfg(unix)]
#[test]
fn apply_backs_up_existing_entry_and_verifies_result() {
    use std::os::unix::fs::symlink;
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let old = temp.path().join("old");
    let new = temp.path().join("new");
    fs::create_dir_all(prefix.join("dosdevices")).unwrap();
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    symlink(&old, prefix.join("dosdevices/w:")).unwrap();
    let plan = wineforge_core::MappingPlan {
        actions: vec![MappingAction::Replace {
            drive: 'W',
            old_host_path: old.clone(),
            host_path: new.clone(),
            access: MappingAccess::ReadWrite,
        }],
    };

    let receipt = apply_mapping_plan(&prefix, &plan).unwrap();
    assert_eq!(fs::read_link(prefix.join("dosdevices/w:")).unwrap(), new);
    assert_eq!(
        fs::read_link(receipt.backup_directory.join("w:")).unwrap(),
        old
    );
    assert!(!prefix.join(".wineforge/mapping.lock").exists());
    verify_mappings(&prefix, &plan).unwrap();
}

#[test]
fn apply_rejects_reserved_actions_before_mutation() {
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    fs::create_dir(&prefix).unwrap();
    let reserved = wineforge_core::MappingPlan {
        actions: vec![MappingAction::Remove {
            drive: 'C',
            old_host_path: temp.path().join("old"),
        }],
    };
    assert!(matches!(
        apply_mapping_plan(&prefix, &reserved),
        Err(ApplyError::UnsafeDrive('C'))
    ));
    assert!(!prefix.join("dosdevices").exists());
}

#[test]
fn apply_honors_an_existing_prefix_lock() {
    let temp = tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    fs::create_dir_all(prefix.join(".wineforge")).unwrap();
    fs::write(prefix.join(".wineforge/mapping.lock"), b"held").unwrap();
    let plan = wineforge_core::MappingPlan::default();
    assert!(matches!(
        apply_mapping_plan(&prefix, &plan),
        Err(ApplyError::Locked(_))
    ));
}
