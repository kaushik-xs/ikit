//! Shell scripts that inspect and change the edge on a host, and parsing of the probe's
//! output. The scripts run as root (the app wraps them in `sudo -n` when needed); every
//! script that changes something holds `/etc/ikit/edge/.lock`, so applies from different
//! orgs or machines take turns.

use std::collections::BTreeMap;

use ikit_core::{EdgeKind, HostManifest};
use serde::Serialize;

use super::{
    EDGE_DIR, HOST_FILE, PLACEHOLDER_SITE, SITES_DIR, SiteHeader, base_config, include_line,
    parse_site_header, server, validate_cmd, version_cmd,
};

/// How packages are installed on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PkgFamily {
    /// Debian, Ubuntu and derivatives.
    Apt,
    /// RHEL, Fedora, Rocky, Alma, CentOS Stream.
    Dnf,
}

/// A proxy binary found on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledServer {
    pub version: String,
    /// `systemctl is-active` says `active`.
    pub active: bool,
    /// Installed from a package (so ikit can upgrade it).
    pub packaged: bool,
}

/// What [`probe_script`] found on a host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HostProbe {
    /// `uname -s`.
    pub os: String,
    pub systemd: bool,
    pub family: Option<PkgFamily>,
    /// The probe ran as root, so listeners show their process.
    pub root: bool,
    pub servers: BTreeMap<EdgeKind, InstalledServer>,
    /// Sockets listening on port 80 or 443: `address process`.
    pub listeners: Vec<String>,
    /// `host.json`, or why it could not be read.
    pub manifest: Option<Result<HostManifest, String>>,
    /// Site files (other than the placeholder) and their headers.
    pub sites: Vec<(String, Option<SiteHeader>)>,
}

impl HostProbe {
    /// Processes listening on 80/443 that are not `kind`.
    pub fn foreign_listeners(&self, kind: Option<EdgeKind>) -> Vec<&str> {
        let own = kind.map(|k| server(k).binary);
        self.listeners
            .iter()
            .filter(|l| {
                let process = l.split_once(' ').map(|(_, p)| p).unwrap_or("");
                own.is_none_or(|o| !process.starts_with(o))
            })
            .map(String::as_str)
            .collect()
    }

    /// Site files whose header names `org` and `workload`.
    pub fn site_of(&self, org: &str, workload: &str) -> Option<&SiteHeader> {
        self.sites
            .iter()
            .filter_map(|(_, h)| h.as_ref())
            .find(|h| h.org == org && h.workload == workload)
    }
}

/// Read-only: OS, init system, package family, installed proxies, listeners on 80/443,
/// `host.json` and the site file headers.
pub fn probe_script() -> String {
    let mut s = String::from(
        r#"echo "os=$(uname -s)"
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then echo systemd=yes; else echo systemd=no; fi
family=
if [ -r /etc/os-release ]; then
  . /etc/os-release
  case " ${ID:-} ${ID_LIKE:-} " in
    *" debian "*|*" ubuntu "*) family=apt ;;
    *" rhel "*|*" fedora "*|*" centos "*|*" rocky "*|*" almalinux "*) family=dnf ;;
  esac
