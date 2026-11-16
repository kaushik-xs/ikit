//! The screens of the TUI, in the order of ikit's end-to-end flow, and the `ikit` commands
//! each one lists with and acts with.

use serde_json::Value;

/// What a section needs selected before it can list anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Home,
    Org,
    Workload,
}

/// How an action's command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In the background with `--no-input`; output streams into a panel.
    Capture,
    /// With the TUI suspended and the command attached to the terminal, for prompts and
    /// pickers.
    Terminal,
}

/// A key-bound command in a section.
///
/// `args` tokens may hold placeholders:
/// - `{sel}`: the selected row's key, `{sel.a.b}`: a field of the selected row;
/// - `{0}`, `{1}`, ...: answers to `inputs`, required;
/// - `{0?}`: an optional positional, dropped when empty;
/// - `--flag={0}`: an optional flag, passed as `--flag <value>` and dropped when empty;
/// - `{wl}`, `{org}`: the current selection.
pub struct Action {
    pub key: char,
    pub label: &'static str,
    pub args: &'static [&'static str],
    pub inputs: &'static [&'static str],
    /// Asked with a yes/no dialog first; may hold placeholders.
    pub confirm: Option<&'static str>,
    pub mode: Mode,
    /// After it succeeds, re-read which org and workload are selected.
    pub reselect: bool,
}

pub struct Section {
    pub title: &'static str,
    pub scope: Scope,
    /// The command whose `-o json` output the section lists.
    pub list: &'static [&'static str],
    /// Turns that JSON into rows (objects).
    pub rows: fn(Value) -> Vec<Value>,
    /// (header, path) per column; paths are dotted, `a[].b` joins a field of an array.
    pub columns: &'static [(&'static str, &'static str)],
    /// The path of the row field that `{sel}` stands for.
    pub key: &'static str,
    pub actions: &'static [Action],
    /// One line explaining where the section sits in the flow.
    pub hint: &'static str,
}

const fn act(key: char, label: &'static str, args: &'static [&'static str]) -> Action {
    Action {
        key,
        label,
        args,
        inputs: &[],
        confirm: None,
        mode: Mode::Capture,
        reselect: false,
    }
}

impl Action {
    const fn inputs(mut self, inputs: &'static [&'static str]) -> Self {
        self.inputs = inputs;
        self
    }
    const fn confirm(mut self, q: &'static str) -> Self {
        self.confirm = Some(q);
        self
    }
    const fn terminal(mut self) -> Self {
        self.mode = Mode::Terminal;
        self
    }
    const fn reselect(mut self) -> Self {
        self.reselect = true;
        self
    }

    /// Whether the command needs a selected row.
    pub fn needs_selection(&self) -> bool {
        self.args.iter().any(|a| a.contains("{sel"))
            || self.confirm.is_some_and(|c| c.contains("{sel"))
    }
}

fn array(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a,
        Value::Null => vec![],
        other => vec![other],
    }
}

fn field(name: &'static str) -> impl Fn(Value) -> Vec<Value> {
    move |mut v| array(v.get_mut(name).map(Value::take).unwrap_or(Value::Null))
}

fn groups(v: Value) -> Vec<Value> {
    match v {
        Value::Object(m) => m
            .into_iter()
            .map(|(group, services)| serde_json::json!({ "group": group, "services": services }))
            .collect(),
        other => array(other),
    }
}

fn routes(v: Value) -> Vec<Value> {
    field("routes")(v)
        .into_iter()
        .map(|mut r| {
            let host = r
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let path = r
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            r["_key"] = Value::String(format!("{host}{path}"));
            r
        })
        .collect()
}

fn resources(v: Value) -> Vec<Value> {
    array(v)
        .into_iter()
        .map(|mut r| {
            let s = text(lookup(&r, "resource.service"));
            let n = text(lookup(&r, "resource.name"));
            r["_key"] = Value::String(format!("{s}.{n}"));
            r
        })
        .collect()
}

fn containers(v: Value) -> Vec<Value> {
    field("containers")(v)
}

