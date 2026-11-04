//! The edge on a target's host: installing, adopting, joining, upgrading and removing the
//! reverse proxy, and putting a workload's site file in place during apply and destroy.
//!
//! One proxy serves every org that uses the host, so the host decides: `host.json` says
//! which proxy runs and which orgs joined it, and the site files' headers say which
//! domains and upstream ports are taken. Every change on the host runs as root (directly
//! or with `sudo -n`) under the host's edge lock.

use std::time::Duration;

use chrono::Utc;
use ikit_core::{
    AcmeCa, Connection, EdgeConfig, EdgeKind, HostManifest, Target, TlsMode, Workload,
    WorkloadStatus,
};
use ikit_provider::edge::{
    self as provider, EdgeChange, HostProbe, InstalledServer, PkgFamily, SiteInput,
};
use serde::Serialize;

use crate::deploy::{failed, sh};
use crate::targets::ssh_args;
use crate::{App, AppError, ExecOutput, Result};

/// Installing or upgrading pulls packages.
const INSTALL: Duration = Duration::from_secs(10 * 60);
const EDGE: Duration = Duration::from_secs(2 * 60);
const DNS: Duration = Duration::from_secs(3);

/// How a script gets root on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    /// Fails with exit 77 without root or passwordless sudo.
    Required,
    /// Root when available, the login user otherwise (for reading).
    IfAvailable,
}

