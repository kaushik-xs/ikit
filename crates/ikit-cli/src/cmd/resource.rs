//! `ikit res ...`: buckets and queues services need, and the backends that provide them.

use clap::Subcommand;
use ikit_app::{BackendChanges, NewBackendRequest, ResourceInfo};
use ikit_core::{Backend, BackendOptions, ResourceApi, ResourceProvider};

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum ResourceCmd {
    /// List the workload's resources (or, with --all, every workload's).
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        all: bool,
    },
    /// Show one resource: <service>.<resource>.
    Show { resource: String },
    /// Create a resource on this backend instead of the one bound to the workload's env.
    /// Only before it exists.
    Bind { resource: String, backend: String },
    /// Use an existing bucket or queue for the resource instead of creating one. ikit never
    /// deletes adopted resources, only forgets them.
    Adopt {
        resource: String,
        /// Bucket name, or queue name or URL.
        id: String,
    },
    /// Remove a resource no service needs any more (after `ikit svc rm`). One ikit created is
    /// deleted at its backend; a bucket must be empty.
    #[command(alias = "remove")]
    Rm {
        resource: String,
        /// Forget it but leave it at the backend.
        #[arg(long)]
        keep: bool,
        #[arg(long, short)]
        yes: bool,
    },
    /// Backends that provide resources, and which one each env uses.
    #[command(subcommand)]
    Backend(BackendCmd),
    /// IAM users ikit created for the workload's services (backends with per-service keys).
    /// Lists them without a subcommand.
    Keys {
        /// Every workload's.
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        cmd: Option<KeysCmd>,
    },
    /// Give a service a new access key. The next `ikit apply` redeploys it with the new key and
    /// then deletes the old one.
    Rotate { service: String },
}

