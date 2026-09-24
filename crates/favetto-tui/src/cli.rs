//! Command-line arguments for the TUI client.
//!
//! [`TuiArgs`] is shared: the `favetto tui` subcommand embeds it (so the daemon
//! binary can dispatch to this crate) and the standalone `favetto-tui` binary
//! flattens it as its whole command line.

use std::path::PathBuf;

use clap::Args;

pub use favetto_core::paths::{default_config_path, default_data_dir, default_token_path};

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
