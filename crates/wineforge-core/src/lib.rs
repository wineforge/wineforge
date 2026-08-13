//! Validated, platform-neutral models and filesystem planning for Wineforge.

mod apply;
mod inspect;
mod plan;
mod schema;
mod validate;

pub use apply::{ApplyError, ApplyReceipt, apply_mapping_plan, verify_mappings};
pub use inspect::{Inspection, InspectionError, SymlinkFinding, inspect_prefix};
pub use plan::{CurrentMapping, MappingAction, MappingPlan, plan_mappings};
pub use schema::{
    ApplicationProfile, Artifact, ArtifactSource, EngineManifest, EngineSelection, Environment,
    HostMapping, IsolationMode, IsolationPolicy, License, MappingAccess, Platform, Sha256Digest,
    Translation,
};
pub use validate::{Validate, ValidationError, ValidationErrors};
