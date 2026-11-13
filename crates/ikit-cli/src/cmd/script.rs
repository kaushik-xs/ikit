use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::bail;
use clap::Subcommand;
use ikit_app::{ImportStatus, ScriptAddRequest, ScriptPreview, ScriptRunRequest};
use ikit_core::Interpreter;

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum ScriptCmd {
    /// Add a script to the org's library, or update it from its file.
    ///
    /// The file's `# ikit:` header names it, says where it runs and maps the env vars it
    /// reads to sources in ikit's state (see docs/scripts.md). Flags fill in or override
    /// the header.
    Add {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// target (the workload's host, in its compose folder), local (this machine) or
        /// service:<name> (inside that service's container).
        #[arg(long, value_name = "WHERE")]
        runs_on: Option<String>,
        /// sh, bash or python3 (default: from the #! line).
        #[arg(long)]
        interpreter: Option<Interpreter>,
        #[arg(long)]
        description: Option<String>,
        /// An env var the script reads: `NAME=value`, or `NAME=@source` to resolve it, e.g.
        /// `PGPASS=@service.postgres.env.POSTGRES_PASSWORD` (repeatable).
        #[arg(long = "param", value_name = "NAME=VALUE")]
        params: Vec<String>,
    },
    /// List the org's scripts.
    #[command(alias = "ls")]
    List,
    /// Show a script's params, input and where it runs.
    Show {
        name: String,
        /// Also print the script itself.
        #[arg(long)]
        body: bool,
    },
    /// Print a script exactly as it was added.
    Export { name: String },
    /// Remove a script and its history (refused while a hook runs it).
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// Run a script against the selected workload (or --to), after showing what it will
    /// run with.
    Run {
        name: String,
        /// The workload it runs against (default: --wl or the selected one).
        #[arg(long, value_name = "WORKLOAD")]
        to: Option<String>,
        /// A second workload whose values `from.` params read (e.g. prod, when copying a
        /// dump into dev).
        #[arg(long, value_name = "WORKLOAD")]
        from: Option<String>,
        /// Override or add a param: `NAME=value` or `NAME=@source` (repeatable).
        #[arg(long = "param", value_name = "NAME=VALUE")]
        params: Vec<String>,
        /// Use this CSV file instead of the rows built from the services.
        #[arg(long, value_name = "FILE")]
        csv: Option<PathBuf>,
        /// Seconds before the script is stopped (default: its header's, or 600).
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
        /// Only show what it would run with.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, short)]
        yes: bool,
        /// Arguments passed to the script, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Show script runs, newest first (of every workload, or --wl's).
    History {
        name: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

/// Parses repeated `NAME=VALUE` flags.
pub fn parse_params(raw: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    raw.iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
            _ => bail!("--param takes NAME=VALUE (or NAME=@source), got `{p}`"),
        })
        .collect()
}

fn short(checksum: &str) -> &str {
    &checksum[..12.min(checksum.len())]
}

fn print_preview(p: &ScriptPreview) {
    println!(
        "Script {} ({}) on {} ({})",
        p.script,
        short(&p.checksum),
        p.workload,
        p.env
    );
    println!("  runs on: {}", p.location);
    if let Some(from) = &p.from {
        println!("  from:    {from}");
    }
    println!("  command: {}", p.command);
    println!("  timeout: {}s", p.timeout_secs);
    if !p.params.is_empty() {
        let mut t = table(&["PARAM", "VALUE", "FROM"]);
        for r in &p.params {
            let from = match &r.note {
                Some(n) => format!("{} ({n})", r.source),
                None => r.source.clone(),
            };
            t.add_row(vec![r.name.clone(), r.value.clone(), from]);
        }
        println!("{t}");
    }
    if let Some(input) = &p.input {
        let rows = input.lines.len().saturating_sub(1);
        let mut head = format!("Input ({rows} row(s) from {}", input.source);
        if !input.skipped.is_empty() {
            head.push_str(&format!("; skipped: {}", input.skipped.join(", ")));
        }
        println!("{head}):");
        for line in &input.lines {
            println!("  {line}");
        }
    }
}

pub async fn run(ctx: &Ctx, cmd: ScriptCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    match cmd {
        ScriptCmd::Add {
            file,
            name,
            runs_on,
            interpreter,
            description,
            params,
        } => {
            let report = app
                .script_add(ScriptAddRequest {
                    file,
                    name,
                    runs_on,
                    interpreter,
                    description,
                    params: parse_params(&params)?,
                })
                .await?;
            if ctx.output == Output::Json {
                return print_json(&report);
            }
            let s = &report.script;
            let what = match report.status {
                ImportStatus::Added => "Added",
                ImportStatus::Updated => "Updated",
                ImportStatus::Unchanged => "Unchanged:",
            };
            println!(
                "{what} script `{}` ({}): {} param(s), runs on {}.",
                s.name,
                short(&s.checksum),
                s.spec.params.len(),
                s.runs_on
            );
        }
        ScriptCmd::List => {
            let scripts = app.script_list().await?;
            if ctx.output == Output::Json {
                return print_json(&scripts);
            }
            if scripts.is_empty() {
                println!("No scripts yet. Add one with `ikit script add <file>`.");
                return Ok(());
            }
            let mut t = table(&[
                "SCRIPT",
                "RUNS ON",
                "INTERPRETER",
                "PARAMS",
                "CHECKSUM",
                "UPDATED",
                "DESCRIPTION",
            ]);
            for s in &scripts {
                let params: Vec<&str> = s.spec.params.iter().map(|p| p.name.as_str()).collect();
                t.add_row(vec![
                    s.name.clone(),
                    s.runs_on.to_string(),
                    s.interpreter.to_string(),
                    params.join(", "),
                    short(&s.checksum).to_owned(),
                    s.updated_at.format("%Y-%m-%d %H:%M").to_string(),
                    s.description.clone(),
                ]);
            }
            println!("{t}");
        }
        ScriptCmd::Show { name, body } => {
            let s = app.script_show(&name).await?;
            if ctx.output == Output::Json {
                return print_json(&s);
            }
            println!("Script {} ({})", s.name, short(&s.checksum));
            if !s.description.is_empty() {
                println!("  {}", s.description);
            }
            println!("  runs on:     {}", s.runs_on);
            println!("  interpreter: {}", s.interpreter);
            println!("  timeout:     {}s", s.spec.timeout_secs);
            if !s.spec.args.is_empty() {
                println!("  args:        {}", s.spec.args.join(" "));
            }
            if let Some(src) = &s.source {
                println!("  added from:  {src}");
            }
            println!(
                "  updated:     {}",
                s.updated_at.format("%Y-%m-%d %H:%M:%S")
            );
            if !s.spec.params.is_empty() {
                let mut t = table(&["PARAM", "FROM", "DEFAULT", "REQUIRED", "SECRET"]);
                for p in &s.spec.params {
                    let from = match (&p.from, &p.value) {
                        (Some(f), _) => f.to_string(),
                        (None, Some(v)) => format!("value: {v}"),
                        (None, None) => String::new(),
                    };
                    t.add_row(vec![
                        p.name.clone(),
                        from,
                        p.default.clone().unwrap_or_default(),
                        if p.required { "yes" } else { "no" }.to_owned(),
                        if p.secret { "yes" } else { "" }.to_owned(),
                    ]);
                }
                println!("{t}");
            }
            if let Some(csv) = &s.spec.csv {
                let group = csv
                    .services
                    .map(|g| format!(" of the {g} group"))
                    .unwrap_or_default();
                println!(
                    "Input: CSV `{}`, one row per service{group} with {}{}.",
                    csv.header,
                    csv.columns.join(", "),
                    if csv.arg {
                        "; its path is the last argument"
                    } else {
                        ""
                    }
                );
            }
            if body {
                println!("\n{}", s.body);
            }
        }
        ScriptCmd::Export { name } => {
            print!("{}", app.script_show(&name).await?.body);
        }
        ScriptCmd::Rm { name, yes } => {
            if ctx.confirm(&format!("Remove script {name} and its history?"), yes)? {
                app.script_remove(&name).await?;
                if ctx.output == Output::Table {
                    println!("Removed script {name}.");
                }
            }
        }
        ScriptCmd::Run {
            name,
            to,
            from,
            params,
            csv,
            timeout,
            dry_run,
            yes,
            args,
        } => {
            let workload = app.resolve_workload_name(to.as_deref().or(ctx.wl.as_deref()))?;
            let csv = match csv {
                Some(path) => Some(
                    std::fs::read_to_string(&path)
                        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?,
                ),
                None => None,
            };
            let mut req = ScriptRunRequest {
                script: name.clone(),
                workload,
                from,
                params: parse_params(&params)?,
                csv,
                args,
                timeout: timeout.map(Duration::from_secs),
                expected_checksum: None,
            };
            let preview = app.script_preview(&req).await?;
            if dry_run {
                if ctx.output == Output::Json {
                    return print_json(&preview);
                }
                print_preview(&preview);
                return Ok(());
            }
            if ctx.output == Output::Table {
                print_preview(&preview);
            }
            let question = format!(
                "Run {} on {} ({})?",
                preview.script, preview.workload, preview.env
            );
            if !ctx.confirm(&question, yes)? {
                return Ok(());
            }
            req.expected_checksum = Some(preview.checksum.clone());
            let report = app.script_run(&req, &super::deploy::actor()).await?;
            if ctx.output == Output::Json {
                print_json(&report)?;
            } else {
                print!("{}", report.stdout);
                eprint!("{}", report.stderr);
            }
            if !report.ok {
                let why = if report.timed_out {
                    "timed out".to_owned()
                } else {
                    match report.exit_code {
                        Some(c) => format!("exited with code {c}"),
                        None => "was killed".to_owned(),
                    }
                };
                bail!("script {} {why} (run {})", report.script, report.run_id);
            }
            if ctx.output == Output::Table {
                println!(
                    "Script {} finished on {} (run {}).",
                    report.script, report.workload, report.run_id
                );
            }
        }
        ScriptCmd::History { name, limit } => {
            let history = app
                .script_history(name.as_deref(), ctx.wl.as_deref(), limit)
                .await?;
            if ctx.output == Output::Json {
                return print_json(&history);
            }
            if history.is_empty() {
                println!("No scripts have run yet.");
                return Ok(());
            }
            let mut t = table(&[
                "SCRIPT", "WORKLOAD", "FROM", "HOOK", "RESULT", "EXIT", "RAN", "CHECKSUM", "ERROR",
            ]);
            for r in &history {
                t.add_row(vec![
                    r.script.clone(),
                    r.workload.clone(),
                    r.from_workload.clone().unwrap_or_default(),
                    r.hook.clone().unwrap_or_default(),
                    r.result.to_string(),
                    r.exit_code.map(|c| c.to_string()).unwrap_or_default(),
                    r.ran_at.format("%Y-%m-%d %H:%M:%S").to_string(),
                    short(&r.checksum).to_owned(),
                    r.error
                        .as_deref()
                        .unwrap_or("")
                        .lines()
                        .last()
                        .unwrap_or("")
                        .to_owned(),
                ]);
            }
            println!("{t}");
        }
    }
    Ok(())
}
