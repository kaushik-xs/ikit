//! ikit as an MCP server over stdio or streamable HTTP, bound to one org so an agent cannot
//! see other orgs. HTTP needs a bearer token from `ikit mcp token create` and only listens on
//! loopback or ZeroTier addresses (see [`ikit_app::App::mcp_http_addr`]).
//!
//! Read tools list and show workloads, targets and the registry, hooks, container status,
//! run history and `plan`. Definition tools (create workloads, add services and SQL hooks,
//! set env, publish ports) are off unless the org's `mcp.yaml` enables them for the
//! workload's env. Deploy tools (apply by plan ID, destroy by confirmation token) are off
//! unless `mcp.yaml` enables them too, as is `zt_authorize` for ZeroTier members. Prompts give agents ready-made workflows: diagnosing
//! a workload, planning upgrades, and reviewing a plan before it is applied.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ikit_app::{App, KeyringStore, Paths, RegistryFilter};
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, PromptMessage, Role, ServerCapabilities, ServerConfig};
use rmcp::{
    ServerHandler, ServiceExt, prompt, prompt_handler, prompt_router, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct IkitServer {
    paths: Paths,
    org: String,
    /// Random per-process key: plan IDs and destroy tokens cannot be forged or reused across
    /// server restarts.
    key: Arc<String>,
    /// Pending destroy confirmations: token → (workload, issued at).
    destroy_tokens: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

/// How long a destroy confirmation token stays valid.
const TOKEN_TTL: Duration = Duration::from_secs(300);

fn digest(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    format!("{:x}", h.finalize())[..24].to_owned()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkloadArgs {
    /// Workload name, e.g. wl-acme-dev-001.
    pub workload: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptionalWorkloadArgs {
    /// Workload name; omit for every workload in the org.
    #[serde(default)]
    pub workload: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkloadListArgs {
    /// Only workloads in this env (e.g. dev, prod).
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegistryListArgs {
    /// Only this category: app, platform or data.
    #[serde(default)]
    pub category: Option<String>,
    /// Only services with this tag.
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NameArgs {
    /// Registry service name, e.g. postgres.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunsArgs {
    pub workload: String,
    /// How many runs to return, newest first (default 20).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkloadCreateArgs {
    /// Env from the org's config, e.g. dev.
    pub env: String,
    /// Target to run on (see target_list).
    pub target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServiceAddArgs {
    pub workload: String,
    /// Group: app, platform or data.
    pub group: String,
    /// Registry services as `name` or `name@constraint`, e.g. `postgres@16`.
    pub services: Vec<String>,
    /// Non-secret env values per service, e.g. {"postgres": {"POSTGRES_DB": "orders"}}.
    /// Secrets cannot be set over MCP; services needing one must be added with the CLI.
    #[serde(default)]
    pub env: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServiceSetEnvArgs {
    pub workload: String,
    pub service: String,
    /// Non-secret values to set.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Keys to unset.
    #[serde(default)]
    pub unset: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServicePublishArgs {
    pub workload: String,
    pub service: String,
    /// Mappings `[BIND:]HOST:CONTAINER[/udp]`, e.g. `8080:80`.
    #[serde(default)]
    pub add: Vec<String>,
    /// Host ports to stop publishing.
    #[serde(default)]
    pub remove: Vec<u16>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SqlHookAddArgs {
    pub workload: String,
    /// Hook name, a lowercase slug.
    pub name: String,
    /// Database service the scripts run in.
    pub service: String,
    /// Absolute path of a .sql file or a folder of them, on the machine running ikit.
    pub source: String,
}

type ToolResult = Result<String, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn json(value: &impl Serialize) -> ToolResult {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

impl IkitServer {
    pub fn new(paths: Paths, org: String) -> Self {
        Self {
            paths,
            org,
            key: Arc::new(digest(&[
                &format!("{:?}", std::time::SystemTime::now()),
                &std::process::id().to_string(),
                &format!("{:p}", &TOKEN_TTL),
            ])),
            destroy_tokens: Arc::default(),
            tool_router: Self::tool_router()
                + Self::mutate_router()
                + Self::deploy_router()
                + Self::zt_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    async fn app(&self) -> Result<App, String> {
        App::open(&self.paths, &self.org, Arc::new(KeyringStore))
            .await
            .map_err(|e| e.to_string())
    }

    /// Identifies a plan: its changes and the exact files it would write (secret values
    /// included, but only inside a keyed hash).
    fn plan_id(&self, plan: &ikit_app::DeployPlan) -> String {
        let changes = serde_json::to_string(&(&plan.plan, &plan.hooks)).unwrap_or_default();
        digest(&[
            &self.key,
            &plan.workload,
            &changes,
            &plan.rendered.compose,
            &plan.rendered.secrets,
        ])
    }

    /// The app, after checking `mcp.yaml` allows changing `workload`'s definitions.
    async fn mutable(&self, workload: &str) -> Result<App, String> {
        let app = self.app().await?;
        let env = app.workload_get(workload).await.map_err(err)?.env;
        app.mcp_check_mutate(&env).map_err(err)?;
        Ok(app)
    }
}

#[tool_router]
impl IkitServer {
    #[tool(
        description = "List the org's workloads with env, target and status (draft, applied, failed)."
    )]
    async fn workload_list(&self, Parameters(args): Parameters<WorkloadListArgs>) -> ToolResult {
        let app = self.app().await?;
        json(
            &app.workload_list(args.env.as_deref())
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    #[tool(
        description = "Show a workload: target, status and its services with versions, images, env (secret values are never included) and published ports."
    )]
    async fn workload_show(&self, Parameters(args): Parameters<WorkloadArgs>) -> ToolResult {
        let app = self.app().await?;
        let detail = app
            .workload_show(&args.workload)
            .await
            .map_err(|e| e.to_string())?;
        let unmet = app
            .workload_unmet_dependencies(&args.workload)
            .await
            .map_err(|e| e.to_string())?;
        json(&serde_json::json!({ "workload": detail, "unmet_dependencies": unmet }))
    }

    #[tool(
        description = "List targets (VMs and Kubernetes clusters) with their connection and last test result."
    )]
    async fn target_list(&self) -> ToolResult {
        let app = self.app().await?;
        json(&app.target_list().await.map_err(|e| e.to_string())?)
    }

    #[tool(
        description = "List the org's service registry: services, categories, tags and versions."
    )]
    async fn registry_list(&self, Parameters(args): Parameters<RegistryListArgs>) -> ToolResult {
        let app = self.app().await?;
        let category = args
            .category
            .map(|c| c.parse())
            .transpose()
            .map_err(|e: ikit_core::UnknownVariant| e.to_string())?;
        let filter = RegistryFilter {
            category,
            tag: args.tag,
        };
        json(
            &app.registry_list(&filter)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    #[tool(
        description = "Show one registry service with every version's image, ports, env schema and dependencies."
    )]
    async fn registry_show(&self, Parameters(args): Parameters<NameArgs>) -> ToolResult {
        let app = self.app().await?;
        let entry = app
            .registry_show(&args.name)
            .await
            .map_err(|e| e.to_string())?;
        let usage = app
            .registry_usage(&args.name)
            .await
            .map_err(|e| e.to_string())?;
        json(&serde_json::json!({ "service": entry, "used_by": usage }))
    }

    #[tool(
        description = "Show what `ikit apply` would change for a workload on its target (added, removed and updated services). Read-only: nothing is applied. Secret values are never shown, only which secret keys changed."
    )]
    async fn deploy_plan(&self, Parameters(args): Parameters<WorkloadArgs>) -> ToolResult {
        let app = self.app().await?;
        let plan = app
            .deploy_plan(&args.workload)
            .await
            .map_err(|e| e.to_string())?;
        let plan_id = self.plan_id(&plan);
        json(&serde_json::json!({
            "plan_id": plan_id,
            "workload": plan.workload,
            "target": plan.target,
            "status": plan.status,
            "needs_apply": plan.needs_apply(),
            "plan": plan.plan,
            "hooks": plan.hooks,
            "blockers": plan.blockers,
        }))
    }

    #[tool(
        description = "Show a workload's containers on its target: state, health and published ports."
    )]
    async fn deploy_status(&self, Parameters(args): Parameters<WorkloadArgs>) -> ToolResult {
        let app = self.app().await?;
        json(
            &app.deploy_status(&args.workload)
                .await
                .map_err(|e| e.to_string())?,
        )
    }

    #[tool(
        description = "Compare a workload's running containers with the files last applied: missing, stopped, unhealthy, \
                       hand-modified and extra containers. Omit workload to check every applied workload."
    )]
    async fn deploy_drift(&self, Parameters(args): Parameters<OptionalWorkloadArgs>) -> ToolResult {
        let app = self.app().await?;
        match args.workload {
            Some(w) => json(&app.deploy_drift(&w).await.map_err(err)?),
            None => json(&app.deploy_drift_all().await.map_err(err)?),
        }
    }

    #[tool(
        description = "Workload services on an older or deprecated registry version: `wanted` is the newest version \
                       their constraint allows, `latest` the newest in the registry. Omit workload for the whole org."
    )]
    async fn registry_outdated(
        &self,
        Parameters(args): Parameters<OptionalWorkloadArgs>,
    ) -> ToolResult {
        let app = self.app().await?;
        json(
            &app.registry_outdated(args.workload.as_deref())
                .await
                .map_err(err)?,
        )
    }

    #[tool(
        description = "Show a workload's apply and destroy history, newest first, with who ran them and any error."
    )]
    async fn workload_runs(&self, Parameters(args): Parameters<RunsArgs>) -> ToolResult {
        let app = self.app().await?;
        json(
            &app.workload_runs(&args.workload, args.limit.unwrap_or(20))
                .await
                .map_err(|e| e.to_string())?,
        )
    }
}