#[derive(Debug, Subcommand)]
pub enum KeysCmd {
    /// List the workload's IAM users (or, with --all, every workload's).
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        all: bool,
    },
    /// Delete the IAM user and keys of a service removed from the workload.
    #[command(alias = "remove")]
    Rm {
        service: String,
        /// Forget them but leave them in IAM.
        #[arg(long)]
        keep: bool,
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum BackendCmd {
    /// Add a backend: AWS itself, or an S3- or SQS-compatible server with --endpoint.
    Add {
        name: String,
        /// s3 (buckets) or sqs (queues).
        #[arg(long)]
        api: ResourceApi,
        #[arg(long, default_value = "aws")]
        provider: ResourceProvider,
        /// aws profile with credentials; services get them too. Omit to use the environment.
        #[arg(long)]
        profile: Option<String>,
        /// Default: the profile's region, else us-east-1.
        #[arg(long)]
        region: Option<String>,
        /// A compatible server, e.g. http://minio.lan:9000. Must be reachable from targets.
        #[arg(long)]
        endpoint: Option<String>,
        /// Give each service an IAM user and key of its own (AWS only; needs --boundary).
        #[arg(long, requires = "boundary")]
        per_service_keys: bool,
        /// Permissions boundary (policy ARN) every IAM user ikit creates carries.
        #[arg(long)]
        boundary: Option<String>,
    },
    /// Turn per-service keys on or off, or change the boundary. Takes effect on each workload's
    /// next apply.
    Set {
        name: String,
        #[arg(long, conflicts_with = "no_per_service_keys")]
        per_service_keys: bool,
        /// Back to the profile's keys; existing IAM users stay until `res keys rm` or `wl rm`.
        #[arg(long)]
        no_per_service_keys: bool,
        /// Permissions boundary (policy ARN); "" clears it.
        #[arg(long)]
        boundary: Option<String>,
    },
    /// Print the permissions boundary and the provisioning identity's policy that per-service
    /// keys need, for this backend's account. Apply them yourself (e.g. with Terraform).
    IamPolicy {
        name: String,
    },
    /// Check the backend's credentials: who they are, and for per-service keys the boundary.
    Test {
        name: String,
    },
    /// List backends and the envs bound to them.
    #[command(alias = "ls")]
    List,
    Show {
        name: String,
    },
    /// Remove a backend (refused while bound or holding resources).
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Create new resources of an env's workloads on these backends (one per API).
    Bind {
        env: String,
        #[arg(required = true)]
        backends: Vec<String>,
    },
    /// Stop creating an env's resources of this API anywhere.
    Unbind {
        env: String,
        api: ResourceApi,
    },
}

pub async fn run(ctx: &Ctx, cmd: ResourceCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    let workload = || app.resolve_workload_name(ctx.wl.as_deref());
    match cmd {
        ResourceCmd::List { all } => {
            let wl = if all { None } else { Some(workload()?) };
            let list = app.resources_list(wl.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!(
                    "No resources. Services get them from `needs` in their registry version (see `ikit registry show <service>`)."
                );
                return Ok(());
            }
            let mut headers = vec!["RESOURCE", "API", "ID", "BACKEND", "STATUS", "USED BY"];
            if all {
                headers.insert(0, "WORKLOAD");
            }
            let mut t = table(&headers);
            for r in &list {
                let mut row = vec![
                    r.resource.address(),
                    r.resource.api.to_string(),
                    r.resource.external_id.clone(),
                    backend_label(r),
                    status_label(r),
                    r.used_by.join(", "),
                ];
                if all {
                    row.insert(0, r.resource.workload.clone());
                }
                t.add_row(row);
            }
            println!("{t}");
        }
        ResourceCmd::Show { resource } => {
            let r = app.resource_show(&workload()?, &resource).await?;
            if ctx.output == Output::Json {
                return print_json(&r);
            }
            let mut t = table(&["FIELD", "VALUE"]);
            t.add_row(vec!["resource".into(), r.resource.address()]);
            t.add_row(vec!["workload".into(), r.resource.workload.clone()]);
            t.add_row(vec![
                "kind".into(),
                format!("{} ({})", r.resource.kind(), r.resource.api),
            ]);
            t.add_row(vec!["id".into(), r.resource.external_id.clone()]);
            t.add_row(vec!["backend".into(), backend_label(&r)]);
            t.add_row(vec!["status".into(), status_label(&r)]);
            t.add_row(vec!["origin".into(), r.resource.origin.to_string()]);
            for (k, v) in &r.resource.outputs {
                t.add_row(vec![k.clone(), v.clone()]);
            }
            if !r.used_by.is_empty() {
                t.add_row(vec!["used by".into(), r.used_by.join(", ")]);
            }
            println!("{t}");
        }
        ResourceCmd::Bind { resource, backend } => {
            let wl = workload()?;
            let r = app.resource_bind(&wl, &resource, &backend).await?;
            if ctx.output == Output::Json {
                return print_json(&r);
            }
            println!(
                "{} will be created on {backend} by the next `ikit apply`.",
                r.address()
            );
        }
        ResourceCmd::Adopt { resource, id } => {
            let wl = workload()?;
            let r = app.resource_adopt(&wl, &resource, &id).await?;
            if ctx.output == Output::Json {
                return print_json(&r);
            }
            println!(
                "{} is now {} on {}. ikit won't delete it; `ikit apply` passes it to the service.",
                r.address(),
                r.external_id,
                r.backend.as_deref().unwrap_or_default()
            );
        }
        ResourceCmd::Rm {
            resource,
            keep,
            yes,
        } => {
            let wl = workload()?;
            let r = app.resource_show(&wl, &resource).await?;
            let question = if keep || r.resource.origin == ikit_core::ResourceOrigin::Adopted {
                format!(
                    "Forget {resource} ({})? It stays at its backend.",
                    r.resource.external_id
                )
            } else {
                format!(
                    "Delete {} {} at {} and forget {resource}? Its data is lost.",
                    r.resource.kind(),
                    r.resource.external_id,
                    backend_label(&r)
                )
            };
            if ctx.confirm(&question, yes)? {
                let deleted = app.resource_remove(&wl, &resource, keep).await?;
                if ctx.output == Output::Json {
                    return print_json(
                        &serde_json::json!({ "resource": resource, "deleted": deleted }),
                    );
                }
                if deleted {
                    println!("Deleted {} and removed {resource}.", r.resource.external_id);
                } else {
                    println!(
                        "Removed {resource}; {} was left in place.",
                        r.resource.external_id
                    );
                }
            }
        }
        ResourceCmd::Backend(cmd) => return backend(ctx, &app, cmd).await,
        ResourceCmd::Keys {
            all,
            cmd: None | Some(KeysCmd::List { .. }),
        } => {
            let all = all
                || matches!(&cmd, ResourceCmd::Keys { cmd: Some(KeysCmd::List { all: true }), .. });
            let wl = if all { None } else { Some(workload()?) };
            let list = app.identities_list(wl.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&list);
            }
            if list.is_empty() {
                println!(
                    "No IAM users. Services get their own on backends with per-service keys (`ikit res backend set <name> --per-service-keys`)."
                );
                return Ok(());
            }
            let mut headers = vec!["SERVICE", "IAM USER", "KEY", "SINCE", "COVERS", "STATUS"];
            if all {
                headers.insert(0, "WORKLOAD");
            }
            let mut t = table(&headers);
            for i in &list {
                let mut status = i.status.to_string();
                if i.retiring_key_id.is_some() {
                    status.push_str(", old key retires on apply");
                }
                if i.orphaned {
                    status.push_str(", orphaned");
                }
                let since = i
                    .rotated_at
                    .unwrap_or(i.created_at)
                    .format("%Y-%m-%d")
                    .to_string();
                let mut row = vec![
                    i.service.clone(),
                    i.user_name.clone(),
                    i.access_key_id.clone().unwrap_or_default(),
                    since,
                    i.covers.join("\n"),
                    status,
                ];
                if all {
                    row.insert(0, i.workload.clone());
                }
                t.add_row(row);
            }
            println!("{t}");
        }
        ResourceCmd::Keys {
            cmd: Some(KeysCmd::Rm { service, keep, yes }),
            ..
        } => {
            let wl = workload()?;
            let question = if keep {
                format!("Forget the IAM user of {service}? It stays in IAM.")
            } else {
                format!("Delete the IAM user and keys of {service}?")
            };
            if ctx.confirm(&question, yes)? {
                let deleted = app.identity_remove(&wl, &service, keep).await?;
                if ctx.output == Output::Table {
                    println!(
                        "{} the IAM user of {service}.",
                        if deleted { "Deleted" } else { "Forgot" }
                    );
                }
            }
        }
        ResourceCmd::Rotate { service } => {
            let wl = workload()?;
            let rotated = app.identity_rotate(&wl, &service).await?;
            if ctx.output == Output::Json {
                return print_json(&rotated);
            }
            for i in &rotated {
                println!(
                    "{} has a new key {}; run `ikit apply` to redeploy {service} with it, which then deletes {}.",
                    i.user_name,
                    i.access_key_id.as_deref().unwrap_or_default(),
                    i.retiring_key_id.as_deref().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

fn backend_label(r: &ResourceInfo) -> String {
    match (&r.resource.backend, &r.bound_to) {
        (Some(b), _) => b.clone(),
        (None, Some(b)) => format!("{b} (env binding)"),
        (None, None) => "none bound".to_owned(),
    }
}

fn status_label(r: &ResourceInfo) -> String {
    let mut s = r.resource.status.to_string();
    if r.resource.origin == ikit_core::ResourceOrigin::Adopted {
        s.push_str(", adopted");
    }
    if r.resource.orphaned {
        s.push_str(", orphaned");
    }
    s
}

async fn backend(ctx: &Ctx, app: &ikit_app::App, cmd: BackendCmd) -> anyhow::Result<()> {
    match cmd {
        BackendCmd::Add {
            name,
            api,
            provider,
            profile,
            region,
            endpoint,
            per_service_keys,
            boundary,
        } => {
            let b = app
                .resource_backend_add(NewBackendRequest {
                    name,
                    api,
                    provider,
                    profile,
                    options: BackendOptions {
                        region,
                        endpoint,
                        per_service_keys,
                        boundary,
                    },
                })
                .await?;
            if ctx.output == Output::Json {
                return print_json(&b);
            }
            println!(
                "Added backend `{}` for {} {}s. Use it for an env with `ikit res backend bind <env> {}`.",
                b.name,
                b.api,
                b.api.kind(),
                b.name
            );
        }
        BackendCmd::List => {
            let all = app.resource_backend_list().await?;
            let bindings = app.resource_bindings().await?;
            if ctx.output == Output::Json {
                return print_json(&serde_json::json!({ "backends": all, "bindings": bindings }));
            }
            if all.is_empty() {
                println!(
                    "No backends yet. Add one with `ikit res backend add <name> --api s3|sqs --profile <aws profile>`."
                );
                return Ok(());
            }
            let mut t = table(&["NAME", "API", "PROVIDER", "WHERE", "PROFILE", "ENVS"]);
            for b in &all {
                let envs: Vec<String> = bindings
                    .iter()
                    .filter(|x| x.backend == b.name)
                    .map(|x| x.env.clone())
                    .collect();
                t.add_row(vec![
                    b.name.clone(),
                    b.api.to_string(),
                    b.provider.to_string(),
                    location(b),
                    b.profile.clone().unwrap_or_default(),
                    envs.join(", "),
                ]);
            }
            println!("{t}");
        }
        BackendCmd::Set {
            name,
            per_service_keys,
            no_per_service_keys,
            boundary,
        } => {
            let changes = BackendChanges {
                per_service_keys: (per_service_keys || no_per_service_keys)
                    .then_some(per_service_keys),
                boundary: boundary.map(|b| (!b.is_empty()).then_some(b)),
            };
            if changes.per_service_keys.is_none() && changes.boundary.is_none() {
                anyhow::bail!(
                    "nothing to change; pass --per-service-keys, --no-per-service-keys or --boundary"
                );
            }
            let b = app.resource_backend_set(&name, changes).await?;
            if ctx.output == Output::Json {
                return print_json(&b);
            }
            if b.options.per_service_keys {
                println!(
                    "Services on {name} get IAM users of their own from their workload's next `ikit apply`."
                );
            } else {
                println!(
                    "Services on {name} get the profile's keys from their workload's next `ikit apply`."
                );
            }
        }
        BackendCmd::IamPolicy { name } => {
            let p = app.identity_policies(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&p);
            }
            println!(
                "# Account {} (provisioning identity {}).",
                p.account, p.caller
            );
            println!(
                "# 1. Create this policy as the permissions boundary {}:",
                p.boundary_arn
            );
            println!("{}", serde_json::to_string_pretty(&p.boundary)?);
            println!();
            println!("# 2. Attach this policy to the provisioning identity:");
            println!("{}", serde_json::to_string_pretty(&p.provisioning)?);
            println!();
            println!(
                "# 3. Then: ikit res backend set {} --per-service-keys --boundary {}",
                p.backend, p.boundary_arn
            );
        }
        BackendCmd::Test { name } => {
            let b = app.resource_backend_show(&name).await?;
            let p = app.identity_policies(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&serde_json::json!({
                    "backend": name, "account": p.account, "caller": p.caller,
                    "per_service_keys": b.options.per_service_keys, "boundary": b.options.boundary,
                }));
            }
            println!("ok: {name} reaches account {} as {}", p.account, p.caller);
            match (&b.options.per_service_keys, &b.options.boundary) {
                (true, Some(boundary)) => println!(
                    "per-service keys: on; IAM users get the boundary {boundary} (not checked: reading policies needs more than the provisioning policy allows)"
                ),
                _ => println!("per-service keys: off; services get the profile's keys"),
            }
        }
        BackendCmd::Show { name } => {
            let b = app.resource_backend_show(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&b);
            }
            let mut t = table(&["FIELD", "VALUE"]);
            t.add_row(vec!["name".into(), b.name.clone()]);
            t.add_row(vec!["api".into(), b.api.to_string()]);
            t.add_row(vec!["provider".into(), b.provider.to_string()]);
            t.add_row(vec!["where".into(), location(&b)]);
            t.add_row(vec![
                "profile".into(),
                b.profile.clone().unwrap_or_else(|| "(environment)".into()),
            ]);
            let keys = if b.options.per_service_keys {
                "own IAM user per service"
            } else {
                "the profile's"
            };
            t.add_row(vec!["keys for services".to_owned(), keys.to_owned()]);
            if let Some(boundary) = &b.options.boundary {
                t.add_row(vec!["boundary".into(), boundary.clone()]);
            }
            println!("{t}");
        }
        BackendCmd::Rm { name, yes } => {
            if ctx.confirm(&format!("Remove backend {name} from ikit?"), yes)? {
                app.resource_backend_remove(&name).await?;
                if ctx.output == Output::Table {
                    println!("Removed backend {name}.");
                }
            }
        }
        BackendCmd::Bind { env, backends } => {
            let mut bound = Vec::new();
            for b in &backends {
                bound.push(app.resource_bind_env(&env, b).await?);
            }
            if ctx.output == Output::Json {
                return print_json(&bound);
            }
            for b in &bound {
                println!(
                    "{} {}s of {env} workloads are created on {}.",
                    b.api,
                    b.api.kind(),
                    b.backend
                );
            }
        }
        BackendCmd::Unbind { env, api } => {
            app.resource_unbind_env(&env, api).await?;
            if ctx.output == Output::Table {
                println!("{env} has no {api} backend now.");
            }
        }
    }
    Ok(())
}

/// `AWS ap-south-1` or the endpoint.
fn location(b: &Backend) -> String {
    let region = b.options.region.as_deref().unwrap_or("profile region");
    match &b.options.endpoint {
        Some(e) => format!("{e} ({region})"),
        None => format!("AWS {region}"),
    }
}