/// What `ikit target edge install` should set up.
#[derive(Debug, Clone)]
pub struct EdgeInstallRequest {
    pub kind: EdgeKind,
    /// ACME account email for this org's certificates; saved as the org's `edge.acme_email`.
    /// `None` uses the one already in the org config.
    pub email: Option<String>,
    pub ca: AcmeCa,
    /// A package version to install (`apt-cache policy` / `dnf list` spelling).
    pub version: Option<String>,
    /// Keep other orgs off this proxy.
    pub exclusive: bool,
    /// Allow 80/443 in ufw or firewalld.
    pub open_firewall: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeInstallAction {
    /// ikit installed the proxy from its package repository.
    Installed,
    /// The proxy was already installed; ikit added its include line to its config.
    Adopted,
    /// Another org runs the proxy through ikit; this org joined it.
    Joined,
    /// This org already used the proxy; its settings were updated.
    Updated,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeInstallReport {
    pub target: String,
    pub action: EdgeInstallAction,
    pub kind: EdgeKind,
    pub version: Option<String>,
    /// Orgs using the proxy.
    pub orgs: Vec<String>,
    /// `ufw`, `firewalld` or `none`, with `open_firewall`.
    pub firewall: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeStatus {
    pub target: String,
    /// This org's settings, if it uses the proxy.
    pub config: Option<EdgeConfig>,
    pub manifest: Option<HostManifest>,
    /// Why the host's edge is not usable, if it is not.
    pub problem: Option<String>,
    /// The proxy binary on the host.
    pub server: Option<InstalledServer>,
    /// Sockets on 80/443.
    pub listeners: Vec<String>,
    pub sites: Vec<EdgeSite>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeSite {
    pub file: String,
    pub org: Option<String>,
    pub workload: Option<String>,
    pub hosts: Vec<String>,
    /// The site belongs to this org.
    pub mine: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeUpgradeReport {
    pub target: String,
    pub kind: EdgeKind,
    pub previous: Option<String>,
    pub version: Option<String>,
    /// Every org on the host was affected.
    pub orgs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeUninstallAction {
    /// Other orgs still use the proxy; this org left it.
    Left,
    /// This org was the last one; ikit removed itself from the proxy.
    Removed,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeUninstallReport {
    pub target: String,
    pub action: EdgeUninstallAction,
    pub kind: EdgeKind,
    /// The proxy was adopted, so it keeps running.
    pub adopted: bool,
}

/// What `apply` would change in the workload's site on the target's proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EdgePlan {
    pub kind: EdgeKind,
    /// The site file in `/etc/ikit/edge/sites`.
    pub file: String,
    pub changes: Vec<EdgeChange>,
    /// Problems that do not block the apply, e.g. a domain that does not resolve yet.
    pub warnings: Vec<String>,
    /// The site file to write; `None` removes it.
    #[serde(skip)]
    pub rendered: Option<String>,
    /// Domains the site answers for, re-checked on the host when it is written.
    #[serde(skip)]
    pub hosts: Vec<String>,
}

/// What `edge_install` does on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decision {
    Install(PkgFamily),
    Adopt,
    Join(HostManifest),
    /// This org is already on the host's proxy.
    Update(HostManifest),
}

/// Decides how to set up `req` on a host from what the probe found there.
fn decide(probe: &HostProbe, org: &str, req: &EdgeInstallRequest) -> Result<Decision> {
    let kind = req.kind;
    if probe.os != "Linux" {
        return Err(AppError::Invalid(format!(
            "the reverse proxy runs on Linux hosts only (this host runs {})",
            if probe.os.is_empty() {
                "an unknown OS"
            } else {
                &probe.os
            }
        )));
    }
    if !probe.systemd {
        return Err(AppError::Invalid(
            "the reverse proxy needs systemd on the host".into(),
        ));
    }
    match &probe.manifest {
        Some(Err(e)) => Err(AppError::Invalid(format!(
            "{e}; fix or remove {} on the host",
            provider::HOST_FILE
        ))),
        Some(Ok(m)) => {
            if m.kind != kind {
                return Err(AppError::Invalid(format!(
                    "this host already runs {} through ikit (installed by org {}); \
                     install {} instead to share it",
                    m.kind, m.installed_by, m.kind
                )));
            }
            if m.exclusive && m.installed_by != org {
                return Err(AppError::Invalid(format!(
                    "the {} proxy on this host is reserved for org {}",
                    m.kind, m.installed_by
                )));
            }
            let others: Vec<&String> = m.orgs.iter().filter(|o| *o != org).collect();
            if req.exclusive && !others.is_empty() {
                return Err(AppError::Invalid(format!(
                    "--exclusive: the proxy is already used by org(s) {}",
                    others
                        .iter()
                        .map(|o| o.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            if m.orgs.iter().any(|o| o == org) {
                Ok(Decision::Update(m.clone()))
            } else {
                Ok(Decision::Join(m.clone()))
            }
        }
        None => {
            let foreign = probe.foreign_listeners(Some(kind));
            if !foreign.is_empty() {
                return Err(AppError::Invalid(format!(
                    "ports 80/443 are taken on this host ({}); stop that server, or adopt it \
                     if it is caddy or ferron",
                    foreign.join(", ")
                )));
            }
            if let Some(other) = EdgeKind::ALL
                .iter()
                .find(|k| **k != kind && probe.servers.get(k).is_some_and(|s| s.active))
            {
                return Err(AppError::Invalid(format!(
                    "{other} is already running on this host; install {other} to adopt it, \
                     or stop it first"
                )));
            }
            if probe.servers.contains_key(&kind) {
                return Ok(Decision::Adopt);
            }
            match probe.family {
                Some(f) => Ok(Decision::Install(f)),
                None => Err(AppError::Invalid(format!(
                    "cannot install {kind} here: only Debian/Ubuntu (apt) and RHEL/Fedora (dnf) \
                     hosts are supported; install it by hand and run this again to adopt it"
                ))),
            }
        }
    }
}

/// `version=…` (and other `key=value`) lines printed by the edge scripts.
fn script_value(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(|v| {
            v.split_whitespace()
                .map(|w| w.trim_start_matches('v'))
                .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
                .unwrap_or(v)
                .to_owned()
        })
        .filter(|v| !v.is_empty())
}

fn check_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 64
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+~:-_".contains(c))
    {
        return Err(AppError::Invalid(format!(
            "invalid package version `{version}`"
        )));
    }
    Ok(())
}

impl App {
    /// Runs `script` on the target's host with `sh`, locally or over ssh.
    async fn edge_sh(
        &self,
        target: &Target,
        script: &str,
        root: Root,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput> {
        let q = sh(script);
        let fallback = match root {
            Root::Required => "echo 'ikit needs root or passwordless sudo (sudo -n) on this host \
                               to manage its reverse proxy' >&2; exit 77"
                .to_owned(),
            Root::IfAvailable => format!("sh -c {q}"),
        };
        let wrapper = format!(
            "if [ \"$(id -u)\" = 0 ]; then sh -c {q}; \
             elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then sudo -n sh -c {q}; \
             else {fallback}; fi"
        );
        let (program, args) = match &target.connection {
            Connection::Local => ("sh", vec!["-c".to_owned(), wrapper]),
            c @ Connection::Ssh { .. } => {
                let mut args = ssh_args(c);
                args.push(wrapper);
                ("ssh", args)
            }
            Connection::K8s { .. } => {
                return Err(AppError::Invalid(format!(
                    "target `{}` is a Kubernetes cluster; the reverse proxy runs on VM targets",
                    target.name
                )));
            }
        };
        Ok(self.exec.run(program, &args, stdin, timeout).await?)
    }

    /// Fails with a readable error unless `out` succeeded.
    fn edge_ok(&self, what: &str, target: &Target, out: &ExecOutput) -> Result<()> {
        match out.code {
            Some(0) => Ok(()),
            Some(77) => Err(AppError::Invalid(format!(
                "{what} on `{}` needs root: log in as root or allow passwordless sudo for the SSH user",
                target.name
            ))),
            Some(75) => Err(AppError::Invalid(format!(
                "{what} on `{}`: another ikit run is changing the reverse proxy; try again",
                target.name
            ))),
            _ => Err(failed(&format!("{what} on `{}`", target.name), out)),
        }
    }

    async fn probe_host(&self, target: &Target, root: Root) -> Result<HostProbe> {
        let out = self
            .edge_sh(target, &provider::probe_script(), root, None, EDGE)
            .await?;
        self.edge_ok("inspecting the host", target, &out)?;
        Ok(provider::parse_probe(&out.stdout))
    }

    /// What is on a target's host: OS, installed proxies, `host.json` and site files.
    pub async fn edge_probe(&self, target: &str) -> Result<HostProbe> {
        let target = self.target_show(target).await?;
        self.probe_host(&target, Root::IfAvailable).await
    }

    /// Sets up the reverse proxy on a target for this org: installs it, adopts one that was
    /// installed without ikit, joins the one another org installed, or updates this org's
    /// settings when it already uses it.
    pub async fn edge_install(
        &self,
        target: &str,
        req: EdgeInstallRequest,
    ) -> Result<EdgeInstallReport> {
        if let Some(email) = &req.email
            && !ikit_core::edge::is_email(email)
        {
            return Err(AppError::Invalid(format!("invalid email `{email}`")));
        }
        if let Some(v) = &req.version {
            check_version(v)?;
        }
        let mut org_config = self.config()?;
        let email = req.email.clone().or(org_config.edge.acme_email.clone());
        let target = self.target_show(target).await?;
        let org = self.org.name.clone();
        let probe = self.probe_host(&target, Root::Required).await?;
        let decision = decide(&probe, &org, &req)?;
        let kind = req.kind;
        let now = Utc::now();
        let (action, manifest) = match decision {
            Decision::Install(family) => {
                let mut manifest = HostManifest {
                    kind,
                    installed_by: org.clone(),
                    adopted: false,
                    exclusive: req.exclusive,
                    orgs: vec![org.clone()],
                    version: None,
                    installed_at: now,
                    upgraded_at: None,
                };
                let script =
                    provider::install_script(kind, family, req.version.as_deref(), &manifest);
                let out = self
                    .edge_sh(&target, &script, Root::Required, None, INSTALL)
                    .await?;
                self.edge_ok(&format!("installing {kind}"), &target, &out)?;
                manifest.version = script_value(&out.stdout, "version");
                self.write_manifest(&target, &manifest).await?;
                (EdgeInstallAction::Installed, manifest)
            }
            Decision::Adopt => {
                let manifest = HostManifest {
                    kind,
                    installed_by: org.clone(),
                    adopted: true,
                    exclusive: req.exclusive,
                    orgs: vec![org.clone()],
                    version: probe.servers.get(&kind).map(|s| s.version.clone()),
                    installed_at: now,
                    upgraded_at: None,
                };
                let out = self
                    .edge_sh(
                        &target,
                        &provider::adopt_script(kind, &manifest),
                        Root::Required,
                        None,
                        EDGE,
                    )
                    .await?;
                self.edge_ok(&format!("adopting {kind}"), &target, &out)?;
                (EdgeInstallAction::Adopted, manifest)
            }
            Decision::Join(mut manifest) => {
                manifest.orgs.push(org.clone());
                self.write_manifest(&target, &manifest).await?;
                (EdgeInstallAction::Joined, manifest)
            }
            Decision::Update(mut manifest) => {
                if req.exclusive && !manifest.exclusive && manifest.installed_by == org {
                    manifest.exclusive = true;
                    self.write_manifest(&target, &manifest).await?;
                }
                (EdgeInstallAction::Updated, manifest)
            }
        };
        let firewall = if req.open_firewall {
            let out = self
                .edge_sh(
                    &target,
                    &provider::firewall_script(),
                    Root::Required,
                    None,
                    EDGE,
                )
                .await?;
            self.edge_ok("opening ports 80 and 443", &target, &out)?;
            out.stdout
                .lines()
                .find_map(|l| l.strip_prefix("firewall="))
                .map(str::to_owned)
        } else {
            None
        };
        if org_config.edge.acme_email != email {
            org_config.edge.acme_email = email.clone();
            crate::orgs::write_org_config(&self.paths, &self.org.name, &org_config)?;
        }
        let config = EdgeConfig {
            kind,
            email,
            ca: req.ca,
            version: manifest.version.clone(),
        };
        self.store
            .targets()
            .set_edge(&target.name, Some(&config))
            .await?;
        Ok(EdgeInstallReport {
            target: target.name,
            action,
            kind,
            version: manifest.version,
            orgs: manifest.orgs,
            firewall,
        })
    }

    async fn write_manifest(&self, target: &Target, manifest: &HostManifest) -> Result<()> {
        let out = self
            .edge_sh(
                target,
                &provider::write_host_script(manifest),
                Root::Required,
                None,
                EDGE,
            )
            .await?;
        self.edge_ok("writing host.json", target, &out)
    }

    /// The target's edge config, or an error telling how to set one up.
    fn edge_config(target: &Target) -> Result<&EdgeConfig> {
        target.edge.as_ref().ok_or_else(|| {
            AppError::NotFound(format!(
                "reverse proxy on `{0}` for this org (set one up with `ikit target edge install {0}`)",
                target.name
            ))
        })
    }

    /// The host's manifest, checked against this org's config.
    fn usable_manifest(
        &self,
        target: &Target,
        config: &EdgeConfig,
        probe: &HostProbe,
    ) -> Result<HostManifest> {
        let manifest = match &probe.manifest {
            None => {
                return Err(AppError::Invalid(format!(
                    "the reverse proxy is gone from `{0}` ({1} is missing); set it up again with \
                     `ikit target edge install {0} {2}`",
                    target.name,
                    provider::HOST_FILE,
                    config.kind
                )));
            }
            Some(Err(e)) => return Err(AppError::Invalid(e.clone())),
            Some(Ok(m)) => m.clone(),
        };
        if manifest.kind != config.kind {
            return Err(AppError::Invalid(format!(
                "`{}` now runs {} (installed by org {}), not {}; run `ikit target edge install {} {}`",
                target.name,
                manifest.kind,
                manifest.installed_by,
                config.kind,
                target.name,
                manifest.kind
            )));
        }
        if !manifest.orgs.contains(&self.org.name) {
            return Err(AppError::Invalid(format!(
                "org {} is not on the reverse proxy of `{1}` any more; join it again with \
                 `ikit target edge install {1} --kind {2}`",
                self.org.name, target.name, config.kind
            )));
        }
        Ok(manifest)
    }

    /// The proxy on a target and the sites it serves.
    pub async fn edge_status(&self, target: &str) -> Result<EdgeStatus> {
        let target = self.target_show(target).await?;
        let probe = self.probe_host(&target, Root::IfAvailable).await?;
        let (manifest, problem) = match (&target.edge, &probe.manifest) {
            (Some(c), _) => match self.usable_manifest(&target, c, &probe) {
                Ok(m) => (Some(m), None),
                Err(e) => (
                    probe.manifest.clone().and_then(|m| m.ok()),
                    Some(e.to_string()),
                ),
            },
            (None, Some(Ok(m))) => (Some(m.clone()), None),
            (None, Some(Err(e))) => (None, Some(e.clone())),
            (None, None) => (None, None),
        };
        let kind = manifest
            .as_ref()
            .map(|m| m.kind)
            .or(target.edge.as_ref().map(|c| c.kind));
        let server = kind.and_then(|k| probe.servers.get(&k).cloned());
        let problem = problem.or_else(|| match (&manifest, &server) {
            (Some(m), Some(s)) if !s.active => Some(format!("{} is not running", m.kind)),
            (Some(m), None) => Some(format!("{} is not installed", m.kind)),
            _ => None,
        });
        let sites = probe
            .sites
            .iter()
            .map(|(file, h)| EdgeSite {
                file: file.clone(),
                org: h.as_ref().map(|h| h.org.clone()),
                workload: h.as_ref().map(|h| h.workload.clone()),
                hosts: h
                    .as_ref()
                    .map(|h| h.hosts().into_iter().map(str::to_owned).collect())
                    .unwrap_or_default(),
                mine: h.as_ref().is_some_and(|h| h.org == self.org.name),
            })
            .collect();
        Ok(EdgeStatus {
            target: target.name.clone(),
            config: target.edge.clone(),
            manifest,
            problem,
            server,
            listeners: probe.listeners,
            sites,
        })
    }

    /// Checks the proxy's config and reloads it.
    pub async fn edge_reload(&self, target: &str) -> Result<()> {
        let target = self.target_show(target).await?;
        let config = Self::edge_config(&target)?;
        let out = self
            .edge_sh(
                &target,
                &provider::reload_script(config.kind),
                Root::Required,
                None,
                EDGE,
            )
            .await?;
        self.edge_ok(&format!("reloading {}", config.kind), &target, &out)
    }

    /// Upgrades the proxy's package to `version` or the newest one. Every org on the host
    /// is affected; if the new version rejects the config or does not start, the previous
    /// one is reinstalled.
    pub async fn edge_upgrade(
        &self,
        target: &str,
        version: Option<&str>,
    ) -> Result<EdgeUpgradeReport> {
        if let Some(v) = version {
            check_version(v)?;
        }
        let target = self.target_show(target).await?;
        let config = Self::edge_config(&target)?.clone();
        let probe = self.probe_host(&target, Root::Required).await?;
        let mut manifest = self.usable_manifest(&target, &config, &probe)?;
        let kind = manifest.kind;
        let family = probe.family.ok_or_else(|| {
            AppError::Invalid(format!(
                "cannot upgrade {kind} here: only apt and dnf hosts are supported; upgrade it by hand"
            ))
        })?;
        if !probe.servers.get(&kind).is_some_and(|s| s.packaged) {
            return Err(AppError::Invalid(format!(
                "{kind} on `{}` was not installed from a package, so ikit cannot upgrade it; \
                 upgrade it by hand, then run `ikit target edge reload {}`",
                target.name, target.name
            )));
        }
        let out = self
            .edge_sh(
                &target,
                &provider::upgrade_script(kind, family, version),
                Root::Required,
                None,
                INSTALL,
            )
            .await?;
        match out.code {
            Some(4) | Some(5) => {
                return Err(failed(
                    &format!(
                        "upgrading {kind} on `{}` (the previous version was reinstalled)",
                        target.name
                    ),
                    &out,
                ));
            }
            _ => self.edge_ok(&format!("upgrading {kind}"), &target, &out)?,
        }
        let previous = script_value(&out.stdout, "previous");
        manifest.version = script_value(&out.stdout, "version");
        manifest.upgraded_at = Some(Utc::now());
        self.write_manifest(&target, &manifest).await?;
        let config = EdgeConfig {
            version: manifest.version.clone(),
            ..config
        };
        self.store
            .targets()
            .set_edge(&target.name, Some(&config))
            .await?;
        Ok(EdgeUpgradeReport {
            target: target.name,
            kind,
            previous,
            version: manifest.version,
            orgs: manifest.orgs,
        })
    }

    /// Takes this org off the target's proxy. The last org to leave removes ikit from it:
    /// a proxy ikit installed is stopped (and its package removed with `purge`); an adopted
    /// one keeps running without ikit's include line.
    pub async fn edge_uninstall(&self, target: &str, purge: bool) -> Result<EdgeUninstallReport> {
        let target = self.target_show(target).await?;
        let config = Self::edge_config(&target)?.clone();
        let probe = self.probe_host(&target, Root::Required).await?;
        let mine: Vec<String> = probe
            .sites
            .iter()
            .filter_map(|(_, h)| h.as_ref())
            .filter(|h| h.org == self.org.name)
            .map(|h| h.workload.clone())
            .collect();
        if !mine.is_empty() {
            return Err(AppError::InUse {
                what: format!("the reverse proxy on `{}`", target.name),
                by: vec![format!(
                    "{} (destroy them, or remove their routes and apply)",
                    mine.join(", ")
                )],
            });
        }
        let report = |action, adopted| EdgeUninstallReport {
            target: target.name.clone(),
            action,
            kind: config.kind,
            adopted,
        };
        let manifest = match &probe.manifest {
            Some(Ok(m)) if m.kind == config.kind => m.clone(),
            // Nothing of ikit's is left on the host; forget it here too.
            _ => {
                self.store.targets().set_edge(&target.name, None).await?;
                return Ok(report(EdgeUninstallAction::Left, false));
            }
        };
        let others: Vec<String> = manifest
            .orgs
            .iter()
            .filter(|o| **o != self.org.name)
            .cloned()
            .collect();
        if !others.is_empty() {
            let mut m = manifest.clone();
            if m.installed_by == self.org.name {
                m.installed_by = others[0].clone();
                m.exclusive = false;
            }
            m.orgs = others;
            self.write_manifest(&target, &m).await?;
            self.store.targets().set_edge(&target.name, None).await?;
            return Ok(report(EdgeUninstallAction::Left, manifest.adopted));
        }
        let purge = purge && !manifest.adopted;
        let family = match probe.family {
            Some(f) => f,
            None if purge => {
                return Err(AppError::Invalid(
                    "cannot remove the package here (not an apt or dnf host); drop --purge".into(),
                ));
            }
            None => PkgFamily::Apt,
        };
        let out = self
            .edge_sh(
                &target,
                &provider::uninstall_script(config.kind, family, manifest.adopted, purge),
                Root::Required,
                None,
                INSTALL,
            )
            .await?;
        if out.code == Some(6) {
            return Err(failed(
                &format!("removing the reverse proxy from `{}`", target.name),
                &out,
            ));
        }
        self.edge_ok(
            &format!("removing the reverse proxy from `{}`", target.name),
            &target,
            &out,
        )?;
        self.store.targets().set_edge(&target.name, None).await?;
        Ok(report(EdgeUninstallAction::Removed, manifest.adopted))
    }

    /// The proxy part of a workload's plan, and the reasons it cannot be applied.
    /// `None` when the workload has nothing on a proxy and is not getting anything.
    pub(crate) async fn edge_plan(
        &self,
        wl: &Workload,
        target: &Target,
    ) -> Result<(Option<EdgePlan>, Vec<String>)> {
        let routes = self.store.edge().routes(wl.id).await?;
        let redirects = self.store.edge().redirects(wl.id).await?;
        let wanted = !routes.is_empty() || !redirects.is_empty();
        let Some(config) = &target.edge else {
            if wanted {
                return Ok((
                    None,
                    vec![format!(
                        "{} has routes but `{1}` has no reverse proxy for this org; set one up with \
                         `ikit target edge install {1}`",
                        wl.name, target.name
                    )],
                ));
            }
            return Ok((None, vec![]));
        };
        // A draft without routes has nothing on the proxy to remove.
        if !wanted && wl.status == WorkloadStatus::Draft {
            return Ok((None, vec![]));
        }
        let mut blockers = Vec::new();
        let probe = self.probe_host(target, Root::IfAvailable).await?;
        if let Err(e) = self.usable_manifest(target, config, &probe) {
            blockers.push(e.to_string());
        }
        let file = provider::site_file(&self.org.name, &wl.name);
        let applied = self.read_site(target, &file).await?;
        let rendered = if wanted {
            // The org's email wins over the one saved on the target when the proxy was set up.
            let edge = EdgeConfig {
                email: self.config()?.edge.acme_email.or(config.email.clone()),
                ..config.clone()
            };
            let input = SiteInput {
                org: &self.org.name,
                workload: &wl.name,
                routes: &routes,
                redirects: &redirects,
                edge: &edge,
            };
            match provider::render_site(config.kind, &input) {
                Ok(r) => Some(r),
                Err(e) => {
                    blockers.push(e.to_string());
                    None
                }
            }
        } else {
            None
        };

        // Domains and upstream ports other workloads (of any org) hold on this host.
        for (other_file, header) in &probe.sites {
            let Some(h) = header else { continue };
            if *other_file == file || (h.org == self.org.name && h.workload == wl.name) {
                continue;
            }
            let hosts = h.hosts();
            for host in routes
                .iter()
                .map(|r| r.host.as_str())
                .chain(redirects.iter().map(|r| r.host.as_str()))
                .collect::<std::collections::BTreeSet<_>>()
            {
                if hosts.contains(host) {
                    blockers.push(format!(
                        "{host} is already served on `{}` by {}; pick another domain",
                        target.name,
                        h.owner()
                    ));
                }
            }
            for r in &routes {
                if h.routes.iter().any(|o| o.upstream == r.upstream_port) {
                    blockers.push(format!(
                        "upstream port {} of {} is used on `{}` by {}; run `ikit route add` again \
                         after `ikit route rm {}` to get a new one",
                        r.upstream_port,
                        r.key(),
                        target.name,
                        h.owner(),
                        r.key()
                    ));
                }
            }
        }

        // The proxy owns 80 and 443; containers must not publish them.
        for s in self.store.workloads().services(wl.id).await? {
            if let Some(p) = s
                .publish
                .iter()
                .find(|p| !p.udp && matches!(p.host, 80 | 443))
            {
                blockers.push(format!(
                    "`{}` publishes host port {}, which the reverse proxy on `{}` uses; route it \
                     with `ikit route add {}` and stop publishing it with \
                     `ikit svc publish {} --remove {}`",
                    s.name, p.host, target.name, s.name, s.name, p.host
                ));
            }
        }

        let env_config = self.config()?;
        if env_config.edge.require_tls_envs.contains(&wl.env)
            && let Some(r) = routes.iter().find(|r| r.tls == TlsMode::Off)
        {
            blockers.push(format!(
                "`{}` routes need TLS; {} uses `tls off` (change it with `ikit route add ... --tls acme`)",
                wl.env,
                r.key()
            ));
        }

        let warnings = self.edge_warnings(target, &routes, &redirects).await;
        let changes = provider::diff_site(applied.as_deref(), rendered.as_deref());
        let hosts = routes
            .iter()
            .map(|r| r.host.clone())
            .chain(redirects.iter().map(|r| r.host.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok((
            Some(EdgePlan {
                kind: config.kind,
                file,
                changes,
                warnings,
                rendered,
                hosts,
            }),
            blockers,
        ))
    }

    /// Domains that ACME cannot get certificates for yet.
    async fn edge_warnings(
        &self,
        target: &Target,
        routes: &[ikit_core::Route],
        redirects: &[ikit_core::Redirect],
    ) -> Vec<String> {
        let acme: std::collections::BTreeSet<&str> = routes
            .iter()
            .filter(|r| r.tls == TlsMode::Acme)
            .map(|r| r.host.as_str())
            .chain(
                redirects
                    .iter()
                    .filter(|r| r.tls == TlsMode::Acme)
                    .map(|r| r.host.as_str()),
            )
            .collect();
        let mut warnings = Vec::new();
        if !acme.is_empty() && crate::zerotier::zt_route(target).is_some() {
            warnings.push(format!(
                "`{}` is reached over ZeroTier; ACME certificates need the domains to reach its \
                 public address on ports 80/443 (use `--tls internal` for private sites)",
                target.name
            ));
        }
        for host in acme {
            let lookup = tokio::time::timeout(DNS, tokio::net::lookup_host((host, 443))).await;
            let resolves = match lookup {
                Ok(Ok(mut addrs)) => addrs.next().is_some(),
                _ => false,
            };
            if !resolves {
                warnings.push(format!(
                    "{host} does not resolve; point it at `{}` before applying, or the \
                     certificate request fails",
                    target.name
                ));
            }
        }
        warnings
    }

    /// The workload's site file on the host, if there is one (site files are world-readable).
    async fn read_site(&self, target: &Target, file: &str) -> Result<Option<String>> {
        let path = format!("{}/{file}", provider::SITES_DIR);
        let script = format!(
            "if [ -f {p} ]; then cat {p}; else exit 3; fi",
            p = sh(&path)
        );
        let out = self
            .edge_sh(target, &script, Root::IfAvailable, None, EDGE)
            .await?;
        match out.code {
            Some(0) => Ok(Some(out.stdout)),
            Some(3) => Ok(None),
            _ => Err(failed(
                &format!("reading {path} on `{}`", target.name),
                &out,
            )),
        }
    }

    /// Puts the planned site file in place (or removes it), checks the config and reloads.
    pub(crate) async fn edge_apply(&self, target: &Target, plan: &EdgePlan) -> Result<()> {
        if plan.changes.is_empty() {
            return Ok(());
        }
        let (script, stdin) = match &plan.rendered {
            Some(text) => {
                let hosts: Vec<&str> = plan.hosts.iter().map(String::as_str).collect();
                (
                    provider::write_site_script(plan.kind, &plan.file, &hosts),
                    Some(text.as_bytes()),
                )
            }
            None => (provider::remove_site_script(plan.kind, &plan.file), None),
        };
        let out = self
            .edge_sh(target, &script, Root::Required, stdin, EDGE)
            .await?;
        if out.code == Some(8) {
            return Err(failed(
                &format!(
                    "updating the {} config on `{}`: a domain was taken since the plan",
                    plan.kind, target.name
                ),
                &out,
            ));
        }
        if out.code == Some(4) {
            return Err(failed(
                &format!(
                    "updating the {} config on `{}` (the previous site was kept)",
                    plan.kind, target.name
                ),
                &out,
            ));
        }
        self.edge_ok(&format!("updating the {} config", plan.kind), target, &out)
    }

    /// Takes the workload's site off the target's proxy, before its containers stop.
    pub(crate) async fn edge_remove_site(&self, target: &Target, workload: &str) -> Result<()> {
        let Some(config) = &target.edge else {
            return Ok(());
        };
        let file = provider::site_file(&self.org.name, workload);
        let out = self
            .edge_sh(
                target,
                &provider::remove_site_script(config.kind, &file),
                Root::Required,
                None,
                EDGE,
            )
            .await?;
        self.edge_ok(
            &format!("removing {workload} from {}", config.kind),
            target,
            &out,
        )
    }

    /// Proxy problems for `ikit status --drift`: the proxy is gone or stopped, or the
    /// workload's site file is missing.
    pub(crate) async fn edge_drift(&self, wl: &Workload, target: &Target) -> Result<Vec<String>> {
        let Some(config) = &target.edge else {
            return Ok(vec![]);
        };
        let routed = !self.store.edge().routes(wl.id).await?.is_empty()
            || !self.store.edge().redirects(wl.id).await?.is_empty();
        if !routed || wl.status == WorkloadStatus::Draft {
            return Ok(vec![]);
        }
        let probe = self.probe_host(target, Root::IfAvailable).await?;
        if let Err(e) = self.usable_manifest(target, config, &probe) {
            return Ok(vec![e.to_string()]);
        }
        let mut items = Vec::new();
        match probe.servers.get(&config.kind) {
            Some(s) if s.active => {}
            Some(_) => items.push(format!("{} is not running", config.kind)),
            None => items.push(format!("{} is not installed", config.kind)),
        }
        if probe.site_of(&self.org.name, &wl.name).is_none() {
            items.push(format!(
                "the site file {} is missing",
                provider::site_file(&self.org.name, &wl.name)
            ));
        }
        Ok(items)
    }

    /// `target test`'s proxy check.
    pub(crate) async fn edge_check(&self, target: &Target) -> Option<ikit_core::target::Check> {
        let config = target.edge.as_ref()?;
        let check = |ok, detail| ikit_core::target::Check {
            name: "reverse proxy".into(),
            ok,
            detail,
        };
        let probe = match self.probe_host(target, Root::IfAvailable).await {
            Ok(p) => p,
            Err(e) => return Some(check(false, e.to_string())),
        };
        if let Err(e) = self.usable_manifest(target, config, &probe) {
            return Some(check(false, e.to_string()));
        }
        Some(match probe.servers.get(&config.kind) {
            Some(s) if s.active => check(true, format!("{} {} running", config.kind, s.version)),
            Some(s) => check(
                false,
                format!("{} {} is not running", config.kind, s.version),
            ),
            None => check(false, format!("{} is not installed", config.kind)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn req(kind: EdgeKind) -> EdgeInstallRequest {
        EdgeInstallRequest {
            kind,
            email: None,
            ca: AcmeCa::Production,
            version: None,
            exclusive: false,
            open_firewall: false,
        }
    }

    fn linux() -> HostProbe {
        HostProbe {
            os: "Linux".into(),
            systemd: true,
            family: Some(PkgFamily::Apt),
            root: true,
            ..Default::default()
        }
    }

    fn manifest(kind: EdgeKind, orgs: &[&str]) -> HostManifest {
        HostManifest {
            kind,
            installed_by: orgs[0].into(),
            adopted: false,
            exclusive: false,
            orgs: orgs.iter().map(|o| o.to_string()).collect(),
            version: Some("2.10.0".into()),
            installed_at: Utc::now(),
            upgraded_at: None,
        }
    }

    fn installed(active: bool) -> InstalledServer {
        InstalledServer {
            version: "2.10.0".into(),
            active,
            packaged: true,
        }
    }

    #[test]
    fn installs_on_a_clean_host() {
        assert_eq!(
            decide(&linux(), "acme", &req(EdgeKind::Caddy)).unwrap(),
            Decision::Install(PkgFamily::Apt)
        );
        let unknown = HostProbe {
            family: None,
            ..linux()
        };
        assert!(decide(&unknown, "acme", &req(EdgeKind::Caddy)).is_err());
    }

    #[test]
    fn refuses_non_linux_and_no_systemd() {
        let mac = HostProbe {
            os: "Darwin".into(),
            ..linux()
        };
        let err = decide(&mac, "acme", &req(EdgeKind::Caddy)).unwrap_err();
        assert!(err.to_string().contains("Linux"), "{err}");
        let init = HostProbe {
            systemd: false,
            ..linux()
        };
        assert!(decide(&init, "acme", &req(EdgeKind::Caddy)).is_err());
    }

    #[test]
    fn adopts_an_existing_proxy_without_host_json() {
        let probe = HostProbe {
            servers: BTreeMap::from([(EdgeKind::Caddy, installed(true))]),
            listeners: vec!["0.0.0.0:443 caddy".into()],
            ..linux()
        };
        assert_eq!(
            decide(&probe, "acme", &req(EdgeKind::Caddy)).unwrap(),
            Decision::Adopt
        );
        // Asking for Ferron while Caddy runs.
        let err = decide(&probe, "acme", &req(EdgeKind::Ferron)).unwrap_err();
        assert!(err.to_string().contains("ports 80/443 are taken"), "{err}");
        // A stopped Caddy does not block Ferron.
        let stopped = HostProbe {
            servers: BTreeMap::from([(EdgeKind::Caddy, installed(false))]),
            ..linux()
        };
        assert!(matches!(
            decide(&stopped, "acme", &req(EdgeKind::Ferron)).unwrap(),
            Decision::Install(_)
        ));
    }

    #[test]
    fn refuses_other_servers_on_80_and_443() {
        let probe = HostProbe {
            listeners: vec!["0.0.0.0:80 nginx".into()],
            ..linux()
        };
        let err = decide(&probe, "acme", &req(EdgeKind::Caddy)).unwrap_err();
        assert!(err.to_string().contains("nginx"), "{err}");
    }

    #[test]
    fn joins_or_updates_a_shared_proxy() {
        let probe = HostProbe {
            manifest: Some(Ok(manifest(EdgeKind::Caddy, &["globex"]))),
            ..linux()
        };
        assert!(matches!(
            decide(&probe, "acme", &req(EdgeKind::Caddy)).unwrap(),
            Decision::Join(_)
        ));
        assert!(matches!(
            decide(&probe, "globex", &req(EdgeKind::Caddy)).unwrap(),
            Decision::Update(_)
        ));
        let err = decide(&probe, "acme", &req(EdgeKind::Ferron)).unwrap_err();
        assert!(err.to_string().contains("already runs caddy"), "{err}");
        let exclusive = EdgeInstallRequest {
            exclusive: true,
            ..req(EdgeKind::Caddy)
        };
        assert!(decide(&probe, "acme", &exclusive).is_err());
        let mut m = manifest(EdgeKind::Caddy, &["globex"]);
        m.exclusive = true;
        let probe = HostProbe {
            manifest: Some(Ok(m)),
            ..linux()
        };
        let err = decide(&probe, "acme", &req(EdgeKind::Caddy)).unwrap_err();
        assert!(err.to_string().contains("reserved for org globex"), "{err}");
    }

    #[test]
    fn reads_script_values() {
        assert_eq!(
            script_value("installing\nversion=v2.10.0 h1:abc\n", "version").as_deref(),
            Some("2.10.0")
        );
        assert_eq!(
            script_value("version=Ferron 3.0.1\n", "version").as_deref(),
            Some("3.0.1")
        );
        assert_eq!(script_value("version=\n", "version"), None);
        assert!(check_version("2.10.0-1").is_ok());
        assert!(check_version("2.10; rm -rf /").is_err());
    }

    /// A Linux host with apt and systemd: runs the edge scripts against in-memory state
    /// (host.json, site files, an installed proxy) and answers everything else (docker)
    /// with success.
    #[derive(Default)]
    pub(crate) struct FakeHost {
        pub installed: std::sync::Mutex<Option<String>>,
        pub manifest: std::sync::Mutex<Option<String>>,
        pub sites: std::sync::Mutex<BTreeMap<String, String>>,
        pub scripts: std::sync::Mutex<Vec<String>>,
    }

    /// The first site file a script names (skipping the `*.conf` glob).
    fn site_name(script: &str) -> String {
        script
            .split("/etc/ikit/edge/sites/")
            .skip(1)
            .map(|rest| {
                rest.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || "_.-".contains(*c))
                    .collect::<String>()
            })
            .find(|name| !name.is_empty())
            .unwrap()
            .trim_end_matches(".ikit-new")
            .trim_end_matches(".ikit-prev")
            .to_owned()
    }

    fn heredoc(script: &str) -> String {
        let start = script.find("<<'IKIT_EOF'\n{").unwrap() + "<<'IKIT_EOF'\n".len();
        let end = start + script[start..].find("\nIKIT_EOF").unwrap();
        script[start..end].to_owned()
    }

    #[async_trait::async_trait]
    impl crate::Exec for FakeHost {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            stdin: Option<&[u8]>,
            _: Duration,
        ) -> std::io::Result<ExecOutput> {
            let ok = |stdout: String| {
                Ok(ExecOutput {
                    code: Some(0),
                    stdout,
                    ..Default::default()
                })
            };
            if program != "sh" {
                return ok(String::new());
            }
            let script = args.last().unwrap().replace(r"'\''", "'");
            self.scripts.lock().unwrap().push(script.clone());
            if script.contains("@@host") {
                let mut out = "os=Linux\nsystemd=yes\nfamily=apt\nroot=yes\n".to_owned();
                if let Some(v) = &*self.installed.lock().unwrap() {
                    out.push_str(&format!(
                        "caddy.version=v{v} h1:x\ncaddy.active=active\ncaddy.packaged=yes\n\
                         listen=0.0.0.0:443 users:((\"caddy\",pid=1,fd=3))\n"
                    ));
                }
                if let Some(m) = &*self.manifest.lock().unwrap() {
                    out.push_str(&format!("@@host\n{m}\n@@end\n"));
                }
                for (name, text) in &*self.sites.lock().unwrap() {
                    out.push_str(&format!("@@site {name}\n"));
                    for l in text.lines().filter(|l| l.starts_with("# ikit ")) {
                        out.push_str(l);
                        out.push('\n');
                    }
                }
                return ok(out);
            }
            if script.contains("else exit 3") && script.contains("/etc/ikit/edge/sites/") {
                return match self.sites.lock().unwrap().get(&site_name(&script)) {
                    Some(text) => ok(text.clone()),
                    None => Ok(ExecOutput {
                        code: Some(3),
                        ..Default::default()
                    }),
                };
            }
            if script.contains("apt-get install -y -q --allow-downgrades caddy")
                || script.contains("--only-upgrade caddy")
            {
                let previous = self.installed.lock().unwrap().clone().unwrap_or_default();
                let version = if script.contains("--only-upgrade") {
                    "2.11.0"
                } else {
                    "2.10.0"
                };
                *self.installed.lock().unwrap() = Some(version.into());
                if script.contains("host.json.ikit-new") {
                    *self.manifest.lock().unwrap() = Some(heredoc(&script));
                }
                return ok(format!("previous=v{previous}\nversion=v{version} h1:x\n"));
            }
            if script.contains("rm -rf /etc/ikit/edge") {
                *self.manifest.lock().unwrap() = None;
                return ok(String::new());
            }
            if script.contains("host.json.ikit-new") {
                *self.manifest.lock().unwrap() = Some(heredoc(&script));
                return ok(String::new());
            }
            if script.contains(".ikit-prev") {
                let name = site_name(&script);
                match stdin {
                    Some(text) => {
                        self.sites
                            .lock()
                            .unwrap()
                            .insert(name, String::from_utf8_lossy(text).into_owned());
                    }
                    None => {
                        self.sites.lock().unwrap().remove(&name);
                    }
                }
                return ok(String::new());
            }
            ok(String::new())
        }
    }

    async fn org_app(dir: &std::path::Path, name: &str, host: std::sync::Arc<FakeHost>) -> App {
        let paths = crate::Paths::new(dir);
        crate::org_create(
            &paths,
            crate::NewOrg {
                name: name.into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut config = crate::org_config(&paths, name).unwrap();
        config.edge.base_domain = Some(format!("{name}.invalid"));
        std::fs::write(
            paths.org(name).config_file(),
            serde_yaml_ng::to_string(&config).unwrap(),
        )
        .unwrap();
        let secrets = std::sync::Arc::new(crate::MemorySecretStore::default());
        let app = App::open(&paths, name, secrets)
            .await
            .unwrap()
            .with_exec(host);
        app.target_attach("box", Connection::Local).await.unwrap();
        let wl = app.workload_create("dev", "box").await.unwrap();
        let req = crate::ServiceRequest {
            name: Some("web".into()),
            registry: "nginx".into(),
            constraint: "1".parse().unwrap(),
            group: ikit_core::Category::App,
            env: BTreeMap::new(),
        };
        app.services_add(&wl.name, vec![req]).await.unwrap();
        app
    }

    #[tokio::test]
    async fn two_orgs_share_one_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let host = std::sync::Arc::new(FakeHost::default());
        let acme = org_app(dir.path(), "acme", host.clone()).await;
        let globex = org_app(dir.path(), "globex", host.clone()).await;

        // Routes need a proxy first.
        let route = acme
            .route_add(
                "wl-acme-dev-001",
                crate::RouteRequest {
                    service: "web".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(route.host, "web.wl-acme-dev-001.acme.invalid");
        assert_eq!(route.port, 80);
        assert_eq!(route.upstream_port, 20000);
        let plan = acme.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(
            plan.blockers[0].contains("no reverse proxy"),
            "{:?}",
            plan.blockers
        );

        let install = |kind| EdgeInstallRequest {
            kind,
            email: Some("ops@acme.invalid".into()),
            ..req(kind)
        };
        let r = acme
            .edge_install("box", install(EdgeKind::Caddy))
            .await
            .unwrap();
        assert_eq!(r.action, EdgeInstallAction::Installed);
        assert_eq!(r.version.as_deref(), Some("2.10.0"));
        let m: HostManifest =
            serde_json::from_str(host.manifest.lock().unwrap().as_ref().unwrap()).unwrap();
        assert_eq!(
            (m.installed_by.as_str(), m.version.as_deref()),
            ("acme", Some("2.10.0"))
        );
        // The email is the org's, kept in its config for later installs.
        assert_eq!(
            acme.config().unwrap().edge.acme_email.as_deref(),
            Some("ops@acme.invalid")
        );

        // Globex cannot bring Ferron, but joins Caddy.
        assert!(
            globex
                .edge_install("box", install(EdgeKind::Ferron))
                .await
                .is_err()
        );
        let r = globex
            .edge_install("box", install(EdgeKind::Caddy))
            .await
            .unwrap();
        assert_eq!(r.action, EdgeInstallAction::Joined);
        assert_eq!(r.orgs, ["acme", "globex"]);
        let r = acme
            .edge_install("box", install(EdgeKind::Caddy))
            .await
            .unwrap();
        assert_eq!(r.action, EdgeInstallAction::Updated);

        // Apply puts the site in place and publishes the upstream on loopback.
        let plan = acme.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
        let edge = plan.edge.as_ref().unwrap();
        assert_eq!(edge.changes.len(), 1);
        assert!(plan.rendered.compose.contains("127.0.0.1:20000:80"));
        acme.deploy_apply(&plan, "test", false).await.unwrap();
        let site = host.sites.lock().unwrap()["acme__wl-acme-dev-001.conf"].clone();
        assert!(site.contains("web.wl-acme-dev-001.acme.invalid {"));
        assert!(site.contains("tls ops@acme.invalid"));
        assert!(site.contains("reverse_proxy 127.0.0.1:20000"));
        let plan = acme.deploy_plan("wl-acme-dev-001").await.unwrap();
        assert!(!plan.needs_apply());

        // Globex may not take acme's domain, and gets its own upstream port.
        let theirs = globex
            .route_add(
                "wl-globex-dev-001",
                crate::RouteRequest {
                    service: "web".into(),
                    host: Some(route.host.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(theirs.upstream_port, 20001);
        let plan = globex.deploy_plan("wl-globex-dev-001").await.unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|b| b.contains("already served") && b.contains("wl-acme-dev-001 (org acme)")),
            "{:?}",
            plan.blockers
        );

        // Upgrading touches everyone; it is recorded on the host.
        let up = acme.edge_upgrade("box", None).await.unwrap();
        assert_eq!(up.previous.as_deref(), Some("2.10.0"));
        assert_eq!(up.version.as_deref(), Some("2.11.0"));
        assert_eq!(up.orgs, ["acme", "globex"]);

        // Acme cannot leave while its site is up; destroy takes the site down first.
        assert!(acme.edge_uninstall("box", false).await.is_err());
        acme.deploy_destroy("wl-acme-dev-001", false, "test")
            .await
            .unwrap();
        assert!(host.sites.lock().unwrap().is_empty());
        let r = acme.edge_uninstall("box", false).await.unwrap();
        assert_eq!(r.action, EdgeUninstallAction::Left);
        let m: HostManifest =
            serde_json::from_str(host.manifest.lock().unwrap().as_ref().unwrap()).unwrap();
        assert_eq!((m.installed_by.as_str(), m.orgs.len()), ("globex", 1));
        assert_eq!(acme.target_show("box").await.unwrap().edge, None);

        // The last org out removes ikit from the proxy.
        let r = globex.edge_uninstall("box", true).await.unwrap();
        assert_eq!(r.action, EdgeUninstallAction::Removed);
        assert!(host.manifest.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn auto_routes_follow_services() {
        let dir = tempfile::tempdir().unwrap();
        let host = std::sync::Arc::new(FakeHost::default());
        let app = org_app(dir.path(), "acme", host).await;
        let wl = "wl-acme-dev-001";
        // `web` was added to the app group.
        let list = app
            .route_auto_on(wl, vec![ikit_core::Category::Platform], None)
            .await
            .unwrap();
        assert!(list.routes.is_empty());
        let list = app
            .route_auto_on(wl, vec![ikit_core::Category::App], None)
            .await
            .unwrap();
        assert_eq!(list.routes.len(), 1);
        assert_eq!(list.routes[0].origin, ikit_core::RouteOrigin::Auto);

        // Removed by hand: not added back.
        app.route_remove(wl, &list.routes[0].key()).await.unwrap();
        app.after_services_changed(wl).await;
        let list = app.route_list(wl).await.unwrap();
        assert!(list.routes.is_empty());
        assert_eq!(list.auto.unwrap().excluded, ["web"]);

        // A manual route survives `auto off`; removing the service removes it.
        app.route_add(
            wl,
            crate::RouteRequest {
                service: "web".into(),
                host: Some("www.acme.invalid".into()),
                path: Some("/app/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let list = app.route_auto_off(wl).await.unwrap();
        assert_eq!(list.routes[0].key(), "www.acme.invalid/app");
        assert!(list.auto.is_none());
        app.services_remove(wl, &["web".into()]).await.unwrap();
        assert!(app.route_list(wl).await.unwrap().routes.is_empty());
    }
}
