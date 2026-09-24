//! `favetto-tui` — the standalone ratatui client for the favetto daemon.
//!
//! Thin entry point: the implementation lives in the [`tui`] module. The daemon
//! binary (`favetto`) dispatches its `tui` subcommand here so this process links
//! only the client's dependency tree.

use clap::Parser;

mod cli;
mod client;
mod tui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "favetto_tui=info".into()),
        )
        .init();

    let cli = cli::Cli::parse();
    tui::run(cli.args).await
}
