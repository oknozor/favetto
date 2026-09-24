//! Command-line interface for the `favetto` binary.
//!
//! Single binary, many subcommands. Settings resolve from CLI flags first, then the
//! global config (`~/.config/favetto/config.toml`), then built-in defaults.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

pub use favetto_core::paths::{default_config_path, default_data_dir, default_token_path};
pub use favetto_tui::TuiArgs;

/// favetto — LLM-driven agent orchestrator (daemon + remote TUI).
#[derive(Parser)]
#[command(name = "favetto", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the favetto daemon (event bus, API server, scheduler, executor).
    Daemon(DaemonArgs),
    /// Attach the TUI to a running daemon (local Unix socket or remote WebSocket).
    Tui(TuiArgs),
    /// Print a short-lived pairing code for remote TUI attachment.
    Pair(PairArgs),
    /// Rotate the bearer token.
    TokenRotate(TokenRotateArgs),
    /// Internal: regenerate the generated documentation reference.
    #[command(name = "__doc", hide = true)]
    Doc(DocArgs),
    /// Internal: exec an agent process with a parent-death signal (spawned by the daemon).
    #[command(name = "__agent-exec", hide = true)]
    InternalAgentExec {
        /// The agent command and its arguments (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
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
    /// Directory of task-definition `.md` files (the task catalog).
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    /// Serve the web client from this directory instead of the embedded assets.
    #[arg(long)]
    pub web_dir: Option<PathBuf>,
}

/// Hidden generator: regenerate `docs/reference/{config,cli,events,remote-api}.md`
/// and `docs/public/favetto-schema.json` from the source of truth.
#[derive(Args)]
pub struct DocArgs {
    /// Output docs directory (defaults to `<repo>/docs`).
    #[arg(long)]
    pub docs_dir: Option<PathBuf>,
}

#[derive(Args)]
pub struct PairArgs {
    /// Daemon HTTP base URL (e.g. http://127.0.0.1:7878).
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    pub url: String,
}

#[derive(Args)]
pub struct TokenRotateArgs {
    /// Directory for the token file.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}
