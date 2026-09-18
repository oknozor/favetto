//! `favetto` — LLM-driven agent orchestrator (daemon, remote TUI, MCP tooling).

mod chat;
mod cli;
mod config;
mod daemon;
mod db;
mod event_bus;
mod hooks;
mod mcp_client;
mod runtime;
mod server;
mod skills;
mod state;
mod synthetic;
mod tool_registry;
mod transport;
mod tui;
mod webhooks;

use std::collections::BTreeMap;

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

    // Connect global MCP servers (from config) plus the skill's own, with the skill
    // overriding a global server of the same name.
    let servers = merge_servers(config.mcp_servers()?, skill.mcp_servers()?);
    let mut sessions = Vec::new();
    for cfg in &servers {
        tracing::info!(server = %cfg.name, "connecting MCP server");
        let session = mcp_client::McpSession::connect(cfg).await?;
        tracing::info!(
            server = %cfg.name,
            tools = session.tools.len(),
            "MCP server connected"
        );
        sessions.push(session);
    }

    let registry = tool_registry::ToolRegistry::new(sessions);
    let backend = runtime::build_backend(&skill, &config)?;
    let runtime = runtime::Runtime::new(registry, backend);

    let input: serde_json::Value = args
        .input
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(serde_json::json!({}));

    tracing::info!(skill = %skill.name, model = %skill.config.agent.model, "running skill");
    let output = runtime.run(&skill, input).await?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// Merge global and per-skill MCP servers; the skill's own servers win on name clash.
fn merge_servers(
    global: Vec<skills::McpServerConfig>,
    skill: Vec<skills::McpServerConfig>,
) -> Vec<skills::McpServerConfig> {
    let mut map: BTreeMap<String, skills::McpServerConfig> = global
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();
    for s in skill {
        map.insert(s.name.clone(), s);
    }
    map.into_values().collect()
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
