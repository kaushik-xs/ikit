use crate::Ctx;
use crate::output::{Output, print_json, table};

pub async fn run(ctx: &Ctx) -> anyhow::Result<()> {
    let report = ikit_app::init(&ctx.paths, &ikit_app::KeyringStore).await?;
    if ctx.output == Output::Json {
        return print_json(&report);
    }

    println!("ikit home: {}", report.home.display());
    if let Some(m) = &report.migrated_legacy {
        println!(
            "Moved the existing state into org `{}` ({}). Workload names are unchanged.",
            m.org,
            m.moved.join(", ")
        );
    }
    if let Some(n) = report.legacy_secrets_renamed {
        println!("Renamed {n} keychain entries into the `default` org.");
    }
    if report.orgs.is_empty() {
        println!("No orgs yet. Create one with `ikit org create <name>`.");
        return Ok(());
    }
    let mut t = table(&["ORG", "MIGRATIONS", "REGISTRY", "BACKUP"]);
    for o in &report.orgs {
        let migrations = if o.migrations_applied.is_empty() {
            "up to date".to_owned()
        } else {
            let v: Vec<String> = o
                .migrations_applied
                .iter()
                .map(|v| format!("{v:04}"))
                .collect();
            format!("applied {}", v.join(", "))
        };
        let registry = if o.seeded.is_empty() {
            "up to date".to_owned()
        } else {
            format!("seeded {} built-in versions", o.seeded.len())
        };
        let backup = o
            .backup
            .as_ref()
            .map(|b| b.display().to_string())
            .unwrap_or_default();
        t.add_row(vec![o.name.clone(), migrations, registry, backup]);
    }
    println!("{t}");
    Ok(())
}
