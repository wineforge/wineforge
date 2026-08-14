use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recipe {
    pub schema_version: u32,
    pub id: String,
    pub version: String,
    pub name: String,
    pub summary: String,
    pub homepage: Option<String>,
    pub license: License,
    pub application: Application,
    pub variants: Vec<Variant>,
    pub runtime_access: RuntimeAccess,
    pub sources: Vec<Source>,
    pub install: Vec<InstallStep>,
    pub verify: Vec<Postcondition>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct License {
    pub spdx: String,
    pub notice_url: Option<String>,
    pub acceptance: LicenseAcceptance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseAcceptance {
    None,
    Required,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Application {
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Variant {
    pub platform: String,
    pub host_architecture: String,
    pub translation: String,
    pub engine: VariantEngine,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VariantEngine {
    pub family: String,
    pub version: String,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeAccess {
    pub network: String,
    #[serde(default)]
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Source {
    Remote {
        id: String,
        url: String,
        sha256: String,
    },
    Repository {
        id: String,
        path: PathBuf,
        sha256: String,
    },
}

impl Source {
    pub fn id(&self) -> &str {
        match self {
            Self::Remote { id, .. } | Self::Repository { id, .. } => id,
        }
    }

    pub fn sha256(&self) -> &str {
        match self {
            Self::Remote { sha256, .. } | Self::Repository { sha256, .. } => sha256,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InstallStep {
    RunInstaller {
        source: String,
        #[serde(rename = "archiveMember", default)]
        archive_member: Option<String>,
        #[serde(rename = "installerType")]
        installer_type: InstallerType,
        arguments: Vec<String>,
        #[serde(rename = "successExitCodes")]
        success_exit_codes: Vec<i32>,
    },
    ChocolateyPackage {
        source: String,
        #[serde(rename = "packageId")]
        package_id: String,
        #[serde(rename = "packageVersion")]
        package_version: String,
        mode: ChocolateyMode,
    },
    CreateDirectory {
        path: String,
    },
    CopyFile {
        source: String,
        destination: String,
    },
    ExtractArchive {
        source: String,
        destination: String,
        #[serde(rename = "stripComponents", default)]
        strip_components: u8,
    },
    SetRegistryValue {
        hive: String,
        key: String,
        name: String,
        #[serde(rename = "valueType")]
        value_type: String,
        value: toml::Value,
    },
    Winetricks {
        verbs: Vec<String>,
    },
}

impl InstallStep {
    pub fn source(&self) -> Option<&str> {
        match self {
            Self::RunInstaller { source, .. }
            | Self::ChocolateyPackage { source, .. }
            | Self::CopyFile { source, .. }
            | Self::ExtractArchive { source, .. } => Some(source),
            _ => None,
        }
    }

    pub fn action_name(&self) -> &'static str {
        match self {
            Self::RunInstaller { .. } => "run-installer",
            Self::ChocolateyPackage { .. } => "chocolatey-package",
            Self::CreateDirectory { .. } => "create-directory",
            Self::CopyFile { .. } => "copy-file",
            Self::ExtractArchive { .. } => "extract-archive",
            Self::SetRegistryValue { .. } => "set-registry-value",
            Self::Winetricks { .. } => "winetricks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallerType {
    Exe,
    Msi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChocolateyMode {
    Translate,
    SandboxedScript,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "check", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Postcondition {
    FileExists {
        path: String,
        sha256: Option<String>,
    },
    RegistryValueEquals {
        hive: String,
        key: String,
        name: String,
        value: toml::Value,
    },
}

pub fn read(path: &Path) -> Result<Recipe> {
    if path.extension().and_then(|value| value.to_str()) != Some("toml") {
        bail!("recipes must use TOML: {}", path.display());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read recipe {}", path.display()))?;
    let recipe: Recipe = toml::from_str(&text)
        .with_context(|| format!("invalid recipe TOML in {}", path.display()))?;
    recipe.validate(path)?;
    Ok(recipe)
}

impl Recipe {
    pub fn validate(&self, path: &Path) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported recipe schemaVersion {}", self.schema_version);
        }
        validate_identifier(&self.id, "recipe id")?;
        if self.version.trim().is_empty()
            || self.name.trim().is_empty()
            || self.summary.trim().is_empty()
        {
            bail!("recipe version, name, and summary must not be empty");
        }
        if self.license.spdx.trim().is_empty() {
            bail!("license.spdx must not be empty");
        }
        if self.license.acceptance == LicenseAcceptance::Required
            && self.license.notice_url.as_deref().is_none_or(str::is_empty)
        {
            bail!("license.noticeUrl is required when acceptance is required");
        }
        if self.variants.is_empty() || self.verify.is_empty() {
            bail!("recipes require at least one variant and verification postcondition");
        }
        validate_windows_path(&self.application.executable, "application.executable")?;

        let mut source_ids = BTreeSet::new();
        for source in &self.sources {
            validate_source_identifier(source.id(), "source id")?;
            if !source_ids.insert(source.id()) {
                bail!("duplicate source id {}", source.id());
            }
            validate_sha256(source.sha256(), "source sha256")?;
            match source {
                Source::Remote { url, .. } => validate_https_url(url)?,
                Source::Repository { path: relative, .. } => {
                    if relative.is_absolute()
                        || relative
                            .components()
                            .any(|component| matches!(component, std::path::Component::ParentDir))
                    {
                        bail!("repository source path must remain beneath the recipe directory");
                    }
                    let parent = path.parent().unwrap_or_else(|| Path::new("."));
                    let candidate = parent.join(relative);
                    if candidate.exists() && !candidate.is_file() {
                        bail!(
                            "repository source is not a regular file: {}",
                            candidate.display()
                        );
                    }
                }
            }
        }
        for step in &self.install {
            if let Some(source) = step.source() {
                if !source_ids.contains(source) {
                    bail!("{} references unknown source {source}", step.action_name());
                }
            }
            if let InstallStep::ChocolateyPackage {
                package_id,
                package_version,
                mode,
                ..
            } = step
            {
                validate_package_component(package_id, "packageId")?;
                validate_package_component(package_version, "packageVersion")?;
                if *mode == ChocolateyMode::SandboxedScript {
                    bail!(
                        "sandboxed-script Chocolatey execution is not implemented; use mode = \"translate\""
                    );
                }
            }
            if let InstallStep::RunInstaller {
                archive_member,
                success_exit_codes,
                ..
            } = step
            {
                if success_exit_codes.is_empty() {
                    bail!("run-installer requires at least one successExitCodes value");
                }
                if let Some(member) = archive_member {
                    if member.is_empty()
                        || member.len() > 1024
                        || member.contains('\\')
                        || member.contains('\0')
                        || member.split('/').any(|component| {
                            component.is_empty() || matches!(component, "." | "..")
                        })
                    {
                        bail!("run-installer archiveMember must be a safe ZIP member path");
                    }
                }
            }
            if let InstallStep::CopyFile { destination, .. } = step {
                validate_install_path(destination, "copy-file destination")?;
            }
            if let InstallStep::Winetricks { verbs } = step {
                if verbs.is_empty() || verbs.len() > 32 {
                    bail!("winetricks requires between 1 and 32 verbs");
                }
                let mut unique = BTreeSet::new();
                for verb in verbs {
                    let valid = verb.len() <= 64
                        && !verb.is_empty()
                        && verb
                            .as_bytes()
                            .first()
                            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                        && verb.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || matches!(byte, b'_' | b'-' | b'.')
                        });
                    if !valid {
                        bail!("invalid Winetricks recipe verb {verb:?}");
                    }
                    if !unique.insert(verb) {
                        bail!("duplicate Winetricks recipe verb {verb}");
                    }
                }
            }
        }
        for check in &self.verify {
            if let Postcondition::FileExists { path, sha256 } = check {
                validate_windows_path(path, "verify.path")?;
                if let Some(digest) = sha256 {
                    validate_sha256(digest, "verify sha256")?;
                }
            }
        }
        Ok(())
    }

    pub fn source_map(&self) -> BTreeMap<&str, &Source> {
        self.sources
            .iter()
            .map(|source| (source.id(), source))
            .collect()
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    let pattern = Regex::new(r"^[a-z0-9]+(?:[.-][a-z0-9]+)+$").expect("static regex");
    if !pattern.is_match(value) {
        bail!("invalid {label}: {value}");
    }
    Ok(())
}

fn validate_source_identifier(value: &str, label: &str) -> Result<()> {
    let pattern = Regex::new(r"^[a-z][a-z0-9-]{0,63}$").expect("static regex");
    if !pattern.is_match(value) {
        bail!("invalid {label}: {value}");
    }
    Ok(())
}

fn validate_package_component(value: &str, label: &str) -> Result<()> {
    let pattern = Regex::new(r"^[A-Za-z0-9_.-]{1,200}$").expect("static regex");
    if !pattern.is_match(value) {
        bail!("invalid Chocolatey {label}: {value}");
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{label} must be a lowercase SHA-256 digest");
    }
    Ok(())
}

fn validate_https_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).context("invalid source URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("remote sources require an HTTPS URL without credentials");
    }
    Ok(())
}

pub fn windows_path_to_prefix(prefix: &Path, value: &str) -> Result<PathBuf> {
    validate_windows_path(value, "Windows path")?;
    let bytes = value.as_bytes();
    if !bytes[0].eq_ignore_ascii_case(&b'c') {
        bail!("installation paths are currently restricted to the private C: drive");
    }
    let relative = value[3..].replace('\\', "/");
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("unsafe Windows path: {value}");
    }
    Ok(prefix.join("drive_c").join(relative))
}

fn validate_windows_path(value: &str, label: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
        || value.contains('\0')
    {
        bail!("{label} must be an absolute drive-letter Windows path");
    }
    if value[3..]
        .split('\\')
        .any(|part| part == "." || part == "..")
    {
        bail!("{label} contains an unsafe path component");
    }
    Ok(())
}

fn validate_install_path(value: &str, label: &str) -> Result<()> {
    if let Some(relative) = value.strip_prefix("%APPDATA%\\") {
        if relative.is_empty()
            || relative
                .split('\\')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            bail!("{label} contains an unsafe %APPDATA% path");
        }
        return Ok(());
    }
    validate_windows_path(value, label)
}