/// Every screen, in the order of the flow: org → registry → targets → workload →
/// services → deploy → routes and hooks → operations.
pub static SECTIONS: &[Section] = &[
    Section {
        title: "Orgs",
        scope: Scope::Home,
        list: &["org", "list"],
        rows: array,
        columns: &[
            ("", "_active"),
            ("NAME", "name"),
            ("DISPLAY NAME", "display_name"),
            ("PREFIX", "prefix"),
            ("ENVS", "envs"),
            ("WORKLOADS", "workloads"),
        ],
        key: "name",
        hint: "1. An org holds its own registry, targets and workloads. Enter selects one.",
        actions: &[
            act('\n', "use", &["org", "use", "{sel}"]).reselect(),
            act(
                'c',
                "create",
                &[
                    "org",
                    "create",
                    "{0}",
                    "--display-name={1}",
                    "--prefix={2}",
                    "--envs={3}",
                ],
            )
            .inputs(&[
                "Org name",
                "Display name (optional)",
                "Workload prefix (optional, default wl-<name>)",
                "Envs, comma-separated (optional, default dev,qa,stg,prod)",
            ])
            .terminal()
            .reselect(),
            act('s', "status", &["--org", "{sel}", "org", "status"]),
            act('h', "history", &["--org", "{sel}", "org", "history"]),
            act('P', "pull", &["--org", "{sel}", "org", "pull"]),
            act('U', "push", &["--org", "{sel}", "org", "push"]),
            act('x', "remove", &["org", "rm", "{sel}", "--yes"])
                .confirm("Remove org {sel} and its local state?")
                .reselect(),
        ],
    },
    Section {
        title: "Profiles",
        scope: Scope::Org,
        list: &["profile", "list"],
        rows: array,
        columns: &[
            ("KIND", "kind"),
            ("NAME", "name"),
            ("CONFIG", "config"),
            ("SECRETS", "secret_keys"),
        ],
        key: "name",
        hint: "2. Provider credentials (aws, azure, gcp, zerotier, k8s) other steps refer to.",
        actions: &[
            act(
                '\n',
                "show",
                &["profile", "show", "{sel.kind}", "{sel.name}"],
            ),
            act('a', "add", &["profile", "add", "{0}", "{1}"])
                .inputs(&["Kind (aws, azure, gcp, zerotier, k8s)", "Profile name"])
                .terminal(),
            act(
                'x',
                "remove",
                &["profile", "rm", "{sel.kind}", "{sel.name}"],
            )
            .confirm("Remove profile {sel.kind}/{sel.name}?")
            .terminal(),
        ],
    },
    Section {
        title: "Registry",
        scope: Scope::Org,
        list: &["registry", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("CATEGORY", "category"),
            ("VERSIONS", "versions[].version"),
            ("DESCRIPTION", "description"),
        ],
        key: "name",
        hint: "3. The services workloads can run, with their versions and images.",
        actions: &[
            act('\n', "show", &["registry", "show", "{sel}"]),
            act('u', "usage", &["registry", "usage", "{sel}"]),
            act('s', "sync", &["registry", "sync", "{sel}"]),
            act('O', "outdated", &["registry", "outdated"]),
            act('i', "import", &["registry", "import", "{0}"]).inputs(&["Registry YAML file"]),
            act('l', "logins", &["registry", "logins"]),
            act(
                'L',
                "login",
                &["registry", "login", "{0}", "--username", "{1}"],
            )
            .inputs(&["Image registry host (e.g. ghcr.io)", "Username"])
            .terminal(),
        ],
    },
    Section {
        title: "Targets",
        scope: Scope::Org,
        list: &["target", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("KIND", "kind"),
            ("CONNECTION", "connection.type"),
            ("STATUS", "status"),
            ("EDGE", "edge.proxy"),
        ],
        key: "name",
        hint: "4. The VMs and clusters workloads run on.",
        actions: &[
            act('\n', "show", &["target", "show", "{sel}"]),
            act('t', "test", &["target", "test", "{sel}"]),
            act('p', "ports", &["target", "ports", "{sel}"]),
            act('e', "edge status", &["target", "edge", "status", "{sel}"]),
            act('E', "edge install", &["target", "edge", "install", "{sel}"]).terminal(),
            act(
                'a',
                "attach local",
                &["target", "attach", "vm", "{0}", "--local"],
            )
            .inputs(&["Target name"]),
            act(
                'A',
                "attach ssh",
                &["target", "attach", "vm", "{0}", "--ssh", "{1}"],
            )
            .inputs(&["Target name", "[user@]host[:port]"])
            .terminal(),
            act('x', "detach", &["target", "detach", "{sel}", "--yes"])
                .confirm("Detach target {sel}?"),
        ],
    },
    Section {
        title: "Workloads",
        scope: Scope::Org,
        list: &["wl", "list"],
        rows: array,
        columns: &[
            ("", "_active"),
            ("NAME", "name"),
            ("ENV", "env"),
            ("TARGET", "target"),
            ("STATUS", "status"),
            ("UPDATED", "updated_at"),
        ],
        key: "name",
        hint: "5. A workload is one env's deployment on a target. Enter selects one.",
        actions: &[
            act('\n', "use", &["wl", "use", "{sel}"]).reselect(),
            act(
                'c',
                "create",
                &["wl", "create", "--env={0}", "--target={1}"],
            )
            .inputs(&[
                "Env (optional: picked from a list)",
                "Target (optional: picked from a list)",
            ])
            .terminal()
            .reselect(),
            act('l', "links", &["wl", "links", "{sel}"]),
            act('x', "remove", &["wl", "rm", "{sel}"])
                .terminal()
                .reselect(),
        ],
    },
    Section {
        title: "Groups",
        scope: Scope::Workload,
        list: &["sg", "list"],
        rows: groups,
        columns: &[("GROUP", "group"), ("SERVICES", "services")],
        key: "group",
        hint: "6. Service groups (app, platform, data) of the selected workload.",
        actions: &[
            act('a', "add", &["sg", "add", "{0}", "--services={1}"])
                .inputs(&[
                    "Group (app, platform, data)",
                    "Services, e.g. postgres@16,redis (optional: picked from a list)",
                ])
                .terminal(),
            act('x', "remove", &["sg", "rm", "{sel}", "--yes"])
                .confirm("Remove group {sel} and its services from {wl}?"),
        ],
    },
    Section {
        title: "Services",
        scope: Scope::Workload,
        list: &["svc", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("GROUP", "group"),
            ("VERSION", "version"),
            ("CONSTRAINT", "constraint"),
            ("PUBLISH", "publish"),
            ("IMAGE", "image"),
        ],
        key: "name",
        hint: "7. The services in the selected workload.",
        actions: &[
            act('\n', "show env", &["svc", "show", "{sel}"]),
            act('e', "set env", &["svc", "set", "{sel}", "{0}"])
                .inputs(&["KEY=VALUE (a var its registry version declares; n declares a new one)"]),
            // A service only takes env vars its registry version declares; these add one to
            // that version (shared by every workload on it).
            act('n', "declare env", &["registry", "version", "set", "{sel.registry_service}", "{sel.version}", "--env", "{0}", "--yes"])
                .inputs(&["NAME (required) or NAME=DEFAULT (optional)"])
                .confirm("Declare it in registry {sel.registry_service}@{sel.version}? Every workload on that version gets the var."),
            act('N', "declare secret", &["registry", "version", "set", "{sel.registry_service}", "{sel.version}", "--secret-env", "{0}", "--yes"])
                .inputs(&["Secret env var NAME"])
                .confirm("Declare a required secret in registry {sel.registry_service}@{sel.version}? Every workload on that version must then set it."),
            act('E', "unset env", &["svc", "set", "{sel}", "--unset", "{0}"])
                .inputs(&["Env var to unset"]),
            // Attached to the terminal so the CLI's masked prompt takes the value: it never
            // passes through the TUI or a command line.
            act('S', "set secret", &["svc", "set", "{sel}", "--secret", "{0}"])
                .inputs(&["Secret env var (declared with --secret-env in the registry)"])
                .terminal(),
            // Shown in the output panel, not on the terminal, so the value is gone when the
            // panel closes instead of staying in the scrollback.
            act('r', "reveal secret", &["svc", "secret", "show", "{sel}", "{0}", "--yes"])
                .inputs(&["Secret env var to reveal"])
                .confirm("Show this secret of {sel} in clear text? It is on screen until you close the panel."),
            act('g', "generate secret", &["svc", "set", "{sel}", "--generate", "{0}"])
                .inputs(&["Generated secret to (re)generate"])
                .confirm("Regenerate {sel}'s secret? Services reading it get the new value on the next apply."),
            act('u', "upgrade", &["svc", "upgrade", "{sel}", "--to", "{0}"])
                .inputs(&["New constraint (e.g. ^17 or 17.2.0)"]),
            act('p', "publish", &["svc", "publish", "{sel}", "{0}"])
                .inputs(&["Port: CONTAINER, HOST:CONTAINER or BIND:HOST:CONTAINER"]),
            act('b', "backup", &["backup", "run", "{sel}"]),
            act('x', "remove", &["svc", "rm", "{sel}", "--yes"])
                .confirm("Remove service {sel} from {wl}?"),
        ],
    },
    Section {
        title: "Deploy",
        scope: Scope::Workload,
        list: &["status"],
        rows: containers,
        columns: &[
            ("SERVICE", "service"),
            ("STATE", "state"),
            ("HEALTH", "health"),
            ("STATUS", "status"),
            ("PORTS", "ports"),
        ],
        key: "service",
        hint: "8. Render, plan and apply the selected workload; its containers are listed here.",
        actions: &[
            act('p', "plan", &["plan"]),
            act('a', "apply", &["apply", "--yes"]).confirm("Apply {wl} to its target?"),
            act('f', "force apply", &["apply", "--yes", "--force"])
                .confirm("Re-apply {wl} even without changes (recreates drifted containers)?"),
            act('r', "render", &["render"]),
            act('d', "drift", &["status", "--drift"]),
            act('D', "drift (all)", &["status", "--drift", "--all"]),
            act('x', "destroy", &["destroy", "--yes"])
                .confirm("Destroy {wl}: remove its containers (volumes are kept)?"),
            act('X', "destroy + volumes", &["destroy", "--volumes"]).terminal(),
        ],
    },
    Section {
        title: "Routes",
        scope: Scope::Workload,
        list: &["route", "list"],
        rows: routes,
        columns: &[
            ("HOST", "host"),
            ("PATH", "path"),
            ("SERVICE", "service"),
            ("PORT", "port"),
            ("TLS", "tls"),
        ],
        key: "_key",
        hint: "9. Domains routed to the workload's services through the target's proxy.",
        actions: &[
            act(
                'a',
                "add",
                &[
                    "route",
                    "add",
                    "{0?}",
                    "--host={1}",
                    "--path={2}",
                    "--port={3}",
                ],
            )
            .inputs(&[
                "Service (optional: picked from a list)",
                "Host (optional)",
                "Path prefix (optional)",
                "Port (optional)",
            ])
            .terminal(),
            act('A', "auto on", &["route", "auto", "on"]),
            act('O', "auto off", &["route", "auto", "off"]),
            act('x', "remove", &["route", "rm", "{sel}"]).confirm("Remove route {sel}?"),
        ],
    },
    Section {
        title: "Hooks",
        scope: Scope::Workload,
        list: &["hook", "list"],
        rows: array,
        columns: &[
            ("ORD", "ord"),
            ("NAME", "name"),
            ("PHASE", "phase"),
            ("KIND", "kind"),
            ("FAIL ON ERROR", "fail_on_error"),
        ],
        key: "name",
        hint: "10. SQL, shell, HTTP and MCP hooks run around the workload's deploys.",
        actions: &[
            act('\n', "run", &["hook", "run", "{sel}"]),
            act('R', "run all", &["hook", "run"]),
            act('h', "history", &["hook", "history", "{sel}"]),
            act('x', "remove", &["hook", "rm", "{sel}", "--yes"]).confirm("Remove hook {sel}?"),
        ],
    },
    Section {
        title: "Resources",
        scope: Scope::Workload,
        list: &["res", "list"],
        rows: resources,
        columns: &[
            ("RESOURCE", "_key"),
            ("API", "resource.api"),
            ("ID", "resource.external_id"),
            ("BACKEND", "bound_to"),
            ("STATUS", "resource.status"),
            ("USED BY", "used_by"),
        ],
        key: "_key",
        hint: "11. Buckets and queues the workload's services need.",
        actions: &[
            act('\n', "show", &["res", "show", "{sel}"]),
            act('y', "keys", &["res", "keys"]),
            act('b', "backends", &["res", "backend", "list"]),
        ],
    },
    Section {
        title: "Runs",
        scope: Scope::Workload,
        list: &["wl", "runs"],
        rows: array,
        columns: &[
            ("ID", "id"),
            ("ACTION", "action"),
            ("RESULT", "result"),
            ("ACTOR", "actor"),
            ("STARTED", "started_at"),
            ("ERROR", "error"),
        ],
        key: "id",
        hint: "12. Apply, destroy and backup history of the selected workload.",
        actions: &[],
    },
    Section {
        title: "Backups",
        scope: Scope::Workload,
        list: &["backup", "list"],
        rows: array,
        columns: &[
            ("ID", "id"),
            ("SERVICE", "service"),
            ("DATABASE", "database"),
            ("STORAGE", "storage"),
            ("SIZE", "size"),
            ("CREATED", "created_at"),
        ],
        key: "id",
        hint: "13. Database backups of the selected workload (run one from Services).",
        actions: &[act('r', "restore", &["backup", "restore", "{sel}"]).terminal()],
    },
    Section {
        title: "Storage",
        scope: Scope::Org,
        list: &["storage", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("PROVIDER", "provider"),
            ("BUCKET", "bucket"),
            ("PREFIX", "prefix"),
            ("PROFILE", "profile"),
        ],
        key: "name",
        hint: "14. Blob storage backups and remote state go to.",
        actions: &[act('t', "test", &["storage", "test", "{sel}"])],
    },
    Section {
        title: "Networks",
        scope: Scope::Org,
        list: &["net", "zt", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("NETWORK ID", "network_id"),
            ("PROFILE", "profile"),
            ("WORKLOADS", "workloads"),
        ],
        key: "name",
        hint: "15. ZeroTier networks and the workloads linked to them.",
        actions: &[
            act('\n', "show", &["net", "zt", "show", "{sel}"]),
            act('m', "members", &["net", "zt", "members", "{sel}"]),
            act('l', "link wl", &["net", "zt", "link", "{sel}"]),
            act('s', "status", &["net", "zt", "status"]),
            act('d', "docker nets", &["net", "docker", "list"]),
        ],
    },
    Section {
        title: "Scripts",
        scope: Scope::Org,
        list: &["script", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("INTERPRETER", "interpreter"),
            ("RUNS ON", "runs_on"),
            ("DESCRIPTION", "description"),
        ],
        key: "name",
        hint: "16. The org's script library, run against the selected workload.",
        actions: &[
            act('\n', "show", &["script", "show", "{sel}", "--body"]),
            act('d', "dry run", &["script", "run", "{sel}", "--dry-run"]),
            act('r', "run", &["script", "run", "{sel}"]).terminal(),
        ],
    },
    Section {
        title: "MCP",
        scope: Scope::Org,
        list: &["mcp", "token", "list"],
        rows: array,
        columns: &[
            ("NAME", "name"),
            ("WORKLOADS", "workloads"),
            ("CREATED", "created_at"),
        ],
        key: "name",
        hint: "17. Bearer tokens for `ikit mcp serve --http`.",
        actions: &[
            act(
                'c',
                "create token",
                &["mcp", "token", "create", "{0}", "--workload={1}"],
            )
            .inputs(&[
                "Token name",
                "Only these workloads (optional, comma-separated)",
            ]),
            act('x', "remove", &["mcp", "token", "rm", "{sel}"]).confirm("Remove MCP token {sel}?"),
        ],
    },
];

