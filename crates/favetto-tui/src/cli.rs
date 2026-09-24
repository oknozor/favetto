//! Command-line interface for the `favetto-tui` binary.
//!
//! The flags mirror the `tui` subcommand of the `favetto` launcher (see
//! `crates/favetto/src/cli.rs`). Keep the two definitions in sync: the launcher
//! forwards its argv verbatim, so a flag added here must be added there too (and
//! vice versa) for `favetto tui …` to accept it.

use std::path::PathBuf;

use clap::{Args, Parser};

/// favetto-tui — ratatui client for the favetto daemon.
#[derive(Parser)]
#[command(name = "favetto-tui", version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub args: TuiArgs,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The long flags this binary accepts. It must match the `tui` subcommand in
    /// `crates/favetto/src/cli.rs`: the launcher forwards its argv verbatim.
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

    #[test]
    fn accepts_the_flags_the_launcher_forwards() {
        let cmd = Cli::command();
        let mut longs: Vec<String> = cmd
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_string))
            .filter(|long| long != "help" && long != "version")
            .collect();
        longs.sort();
        assert_eq!(longs, TUI_LONG_FLAGS);
    }

    #[test]
    fn parses_a_full_remote_invocation() {
        let cli = Cli::parse_from([
            "favetto-tui",
            "--remote",
            "ws://h:1",
            "--token-file",
            "/tmp/token",
            "--socket",
            "/tmp/sock",
            "--pair-code",
            "123456",
            "--config",
            "/tmp/config.toml",
            "--no-sound",
            "--sound-command",
            "paplay {file}",
            "--test-sound",
        ]);
        assert_eq!(cli.args.remote.as_deref(), Some("ws://h:1"));
        assert_eq!(
            cli.args.token_file.as_deref(),
            Some(std::path::Path::new("/tmp/token"))
        );
        assert_eq!(cli.args.pair_code.as_deref(), Some("123456"));
        assert!(cli.args.no_sound);
        assert!(cli.args.test_sound);
        assert_eq!(cli.args.sound_command.as_deref(), Some("paplay {file}"));
    }
}
