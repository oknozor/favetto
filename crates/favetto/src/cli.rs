//! Command-line interface for the `favetto` binary.
//!
//! Single binary, many subcommands. Settings resolve from CLI flags first, then the
//! global config (`~/.config/favetto/config.toml`), then built-in defaults.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// favetto — LLM-driven agent orchestrator (daemon, remote TUI, MCP tooling).
#[derive(Parser)]
#[command(name = "favetto", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the favetto daemon (event bus, API server, synthetic driver).
    Daemon(DaemonArgs),
    /// Attach the TUI to a running daemon (local Unix socket or remote WebSocket).
    Tui(TuiArgs),
    /// Run a single skill as a one-off task.
    TaskRun(TaskRunArgs),
    /// Expose favetto itself as an MCP server (M6+).
    McpServe(McpServeArgs),
    /// Print a short-lived pairing code for remote TUI attachment (M6+).
    Pair(PairArgs),
    /// Rotate the bearer token.
    TokenRotate(TokenRotateArgs),
}

#[derive(Args)]
pub struct DaemonArgs {
    /// Path to the config file (default `~/.config/favetto/config.toml`).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Path to the Unix domain socket for local TUI attach.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// TCP listen address for the WebSocket API (loopback by default).
    #[arg(long)]
    pub listen: Option<String>,
    /// Directory for SQLite + token (default `~/.local/share/favetto`).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Disable the synthetic event driver (useful for tests).
    #[arg(long)]
    pub no_synthetic: bool,
    /// Path to `hooks.toml`.
    #[arg(long)]
    pub hooks: Option<PathBuf>,
    /// Directory containing skill folders (for chat context).
    #[arg(long)]
    pub skills_dir: Option<PathBuf>,
}

#[derive(Args)]
pub struct TuiArgs {
    /// Remote WebSocket URL (`ws://...`). If unset, attach to the local Unix socket.
    #[arg(long)]
    pub remote: Option<String>,
    /// Bearer token file for remote auth (defaults to `<data_dir>/token`).
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Unix socket path (overrides the default).
    #[arg(long)]
    pub socket: Option<PathBuf>,
}

#[derive(Args)]
pub struct TaskRunArgs {
    /// Skill to run.
    pub skill: String,
    /// Path to the config file (default `~/.config/favetto/config.toml`).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Directory containing skill folders.
    #[arg(long, default_value = "skills")]
    pub skills_dir: PathBuf,
    /// JSON input passed to the skill.
    #[arg(long)]
    pub input: Option<String>,
}

#[derive(Args)]
pub struct McpServeArgs {
    /// TCP listen address for the MCP server.
    #[arg(long, default_value = "127.0.0.1:7879")]
    pub listen: String,
}

#[derive(Args)]
pub struct PairArgs {}

#[derive(Args)]
pub struct TokenRotateArgs {
    /// Directory for the token file.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

/// Default data directory (SQLite + token): `$FAVETTO_DATA_DIR`, else the XDG data
/// dir (`~/.local/share/favetto`).
pub fn default_data_dir() -> PathBuf {
    std::env::var("FAVETTO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("favetto"))
}

/// Default config file: `$FAVETTO_CONFIG`, else `~/.config/favetto/config.toml`.
pub fn default_config_path() -> PathBuf {
    std::env::var("FAVETTO_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("favetto")
                .join("config.toml")
        })
}

pub fn default_token_path() -> PathBuf {
    default_data_dir().join("token")
}
