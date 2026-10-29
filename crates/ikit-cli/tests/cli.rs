//! End-to-end tests of the `ikit` binary against a throwaway home directory.
//!
//! The default tests need nothing installed. `docker_round_trip` deploys real containers and
//! is ignored unless asked for: `cargo test -p ikit-cli --test cli -- --ignored`.

use std::path::Path;
use std::process::{Command, Output};

struct Ikit {
    home: tempfile::TempDir,
    org: String,
}

impl Ikit {
    /// A fresh ikit home with one org. The org name is unique per process so Docker
    /// projects from parallel or leftover runs never collide.
    fn new(tag: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let org = format!("it{tag}{}", std::process::id());
        let ikit = Self { home, org };
        ikit.ok(&["init"]);
        ikit.ok(&["org", "create", &ikit.org.clone()]);
        ikit
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.org, args)
    }

    fn run_in(&self, org: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ikit"))
            .args([
                "--home",
                &self.home.path().display().to_string(),
                "--org",
                org,
                "--no-input",
            ])
            .args(args)
            .env_remove("IKIT_WL")
            .env_remove("IKIT_ORG")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "ikit {args:?} failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["-o", "json"];
        all.extend(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    fn wl(&self) -> String {
        format!("wl-{}-dev-001", self.org)
    }
}

#[test]
fn workload_definition_round_trip() {
    let ikit = Ikit::new("def");
    ikit.ok(&["target", "attach", "vm", "laptop", "--local"]);
    ikit.ok(&["wl", "create", "--env", "dev", "--target", "laptop"]);
    ikit.ok(&["sg", "add", "data", "--services", "redis@7"]);
    ikit.ok(&["sg", "add", "platform", "--services", "nginx"]);

    let wl = ikit.json(&["wl", "show"]);
    let services: Vec<&str> = wl["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(services, ["nginx", "redis"]);

    let compose = ikit.ok(&["render"]);
    assert!(compose.contains("image: redis:7.4.1"), "{compose}");

    let plan = ikit.json(&["plan"]);
    assert_eq!(plan["plan"]["first_apply"], true);
    assert_eq!(plan["plan"]["changes"].as_array().unwrap().len(), 2);

    // Nothing applied yet: no drift, and nothing is outdated against the seeds.
    ikit.ok(&["status", "--drift"]);
    assert_eq!(ikit.json(&["registry", "outdated"]), serde_json::json!([]));
}

#[test]
fn outdated_exits_2_and_lists_services() {
    let ikit = Ikit::new("old");
    ikit.ok(&["target", "attach", "vm", "laptop", "--local"]);
    ikit.ok(&["wl", "create", "--env", "dev", "--target", "laptop"]);
    ikit.ok(&["sg", "add", "data", "--services", "redis@7"]);
    ikit.ok(&[
        "registry",
        "version",
        "add",
        "redis",
        "7.4.2",
        "--image",
        "redis:7.4.2",
    ]);

    let out = ikit.run(&["-o", "json", "registry", "outdated"]);
    assert_eq!(out.status.code(), Some(2));
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows[0]["service"], "redis");
    assert_eq!(rows[0]["wanted"], "7.4.2");

    ikit.ok(&["svc", "upgrade", "redis"]);
    ikit.ok(&["registry", "outdated"]);
}

#[test]
fn orgs_are_isolated() {
    let a = Ikit::new("a");
    a.ok(&["target", "attach", "vm", "laptop", "--local"]);
    a.ok(&["wl", "create", "--env", "dev", "--target", "laptop"]);
    let other = format!("{}b", a.org);
    a.ok(&["org", "create", &other]);
    let theirs = a.run_in(&other, &["-o", "json", "wl", "list"]);
    assert!(theirs.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&theirs.stdout).unwrap(),
        serde_json::json!([])
    );
    assert!(
        a.home
            .path()
            .join("orgs")
            .join(&other)
            .join("state.db")
            .exists()
    );
}

#[test]
fn mcp_http_tokens_and_networks_offline() {
    let ikit = Ikit::new("mcp");
    let out = ikit.run(&["mcp", "serve", "--http", "127.0.0.1:0"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ikit mcp token create"));

    let created = ikit.json(&["mcp", "token", "create", "agent"]);
    let token = created["token"].as_str().unwrap();
    assert!(token.starts_with("ikit_"));
    let listed = ikit.json(&["mcp", "token", "list"]);
    assert_eq!(listed[0]["name"], "agent");
    assert!(
        !listed.to_string().contains(token),
        "only the hash is listed"
    );

    let out = ikit.run(&["mcp", "serve", "--http", "0.0.0.0:7420"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not allowed"));
    ikit.ok(&["mcp", "token", "rm", "agent"]);
    assert!(!ikit.run(&["mcp", "token", "rm", "agent"]).status.success());

    assert_eq!(ikit.json(&["net", "zt", "list"]), serde_json::json!([]));
    let out = ikit.run(&[
        "net",
        "zt",
        "create",
        "dev-net",
        "--profile",
        "missing",
        "--no-link",
    ]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("zerotier/missing not found"));
}

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn docker(args: &[&str]) {
    let out = Command::new("docker").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Deploys redis, detects a stopped container as drift, repairs it and destroys everything.
#[test]
#[ignore = "deploys real containers; run with --ignored"]
fn docker_round_trip() {
    if !docker_available() {
        panic!("docker is not running");
    }
    let ikit = Ikit::new("dock");
    let wl = ikit.wl();
    // Removes containers and volumes even when an assertion fails.
    struct Cleanup<'a>(&'a Ikit);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = self.0.run(&["destroy", "--volumes", "--yes"]);
        }
    }
    ikit.ok(&["target", "attach", "vm", "laptop", "--local"]);
    ikit.ok(&["wl", "create", "--env", "dev", "--target", "laptop"]);
    ikit.ok(&["sg", "add", "data", "--services", "redis@7"]);
    let _cleanup = Cleanup(&ikit);

    ikit.ok(&["apply", "--yes"]);
    let status = ikit.json(&["status"]);
    assert_eq!(status["containers"][0]["state"], "running", "{status}");
    ikit.ok(&["status", "--drift"]);

    let container = format!("{wl}-redis-1");
    docker(&["stop", &container]);
    let out = ikit.run(&["-o", "json", "status", "--drift"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report[0]["items"][0]["kind"], "stopped", "{report}");

    // Nothing changed in the definition, so only --force re-applies.
    assert!(ikit.ok(&["apply", "--yes"]).contains("nothing to apply"));
    ikit.ok(&["apply", "--yes", "--force"]);
    ikit.ok(&["status", "--drift"]);

    ikit.ok(&["destroy", "--volumes", "--yes"]);
    assert!(
        !Path::new(
            &ikit
                .home
                .path()
                .join("orgs")
                .join(&ikit.org)
                .join("deploy")
                .join(&wl)
        )
        .exists()
    );
}
