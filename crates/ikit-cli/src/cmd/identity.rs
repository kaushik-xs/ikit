use clap::Subcommand;
use ikit_app::KeyringStore;
use serde::Serialize;

use crate::Ctx;
use crate::output::{Output, print_json};

#[derive(Debug, Subcommand)]
pub enum IdentityCmd {
    /// Create this machine's key pair; the private key stays in the keychain. Share the
    /// printed public key so a teammate can grant it an org's secrets.
    Create {
        /// Replace the existing key. Orgs granted the old key must grant the new one.
        #[arg(long)]
        replace: bool,
        /// Print a new key pair without storing it: for a recovery key kept offline, or a CI
        /// key (set its private key as IKIT_AGE_IDENTITY).
        #[arg(long, conflicts_with = "replace")]
        print_only: bool,
        /// Skip the confirmation prompt for --replace.
        #[arg(long, short)]
        yes: bool,
    },
    /// Print this machine's public key.
    Show,
}

#[derive(Serialize)]
struct KeyPair {
    public: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    private: Option<String>,
}

pub async fn run(ctx: &Ctx, cmd: IdentityCmd) -> anyhow::Result<()> {
    match cmd {
        IdentityCmd::Create {
            replace,
            print_only,
            yes,
        } => {
            if print_only {
                let (private, public) = ikit_app::identity_generate();
                if ctx.output == Output::Json {
                    return print_json(&KeyPair {
                        public,
                        private: Some(private),
                    });
                }
                println!("public key:  {public}");
                println!("private key: {private}");
                println!(
                    "This key is not stored anywhere. Keep the private key offline (or in your CI's \
                     secret store as IKIT_AGE_IDENTITY) and grant the public key."
                );
                return Ok(());
            }
            if replace {
                let question = "Replace this machine's key? Orgs that granted the old key will \
                                need to grant the new one.";
                if !ctx.confirm(question, yes)? {
                    return Ok(());
                }
            }
            let public = ikit_app::identity_create(&KeyringStore, replace)?;
            if ctx.output == Output::Json {
                return print_json(&KeyPair {
                    public,
                    private: None,
                });
            }
            println!("{public}");
            eprintln!(
                "Created this machine's key (private half in the keychain). Send the public key \
                 above to a teammate to run `ikit org secrets grant <key> --as <you>`."
            );
        }
        IdentityCmd::Show => {
            let Some(public) = ikit_app::identity_public(&ctx.identity)? else {
                anyhow::bail!(
                    "this machine has no identity yet; create one with `ikit identity create`"
                );
            };
            if ctx.output == Output::Json {
                return print_json(&KeyPair {
                    public,
                    private: None,
                });
            }
            println!("{public}");
        }
    }
    Ok(())
}
