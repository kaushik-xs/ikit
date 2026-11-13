//! What holds ports on a target's host: listening sockets (`ss`), ports Docker publishes for
//! containers (`docker ps`, which also covers containers published without a userland proxy,
//! whose ports `ss` cannot see), and the ports each org has claimed on the host.
//!
//! Claims are one JSON file per org in [`CLAIMS_DIR`] (sticky and world-writable, like `/tmp`,
//! created when ikit has root), or in [`HOME_CLAIMS_DIR`] under the login user's home when it
//! cannot be used. Each org only writes its own file, so writers never race.

use ikit_core::PublishedPort;
use serde::{Deserialize, Serialize};

pub const CLAIMS_DIR: &str = "/var/lib/ikit/ports";
/// Relative to `$HOME`.
pub const HOME_CLAIMS_DIR: &str = ".local/state/ikit/ports";

/// An org's claim file: every host port its workloads on the host publish or route to,
/// whether or not they run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgClaims {
    pub org: String,
    /// When it was written (Unix seconds): with two files for one org (one written as root,
    /// one under a home), the newer wins.
    #[serde(default)]
    pub updated: u64,
    pub ports: Vec<ClaimedPort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedPort {
    pub workload: String,
    pub service: String,
    #[serde(flatten)]
    pub port: PublishedPort,
}

/// Replaces `org`'s claim file with the JSON on stdin. `org` must be a slug.
pub fn write_claims_script(org: &str) -> String {
    format!(
        r#"set -e
d={CLAIMS_DIR}
if [ "$(id -u)" = 0 ]; then mkdir -p "$d" && chmod 1777 "$d"; fi
if [ ! -d "$d" ] || [ ! -w "$d" ] || {{ [ -e "$d/{org}.json" ] && [ ! -O "$d/{org}.json" ]; }}; then
  d="$HOME/{HOME_CLAIMS_DIR}"
  mkdir -p "$d"
fi
tmp="$d/.{org}.json.$$"
cat > "$tmp"
chmod 644 "$tmp"
mv -f "$tmp" "$d/{org}.json"
echo "claims=$d/{org}.json"
"#
    )
}

