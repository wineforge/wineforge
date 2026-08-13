use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use url::Url;

/// A complete description of a Windows application, independent of any launcher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationProfile {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub prefix: PathBuf,
    /// Windows path to the program. Arguments are deliberately represented separately.
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub engines: BTreeMap<Platform, EngineSelection>,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub mappings: Vec<HostMapping>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSelection {
    pub id: String,
}

/// Environment variables passed to Wine. Keys and values are validated before use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Environment(pub BTreeMap<String, String>);

/// Explicit access granted from a prefix to one host directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMapping {
    /// DOS drive letter, without a colon (for example `W`).
    pub drive: String,
    pub host_path: PathBuf,
    #[serde(default)]
    pub access: MappingAccess,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MappingAccess {
    ReadOnly,
    #[default]
    ReadWrite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineManifest {
    pub schema_version: u32,
    pub id: String,
    pub platform: Platform,
    pub host_architecture: String,
    #[serde(default)]
    pub translation: Translation,
    pub artifact: Artifact,
    pub wine_binary: PathBuf,
    #[serde(default)]
    pub environment: Environment,
    pub license: License,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub source: ArtifactSource,
    pub sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ArtifactSource {
    DirectDownload { url: Url },
    UserSupplied,
    BuildFromSource { source_url: Url, revision: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct License {
    pub name: String,
    pub url: Url,
    #[serde(default)]
    pub acceptance_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    MacosX86_64,
    LinuxX86_64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Translation {
    #[default]
    Native,
    Rosetta2,
}

/// Lowercase hexadecimal SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha256Digest(pub String);