#[tool_router(router = mutate_router)]
impl IkitServer {
    #[tool(
        description = "Create a workload (named <prefix>-<env>-<nnn>) on a target. Needs `mutate` for the env in the org's mcp.yaml. Does not deploy."
    )]
    async fn workload_create(
        &self,
        Parameters(args): Parameters<WorkloadCreateArgs>,
    ) -> ToolResult {
        let app = self.app().await?;
        app.mcp_check_mutate(&args.env).map_err(err)?;
        json(
            &app.workload_create(&args.env, &args.target)
                .await
                .map_err(err)?,
        )
    }

    #[tool(
        description = "Add registry services to a workload's group (app, platform or data). Non-secret env only; services that require a secret must be added with the CLI. Needs `mutate` in mcp.yaml. Does not deploy."
    )]
    async fn service_add(&self, Parameters(args): Parameters<ServiceAddArgs>) -> ToolResult {
        let app = self.mutable(&args.workload).await?;
        let group = args
            .group
            .parse()
            .map_err(|e: ikit_core::UnknownVariant| e.to_string())?;
        let mut requests = Vec::new();
        for s in &args.services {
            let dep: ikit_core::Dependency = s.parse().map_err(err)?;
            let (_, version) = app
                .registry_resolve(&dep.name, &dep.constraint)
                .await
                .map_err(err)?;
            let env = args.env.get(&dep.name).cloned().unwrap_or_default();
            if let Some(k) = env
                .keys()
                .find(|k| version.spec.env_var(k).is_some_and(|v| v.secret))
            {
                return Err(format!(
                    "{}.{k} is a secret; secrets can only be set with the ikit CLI",
                    dep.name
                ));
            }
            requests.push(ikit_app::ServiceRequest {
                name: None,
                registry: dep.name,
                constraint: dep.constraint,
                group,
                env,
            });
        }
        json(
            &app.services_add(&args.workload, requests)
                .await
                .map_err(err)?,
        )
    }

    #[tool(
        description = "Set or unset non-secret env values on a service. Needs `mutate` in mcp.yaml. Takes effect on the next apply."
    )]
    async fn service_set_env(&self, Parameters(args): Parameters<ServiceSetEnvArgs>) -> ToolResult {
        let app = self.mutable(&args.workload).await?;
        let (_, version) = app
            .service_show(&args.workload, &args.service)
            .await
            .map_err(err)?;
        for k in args.set.keys().chain(&args.unset) {
            if version.spec.env_var(k).is_some_and(|v| v.secret) {
                return Err(format!(
                    "{k} is a secret; secrets can only be changed with the ikit CLI"
                ));
            }
        }
        json(
            &app.service_set_env(&args.workload, &args.service, args.set, &args.unset)
                .await
                .map_err(err)?,
        )
    }

    #[tool(
        description = "Publish container ports of a service on its target's host, or stop publishing host ports. Needs `mutate` in mcp.yaml. Takes effect on the next apply."
    )]
    async fn service_publish(
        &self,
        Parameters(args): Parameters<ServicePublishArgs>,
    ) -> ToolResult {
        let app = self.mutable(&args.workload).await?;
        let add = args
            .add
            .iter()
            .map(|p| p.parse())
            .collect::<Result<Vec<_>, _>>()?;
        json(
            &app.service_publish(&args.workload, &args.service, add, &args.remove)
                .await
                .map_err(err)?,
        )
    }

    #[tool(
        description = "Add a SQL hook: scripts from a local file or folder run once each in a database service after every apply. Needs `mutate` in mcp.yaml."
    )]
    async fn hook_add_sql(&self, Parameters(args): Parameters<SqlHookAddArgs>) -> ToolResult {
        let app = self.mutable(&args.workload).await?;
        let req = ikit_app::NewHookRequest {
            name: args.name,
            phase: ikit_core::HookPhase::PostApply,
            fail_on_error: None,
            action: ikit_app::HookRequest::Sql {
                service: args.service,
                engine: None,
                database: None,
                source: args.source.into(),
                via: ikit_core::SqlVia::Exec,
            },
        };
        json(&app.hook_add(&args.workload, req).await.map_err(err)?)
    }

    #[tool(description = "List a workload's hooks (SQL, shell, HTTP) and when they run.")]
    async fn hook_list(&self, Parameters(args): Parameters<WorkloadArgs>) -> ToolResult {
        let app = self.app().await?;
        json(&app.hook_list(&args.workload).await.map_err(err)?)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployApplyArgs {
    pub workload: String,
    /// `plan_id` from deploy_plan; apply is refused if the plan changed since.
    pub plan_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeployDestroyArgs {
    pub workload: String,
    /// Leave empty to get a confirmation token; call again with it within 5 minutes to destroy.
    #[serde(default)]
    pub confirm: Option<String>,
}

#[tool_router(router = deploy_router)]
impl IkitServer {
    #[tool(
        description = "Apply a plan from deploy_plan to the workload's target (docker compose up and hooks). Pass the plan_id you were shown; if anything changed since, it is refused. Needs `deploy` for the env in the org's mcp.yaml."
    )]
    async fn deploy_apply(&self, Parameters(args): Parameters<DeployApplyArgs>) -> ToolResult {
        let app = self.app().await?;
        let env = app.workload_get(&args.workload).await.map_err(err)?.env;
        app.mcp_check_deploy(&env).map_err(err)?;
        let plan = app.deploy_plan(&args.workload).await.map_err(err)?;
        if self.plan_id(&plan) != args.plan_id {
            return Err(
                "the plan changed since it was shown (or the id is wrong); call deploy_plan again"
                    .to_owned(),
            );
        }
        if !plan.needs_apply() {
            return json(&serde_json::json!({ "applied": false, "reason": "nothing to apply" }));
        }
        let report = app.deploy_apply(&plan, "mcp", false).await.map_err(err)?;
        json(&serde_json::json!({
            "applied": true,
            "run_id": report.run_id,
            "containers": report.containers,
            "hooks_ran": report.hooks_ran,
        }))
    }

    #[tool(
        description = "Stop and remove a workload's containers (volumes and data are always kept). The first call returns a confirmation token; after the user approves, call again with `confirm` set to it within 5 minutes. Needs `deploy` for the env in the org's mcp.yaml."
    )]
    async fn deploy_destroy(&self, Parameters(args): Parameters<DeployDestroyArgs>) -> ToolResult {
        let app = self.app().await?;
        let env = app.workload_get(&args.workload).await.map_err(err)?.env;
        app.mcp_check_deploy(&env).map_err(err)?;
        let token = {
            let mut tokens = self.destroy_tokens.lock().expect("token map");
            tokens.retain(|_, (_, at)| at.elapsed() < TOKEN_TTL);
            match args.confirm {
                None => {
                    let token = digest(&[&self.key, &args.workload, &format!("{:?}", Instant::now())])[..12].to_owned();
                    tokens.insert(token.clone(), (args.workload.clone(), Instant::now()));
                    Some(token)
                }
                Some(token) => match tokens.remove(&token) {
                    Some((wl, _)) if wl == args.workload => None,
                    _ => return Err("unknown or expired confirmation token; call deploy_destroy without `confirm` first".to_owned()),
                },
            }
        };
        if let Some(token) = token {
            return json(&serde_json::json!({
                "confirm_with": token,
                "message": format!("This removes the containers of {} (volumes are kept). Ask the user to confirm, then call deploy_destroy again with confirm set to this token.", args.workload),
            }));
        }
        let run = app
            .deploy_destroy(&args.workload, false, "mcp")
            .await
            .map_err(err)?;
        json(&serde_json::json!({ "destroyed": true, "run_id": run, "volumes_kept": true }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ZtAuthorizeArgs {
    /// ikit network name (see zt_status).
    pub network: String,
    /// 10-digit ZeroTier node ID.
    pub node: String,
    /// true to authorize, false to revoke.
    pub authorized: bool,
    /// Member name shown in ZeroTier Central.
    #[serde(default)]
    pub name: Option<String>,
}

#[tool_router(router = zt_router)]
impl IkitServer {
    #[tool(
        description = "List the org's ZeroTier networks with their linked workloads and members (node ID, name, authorized, online, addresses)."
    )]
    async fn zt_status(&self) -> ToolResult {
        let app = self.app().await?;
        let mut out = Vec::new();
        for network in app.zt_list().await.map_err(err)? {
            let members = app.zt_members(&network.name).await;
            out.push(match members {
                Ok(m) => serde_json::json!({ "network": network, "members": m }),
                Err(e) => serde_json::json!({ "network": network, "error": e.to_string() }),
            });
        }
        json(&out)
    }

    #[tool(
        description = "Authorize (or revoke with authorized=false) a node on one of the org's ZeroTier networks. Needs `zt_authorize: true` in the org's mcp.yaml."
    )]
    async fn zt_authorize(&self, Parameters(args): Parameters<ZtAuthorizeArgs>) -> ToolResult {
        let app = self.app().await?;
        app.mcp_check_zt_authorize().map_err(err)?;
        json(
            &app.zt_authorize(
                &args.network,
                &args.node,
                args.authorized,
                args.name.as_deref(),
            )
            .await
            .map_err(err)?,
        )
    }
}

