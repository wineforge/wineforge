use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};

use thiserror::Error;

use crate::{
    ApplicationProfile, EngineManifest, HostMapping, IsolationMode, MappingAccess, Sha256Digest,
    Translation,
};

pub trait Validate {
    fn validate(&self) -> Result<(), ValidationErrors>;
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{field}: {message}")]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            error.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

fn push(errors: &mut Vec<ValidationError>, field: impl Into<String>, message: impl Into<String>) {
    errors.push(ValidationError {
        field: field.into(),
        message: message.into(),
    });
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn validate_environment(
    errors: &mut Vec<ValidationError>,
    field: &str,
    env: &crate::Environment,
    reject_launcher_controls: bool,
) {
    for (key, value) in &env.0 {
        let key_ok = !key.is_empty()
            && key.len() <= 128
            && key.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            });
        if !key_ok {
            push(
                errors,
                format!("{field}.{key}"),
                "invalid environment variable name",
            );
        }
        if value.contains('\0') {
            push(
                errors,
                format!("{field}.{key}"),
                "value contains a NUL byte",
            );
        }
        if value.len() > 32 * 1024 {
            push(
                errors,
                format!("{field}.{key}"),
                "value is unreasonably large",
            );
        }
        if reject_launcher_controls
            && matches!(
                key.as_str(),
                "DYLD_INSERT_LIBRARIES"
                    | "HOME"
                    | "LD_AUDIT"
                    | "LD_LIBRARY_PATH"
                    | "LD_PRELOAD"
                    | "PATH"
                    | "WINE"
                    | "WINEARCH"
                    | "WINELOADER"
                    | "WINEPREFIX"
                    | "WINESERVER"
            )
        {
            push(
                errors,
                format!("{field}.{key}"),
                "is controlled by the launcher and cannot be set by an application profile",
            );
        }
    }
}

fn is_filesystem_root(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
}

impl HostMapping {
    pub fn normalized_drive(&self) -> Option<char> {
        let mut chars = self.drive.chars();
        let drive = chars.next()?;
        (chars.next().is_none() && drive.is_ascii_alphabetic()).then(|| drive.to_ascii_uppercase())
    }
}

