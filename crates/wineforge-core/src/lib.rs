//! Validated, platform-neutral models and filesystem planning for Wineforge.

mod apply;
mod capability;
mod configuration;
mod inspect;
mod plan;
mod schema;
mod validate;

pub use apply::{ApplyError, ApplyReceipt, apply_mapping_plan, verify_mappings};
pub use capability::{
    Capability, CapabilityComposition, CapabilityCompositions, CapabilityInventory,
    CapabilityProvider, CapabilityRequirement, CapabilitySet, valid_capability_name,
};
pub use configuration::{
    EffectiveConfiguration, KeyboardMapping, KeyboardOverride, KeyboardPreset,
    KeyboardProfileConfiguration, KeyboardRecommendations, McpBinding, McpEndpoint,
    McpEndpointOverride, McpProfileConfiguration, McpRecipeConfiguration, McpTransport,
    ProfileConfiguration, RecipeConfiguration, RecipeLayerPolicy, ScrollAction, ScrollAxis,
    ScrollingMapping, ScrollingOverride, ScrollingProfileConfiguration, ScrollingRecommendations,
    resolve_configuration,
};
pub use inspect::{Inspection, InspectionError, SymlinkFinding, inspect_prefix};
pub use plan::{CurrentMapping, MappingAction, MappingPlan, plan_mappings};
pub use schema::{
    ApplicationProfile, Artifact, ArtifactSource, EngineDistribution, EngineManifest,
    EngineSelection, Environment, HostMapping, IsolationMode, IsolationPolicy, License,
    MappingAccess, Platform, Sha256Digest, Translation,
};
pub use validate::{Validate, ValidationError, ValidationErrors};