fn user(text: String) -> Vec<PromptMessage> {
    vec![PromptMessage::new_text(Role::User, text)]
}

#[prompt_router]
impl IkitServer {
    #[prompt(
        name = "diagnose_workload",
        description = "Find out why a workload is failing or misbehaving, and what to do about it."
    )]
    async fn diagnose_workload(
        &self,
        Parameters(args): Parameters<WorkloadArgs>,
    ) -> Vec<PromptMessage> {
        let w = args.workload;
        user(format!(
            "Diagnose the ikit workload `{w}` in org `{org}`.\n\
             1. Call workload_show and workload_runs for `{w}`; note its status and the last run's error.\n\
             2. Call deploy_status, then deploy_drift, for `{w}`: look for stopped, unhealthy, missing, \
             hand-modified or extra containers.\n\
             3. Call deploy_plan for `{w}` to see definition changes that were never applied, and any blockers.\n\
             4. Call hook_list for `{w}` if a run failed in a hook.\n\
             Then explain the most likely cause in plain words and give the exact ikit commands that fix it \
             (for example `ikit apply --force` to recreate drifted containers). Do not apply or destroy anything \
             yourself; ask me first.",
            org = self.org
        ))
    }

    #[prompt(
        name = "plan_upgrades",
        description = "Find services on older or deprecated versions and propose a safe upgrade order."
    )]
    async fn plan_upgrades(
        &self,
        Parameters(args): Parameters<OptionalWorkloadArgs>,
    ) -> Vec<PromptMessage> {
        let scope = match &args.workload {
            Some(w) => format!("the workload `{w}`"),
            None => "every workload".to_owned(),
        };
        user(format!(
            "Plan version upgrades for {scope} in ikit org `{org}`.\n\
             1. Call registry_outdated{arg}. For each result, `wanted` is reachable with `ikit svc upgrade <service>`; \
             `latest` needs `ikit svc upgrade <service> --to <version>` and may be a major upgrade.\n\
             2. Call registry_show for each service to read release notes and deprecations.\n\
             3. Group the upgrades: patch and minor first, majors separately, databases last and only after a \
             backup (`ikit backup run <service>`).\n\
             Give me the commands in order, dev envs before prod. Do not change anything yourself.",
            org = self.org,
            arg = args
                .workload
                .as_ref()
                .map(|w| format!(" with workload `{w}`"))
                .unwrap_or_default()
        ))
    }

    #[prompt(
        name = "review_plan",
        description = "Explain a workload's deploy plan and its risks before it is applied."
    )]
    async fn review_plan(&self, Parameters(args): Parameters<WorkloadArgs>) -> Vec<PromptMessage> {
        let w = args.workload;
        user(format!(
            "Review the deploy plan for `{w}` in ikit org `{org}` before it is applied.\n\
             Call deploy_plan for `{w}` and explain each change: services added, removed (their containers are \
             deleted, volumes kept) and updated, hook scripts that will run or be refused, and any blockers. \
             Point out anything risky, such as database image changes, removed services or changed SQL scripts. \
             End with a clear recommendation. If I then ask you to apply it, call deploy_apply with the plan_id \
             from that same deploy_plan call.",
            org = self.org
        ))
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for IkitServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = Implementation::new("ikit", env!("CARGO_PKG_VERSION"));
        info.title = Some(format!("ikit (org {})", self.org));
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
            .with_server_info(info)
            .with_instructions(format!(
                "Access to ikit org `{}`: workloads, targets, the service registry, hooks, deploy plans, container \
                 status, run history and ZeroTier networks. Tools that change definitions or deploy only work where the org's mcp.yaml \
                 allows them; deploy_apply needs the plan_id from deploy_plan, and deploy_destroy needs a \
                 confirmation token the user must approve. Secret values are never returned and cannot be set.",
                self.org
            ))
    }
}

