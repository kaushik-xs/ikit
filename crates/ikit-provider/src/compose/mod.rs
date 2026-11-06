mod diff;
mod render;

pub use diff::{Change, Plan, diff};
pub use render::{
    RenderError, Rendered, ResourceValues, ServiceInput, WorkloadInput, ZT_SIDECAR,
    ZT_SIDECAR_IMAGE, ZtSidecar, image_registry, render,
};

/// Main compose file; holds no secret values.
pub const COMPOSE_FILE: &str = "compose.yaml";
/// Override file with secret env values; written with mode 0600.
pub const SECRETS_FILE: &str = "secrets.compose.yaml";
