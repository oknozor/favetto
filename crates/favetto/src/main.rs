//! `favetto` — LLM-driven agent orchestrator (daemon, remote TUI, MCP tooling).

mod chat;
mod cli;
mod client;
mod config;
mod daemon;
mod db;
mod event_bus;
mod executor;
mod hooks;
mod mcp;
mod mcp_client;
mod mcp_serve;
mod metrics;
mod notify;
mod pair;
mod runtime;
mod scheduler;
mod server;
mod state;
mod tasks;
mod tool_registry;
mod transport;
mod tui;
mod webhooks;

use clap::Parser;

use crate::cli::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "favetto=info".into()),
        )
        .init();

    let cli = cli::Cli::parse();

    match cli.command {
        Command::Daemon(args) => daemon::run(args).await,
        Command::Tui(args) => tui::run(args).await,
        Command::McpServe(args) => mcp_serve::run(args).await,
        Command::Pair(args) => pair(args).await,
        Command::TokenRotate(args) => token_rotate(args),
        Command::InternalMcp { server, args } => internal_mcp(server, args).await,
    }
}

/// Run a built-in MCP server in-process (self-invocation from the daemon).
async fn internal_mcp(server: String, args: Vec<String>) -> anyhow::Result<()> {
    match server.as_str() {
        "mcp-filesystem" => mcp_filesystem::serve(args).await,
        "mcp-linear" => mcp_linear::serve(args).await,
        "mcp-github" => mcp_github::serve(args).await,
        "mcp-gmail" => mcp_gmail::serve(args).await,
        other => anyhow::bail!("unknown built-in MCP server: {other}"),
    }
}

/// `favetto pair`: ask the daemon for a short-lived pairing code and print it.
async fn pair(args: cli::PairArgs) -> anyhow::Result<()> {
    pair::run(args).await
}

fn token_rotate(args: cli::TokenRotateArgs) -> anyhow::Result<()> {
    let data_dir = args.data_dir.unwrap_or_else(cli::default_data_dir);
    let path = data_dir.join("token");
    let token = favetto_core::auth::Token::generate();

    std::fs::create_dir_all(&data_dir)?;
    std::fs::write(&path, format!("{}\n", token.as_str()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("new token written to {}", path.display());
    Ok(())
}
