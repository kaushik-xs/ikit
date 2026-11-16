//! `ikit tui`: the terminal UI. It drives this same executable, so every command keeps
//! working exactly as it does when typed.

use std::io::IsTerminal;

use crate::Ctx;

pub async fn run(ctx: &Ctx) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("`ikit tui` needs an interactive terminal");
    }
    let opts = ikit_tui::Options {
        exe: std::env::current_exe()?,
        base_args: ctx.forward.clone(),
        org: ctx.org.clone(),
        wl: ctx.wl.clone(),
    };
    tokio::task::block_in_place(|| ikit_tui::run(opts))
}
