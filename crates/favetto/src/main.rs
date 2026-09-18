//! `favetto` — LLM-driven agent orchestrator (daemon, remote TUI, MCP tooling).

mod chat;
mod cli;
mod config;
mod daemon;
mod db;
mod event_bus;
mod executor;
mod hooks;
mod mcp_client;
mod notify;
mod runtime;
mod scheduler;
mod server;
mod skills;
mod state;
mod synthetic;
mod tool_registry;
mod transport;
mod tui;
mod webhooks;

use anyhow::Context;
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
        Command::TaskRun(args) => task_run(args).await,
        Command::McpServe(args) => mcp_serve(args),
        Command::Pair(args) => pair(args),
        Command::TokenRotate(args) => token_rotate(args),
    }
}

/// Run one skill end-to-end through the MCP tool registry and the agent runtime.
async fn task_run(args: cli::TaskRunArgs) -> anyhow::Result<()> {
    // Load config (providers + global MCP servers).
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(cli::default_config_path);
    let config = config::FavettoConfig::load_from(&config_path).unwrap_or_default();

    let skills = skills::load_skills(&args.skills_dir)?;
    let skill = skills
        .into_iter()
        .find(|s| s.name == args.skill)
        .with_context(|| {
            format!(
                "skill '{}' not found in {}",
                args.skill,
                args.skills_dir.display()
            )
        })?;

    let input: serde_json::Value = args
        .input
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(serde_json::json!({}));

    tracing::info!(skill = %skill.name, model = %skill.config.agent.model, "running skill");
    let output = runtime::run_skill(&skill, &config, input).await?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn mcp_serve(args: cli::McpServeArgs) -> anyhow::Result<()> {
    println!(
        "`mcp serve` arrives in M6 (favetto-as-MCP-server). Would listen on {}.",
        args.listen
    );
    Ok(())
}

fn pair(_args: cli::PairArgs) -> anyhow::Result<()> {
    let code = format!("{:06}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 1_000_000)
        .unwrap_or(0));
    println!("pairing code (valid 60s): {code}");
    println!("note: the full pairing exchange lands in M6; use --token-file for now.");
    Ok(())
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