/// Serves `org` over stdin/stdout until the client disconnects.
pub async fn serve_stdio(
    paths: Paths,
    org: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let service = IkitServer::new(paths, org)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Serves `org` over streamable HTTP at `http://<addr>/mcp` until Ctrl-C. Every request
/// needs `Authorization: Bearer <token>` with a token from `ikit mcp token create`.
pub async fn serve_http(
    paths: Paths,
    org: String,
    addr: std::net::SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

    let org_paths = paths.org(&org);
    // One server shared by every session, so plan IDs and destroy tokens issued in one
    // request are honored in the next (stateless clients get a new session per request).
    let server = IkitServer::new(paths, org);
    let ip = addr.ip();
    let mut hosts = vec![ip.to_string(), format!("[{ip}]"), addr.to_string()];
    if ip.is_loopback() {
        hosts.extend(["localhost".to_owned(), format!("localhost:{}", addr.port())]);
    }
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(hosts);
    let cancel = config.cancellation_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let router = axum::Router::new().nest_service("/mcp", service).layer(
        axum::middleware::from_fn_with_state(org_paths, require_token),
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        })
        .await?;
    Ok(())
}

/// Rejects requests without a valid bearer token with 401.
async fn require_token(
    axum::extract::State(org): axum::extract::State<ikit_app::OrgPaths>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match presented.map(|t| ikit_app::mcp_token_verify(&org, t)) {
        Some(Ok(Some(name))) => {
            tracing::info!(token = %name, method = %req.method(), "mcp http request");
            next.run(req).await
        }
        Some(Err(e)) => {
            tracing::error!(%e, "reading MCP tokens");
            (StatusCode::INTERNAL_SERVER_ERROR, "cannot read MCP tokens").into_response()
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid bearer token (create one with `ikit mcp token create`)",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_all_tools() {
        let server = IkitServer::new(Paths::new("/nonexistent"), "acme".into());
        let mut names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "deploy_apply",
                "deploy_destroy",
                "deploy_drift",
                "deploy_plan",
                "deploy_status",
                "hook_add_sql",
                "hook_list",
                "registry_list",
                "registry_outdated",
                "registry_show",
                "service_add",
                "service_publish",
                "service_set_env",
                "target_list",
                "workload_create",
                "workload_list",
                "workload_runs",
                "workload_show",
                "zt_authorize",
                "zt_status"
            ]
        );
        let info = server.get_info();
        assert!(info.instructions.unwrap().contains("confirmation token"));
        assert!(info.capabilities.prompts.is_some());
        let mut prompts: Vec<String> = server
            .prompt_router
            .list_all()
            .into_iter()
            .map(|p| p.name.to_string())
            .collect();
        prompts.sort();
        assert_eq!(
            prompts,
            ["diagnose_workload", "plan_upgrades", "review_plan"]
        );
    }

    #[tokio::test]
    async fn tools_report_errors_as_text() {
        let server = IkitServer::new(Paths::new("/nonexistent"), "acme".into());
        let err = server
            .workload_list(Parameters(WorkloadListArgs { env: None }))
            .await
            .unwrap_err();
        assert!(err.contains("org `acme`"), "{err}");
    }

    #[tokio::test]
    async fn definition_tools_are_gated_by_mcp_yaml() {
        let dir = std::env::temp_dir().join(format!("ikit-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(&dir);
        ikit_app::org_create(
            &paths,
            ikit_app::NewOrg {
                name: "acme".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let server = IkitServer::new(paths.clone(), "acme".into());
        let app = server.app().await.unwrap();
        app.target_attach("laptop", ikit_core::Connection::Local)
            .await
            .unwrap();

        let create = || WorkloadCreateArgs {
            env: "dev".into(),
            target: "laptop".into(),
        };
        let e = server
            .workload_create(Parameters(create()))
            .await
            .unwrap_err();
        assert!(e.contains("mutate"), "{e}");

        std::fs::write(
            paths.org("acme").mcp_config_file(),
            "mutate: true\nmutate_envs: [dev]\n",
        )
        .unwrap();
        server.workload_create(Parameters(create())).await.unwrap();
        let add = ServiceAddArgs {
            workload: "wl-acme-dev-001".into(),
            group: "data".into(),
            services: vec!["redis@7".into()],
            env: BTreeMap::new(),
        };
        server.service_add(Parameters(add)).await.unwrap();
        let secret = ServiceAddArgs {
            workload: "wl-acme-dev-001".into(),
            group: "data".into(),
            services: vec!["postgres@16".into()],
            env: BTreeMap::from([(
                "postgres".into(),
                BTreeMap::from([("POSTGRES_PASSWORD".into(), "x".into())]),
            )]),
        };
        let e = server.service_add(Parameters(secret)).await.unwrap_err();
        assert!(e.contains("secret"), "{e}");

        let prod = WorkloadCreateArgs {
            env: "prod".into(),
            target: "laptop".into(),
        };
        assert!(
            server.workload_create(Parameters(prod)).await.is_err(),
            "prod is not in mutate_envs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deploy_tools_need_mcp_yaml_and_valid_ids() {
        let dir = std::env::temp_dir().join(format!("ikit-mcp-deploy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::new(&dir);
        ikit_app::org_create(
            &paths,
            ikit_app::NewOrg {
                name: "acme".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let server = IkitServer::new(paths.clone(), "acme".into());
        let app = server.app().await.unwrap();
        app.target_attach("laptop", ikit_core::Connection::Local)
            .await
            .unwrap();
        app.workload_create("dev", "laptop").await.unwrap();

        let apply = |id: &str| DeployApplyArgs {
            workload: "wl-acme-dev-001".into(),
            plan_id: id.into(),
        };
        let e = server
            .deploy_apply(Parameters(apply("x")))
            .await
            .unwrap_err();
        assert!(e.contains("deploy_envs"), "{e}");

        std::fs::write(
            paths.org("acme").mcp_config_file(),
            "deploy: true\ndeploy_envs: [dev]\n",
        )
        .unwrap();
        let e = server
            .deploy_apply(Parameters(apply("wrong")))
            .await
            .unwrap_err();
        assert!(
            e.contains("no services") || e.contains("plan changed"),
            "{e}"
        );

        // Destroy needs a token, and a token works once.
        let destroy = |confirm: Option<&str>| DeployDestroyArgs {
            workload: "wl-acme-dev-001".into(),
            confirm: confirm.map(Into::into),
        };
        let first: serde_json::Value = serde_json::from_str(
            &server
                .deploy_destroy(Parameters(destroy(None)))
                .await
                .unwrap(),
        )
        .unwrap();
        let token = first["confirm_with"].as_str().unwrap().to_owned();
        assert!(
            server
                .deploy_destroy(Parameters(destroy(Some("bogus"))))
                .await
                .is_err()
        );
        let done: serde_json::Value = serde_json::from_str(
            &server
                .deploy_destroy(Parameters(destroy(Some(&token))))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(done["volumes_kept"], true);

        let zt = ZtAuthorizeArgs {
            network: "dev-net".into(),
            node: "a1b2c3d4e5".into(),
            authorized: true,
            name: None,
        };
        let e = server.zt_authorize(Parameters(zt)).await.unwrap_err();
        assert!(e.contains("zt_authorize: true"), "{e}");
        assert_eq!(server.zt_status().await.unwrap(), "[]");
        assert!(
            server
                .deploy_destroy(Parameters(destroy(Some(&token))))
                .await
                .is_err(),
            "token is single-use"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
