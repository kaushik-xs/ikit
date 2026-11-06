//! Which commands may change an org's state. For an org with remote state, a write takes
//! the remote lock and pushes what changed; a read only brings the local copy up to date.
//! The matches are exhaustive on purpose: a new command must be classified to compile.

use crate::Command;
use crate::cmd::backup::BackupCmd;
use crate::cmd::edge::EdgeCmd;
use crate::cmd::group::GroupCmd;
use crate::cmd::hook::HookCmd;
use crate::cmd::mcp::{McpCmd, TokenCmd};
use crate::cmd::net::{DockerCmd, NetCmd, ZtCmd};
use crate::cmd::org::{OrgCmd, SecretsCmd};
use crate::cmd::profile::ProfileCmd;
use crate::cmd::registry::RegistryCmd;
use crate::cmd::resource::{BackendCmd, KeysCmd, ResourceCmd};
use crate::cmd::route::RouteCmd;
use crate::cmd::service::ServiceCmd;
use crate::cmd::storage::StorageCmd;
use crate::cmd::target::TargetCmd;
use crate::cmd::workload::WorkloadCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Manages orgs or the home itself; no remote session.
    None,
    Read,
    Write,
    /// A long-running server, which syncs around each request itself.
    Server,
}

use Access::{Read, Write};