fi
echo "family=$family"
if [ "$(id -u)" = 0 ]; then echo root=yes; else echo root=no; fi
"#,
    );
    for kind in EdgeKind::ALL {
        let srv = server(*kind);
        s.push_str(&format!(
            r#"if command -v {bin} >/dev/null 2>&1; then
  echo "{kind}.version=$({version} 2>/dev/null | head -n 1)"
  echo "{kind}.active=$(systemctl is-active {svc} 2>/dev/null)"
  if command -v dpkg-query >/dev/null 2>&1 && dpkg-query -W -f='${{Status}}' {pkg} 2>/dev/null | grep -q 'install ok installed'; then echo {kind}.packaged=yes
  elif command -v rpm >/dev/null 2>&1 && rpm -q {pkg} >/dev/null 2>&1; then echo {kind}.packaged=yes; fi
fi
"#,
            bin = srv.binary,
            version = version_cmd(*kind),
            svc = srv.service,
            pkg = srv.package,
        ));
    }
    s.push_str(&format!(
        r#"if command -v ss >/dev/null 2>&1; then
  ss -ltnpH 2>/dev/null | awk '$4 ~ /:(80|443)$/ {{ print "listen=" $4 " " $6 }}'
fi
if [ -f {HOST_FILE} ]; then echo @@host; cat {HOST_FILE}; echo; echo @@end; fi
for f in {SITES_DIR}/*.conf; do
  [ -f "$f" ] || continue
  [ "$(basename "$f")" = {PLACEHOLDER_SITE} ] && continue
  echo "@@site $(basename "$f")"
  grep '^# ikit ' "$f" || true
done
"#
    ));
    s
}

/// The process name in `ss -p` output: `users:(("nginx",pid=1,fd=6))` → `nginx`.
fn process_name(users: &str) -> &str {
    users
        .split_once("((\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or(users, |(name, _)| name)
}

/// The version number in a `--version` line: `v2.10.0 h1:…` → `2.10.0`.
fn version_number(line: &str) -> String {
    line.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(line.trim())
        .to_owned()
}

pub fn parse_probe(stdout: &str) -> HostProbe {
    let mut probe = HostProbe::default();
    let mut lines = stdout.lines();
    let mut site: Option<(String, String)> = None;
    let flush = |probe: &mut HostProbe, site: &mut Option<(String, String)>| {
        if let Some((name, text)) = site.take() {
            probe.sites.push((name, parse_site_header(&text)));
        }
    };
    while let Some(line) = lines.next() {
        if line == "@@host" {
            let mut json = String::new();
            for l in lines.by_ref() {
                if l == "@@end" {
                    break;
                }
                json.push_str(l);
                json.push('\n');
            }
            probe.manifest = Some(
                serde_json::from_str(&json).map_err(|e| format!("{HOST_FILE} is not valid: {e}")),
            );
            continue;
        }
        if let Some(name) = line.strip_prefix("@@site ") {
            flush(&mut probe, &mut site);
            site = Some((name.to_owned(), String::new()));
            continue;
        }
        if let Some((_, text)) = site.as_mut() {
            text.push_str(line);
            text.push('\n');
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "os" => probe.os = value.to_owned(),
            "systemd" => probe.systemd = value == "yes",
            "root" => probe.root = value == "yes",
            "family" => {
                probe.family = match value {
                    "apt" => Some(PkgFamily::Apt),
                    "dnf" => Some(PkgFamily::Dnf),
                    _ => None,
                }
            }
            "listen" => {
                let (addr, users) = value.split_once(' ').unwrap_or((value, ""));
                probe
                    .listeners
                    .push(format!("{addr} {}", process_name(users)).trim().to_owned());
            }
            _ => {
                let Some((kind, field)) = key.split_once('.') else {
                    continue;
                };
                let Ok(kind) = kind.parse::<EdgeKind>() else {
                    continue;
                };
                let entry = probe.servers.entry(kind).or_insert(InstalledServer {
                    version: String::new(),
                    active: false,
                    packaged: false,
                });
                match field {
                    "version" => entry.version = version_number(value),
                    "active" => entry.active = value == "active",
                    "packaged" => entry.packaged = value == "yes",
                    _ => {}
                }
            }
        }
    }
    flush(&mut probe, &mut site);
    probe
}

/// Creates the edge folders and takes the lock for the rest of the script.
fn prelude() -> String {
    format!(
        r#"set -e
umask 022
mkdir -p {SITES_DIR}
exec 9>{EDGE_DIR}/.lock
if command -v flock >/dev/null 2>&1; then
  ikit_wait=0
  while ! flock -n 9; do
    ikit_wait=$((ikit_wait + 1))
    if [ "$ikit_wait" -ge 120 ]; then echo "another ikit run is changing the edge on this host; try again" >&2; exit 75; fi
    sleep 1
  done
fi
"#
    )
}

/// Writes `content` to `path` atomically, readable by everyone (the proxy runs as its
/// own user).
fn write_file(path: &str, content: &str) -> String {
    format!(
        "cat > {path}.ikit-new <<'IKIT_EOF'\n{content}\nIKIT_EOF\nchmod 644 {path}.ikit-new\nmv {path}.ikit-new {path}\n"
    )
}

/// Validates the full config; on failure runs `on_fail`, prints the errors and exits 4.
fn validate(kind: EdgeKind, on_fail: &str) -> String {
    format!(
        "if ! ikit_out=$({} 2>&1); then\n  {on_fail}\n  printf '%s\\n' \"$ikit_out\" >&2\n  echo \"the {kind} config is invalid\" >&2\n  exit 4\nfi\n",
        validate_cmd(kind)
    )
}

fn package_install(kind: EdgeKind, family: PkgFamily, version: Option<&str>) -> String {
    let pkg = server(kind).package;
    match family {
        PkgFamily::Apt => {
            let spec = version.map_or(pkg.to_owned(), |v| format!("{pkg}={v}"));
            format!("apt-get install -y -q --allow-downgrades {spec}\n")
        }
        PkgFamily::Dnf => {
            let spec = version.map_or(pkg.to_owned(), |v| format!("{pkg}-{v}"));
            format!("dnf install -y -q {spec} || dnf downgrade -y -q {spec}\n")
        }
    }
}

/// Adds the proxy's package repository.
fn add_repo(kind: EdgeKind, family: PkgFamily) -> &'static str {
    match (kind, family) {
        (EdgeKind::Caddy, PkgFamily::Apt) => {
            r#"export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q curl gnupg ca-certificates
curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt > /etc/apt/sources.list.d/caddy-stable.list
chmod o+r /usr/share/keyrings/caddy-stable-archive-keyring.gpg /etc/apt/sources.list.d/caddy-stable.list
apt-get update -q
"#
        }
        (EdgeKind::Caddy, PkgFamily::Dnf) => {
            r#"dnf install -y -q 'dnf-command(copr)'
dnf copr enable -y @caddy/caddy
"#
        }
        (EdgeKind::Ferron, PkgFamily::Apt) => {
            r#"export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q curl gnupg ca-certificates lsb-release
curl -fsSL https://deb.ferron.sh/signing.pgp | gpg --batch --yes --dearmor -o /usr/share/keyrings/ferron-keyring.gpg
echo "deb [signed-by=/usr/share/keyrings/ferron-keyring.gpg] https://deb.ferron.sh $(lsb_release -cs) main" > /etc/apt/sources.list.d/ferron.list
apt-get update -q
"#
        }
        (EdgeKind::Ferron, PkgFamily::Dnf) => {
            r#"curl -fsSL https://rpm.ferron.sh/ferron.repo -o /etc/yum.repos.d/ferron.repo
"#
        }
    }
}

fn manifest_json(manifest: &HostManifest) -> String {
    serde_json::to_string_pretty(manifest).expect("manifest serializes")
}

/// Starts or reloads the service and prints its version.
fn start_and_report(kind: EdgeKind) -> String {
    let svc = server(kind).service;
    format!(
        "systemctl enable {svc} >/dev/null 2>&1 || true\nsystemctl reload-or-restart {svc}\necho \"version=$({} 2>/dev/null | head -n 1)\"\n",
        version_cmd(kind)
    )
}

/// Installs the proxy from its package repository, replaces its main config with one
/// that includes the site files (keeping the original as `.ikit-orig`), starts it and
/// writes `host.json`. Prints `version=…`.
pub fn install_script(
    kind: EdgeKind,
    family: PkgFamily,
    version: Option<&str>,
    manifest: &HostManifest,
) -> String {
    let main = server(kind).main_config;
    let mut s = prelude();
    s.push_str(add_repo(kind, family));
    s.push_str(&package_install(kind, family, version));
    s.push_str(&format!(
        "mkdir -p $(dirname {main})\nif [ -f {main} ] && [ ! -f {main}.ikit-orig ]; then cp -p {main} {main}.ikit-orig; fi\n"
    ));
    s.push_str(&write_file(main, base_config(kind).trim_end()));
    s.push_str(&write_file(
        &format!("{SITES_DIR}/{PLACEHOLDER_SITE}"),
        "# Keeps the include glob matching; ikit site files sit next to it.",
    ));
    s.push_str(&validate(kind, ""));
    s.push_str(&write_file(HOST_FILE, &manifest_json(manifest)));
    s.push_str(&start_and_report(kind));
    s
}

/// Takes over a proxy that was installed without ikit: appends the include line to its
/// main config (once), checks the config, starts or reloads it and writes `host.json`.
/// The rest of the config is left alone.
pub fn adopt_script(kind: EdgeKind, manifest: &HostManifest) -> String {
    let main = server(kind).main_config;
    let include = include_line(kind);
    let mut s = prelude();
    s.push_str(&format!(
        r#"[ -f {main} ] || {{ echo "{main} not found: is {kind} installed from its package?" >&2; exit 3; }}
added=
if ! grep -qxF '{include}' {main}; then
  cp -p {main} {main}.ikit-orig
  printf '\n%s\n' '{include}' >> {main}
  added=1
fi
"#
    ));
    s.push_str(&write_file(
        &format!("{SITES_DIR}/{PLACEHOLDER_SITE}"),
        "# Keeps the include glob matching; ikit site files sit next to it.",
    ));
    s.push_str(&validate(
        kind,
        &format!("if [ -n \"$added\" ]; then mv {main}.ikit-orig {main}; fi"),
    ));
    s.push_str(&write_file(HOST_FILE, &manifest_json(manifest)));
    s.push_str(&start_and_report(kind));
    s
}

/// Allows 80 and 443 in ufw or firewalld when one of them is active. Prints
/// `firewall=ufw|firewalld|none`.
pub fn firewall_script() -> String {
    r#"set -e
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; then
  ufw allow 80/tcp >/dev/null
  ufw allow 443/tcp >/dev/null
  echo firewall=ufw
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-service=http --add-service=https >/dev/null
  firewall-cmd --reload >/dev/null
  echo firewall=firewalld
else
  echo firewall=none
fi
"#
    .to_owned()
}

/// Replaces `host.json` (an org joined or left).
pub fn write_host_script(manifest: &HostManifest) -> String {
    let mut s = prelude();
    s.push_str(&write_file(HOST_FILE, &manifest_json(manifest)));
    s
}

/// Installs the site file read from stdin as `file`, keeping the previous one if the
/// config does not validate, then reloads the proxy. Exits 8 without changing anything if
/// another site file (of any org) already claims one of `hosts`: checked under the lock, so
/// two orgs applying the same domain cannot both get it, and Ferron (which accepts
/// duplicate host blocks) never serves one domain from two sites.
pub fn write_site_script(kind: EdgeKind, file: &str, hosts: &[&str]) -> String {
    let path = format!("{SITES_DIR}/{file}");
    let mut s = prelude();
    for host in hosts {
        let pattern = format!("^# ikit (route|redirect) {}( |$)", host.replace('.', "\\."));
        s.push_str(&format!(
            r#"for f in {SITES_DIR}/*.conf; do
  [ -f "$f" ] && [ "$f" != {path} ] || continue
  if grep -qE '{pattern}' "$f"; then echo "{host} is already served by $(basename "$f")" >&2; exit 8; fi
done
"#
        ));
    }
    s.push_str(&format!(
        r#"cat > {path}.ikit-new
chmod 644 {path}.ikit-new
had=
if [ -f {path} ]; then cp -p {path} {path}.ikit-prev; had=1; fi
mv {path}.ikit-new {path}
"#
    ));
    s.push_str(&validate(
        kind,
        &format!("if [ -n \"$had\" ]; then mv {path}.ikit-prev {path}; else rm -f {path}; fi"),
    ));
    s.push_str(&format!(
        "rm -f {path}.ikit-prev\nsystemctl reload-or-restart {}\n",
        server(kind).service
    ));
    s
}

/// Checks the config and reloads the proxy (starting it if it is stopped).
pub fn reload_script(kind: EdgeKind) -> String {
    let mut s = prelude();
    s.push_str(&validate(kind, ""));
    s.push_str(&format!(
        "systemctl reload-or-restart {}\n",
        server(kind).service
    ));
    s
}

/// Removes a workload's site file and reloads the proxy.
pub fn remove_site_script(kind: EdgeKind, file: &str) -> String {
    let path = format!("{SITES_DIR}/{file}");
    let mut s = prelude();
    s.push_str(&format!(
        "if [ ! -f {path} ]; then exit 0; fi\nmv {path} {path}.ikit-prev\n"
    ));
    s.push_str(&validate(kind, &format!("mv {path}.ikit-prev {path}")));
    s.push_str(&format!(
        "rm -f {path}.ikit-prev\nsystemctl reload-or-restart {}\n",
        server(kind).service
    ));
    s
}

/// Upgrades the proxy's package (to `version`, or the newest), checks the config with the
/// new binary and restarts it. If the config no longer validates or the service does not
/// come back, the previous package version is reinstalled (exit 4 or 5). Prints
/// `previous=…` and `version=…`.
pub fn upgrade_script(kind: EdgeKind, family: PkgFamily, version: Option<&str>) -> String {
    let srv = server(kind);
    let pkg = srv.package;
    let svc = srv.service;
    let mut s = prelude();
    let (current, refresh, upgrade, rollback) = match family {
        PkgFamily::Apt => (
            format!("dpkg-query -W -f='${{Version}}' {pkg}"),
            "export DEBIAN_FRONTEND=noninteractive\napt-get update -q\n".to_owned(),
            match version {
                Some(_) => package_install(kind, family, version),
                None => format!("apt-get install -y -q --only-upgrade {pkg}\n"),
            },
            format!("apt-get install -y -q --allow-downgrades {pkg}=\"$old\" >&2 || true"),
        ),
        PkgFamily::Dnf => (
            format!("rpm -q --qf '%{{VERSION}}-%{{RELEASE}}' {pkg}"),
            String::new(),
            match version {
                Some(_) => package_install(kind, family, version),
                None => format!("dnf upgrade -y -q {pkg}\n"),
            },
            format!(
                "dnf downgrade -y -q {pkg}-\"$old\" >&2 || dnf install -y -q {pkg}-\"$old\" >&2 || true"
            ),
        ),
    };
    s.push_str(&format!(
        "old=$({current} 2>/dev/null) || {{ echo \"{pkg} is not installed from a package; upgrade it by hand\" >&2; exit 3; }}\necho \"previous=$({} 2>/dev/null | head -n 1)\"\n",
        version_cmd(kind)
    ));
    s.push_str(&refresh);
    s.push_str(&upgrade);
    s.push_str(&validate(
        kind,
        &format!("{rollback}; systemctl restart {svc} || true"),
    ));
    s.push_str(&format!(
        r#"systemctl restart {svc}
i=0
while ! systemctl is-active --quiet {svc}; do
  i=$((i + 1))
  if [ "$i" -ge 15 ]; then
    journalctl -u {svc} -n 20 --no-pager >&2 || true
    {rollback}
    systemctl restart {svc} || true
    echo "{svc} did not start after the upgrade; {pkg} $old was reinstalled" >&2
    exit 5
  fi
  sleep 1
done
echo "version=$({} 2>/dev/null | head -n 1)"
"#,
        version_cmd(kind)
    ));
    s
}

/// Removes ikit from the proxy once no site files are left. A proxy ikit installed is
/// stopped and disabled (and its package removed with `purge`) and its original config
/// put back; an adopted proxy only loses the include line and keeps running.
pub fn uninstall_script(kind: EdgeKind, family: PkgFamily, adopted: bool, purge: bool) -> String {
    let srv = server(kind);
    let main = srv.main_config;
    let svc = srv.service;
    let include = include_line(kind);
    let mut s = prelude();
    s.push_str(&format!(
        r#"for f in {SITES_DIR}/*.conf; do
  [ -f "$f" ] || continue
  [ "$(basename "$f")" = {PLACEHOLDER_SITE} ] && continue
  echo "site files remain in {SITES_DIR}; destroy those workloads first" >&2
  exit 6
done
"#
    ));
    if adopted {
        s.push_str(&format!(
            r#"if grep -qxF '{include}' {main}; then
  grep -vxF '{include}' {main} > {main}.ikit-new || true
  cat {main}.ikit-new > {main}
  rm -f {main}.ikit-new
fi
rm -f {main}.ikit-orig
"#
        ));
        s.push_str(&validate(kind, ""));
        s.push_str(&format!("systemctl reload-or-restart {svc}\n"));
    } else {
        s.push_str(&format!(
            "systemctl disable --now {svc} >/dev/null 2>&1 || true\nif [ -f {main}.ikit-orig ]; then mv {main}.ikit-orig {main}; fi\n"
        ));
        if purge {
            s.push_str(&match family {
                PkgFamily::Apt => format!(
                    "DEBIAN_FRONTEND=noninteractive apt-get purge -y -q {}\n",
                    srv.package
                ),
                PkgFamily::Dnf => format!("dnf remove -y -q {}\n", srv.package),
            });
        }
    }
    s.push_str(&format!("rm -rf {EDGE_DIR}\n"));
    s
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn parses_a_probe() {
        let out = r#"os=Linux
systemd=yes
family=apt
root=yes
caddy.version=v2.10.0 h1:abc
caddy.active=active
caddy.packaged=yes
listen=0.0.0.0:80 users:(("caddy",pid=10,fd=3))
listen=[::]:443 users:(("nginx",pid=11,fd=7))
@@host
{
  "kind": "caddy",
  "installed_by": "acme",
  "orgs": ["acme", "globex"],
  "installed_at": "2026-11-01T10:00:00Z"
}

@@end
@@site acme__wl-acme-dev-001.conf
# ikit site acme wl-acme-dev-001
# ikit route acme.com / 20000 acme
# ikit route acme.com /api 20001 acme
@@site broken.conf
"#;
        let p = parse_probe(out);
        assert_eq!(p.os, "Linux");
        assert!(p.systemd && p.root);
        assert_eq!(p.family, Some(PkgFamily::Apt));
        let caddy = &p.servers[&EdgeKind::Caddy];
        assert_eq!(caddy.version, "2.10.0");
        assert!(caddy.active && caddy.packaged);
        assert!(!p.servers.contains_key(&EdgeKind::Ferron));
        assert_eq!(p.listeners, ["0.0.0.0:80 caddy", "[::]:443 nginx"]);
        assert_eq!(
            p.foreign_listeners(Some(EdgeKind::Caddy)),
            ["[::]:443 nginx"]
        );
        let m = p.manifest.clone().unwrap().unwrap();
        assert_eq!(m.orgs, ["acme", "globex"]);
        assert!(!m.adopted && !m.exclusive);
        assert_eq!(p.sites.len(), 2);
        let site = p.site_of("acme", "wl-acme-dev-001").unwrap();
        assert_eq!(site.routes[0].path, None);
        assert_eq!(site.routes[1].path.as_deref(), Some("/api"));
        assert_eq!(p.sites[1], ("broken.conf".to_owned(), None));
    }

    #[test]
    fn bad_manifest_is_reported() {
        let p = parse_probe("@@host\nnot json\n@@end\n");
        assert!(p.manifest.unwrap().is_err());
    }

    #[test]
    fn scripts_take_the_lock_and_validate() {
        let m = HostManifest {
            kind: EdgeKind::Ferron,
            installed_by: "acme".into(),
            adopted: false,
            exclusive: false,
            orgs: vec!["acme".into()],
            version: None,
            installed_at: Utc::now(),
            upgraded_at: None,
        };
        let install = install_script(EdgeKind::Ferron, PkgFamily::Apt, Some("3.0.1"), &m);
        assert!(install.contains("while ! flock -n 9; do"));
        assert!(install.contains("apt-get install -y -q --allow-downgrades ferron3=3.0.1"));
        assert!(install.contains("include \"/etc/ikit/edge/sites/*.conf\""));
        assert!(install.contains("ferron validate -c /etc/ferron/ferron.conf"));
        assert!(install.contains("\"installed_by\": \"acme\""));

        let adopt = adopt_script(EdgeKind::Caddy, &m);
        assert!(
            adopt.contains("grep -qxF 'import /etc/ikit/edge/sites/*.conf' /etc/caddy/Caddyfile")
        );
        assert!(adopt.contains("mv /etc/caddy/Caddyfile.ikit-orig /etc/caddy/Caddyfile"));

        let site = write_site_script(EdgeKind::Caddy, "acme__w.conf", &["acme.com"]);
        assert!(site.contains(r"grep -qE '^# ikit (route|redirect) acme\.com( |$)'"));
        assert!(site.contains("caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile"));
        assert!(site.contains("mv /etc/ikit/edge/sites/acme__w.conf.ikit-prev"));
        assert!(site.contains("systemctl reload-or-restart caddy"));

        let up = upgrade_script(EdgeKind::Caddy, PkgFamily::Dnf, None);
        assert!(up.contains("dnf upgrade -y -q caddy"));
        assert!(up.contains("dnf downgrade -y -q caddy-\"$old\""));

        let un = uninstall_script(EdgeKind::Caddy, PkgFamily::Apt, true, true);
        assert!(un.contains("grep -vxF 'import /etc/ikit/edge/sites/*.conf'"));
        assert!(!un.contains("purge"));
        let un = uninstall_script(EdgeKind::Caddy, PkgFamily::Apt, false, true);
        assert!(un.contains("apt-get purge -y -q caddy"));
    }

    #[test]
    fn scripts_parse_as_posix_sh() {
        let m = HostManifest {
            kind: EdgeKind::Caddy,
            installed_by: "acme".into(),
            adopted: true,
            exclusive: false,
            orgs: vec!["acme".into()],
            version: Some("2.10.0".into()),
            installed_at: Utc::now(),
            upgraded_at: None,
        };
        let mut scripts = vec![probe_script(), write_host_script(&m), firewall_script()];
        for kind in EdgeKind::ALL {
            scripts.push(adopt_script(*kind, &m));
            scripts.push(write_site_script(
                *kind,
                "acme__w.conf",
                &["a.acme.com", "b.acme.com"],
            ));
            scripts.push(remove_site_script(*kind, "acme__w.conf"));
            scripts.push(reload_script(*kind));
            for family in [PkgFamily::Apt, PkgFamily::Dnf] {
                scripts.push(install_script(*kind, family, None, &m));
                scripts.push(upgrade_script(*kind, family, Some("1.2.3")));
                scripts.push(uninstall_script(*kind, family, false, true));
                scripts.push(uninstall_script(*kind, family, true, false));
            }
        }
        for script in scripts {
            let out = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}\n---\n{script}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
