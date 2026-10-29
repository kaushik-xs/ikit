//! Domain model shared by every ikit crate. No I/O lives here.

/// Defines a fieldless enum stored and parsed as a fixed lowercase string, with
/// `ALL`, `as_str`, `Display`, `FromStr` and serde support.
macro_rules! string_enum {
    ($label:literal, $(#[$meta:meta])* pub enum $name:ident { $($(#[$vmeta:meta])* $variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $s)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $s),+
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::UnknownVariant;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::ALL.iter().copied().find(|v| v.as_str() == s).ok_or_else(|| $crate::UnknownVariant {
                    label: $label,
                    value: s.to_owned(),
                    expected: Self::ALL.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", "),
                })
            }
        }
    };
}

pub mod config;
pub mod hook;
pub mod network;
pub mod profile;
pub mod registry;
pub mod storage;
pub mod target;
pub mod version;
pub mod workload;

pub use config::Config;
pub use hook::{
    Hook, HookAction, HookKind, HookPhase, HookRunRecord, HttpAction, McpAction, ScriptState,
    ScriptStatus, ShellAction, SqlAction, SqlEngine, SqlVia,
};
pub use network::{Network, NetworkOrigin};
pub use profile::{FieldSpec, Profile, ProfileError, ProfileKind, ProfileValues, Requirement};
pub use registry::{
    Category, RegistryEntry, RegistryError, RegistryService, RegistryVersion, ServiceSpec,
};
pub use storage::{Backup, Storage, StorageOptions, StorageProvider};
pub use target::{Connection, Target, TargetKind, TargetStatus};
pub use version::{Dependency, VersionConstraint, VersionError};
pub use workload::{
    PublishedPort, Run, RunAction, RunResult, ServiceInstance, Workload, WorkloadName,
    WorkloadStatus,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {label} `{value}` (expected one of: {expected})")]
pub struct UnknownVariant {
    pub label: &'static str,
    pub value: String,
    pub expected: String,
}

/// Names of profiles, targets, registry services and service instances are lowercase
/// slugs so they work as CLI args, file names, keychain accounts and DNS labels.
pub fn is_slug(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub const SLUG_RULE: &str = "1-63 lowercase letters, digits or `-`, starting with a letter or digit and not ending with `-`";
