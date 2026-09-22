//! Command-line interface for the `favetto` binary.
//!
//! Single binary, many subcommands. Settings resolve from CLI flags first, then the
//! global config (`~/.config/favetto/config.toml`), then built-in defaults.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
    /// Short-lived pairing code to exchange for a token (remote attach only).
    #[arg(long)]
    pub pair_code: Option<String>,
    /// Path to the client-local config file (default `~/.config/favetto/config.toml`).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Force sound on (overrides config and env).
    #[arg(long, overrides_with = "no_sound")]
    pub sound: bool,
    /// Disable sound (overrides config and env).
    #[arg(long = "no-sound")]
    pub no_sound: bool,
    /// Custom player command; `{file}` is the sound path (implies player = "command").
    #[arg(long = "sound-command")]
    pub sound_command: Option<String>,
    /// Play every configured cue once, print the resolved player, and exit.
    #[arg(long = "test-sound")]
    pub test_sound: bool,
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

/// Default data directory (SQLite + token): `$FAVETTO_DATA_DIR`, else the XDG data
/// dir (`~/.local/share/favetto`).
pub fn default_data_dir() -> PathBuf {
    std::env::var("FAVETTO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("favetto")
        })
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
