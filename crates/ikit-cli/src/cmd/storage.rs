use std::path::PathBuf;

use clap::Subcommand;
use ikit_app::NewStorageRequest;
use ikit_core::{Storage, StorageOptions, StorageProvider};

use crate::Ctx;
use crate::output::{Output, print_json, table};

#[derive(Debug, Subcommand)]
pub enum StorageCmd {
    /// Add a blob storage location (for backups).
    Add {
        name: String,
        #[arg(long)]
        provider: StorageProvider,
        /// Bucket (S3, GCS) or container (Azure); any label for local storage.
        #[arg(long, default_value = "local")]
        bucket: String,
        /// Key prefix inside the bucket.
        #[arg(long, default_value = "")]
        prefix: String,
        /// Profile with credentials: aws for S3, azure, gcp. Omit to use the environment.
        #[arg(long)]
        profile: Option<String>,
        /// S3-compatible endpoint, e.g. http://localhost:9000 for MinIO.
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        region: Option<String>,
        /// Azure storage account.
        #[arg(long)]
        account: Option<String>,
        /// Local: folder to store into.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    #[command(alias = "ls")]
    List,
    Show {
        name: String,
    },
    /// Write, read back and delete a probe object.
    Test {
        name: String,
    },
    /// List objects under the storage's prefix.
    Objects {
        name: String,
        prefix: Option<String>,
    },
    /// Remove a storage from ikit (refused while backups are catalogued in it).
    #[command(alias = "remove")]
    Rm {
        name: String,
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: StorageCmd) -> anyhow::Result<()> {
    let app = ctx.app().await?;
    match cmd {
        StorageCmd::Add {
            name,
            provider,
            bucket,
            prefix,
            profile,
            endpoint,
            region,
            account,
            root,
        } => {
            let options = StorageOptions {
                endpoint,
                region,
                account,
                root,
            };
            let s = app
                .storage_add(NewStorageRequest {
                    name,
                    provider,
                    bucket,
                    prefix,
                    profile,
                    options,
                })
                .await?;
            if ctx.output == Output::Json {
                return print_json(&s);
            }
            println!(
                "Added storage `{}` ({}). Check it with `ikit storage test {}`.",
                s.name,
                s.describe(),
                s.name
            );
        }
        StorageCmd::List => {
            let all = app.storage_list().await?;
            if ctx.output == Output::Json {
                return print_json(&all);
            }
            if all.is_empty() {
                println!(
                    "No storage yet. Add one with `ikit storage add <name> --provider s3|azure|gcs|local ...`."
                );
                return Ok(());
            }
            let mut t = table(&["NAME", "PROVIDER", "LOCATION", "PROFILE"]);
            for s in &all {
                t.add_row(vec![
                    s.name.clone(),
                    s.provider.to_string(),
                    s.describe(),
                    s.profile.clone().unwrap_or_default(),
                ]);
            }
            println!("{t}");
        }
        StorageCmd::Show { name } => render(ctx, &app.storage_show(&name).await?)?,
        StorageCmd::Test { name } => {
            let detail = app.storage_test(&name).await?;
            if ctx.output == Output::Json {
                return print_json(
                    &serde_json::json!({ "storage": name, "ok": true, "detail": detail }),
                );
            }
            println!("ok: {detail}");
        }
        StorageCmd::Objects { name, prefix } => {
            let objects = app.storage_ls(&name, prefix.as_deref()).await?;
            if ctx.output == Output::Json {
                return print_json(&objects);
            }
            let mut t = table(&["KEY", "SIZE"]);
            for o in &objects {
                t.add_row(vec![o.key.clone(), o.size.to_string()]);
            }
            println!("{t}");
        }
        StorageCmd::Rm { name, yes } => {
            if ctx.confirm(
                &format!("Remove storage {name} from ikit (its objects are not deleted)?"),
                yes,
            )? {
                app.storage_remove(&name).await?;
                if ctx.output == Output::Table {
                    println!("Removed storage {name}.");
                }
            }
        }
    }
    Ok(())
}

fn render(ctx: &Ctx, s: &Storage) -> anyhow::Result<()> {
    if ctx.output == Output::Json {
        return print_json(s);
    }
    let mut t = table(&[]);
    t.add_row(vec!["name".to_owned(), s.name.clone()]);
    t.add_row(vec!["location".to_owned(), s.describe()]);
    t.add_row(vec![
        "profile".to_owned(),
        s.profile.clone().unwrap_or_else(|| "(environment)".into()),
    ]);
    if let Some(e) = &s.options.endpoint {
        t.add_row(vec!["endpoint".to_owned(), e.clone()]);
    }
    println!("{t}");
    Ok(())
}