impl Validate for ApplicationProfile {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        if self.schema_version != 1 {
            push(
                &mut errors,
                "schema_version",
                "only schema version 1 is supported",
            );
        }
        if !valid_id(&self.id) {
            push(&mut errors, "id", "must be a lowercase identifier");
        }
        if self.name.trim().is_empty() {
            push(&mut errors, "name", "must not be blank");
        }
        if self.prefix.as_os_str().is_empty() || !self.prefix.is_absolute() {
            push(&mut errors, "prefix", "must be an absolute path");
        } else if is_filesystem_root(&self.prefix) {
            push(&mut errors, "prefix", "must not be a filesystem root");
        } else if self
            .prefix
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            push(&mut errors, "prefix", "must not contain `..` components");
        }
        if self.executable.trim().is_empty() || self.executable.contains('\0') {
            push(
                &mut errors,
                "executable",
                "must be a non-empty NUL-free Windows path",
            );
        }
        for (index, argument) in self.arguments.iter().enumerate() {
            if argument.contains('\0') {
                push(
                    &mut errors,
                    format!("arguments[{index}]"),
                    "contains a NUL byte",
                );
            }
        }
        if self.engines.is_empty() {
            push(
                &mut errors,
                "engines",
                "at least one platform engine is required",
            );
        }
        for (platform, selection) in &self.engines {
            if !valid_id(&selection.id) {
                push(
                    &mut errors,
                    format!("engines.{platform:?}.id"),
                    "must be a lowercase identifier",
                );
            }
            if let Some(root) = &selection.root {
                let field = format!("engines.{platform:?}.root");
                if !root.is_absolute() {
                    push(&mut errors, field, "must be absolute");
                } else if is_filesystem_root(root) {
                    push(&mut errors, field, "must not be a filesystem root");
                } else if root
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
                {
                    push(&mut errors, field, "must not contain `..` components");
                }
            }
        }
        validate_environment(&mut errors, "environment", &self.environment, true);

        let mut drives = BTreeSet::new();
        for (index, mapping) in self.mappings.iter().enumerate() {
            let field = format!("mappings[{index}]");
            let Some(drive) = mapping.normalized_drive() else {
                push(
                    &mut errors,
                    format!("{field}.drive"),
                    "must be one ASCII letter without a colon",
                );
                continue;
            };
            if matches!(drive, 'A' | 'B' | 'C') {
                push(
                    &mut errors,
                    format!("{field}.drive"),
                    "A, B, and C are reserved",
                );
            }
            if !drives.insert(drive) {
                push(&mut errors, format!("{field}.drive"), "drive is duplicated");
            }
            if !mapping.host_path.is_absolute() {
                push(
                    &mut errors,
                    format!("{field}.host_path"),
                    "must be absolute",
                );
            } else if is_filesystem_root(&mapping.host_path) {
                push(
                    &mut errors,
                    format!("{field}.host_path"),
                    "filesystem root mappings are forbidden",
                );
            } else if mapping
                .host_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                push(
                    &mut errors,
                    format!("{field}.host_path"),
                    "must not contain `..` components",
                );
            }
            if mapping.access == MappingAccess::ReadOnly
                && self.isolation.mode == IsolationMode::Disabled
            {
                push(
                    &mut errors,
                    format!("{field}.access"),
                    "read-only access requires operating-system isolation",
                );
            }
        }

        for left in 0..self.mappings.len() {
            for right in left + 1..self.mappings.len() {
                let left_mapping = &self.mappings[left];
                let right_mapping = &self.mappings[right];
                if left_mapping.access != right_mapping.access
                    && (left_mapping.host_path.starts_with(&right_mapping.host_path)
                        || right_mapping.host_path.starts_with(&left_mapping.host_path))
                {
                    push(
                        &mut errors,
                        format!("mappings[{right}].host_path"),
                        "overlaps a mapping with conflicting access",
                    );
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }
}

impl Validate for EngineManifest {
    fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        if self.schema_version != 1 {
            push(
                &mut errors,
                "schema_version",
                "only schema version 1 is supported",
            );
        }
        if !valid_id(&self.id) {
            push(&mut errors, "id", "must be a lowercase identifier");
        }
        if self.host_architecture != "x86_64" {
            push(
                &mut errors,
                "host_architecture",
                "only x86_64 engines are currently supported",
            );
        }
        if self.translation == Translation::Rosetta2
            && !matches!(self.platform, crate::Platform::MacosX86_64)
        {
            push(
                &mut errors,
                "translation",
                "Rosetta 2 is only valid for macOS x86_64 engines",
            );
        }
        if self.wine_binary.is_absolute()
            || self.wine_binary.as_os_str().is_empty()
            || self
                .wine_binary
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            push(
                &mut errors,
                "wine_binary",
                "must be a non-empty relative path without `..`",
            );
        }
        validate_environment(&mut errors, "environment", &self.environment, false);
        validate_sha256(&mut errors, "artifact.sha256", &self.artifact.sha256);
        if self.license.name.trim().is_empty() {
            push(&mut errors, "license.name", "must not be blank");
        }
        if let Some(document) = &self.capabilities {
            validate_engine_capabilities(&mut errors, self, document);
        }
        for (name, composition) in &self.composed_capabilities {
            if !crate::valid_capability_name(name) {
                push(
                    &mut errors,
                    format!("composed_capabilities.{name}"),
                    "invalid capability name",
                );
            }
            if composition.version == 0 {
                push(
                    &mut errors,
                    format!("composed_capabilities.{name}.version"),
                    "must be at least 1",
                );
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }
}

fn validate_engine_capabilities(
    errors: &mut Vec<ValidationError>,
    manifest: &EngineManifest,
    document: &crate::EngineCapabilityDocument,
) {
    if document.schema_version != 1 || document.protocol != 1 {
        push(
            errors,
            "capabilities",
            "only schema and protocol version 1 are supported",
        );
    }
    if document.engine_id != manifest.id {
        push(
            errors,
            "capabilities.engine_id",
            "must match the engine manifest id",
        );
    }
    let expected_target = match manifest.platform {
        crate::Platform::MacosX86_64 => crate::EngineCapabilityTarget::MacosX86_64,
        crate::Platform::LinuxX86_64 => crate::EngineCapabilityTarget::LinuxX86_64,
    };
    if document.target != expected_target {
        push(
            errors,
            "capabilities.target",
            "must match the engine manifest platform",
        );
    }
    let mut ids = BTreeSet::new();
    for (index, declaration) in document.provided.iter().enumerate() {
        let field = format!("capabilities.provided[{index}]");
        if !crate::valid_capability_name(&declaration.id) {
            push(errors, format!("{field}.id"), "invalid capability name");
        } else if !ids.insert(&declaration.id) {
            push(errors, format!("{field}.id"), "capability is duplicated");
        }
        if declaration.version == 0 {
            push(errors, format!("{field}.version"), "must be at least 1");
        }
        if declaration.evidence_patches.is_empty() {
            push(
                errors,
                format!("{field}.evidence_patches"),
                "must not be empty",
            );
        }
        for patch in &declaration.evidence_patches {
            if patch.is_absolute()
                || patch.as_os_str().is_empty()
                || patch
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                push(
                    errors,
                    format!("{field}.evidence_patches"),
                    "must contain safe relative paths",
                );
            }
        }
        if !declaration.targets.contains(&expected_target) {
            push(
                errors,
                format!("{field}.targets"),
                "must include the document target",
            );
        }
        if let Some(transport) = &declaration.transport {
            if transport.variables.is_empty() {
                push(
                    errors,
                    format!("{field}.transport.variables"),
                    "must not be empty",
                );
            }
            let mut variables = BTreeSet::new();
            for variable in &transport.variables {
                if !variables.insert(variable)
                    || !variable.starts_with("WINEFORGE_")
                    || !variable.bytes().all(|byte| {
                        byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit()
                    })
                {
                    push(
                        errors,
                        format!("{field}.transport.variables"),
                        "contains an invalid or duplicate variable",
                    );
                }
            }
        }
        if declaration.id == "macos.window-isolation.strict" {
            let privacy_ok = declaration.scope == Some(crate::EngineCapabilityScope::Process)
                && declaration.privacy.as_ref().is_some_and(|privacy| {
                    !privacy.requires_external_window_observation
                        && !privacy.requires_accessibility
                        && !privacy.requires_screen_recording
                        && !privacy.requires_input_monitoring
                });
            if !privacy_ok {
                push(
                    errors,
                    field,
                    "strict window isolation must declare process-local, permission-free privacy properties",
                );
            }
        }
    }
}

fn validate_sha256(errors: &mut Vec<ValidationError>, field: &str, digest: &Sha256Digest) {
    if digest.0.len() != 64
        || !digest
            .0
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        push(
            errors,
            field,
            "must contain exactly 64 lowercase hexadecimal characters",
        );
    }
}
