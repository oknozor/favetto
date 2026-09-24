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

/// Default paths (data dir, config file, token file), re-exported from
/// `favetto-core` so the daemon and the TUI client resolve them identically.
pub use favetto_core::paths::{default_config_path, default_data_dir, default_token_path};

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The long flags the `tui` subcommand forwards to the standalone
    /// `favetto-tui` binary. `crates/favetto-tui/src/cli.rs` pins the same list;
    /// a flag added on one side without the other breaks `favetto tui …`.
    const TUI_LONG_FLAGS: &[&str] = &[
        "config",
        "no-sound",
        "pair-code",
        "remote",
        "socket",
        "sound",
        "sound-command",
        "test-sound",
        "token-file",
    ];

    fn long_flags(cmd: &clap::Command) -> Vec<String> {
        let mut longs: Vec<String> = cmd
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_string))
            .filter(|long| long != "help" && long != "version")
            .collect();
        longs.sort();
        longs
    }

    #[test]
    fn tui_subcommand_forwards_the_expected_flags() {
        let cmd = Cli::command();
        let tui = cmd
            .get_subcommands()
            .find(|sub| sub.get_name() == "tui")
            .expect("tui subcommand");
        assert_eq!(long_flags(tui), TUI_LONG_FLAGS);
    }
}