/// The script that prints both sections. It needs neither root nor Docker: without root `ss`
/// leaves out process names, and without Docker the second section is empty.
pub fn probe_script() -> String {
    r#"if command -v ss >/dev/null 2>&1; then
  echo '## ss'
  ss -Htulnp 2>/dev/null || ss -Htuln
else
  echo '## no-ss'
fi
if command -v docker >/dev/null 2>&1; then
  echo '## docker'
  docker ps --format '{{.Label "com.docker.compose.project"}}|{{.Names}}|{{.Ports}}' 2>/dev/null
fi
echo '## claims'
extra=
if [ "$(id -u)" = 0 ]; then extra="/root/HOME_DIR/*.json /home/*/HOME_DIR/*.json"; fi
for f in CLAIMS_DIR/*.json "$HOME"/HOME_DIR/*.json $extra; do
  if [ -f "$f" ]; then echo "@@file $f"; cat "$f"; echo; fi
done
exit 0
"#
    .replace("CLAIMS_DIR", CLAIMS_DIR)
    .replace("HOME_DIR", HOME_CLAIMS_DIR)
}

/// Something holding a host port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// `None` when it holds the port on every address.
    pub bind: Option<String>,
    pub port: u16,
    pub udp: bool,
    pub holder: Holder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    /// A process on the host; its name is only known when the probe ran as root (or owns it).
    Process(Option<String>),
    /// A container that Docker publishes the port for.
    Container {
        /// Its Compose project (an ikit workload name), if it has one.
        project: Option<String>,
        name: String,
    },
}

/// What the probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostListeners {
    pub listeners: Vec<Listener>,
    /// `ss` is not installed, so only container ports are known.
    pub no_ss: bool,
    /// Every org's claim file found on the host (one per org; the newest file wins).
    pub claims: Vec<OrgClaims>,
    /// Claim files that could not be read: `path: error`.
    pub bad_claims: Vec<String>,
}

/// Parses [`probe_script`] output. Sockets that belong to a container's published port
/// (Docker's `docker-proxy`) are reported once, as the container.
pub fn parse_probe(out: &str) -> HostListeners {
    let mut section = "";
    let mut sockets = Vec::new();
    let mut containers = Vec::new();
    let mut no_ss = false;
    let mut files: Vec<(String, String)> = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if section == "claims"
            && let Some(path) = line.strip_prefix("@@file ")
        {
            files.push((path.to_owned(), String::new()));
            continue;
        }
        if let Some(name) = line.strip_prefix("## ") {
            section = name;
            no_ss |= name == "no-ss";
            continue;
        }
        match section {
            "ss" => sockets.extend(parse_ss_line(line)),
            "docker" => containers.extend(parse_docker_line(line)),
            "claims" => {
                if let Some((_, body)) = files.last_mut() {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            _ => {}
        }
    }
    let mut claims: Vec<OrgClaims> = Vec::new();
    let mut bad_claims = Vec::new();
    for (path, body) in files {
        match serde_json::from_str::<OrgClaims>(&body) {
            Ok(c) => match claims.iter_mut().find(|o| o.org == c.org) {
                Some(o) if o.updated < c.updated => *o = c,
                Some(_) => {}
                None => claims.push(c),
            },
            Err(e) => bad_claims.push(format!("{path}: {e}")),
        }
    }
    sockets.retain(|s: &Listener| {
        !containers
            .iter()
            .any(|c: &Listener| c.port == s.port && c.udp == s.udp)
    });
    sockets.extend(containers);
    sockets.sort_by_key(|s| (s.port, s.udp));
    sockets.dedup();
    HostListeners {
        listeners: sockets,
        no_ss,
        claims,
        bad_claims,
    }
}

/// `udp UNCONN 0 0 127.0.0.53%lo:53 0.0.0.0:* users:(("systemd-resolve",pid=1,fd=13))`
fn parse_ss_line(line: &str) -> Option<Listener> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    let (netid, local) = (*cols.first()?, *cols.get(4)?);
    let udp = match netid {
        "tcp" => false,
        "udp" => true,
        _ => return None,
    };
    let (bind, port) = split_address(local)?;
    let process = cols
        .get(6)
        .and_then(|p| p.split("((\"").nth(1))
        .and_then(|p| p.split('"').next())
        .map(str::to_owned);
    Some(Listener {
        bind,
        port,
        udp,
        holder: Holder::Process(process),
    })
}

/// `acme-dev-001|acme-dev-001-web-1|0.0.0.0:8080->80/tcp, [::]:8080->80/tcp, 9000/tcp`
fn parse_docker_line(line: &str) -> Vec<Listener> {
    let mut parts = line.splitn(3, '|');
    let (Some(project), Some(name), Some(ports)) = (parts.next(), parts.next(), parts.next())
    else {
        return vec![];
    };
    let project = (!project.is_empty()).then(|| project.to_owned());
    let mut out = Vec::new();
    for mapping in ports.split(',').map(str::trim) {
        let Some((host, container)) = mapping.split_once("->") else {
            continue;
        };
        let udp = container.ends_with("/udp");
        let Some(colon) = host.rfind(':') else {
            continue;
        };
        let bind = normalize_bind(&host[..colon]);
        let span = &host[colon + 1..];
        let (first, last) = match span.split_once('-') {
            Some((a, b)) => (a.parse::<u16>(), b.parse::<u16>()),
            None => (span.parse(), span.parse()),
        };
        let (Ok(first), Ok(last)) = (first, last) else {
            continue;
        };
        for port in first..=last {
            out.push(Listener {
                bind: bind.clone(),
                port,
                udp,
                holder: Holder::Container {
                    project: project.clone(),
                    name: name.to_owned(),
                },
            });
        }
    }
    out
}

/// `0.0.0.0:22`, `[::]:22`, `*:22`, `[::1]:631`, `127.0.0.53%lo:53` → (bind, port).
fn split_address(addr: &str) -> Option<(Option<String>, u16)> {
    let colon = addr.rfind(':')?;
    let port = addr[colon + 1..].parse().ok()?;
    Some((normalize_bind(&addr[..colon]), port))
}

/// `None` for every address; otherwise the address without brackets or `%interface`.
fn normalize_bind(addr: &str) -> Option<String> {
    let addr = addr.split('%').next().unwrap_or(addr);
    let addr = addr.trim_start_matches('[').trim_end_matches(']');
    match addr {
        "" | "*" | "0.0.0.0" | "::" => None,
        a => Some(a.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = r#"## ss
tcp   LISTEN 0      4096         0.0.0.0:22         0.0.0.0:*    users:(("sshd",pid=812,fd=3))
tcp   LISTEN 0      4096            [::]:22            [::]:*    users:(("sshd",pid=812,fd=4))
tcp   LISTEN 0      4096       127.0.0.1:5432       0.0.0.0:*
udp   UNCONN 0      0      127.0.0.53%lo:53         0.0.0.0:*    users:(("systemd-resolve",pid=500,fd=13))
tcp   LISTEN 0      4096         0.0.0.0:30001      0.0.0.0:*    users:(("docker-proxy",pid=9,fd=4))
tcp   LISTEN 0      4096               *:9100             *:*
## docker
wl-acme-dev-001|wl-acme-dev-001-web-1|0.0.0.0:30001->80/tcp, [::]:30001->80/tcp, 9000/tcp
|loose|127.0.0.1:8000-8001->8000-8001/tcp, 0.0.0.0:5353->53/udp
"#;

    fn holder(l: &HostListeners, port: u16, udp: bool) -> Option<&Holder> {
        l.listeners
            .iter()
            .find(|x| x.port == port && x.udp == udp)
            .map(|x| &x.holder)
    }

    #[test]
    fn parses_sockets_and_containers() {
        let l = parse_probe(OUT);
        assert!(!l.no_ss);
        assert_eq!(
            holder(&l, 22, false),
            Some(&Holder::Process(Some("sshd".into())))
        );
        assert_eq!(holder(&l, 5432, false), Some(&Holder::Process(None)));
        assert_eq!(holder(&l, 9100, false), Some(&Holder::Process(None)));
        let dns = l.listeners.iter().find(|x| x.port == 53).unwrap();
        assert_eq!((dns.bind.as_deref(), dns.udp), (Some("127.0.0.53"), true));
        // docker-proxy's socket is reported as the container.
        assert_eq!(
            holder(&l, 30001, false),
            Some(&Holder::Container {
                project: Some("wl-acme-dev-001".into()),
                name: "wl-acme-dev-001-web-1".into()
            })
        );
        assert!(l.listeners.iter().all(|x| x.port != 9000), "unpublished");
        assert!(holder(&l, 8001, false).is_some() && holder(&l, 5353, true).is_some());
        let loose = l.listeners.iter().find(|x| x.port == 8000).unwrap();
        assert_eq!(
            loose.holder,
            Holder::Container {
                project: None,
                name: "loose".into()
            }
        );
        // IPv4 and IPv6 sockets on the same port are one entry each way.
        assert_eq!(l.listeners.iter().filter(|x| x.port == 22).count(), 1);
    }

    #[test]
    fn reads_claim_files() {
        let out = r#"## ss
## claims
@@file /var/lib/ikit/ports/globex.json
{"org":"globex","ports":[{"workload":"wl-globex-dev-001","service":"web","host":30000,"container":80},
 {"workload":"wl-globex-dev-001","service":"dns","bind":"127.0.0.1","host":5353,"container":53,"udp":true}]}
@@file /home/ops/.local/state/ikit/ports/globex.json
{"org":"globex","updated":5,"ports":[]}
@@file /home/ops/.local/state/ikit/ports/acme.json
{"org":"acme","updated":9,"ports":[]}
@@file /var/lib/ikit/ports/acme.json
{"org":"acme","updated":7,"ports":[{"workload":"w","service":"s","host":1,"container":1}]}
@@file /var/lib/ikit/ports/broken.json
{"org":
"#;
        let l = parse_probe(out);
        assert_eq!(l.claims.len(), 2);
        // globex's root file has no timestamp, so its newer home file wins; acme's home file
        // is newer than its root one.
        let g = &l.claims[0];
        assert_eq!((g.updated, g.ports.len()), (5, 0));
        let a = &l.claims[1];
        assert_eq!((a.updated, a.ports.len()), (9, 0));
        let l = parse_probe(&out.replace("\"updated\":5,", ""));
        let g = &l.claims[0];
        assert_eq!(g.ports.len(), 2, "first file wins a tie");
        assert_eq!(g.ports[1].port.to_string(), "127.0.0.1:5353:53/udp");
        assert_eq!(l.bad_claims.len(), 1);
        assert!(l.bad_claims[0].starts_with("/var/lib/ikit/ports/broken.json"));
    }

    #[test]
    fn claim_files_round_trip() {
        let c = OrgClaims {
            org: "acme".into(),
            updated: 1700000000,
            ports: vec![ClaimedPort {
                workload: "wl-acme-dev-001".into(),
                service: "api".into(),
                port: "30000:80".parse().unwrap(),
            }],
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"org":"acme","updated":1700000000,"ports":[{"workload":"wl-acme-dev-001","service":"api","host":30000,"container":80}]}"#
        );
        let script = write_claims_script("acme");
        assert!(script.contains("mv -f \"$tmp\" \"$d/acme.json\""));
    }

    #[test]
    fn probe_succeeds_without_claim_files() {
        // A glob that matches nothing must not become the script's exit status.
        assert!(probe_script().trim_end().ends_with("exit 0"));
    }

    #[test]
    fn notes_a_missing_ss() {
        let l = parse_probe("## no-ss\n## docker\n");
        assert!(l.no_ss && l.listeners.is_empty());
    }
}
