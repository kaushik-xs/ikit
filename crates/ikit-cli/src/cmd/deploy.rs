//! `ikit plan | apply | status | destroy | render`: deploying the selected workload.

use ikit_app::{App, ContainerStatus, DeployPlan, DriftReport};
use ikit_provider::compose::Change;

use crate::output::{Output, print_json, table};
use crate::{Ctx, warn};

/// Who is acting, recorded on runs and locks.
pub fn actor() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    format!("cli:{user}")
}

async fn workload(ctx: &Ctx) -> anyhow::Result<(App, String)> {
    let app = ctx.app().await?;
    let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
    Ok((app, wl))
}

pub async fn render(ctx: &Ctx) -> anyhow::Result<()> {
    let (app, wl) = workload(ctx).await?;
    let rendered = app.render_workload(&wl).await?;
    if ctx.output == Output::Json {
        return print_json(
            &serde_json::json!({ "project": rendered.project, "compose": rendered.compose }),
        );
    }
    print!("{}", rendered.compose);
    if !rendered.secrets.is_empty() {
        eprintln!("(secret env values go to a separate owner-only file and are not shown)");
    }
    Ok(())
}

pub async fn plan(ctx: &Ctx) -> anyhow::Result<()> {
    let (app, wl) = workload(ctx).await?;
    let plan = app.deploy_plan(&wl).await?;
    if ctx.output == Output::Json {
        return print_json(&plan);
    }
    print_plan(&plan);
    if plan.needs_apply() {
        println!("Run `ikit apply` to make these changes.");
    }
    Ok(())
}

pub async fn apply(ctx: &Ctx, yes: bool, allow_changed: bool, force: bool) -> anyhow::Result<()> {
    let (app, wl) = workload(ctx).await?;
    let plan = app.deploy_plan(&wl).await?;
    if !plan.needs_apply() && !force {
        if ctx.output == Output::Json {
            return print_json(&plan);
        }
        println!("{wl} is up to date on {}; nothing to apply.", plan.target);
        return Ok(());
    }
    if ctx.output == Output::Table {
        print_plan(&plan);
    }
    if !ctx.confirm(&format!("Apply to {}?", plan.target), yes)? {
        return Ok(());
    }
    if ctx.output == Output::Table {
        println!("Applying (pulling images and waiting for health checks)...");
    }
    let report = app.deploy_apply(&plan, &actor(), allow_changed).await?;
    if ctx.output == Output::Json {
        return print_json(&report);
    }
    println!("Applied {wl} (run {}).", report.run_id);
    print_containers(&report.containers);
    print_outcomes(&report.hooks_ran);
    Ok(())
}

pub async fn status(ctx: &Ctx) -> anyhow::Result<()> {
    let (app, wl) = workload(ctx).await?;
    let w = app.workload_get(&wl).await?;
    let containers = app.deploy_status(&wl).await?;
    if ctx.output == Output::Json {
        return print_json(&serde_json::json!({ "workload": w, "containers": containers }));
    }
    println!("{wl} on {} is {}.", w.target, w.status);
    if containers.is_empty() {
        println!("Nothing is deployed. Run `ikit apply` to deploy it.");
    } else {
        print_containers(&containers);
    }
    Ok(())
}

/// Exits with status 2 when any workload drifted, so cron jobs and CI can alert on it.
pub async fn drift(ctx: &Ctx, all: bool) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    let reports = if all {
        app.deploy_drift_all().await?
    } else {
        let wl = app.resolve_workload_name(ctx.wl.as_deref())?;
        vec![app.deploy_drift(&wl).await?]
    };
    let drifted = reports.iter().any(DriftReport::drifted);
    if ctx.output == Output::Json {
        print_json(&reports)?;
    } else {
        if reports.is_empty() {
            println!("No applied workloads in this org.");
        }
        for r in &reports {
            print_drift(r);
        }
    }
    if drifted {
        std::process::exit(2);
    }
    Ok(())
}

fn print_drift(r: &DriftReport) {
    let pending = match r.unapplied {
        0 => String::new(),
        n => format!("; {n} definition change(s) not applied yet (see `ikit plan`)"),
    };
    if !r.drifted() {
        println!("{} on {}: no drift{pending}.", r.workload, r.target);
        return;
    }
    println!("{} on {}: drifted{pending}.", r.workload, r.target);
    let mut t = table(&["SERVICE", "DRIFT", "DETAIL"]);
    for i in &r.items {
        t.add_row(vec![
            i.service.clone(),
            i.kind.to_string(),
            i.detail.clone(),
        ]);
    }
    println!("{t}");
    println!("`ikit apply --force` recreates the containers from the applied files.");
}

