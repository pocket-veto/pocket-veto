//! `pocket-veto` binary entry point.
//!
//! A thin wrapper: parse the CLI, initialize tracing once, and dispatch to
//! the matched subcommand's [`Subcommand::run`] impl. No `process::exit`,
//! no hand-rolled `match` over exit codes, no `eprintln` — infrastructure
//! errors flow through `tracing::error!` and the runtime converts the
//! returned [`ExitCode`] via [`Termination`].

use std::process::ExitCode;

use clap::Parser;
use pocket_veto::{Cli, Ctx, Subcommand, init_tracing};

#[tokio::main]
async fn main() -> Result<(), ExitCode> {
    let command = Cli::parse().command;
    init_tracing(command.default_log_filter());
    let ctx = Ctx;
    match command.run(&ctx).await {
        Ok(code) => Err(code),
        Err(e) => {
            tracing::error!(error = %e, "pocket-veto failed");
            Err(ExitCode::FAILURE)
        }
    }
}
