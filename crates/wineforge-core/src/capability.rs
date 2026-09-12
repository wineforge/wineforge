use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ValidationError, ValidationErrors};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(pub BTreeMap<String, Capability>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineCapabilityDocument {
    pub schema_version: u32,
    pub kind: EngineCapabilityDocumentKind,
    pub engine_id: String,
    pub target: EngineCapabilityTarget,
    pub protocol: u32,
    pub provided: Vec<EngineCapabilityDeclaration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineCapabilityDocumentKind {
    WineforgeEngineCapabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineCapabilityTarget {
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "macos-x86_64")]
    MacosX86_64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineCapabilityDeclaration {
    pub id: String,
    pub version: u32,
    pub state: EngineCapabilityState,
    pub evidence_patches: Vec<PathBuf>,
    pub targets: Vec<EngineCapabilityTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<EngineCapabilityTransport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<EngineCapabilityScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<EngineCapabilityPrivacy>,
    #[serde(default)]
    pub transports: Vec<EngineBridgeTransport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineCapabilityState {
    Provided,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineCapabilityTransport {
    pub kind: EngineCapabilityTransportKind,
    pub variables: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineCapabilityTransportKind {
    Environment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineCapabilityScope {
    Process,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineCapabilityPrivacy {
    pub requires_external_window_observation: bool,
    pub requires_accessibility: bool,
    pub requires_screen_recording: bool,
    pub requires_input_monitoring: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineBridgeTransport {
    UnixSocket,
    NamedPipe,
}

impl EngineCapabilityDocument {
    pub fn provided_set(&self) -> CapabilitySet {
        CapabilitySet(
            self.provided
                .iter()
                .map(|item| {
                    (
                        item.id.clone(),
                        Capability {
                            version: item.version,
                        },
                    )
                })
                .collect(),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityProvider {
    Engine,
    Runtime,
    Composed,
    #[default]
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequirement {
    pub name: String,
    #[serde(default = "minimum_version")]
    pub minimum_version: u32,
    #[serde(default)]
    pub provider: CapabilityProvider,
}

fn minimum_version() -> u32 {
    1
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityInventory {
    #[serde(default)]
    pub engine: CapabilitySet,
    #[serde(default)]
    pub runtime: CapabilitySet,
    #[serde(default)]
    pub composed: CapabilitySet,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityComposition {
    pub version: u32,
    pub requires: Vec<CapabilityRequirement>,
}

pub type CapabilityCompositions = BTreeMap<String, CapabilityComposition>;

impl CapabilityInventory {
    pub fn compose(
        mut self,
        definitions: &CapabilityCompositions,
    ) -> Result<Self, ValidationErrors> {
        let mut pending = definitions.iter().collect::<BTreeMap<_, _>>();
        loop {
            let ready = pending
                .iter()
                .filter(|(_, definition)| self.satisfy(&definition.requires).is_ok())
                .map(|(name, _)| (*name).clone())
                .collect::<Vec<_>>();
            if ready.is_empty() {
                break;
            }
            for name in ready {
                let definition = pending.remove(&name).expect("ready definition exists");
                self.composed.0.insert(
                    name,
                    Capability {
                        version: definition.version,
                    },
                );
            }
        }
        let mut errors = Vec::new();
        for (name, definition) in pending {
            if !valid_capability_name(name) || definition.version == 0 {
                errors.push(ValidationError {
                    field: format!("composed_capabilities.{name}"),
                    message: "invalid name or zero version".into(),
                });
            }
        }
        if errors.is_empty() {
            Ok(self)
        } else {
            Err(ValidationErrors(errors))
        }
    }

    pub fn version(&self, requirement: &CapabilityRequirement) -> Option<u32> {
        let in_set = |set: &CapabilitySet| set.0.get(&requirement.name).map(|value| value.version);
        match requirement.provider {
            CapabilityProvider::Engine => in_set(&self.engine),
            CapabilityProvider::Runtime => in_set(&self.runtime),
            CapabilityProvider::Composed => in_set(&self.composed),
            CapabilityProvider::Any => [
                in_set(&self.engine),
                in_set(&self.runtime),
                in_set(&self.composed),
            ]
            .into_iter()
            .flatten()
            .max(),
        }
    }

    pub fn satisfy(&self, requirements: &[CapabilityRequirement]) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        for (index, requirement) in requirements.iter().enumerate() {
            let field = format!("requirements.capabilities[{index}]");
            if !valid_capability_name(&requirement.name) {
                errors.push(ValidationError {
                    field: format!("{field}.name"),
                    message: "must be a lowercase dotted identifier".into(),
                });
            }
            if requirement.minimum_version == 0 {
                errors.push(ValidationError {
                    field: format!("{field}.minimum_version"),
                    message: "must be at least 1".into(),
                });
            }
            match self.version(requirement) {
                Some(version) if version >= requirement.minimum_version => {}
                Some(version) => errors.push(ValidationError {
                    field,
                    message: format!(
                        "requires version {} but provider offers version {version}",
                        requirement.minimum_version
                    ),
                }),
                None => errors.push(ValidationError {
                    field,
                    message: format!("required capability {} is unavailable", requirement.name),
                }),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }
}

pub fn valid_capability_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        })
}
