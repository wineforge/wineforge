use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ValidationError, ValidationErrors};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeConfiguration {
    #[serde(default)]
    pub keyboard: KeyboardRecommendations,
    #[serde(default)]
    pub scrolling: ScrollingRecommendations,
    #[serde(default)]
    pub mcp: McpRecipeConfiguration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfiguration {
    #[serde(default)]
    pub keyboard: KeyboardProfileConfiguration,
    #[serde(default)]
    pub scrolling: ScrollingProfileConfiguration,
    #[serde(default)]
    pub mcp: McpProfileConfiguration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardRecommendations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<KeyboardPreset>,
    #[serde(default)]
    pub mappings: Vec<KeyboardMapping>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyboardPreset {
    Windows,
    MacNative,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardMapping {
    pub id: String,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardOverride {
    pub id: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeLayerPolicy {
    #[serde(default = "enabled")]
    pub preset: bool,
    #[serde(default = "enabled")]
    pub mappings: bool,
}

impl Default for RecipeLayerPolicy {
    fn default() -> Self {
        Self {
            preset: true,
            mappings: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardProfileConfiguration {
    #[serde(default)]
    pub recipe: RecipeLayerPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<KeyboardPreset>,
    #[serde(default)]
    pub overrides: Vec<KeyboardOverride>,
    #[serde(default)]
    pub mappings: Vec<KeyboardMapping>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollingRecommendations {
    #[serde(default)]
    pub mappings: Vec<ScrollingMapping>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollAction {
    Scroll,
    Page,
    Edge,
    DragScroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollingMapping {
    pub id: String,
    pub input: String,
    pub action: ScrollAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<ScrollAxis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollingOverride {
    pub id: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollingProfileConfiguration {
    #[serde(default = "enabled")]
    pub recipe_mappings: bool,
    #[serde(default)]
    pub overrides: Vec<ScrollingOverride>,
    #[serde(default)]
    pub mappings: Vec<ScrollingMapping>,
}

impl Default for ScrollingProfileConfiguration {
    fn default() -> Self {
        Self {
            recipe_mappings: true,
            overrides: Vec::new(),
            mappings: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpRecipeConfiguration {
    #[serde(default)]
    pub endpoints: Vec<McpEndpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEndpoint {
    pub id: String,
    pub transport: McpTransport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEndpointOverride {
    pub id: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpBinding {
    pub endpoint: String,
    pub server: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpProfileConfiguration {
    #[serde(default = "enabled")]
    pub recipe_endpoints: bool,
    #[serde(default)]
    pub overrides: Vec<McpEndpointOverride>,
    #[serde(default)]
    pub endpoints: Vec<McpEndpoint>,
    #[serde(default)]
    pub bindings: Vec<McpBinding>,
}

impl Default for McpProfileConfiguration {
    fn default() -> Self {
        Self {
            recipe_endpoints: true,
            overrides: Vec::new(),
            endpoints: Vec::new(),
            bindings: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_preset: Option<KeyboardPreset>,
    pub keyboard_mappings: Vec<KeyboardMapping>,
    pub scrolling_mappings: Vec<ScrollingMapping>,
    pub mcp_endpoints: Vec<McpEndpoint>,
    pub mcp_bindings: Vec<McpBinding>,
}

pub fn resolve_configuration(
    recipe: &RecipeConfiguration,
    profile: &ProfileConfiguration,
) -> Result<EffectiveConfiguration, ValidationErrors> {
    let mut errors = Vec::new();
    let keyboard_preset = profile.keyboard.preset.or_else(|| {
        profile
            .keyboard
            .recipe
            .preset
            .then_some(recipe.keyboard.preset)
            .flatten()
    });
    let mut keyboard = BTreeMap::new();
    if profile.keyboard.recipe.mappings {
        merge_unique(
            &mut keyboard,
            &recipe.keyboard.mappings,
            |m| &m.id,
            "recipe.configuration.keyboard.mappings",
            &mut errors,
        );
    }
    for item in &profile.keyboard.overrides {
        match keyboard.get_mut(&item.id) {
            Some(mapping) if item.enabled => {
                if let Some(from) = &item.from {
                    mapping.from.clone_from(from);
                }
                if let Some(to) = &item.to {
                    mapping.to.clone_from(to);
                }
            }
            Some(_) => {
                keyboard.remove(&item.id);
            }
            None => errors.push(error(
                "configuration.keyboard.overrides",
                format!("unknown recipe mapping {}", item.id),
            )),
        }
    }
    merge_unique(
        &mut keyboard,
        &profile.keyboard.mappings,
        |m| &m.id,
        "configuration.keyboard.mappings",
        &mut errors,
    );
    validate_ids_and_inputs(
        keyboard.values().map(|m| (&m.id, &m.from)),
        "configuration.keyboard",
        &mut errors,
    );

    let mut scrolling = BTreeMap::new();
    if profile.scrolling.recipe_mappings {
        merge_unique(
            &mut scrolling,
            &recipe.scrolling.mappings,
            |m| &m.id,
            "recipe.configuration.scrolling.mappings",
            &mut errors,
        );
    }
    for item in &profile.scrolling.overrides {
        if scrolling.contains_key(&item.id) {
            if !item.enabled {
                scrolling.remove(&item.id);
            }
        } else {
            errors.push(error(
                "configuration.scrolling.overrides",
                format!("unknown recipe mapping {}", item.id),
            ));
        }
    }
    merge_unique(
        &mut scrolling,
        &profile.scrolling.mappings,
        |m| &m.id,
        "configuration.scrolling.mappings",
        &mut errors,
    );
    validate_ids_and_inputs(
        scrolling.values().map(|m| (&m.id, &m.input)),
        "configuration.scrolling",
        &mut errors,
    );

    let mut endpoints = BTreeMap::new();
    if profile.mcp.recipe_endpoints {
        merge_unique(
            &mut endpoints,
            &recipe.mcp.endpoints,
            |e| &e.id,
            "recipe.configuration.mcp.endpoints",
            &mut errors,
        );
    }
    for item in &profile.mcp.overrides {
        if endpoints.contains_key(&item.id) {
            if !item.enabled {
                endpoints.remove(&item.id);
            }
        } else {
            errors.push(error(
                "configuration.mcp.overrides",
                format!("unknown recipe endpoint {}", item.id),
            ));
        }
    }
    merge_unique(
        &mut endpoints,
        &profile.mcp.endpoints,
        |e| &e.id,
        "configuration.mcp.endpoints",
        &mut errors,
    );
    validate_ids_and_inputs(
        endpoints.values().map(|e| (&e.id, &e.id)),
        "configuration.mcp",
        &mut errors,
    );
    let mut bound = BTreeSet::new();
    for binding in &profile.mcp.bindings {
        if !endpoints.contains_key(&binding.endpoint) {
            errors.push(error(
                "configuration.mcp.bindings",
                format!("unknown endpoint {}", binding.endpoint),
            ));
        }
        if !bound.insert(&binding.endpoint) {
            errors.push(error(
                "configuration.mcp.bindings",
                format!("duplicate binding for {}", binding.endpoint),
            ));
        }
    }
    if errors.is_empty() {
        Ok(EffectiveConfiguration {
            keyboard_preset,
            keyboard_mappings: keyboard.into_values().collect(),
            scrolling_mappings: scrolling.into_values().collect(),
            mcp_endpoints: endpoints.into_values().collect(),
            mcp_bindings: profile.mcp.bindings.clone(),
        })
    } else {
        Err(ValidationErrors(errors))
    }
}

fn merge_unique<T: Clone>(
    target: &mut BTreeMap<String, T>,
    values: &[T],
    id: impl Fn(&T) -> &str,
    field: &str,
    errors: &mut Vec<ValidationError>,
) {
    for value in values {
        let key = id(value).to_owned();
        if target.insert(key.clone(), value.clone()).is_some() {
            errors.push(error(field, format!("duplicate id {key}")));
        }
    }
}

fn error(field: &str, message: String) -> ValidationError {
    ValidationError {
        field: field.into(),
        message,
    }
}

fn validate_ids_and_inputs<'a>(
    values: impl Iterator<Item = (&'a String, &'a String)>,
    field: &str,
    errors: &mut Vec<ValidationError>,
) {
    let mut inputs = BTreeSet::new();
    for (id, input) in values {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
        {
            errors.push(error(field, format!("invalid id {id:?}")));
        }
        if input.trim().is_empty() {
            errors.push(error(field, format!("{id} has an empty input")));
        } else if !inputs.insert(input) {
            errors.push(error(
                field,
                format!("input {input:?} is assigned more than once"),
            ));
        }
    }
}