/// The value at a dotted path; `a[].b` maps over an array.
pub fn lookup(v: &Value, path: &str) -> Value {
    if path.is_empty() {
        return v.clone();
    }
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, r),
        None => (path, ""),
    };
    if let Some(name) = head.strip_suffix("[]") {
        return match v.get(name) {
            Some(Value::Array(items)) => {
                Value::Array(items.iter().map(|i| lookup(i, rest)).collect())
            }
            _ => Value::Null,
        };
    }
    match v.get(head) {
        Some(inner) => lookup(inner, rest),
        None => Value::Null,
    }
}

/// A cell's text.
pub fn text(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s,
        Value::Bool(b) => if b { "yes" } else { "no" }.into(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .into_iter()
            .map(text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(m) if m.is_empty() => String::new(),
        Value::Object(m) => m
            .into_iter()
            .map(|(k, v)| format!("{k}={}", text(v)))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// What fills an action's placeholders.
pub struct Fill<'a> {
    pub row: Option<&'a Value>,
    pub key: &'a str,
    pub inputs: &'a [String],
    pub org: Option<&'a str>,
    pub wl: Option<&'a str>,
}

impl Fill<'_> {
    fn value(&self, name: &str) -> Result<String, String> {
        if name == "sel" || name.starts_with("sel.") {
            let row = self.row.ok_or("select a row first")?;
            let path = name.strip_prefix("sel.").unwrap_or(self.key);
            let path = if name == "sel" { self.key } else { path };
            return Ok(text(lookup(row, path)));
        }
        if name == "wl" {
            return Ok(self.wl.unwrap_or("").to_string());
        }
        if name == "org" {
            return Ok(self.org.unwrap_or("").to_string());
        }
        let idx: usize = name
            .parse()
            .map_err(|_| format!("bad placeholder {{{name}}}"))?;
        Ok(self
            .inputs
            .get(idx)
            .map(|s| s.trim().to_string())
            .unwrap_or_default())
    }

    /// Replaces every `{...}` in `s`.
    pub fn text(&self, s: &str) -> Result<String, String> {
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let end = rest[start..]
                .find('}')
                .ok_or_else(|| format!("unclosed placeholder in {s}"))?
                + start;
            let name = rest[start + 1..end].trim_end_matches('?');
            out.push_str(&self.value(name)?);
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Builds an action's arguments; see [`Action`] for the token forms.
    pub fn args(&self, tokens: &[&str]) -> Result<Vec<String>, String> {
        let mut args = Vec::new();
        for tok in tokens {
            if let Some((flag, value)) = tok.split_once('=')
                && flag.starts_with("--")
                && value.starts_with('{')
            {
                let v = self.text(value)?;
                if !v.is_empty() {
                    args.push(flag.to_string());
                    args.push(v);
                }
                continue;
            }
            let optional = tok.ends_with("?}");
            let v = self.text(tok)?;
            if v.is_empty() && tok.contains('{') {
                if optional {
                    continue;
                }
                return Err(format!("`{tok}` needs a value"));
            }
            args.push(v);
        }
        Ok(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Keys the TUI itself uses everywhere; section actions must not take them.
    const GLOBAL_KEYS: &[char] = &['q', ':', '!', '?', 'j', 'k', '/', 'J', 'K', 'w', 'o'];

    #[test]
    fn looks_up_paths() {
        let v = json!({"a": {"b": 1}, "vs": [{"v": "1.0"}, {"v": "2.0"}], "tags": ["x", "y"]});
        assert_eq!(text(lookup(&v, "a.b")), "1");
        assert_eq!(text(lookup(&v, "vs[].v")), "1.0, 2.0");
        assert_eq!(text(lookup(&v, "tags")), "x, y");
        assert_eq!(text(lookup(&v, "missing.x")), "");
    }

    #[test]
    fn fills_args() {
        let row = json!({"name": "pg", "kind": "aws"});
        let inputs = vec!["".to_string(), "acme.com".to_string(), "  ".to_string()];
        let f = Fill {
            row: Some(&row),
            key: "name",
            inputs: &inputs,
            org: Some("acme"),
            wl: Some("wl-acme-dev-001"),
        };
        assert_eq!(
            f.args(&["route", "add", "{0?}", "--host={1}", "--path={2}"])
                .unwrap(),
            vec!["route", "add", "--host", "acme.com"]
        );
        assert_eq!(
            f.args(&["profile", "show", "{sel.kind}", "{sel}"]).unwrap(),
            vec!["profile", "show", "aws", "pg"]
        );
        assert!(f.args(&["svc", "upgrade", "{sel}", "--to", "{0}"]).is_err());
        assert_eq!(f.text("Apply {wl}?").unwrap(), "Apply wl-acme-dev-001?");
    }

    #[test]
    fn missing_selection_is_an_error() {
        let f = Fill {
            row: None,
            key: "name",
            inputs: &[],
            org: None,
            wl: None,
        };
        assert!(f.args(&["svc", "show", "{sel}"]).is_err());
    }

    #[test]
    fn shapes_rows() {
        let g = groups(json!({"app": ["a", "b"], "data": ["pg"]}));
        assert_eq!(g.len(), 2);
        assert_eq!(text(lookup(&g[0], "services")), "a, b");
        let r = routes(
            json!({"routes": [{"host": "a.com", "path": "/api"}, {"host": "b.com", "path": null}]}),
        );
        assert_eq!(text(lookup(&r[0], "_key")), "a.com/api");
        assert_eq!(text(lookup(&r[1], "_key")), "b.com");
        let res = resources(json!([{"resource": {"service": "sim", "name": "jobs"}}]));
        assert_eq!(text(lookup(&res[0], "_key")), "sim.jobs");
    }

    #[test]
    fn action_keys_are_unique_per_section() {
        for s in SECTIONS {
            let mut keys: Vec<char> = s.actions.iter().map(|a| a.key).collect();
            keys.sort();
            let n = keys.len();
            keys.dedup();
            assert_eq!(n, keys.len(), "duplicate action key in {}", s.title);
            for a in s.actions {
                assert!(
                    !GLOBAL_KEYS.contains(&a.key),
                    "{}: action key {:?} shadows a global key",
                    s.title,
                    a.key
                );
            }
        }
    }
}