impl Command {
    pub fn access(&self) -> Access {
        match self {
            Command::Init | Command::Identity(_) => Access::None,
            Command::Org(c) => match c {
                // The secrets file travels with the org's state.
                OrgCmd::Secrets(s) => match s {
                    SecretsCmd::List
                    | SecretsCmd::ForgetKeychain { .. }
                    | SecretsCmd::Check { .. } => Read,
                    SecretsCmd::Init { .. }
                    | SecretsCmd::Grant { .. }
                    | SecretsCmd::Revoke { .. }
                    | SecretsCmd::Split { .. }
                    | SecretsCmd::Merge { .. } => Write,
                },
                // Restore rewrites the local copy, which the write session then pushes.
                OrgCmd::Restore { .. } | OrgCmd::Push => Write,
                OrgCmd::Create { .. }
                | OrgCmd::List
                | OrgCmd::Show { .. }
                | OrgCmd::Use { .. }
                | OrgCmd::Rm { .. }
                | OrgCmd::Remote(_)
                | OrgCmd::Clone { .. }
                | OrgCmd::Pull { .. }
                | OrgCmd::Status
                | OrgCmd::History { .. }
                | OrgCmd::Unlock { .. } => Access::None,
            },
            Command::Profile(c) => match c {
                ProfileCmd::List { .. } | ProfileCmd::Show { .. } | ProfileCmd::Fields { .. } => {
                    Read
                }
                ProfileCmd::Add { .. } | ProfileCmd::Set { .. } | ProfileCmd::Rm { .. } => Write,
            },
            Command::Registry(c) => match c {
                RegistryCmd::List { .. }
                | RegistryCmd::Show { .. }
                | RegistryCmd::Export { .. }
                | RegistryCmd::Usage { .. }
                | RegistryCmd::Outdated { .. }
                | RegistryCmd::Logins => Read,
                RegistryCmd::Add(_)
                | RegistryCmd::Set(_)
                | RegistryCmd::Version(_)
                | RegistryCmd::Rm { .. }
                | RegistryCmd::Import { .. }
                | RegistryCmd::Sync { .. }
                | RegistryCmd::Login { .. }
                | RegistryCmd::Logout { .. } => Write,
            },
            Command::Target(c) => match c {
                TargetCmd::List | TargetCmd::Show { .. } => Read,
                TargetCmd::Edge(EdgeCmd::Status { .. }) => Read,
                TargetCmd::Edge(
                    EdgeCmd::Install { .. }
                    | EdgeCmd::Upgrade { .. }
                    | EdgeCmd::Reload { .. }
                    | EdgeCmd::Uninstall { .. },
                ) => Write,
                // `test` records how the target was reached.
                TargetCmd::Test { .. }
                | TargetCmd::Attach(_)
                | TargetCmd::UseZt { .. }
                | TargetCmd::Detach { .. } => Write,
            },
            Command::Workload(c) => match c {
                // `use` only changes the per-person context.yaml.
                WorkloadCmd::List { .. }
                | WorkloadCmd::Show { .. }
                | WorkloadCmd::Runs { .. }
                | WorkloadCmd::Use { .. } => Read,
                WorkloadCmd::Create { .. } | WorkloadCmd::Rm { .. } => Write,
            },
            Command::Group(c) => match c {
                GroupCmd::List => Read,
                GroupCmd::Add { .. } | GroupCmd::Rm { .. } => Write,
            },
            Command::Service(c) => match c {
                ServiceCmd::List | ServiceCmd::Show { .. } => Read,
                ServiceCmd::Upgrade { .. }
                | ServiceCmd::Set { .. }
                | ServiceCmd::Publish { .. }
                | ServiceCmd::Rm { .. } => Write,
            },
            Command::Hook(c) => match c {
                HookCmd::List | HookCmd::History { .. } => Read,
                HookCmd::Add { .. } | HookCmd::Rm { .. } | HookCmd::Run { .. } => Write,
            },
            Command::Storage(c) => match c {
                StorageCmd::List
                | StorageCmd::Show { .. }
                | StorageCmd::Test { .. }
                | StorageCmd::Objects { .. } => Read,
                StorageCmd::Add { .. } | StorageCmd::Rm { .. } => Write,
            },
            Command::Backup(c) => match c {
                BackupCmd::List { .. } | BackupCmd::Schedule { .. } => Read,
                BackupCmd::Run { .. } | BackupCmd::Restore { .. } | BackupCmd::Prune { .. } => {
                    Write
                }
            },
            Command::Mcp(c) => match c {
                McpCmd::Serve { .. } => Access::Server,
                McpCmd::Token(TokenCmd::List) => Read,
                McpCmd::Token(TokenCmd::Create { .. } | TokenCmd::Rm { .. })
                | McpCmd::Install { .. } => Write,
            },
            Command::Net(c) => match c {
                NetCmd::Zt(
                    ZtCmd::List | ZtCmd::Show { .. } | ZtCmd::Members { .. } | ZtCmd::Status { .. },
                ) => Read,
                NetCmd::Zt(
                    ZtCmd::Create { .. }
                    | ZtCmd::Attach { .. }
                    | ZtCmd::Join { .. }
                    | ZtCmd::Leave { .. }
                    | ZtCmd::Authorize { .. }
                    | ZtCmd::Deauthorize { .. }
                    | ZtCmd::Link { .. }
                    | ZtCmd::Sidecar { .. }
                    | ZtCmd::Unlink
                    | ZtCmd::Rm { .. },
                ) => Write,
                NetCmd::Docker(DockerCmd::List) => Read,
                NetCmd::Docker(DockerCmd::Attach { .. } | DockerCmd::Detach { .. }) => Write,
            },
            Command::Route(c) => match c {
                RouteCmd::List => Read,
                RouteCmd::Add { .. }
                | RouteCmd::Rm { .. }
                | RouteCmd::Redirect { .. }
                | RouteCmd::Unredirect { .. }
                | RouteCmd::Auto(_) => Write,
            },
            Command::Resource(c) => match c {
                ResourceCmd::List { .. }
                | ResourceCmd::Show { .. }
                | ResourceCmd::Keys(KeysCmd::List { .. })
                | ResourceCmd::Backend(
                    BackendCmd::List
                    | BackendCmd::Show { .. }
                    | BackendCmd::IamPolicy { .. }
                    | BackendCmd::Test { .. },
                ) => Read,
                // Adopting and removing reach the backend; all of these change the state.
                ResourceCmd::Bind { .. }
                | ResourceCmd::Adopt { .. }
                | ResourceCmd::Rm { .. }
                | ResourceCmd::Keys(KeysCmd::Rm { .. })
                | ResourceCmd::Rotate { .. }
                | ResourceCmd::Backend(
                    BackendCmd::Add { .. }
                    | BackendCmd::Set { .. }
                    | BackendCmd::Rm { .. }
                    | BackendCmd::Bind { .. }
                    | BackendCmd::Unbind { .. },
                ) => Write,
            },
            Command::Render | Command::Plan | Command::Status { .. } => Read,
            Command::Apply { .. } | Command::Destroy { .. } => Write,
        }
    }
}