pub async fn destroy(ctx: &Ctx, volumes: bool, yes: bool) -> anyhow::Result<()> {
    let (app, wl) = workload(ctx).await?;
    let target = app.workload_get(&wl).await?.target;
    let what = if volumes {
        "containers AND volumes (all data)"
    } else {
        "containers (volumes are kept)"
    };
    if !ctx.confirm(
        &format!("Destroy {wl} on {target}: remove its {what}?"),
        yes,
    )? {
        return Ok(());
    }
    if volumes
        && ctx.interactive
        && !yes
        && inquire::Text::new(&format!("Type {wl} to confirm deleting its data:")).prompt()? != wl
    {
        anyhow::bail!("confirmation did not match; nothing was destroyed");
    }
    let run = app.deploy_destroy(&wl, volumes, &actor()).await?;
    if ctx.output == Output::Json {
        return print_json(
            &serde_json::json!({ "workload": wl, "run_id": run, "volumes_removed": volumes }),
        );
    }
    println!("Destroyed {wl} on {target} (run {run}). The workload definition is kept as draft.");
    if !volumes {
        println!("Volumes were kept; `ikit destroy --volumes` also deletes them.");
    }
    Ok(())
}

pub async fn runs(ctx: &Ctx, limit: i64) -> anyhow::Result<()> {
    let (app, wl) = workload(ctx).await?;
    let runs = app.workload_runs(&wl, limit).await?;
    if ctx.output == Output::Json {
        return print_json(&runs);
    }
    if runs.is_empty() {
        println!("{wl} has not been applied yet.");
        return Ok(());
    }
    let mut t = table(&["RUN", "ACTION", "RESULT", "BY", "STARTED", "ERROR"]);
    for r in &runs {
        let error = r
            .error
            .as_deref()
            .unwrap_or("")
            .lines()
            .last()
            .unwrap_or("")
            .to_owned();
        t.add_row(vec![
            r.id.to_string(),
            r.action.to_string(),
            r.result.to_string(),
            r.actor.clone(),
            r.started_at.format("%Y-%m-%d %H:%M:%S").to_string(),
            error,
        ]);
    }
    println!("{t}");
    Ok(())
}

fn print_plan(p: &DeployPlan) {
    let header = if p.plan.first_apply {
        "first deploy"
    } else {
        "changes"
    };
    println!("Plan for {} on {} ({header}):", p.workload, p.target);
    if p.plan.is_empty() {
        if p.status != ikit_core::WorkloadStatus::Applied {
            println!(
                "  no file changes, but the workload is {}; apply will retry it",
                p.status
            );
        } else if !p.needs_apply() {
            println!("  no changes");
        } else {
            println!("  no service changes");
        }
    }
    for c in &p.plan.changes {
        match c {
            Change::Add { service, image } => println!("  + {service:<20} {image}"),
            Change::Remove { service } => println!("  - {service}"),
            Change::Update { service, details } => {
                println!("  ~ {service}");
                for d in details {
                    println!("      {d}");
                }
            }
        }
    }
    print_hooks(p);
    for b in &p.blockers {
        warn(format!("apply is blocked: {b}"));
    }
    if p.plan
        .changes
        .iter()
        .any(|c| matches!(c, Change::Remove { .. }))
    {
        warn("removed services' containers are deleted; their volumes are kept");
    }
}

pub fn print_outcomes(outcomes: &[ikit_app::HookOutcome]) {
    for o in outcomes {
        let reason = o
            .reason
            .map(|r| format!(" ({})", format!("{r:?}").to_lowercase()))
            .unwrap_or_default();
        match &o.warning {
            Some(w) => warn(format!("hook {} ({}, {}): {w}", o.hook, o.phase, o.step)),
            None => println!("  ran hook {} {}: {}{reason}", o.phase, o.hook, o.step),
        }
    }
}

fn print_hooks(p: &DeployPlan) {
    use ikit_core::{HookKind, ScriptStatus};
    for h in &p.hooks {
        if h.kind != HookKind::Sql {
            println!("  hook {} runs {}: {}", h.hook, h.phase, h.action);
            continue;
        }
        if h.pending() == 0 && h.changed().is_empty() && h.note.is_none() {
            continue;
        }
        println!("  hook {} ({}):", h.hook, h.action);
        for s in &h.scripts {
            match s.status {
                ScriptStatus::New => println!("      run    {}", s.script),
                ScriptStatus::Rerun => println!("      rerun  {} (repeatable, changed)", s.script),
                ScriptStatus::Changed => {
                    println!(
                        "      REFUSE {} (changed after it was applied; `ikit apply --allow-changed` reruns it)",
                        s.script
                    )
                }
                ScriptStatus::Applied => {}
            }
        }
        if let Some(note) = &h.note {
            println!("      note: {note}");
        }
    }
}

fn print_containers(containers: &[ContainerStatus]) {
    if containers.is_empty() {
        return;
    }
    let mut t = table(&["SERVICE", "STATE", "HEALTH", "PORTS", "STATUS"]);
    for c in containers {
        t.add_row(vec![
            c.service.clone(),
            c.state.clone(),
            c.health.clone(),
            c.ports.join("\n"),
            c.status.clone(),
        ]);
    }
    println!("{t}");
}
