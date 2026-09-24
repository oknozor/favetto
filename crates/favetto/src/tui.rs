//! `favetto tui`: a thin dispatcher that execs the standalone `favetto-tui`
//! binary.
//!
//! The TUI client lives in its own crate (`crates/favetto-tui`) so it links only
//! its own dependency tree instead of the daemon's. This module keeps the
//! documented `favetto tui …` invocation working: it locates the sibling binary,
//! forwards the original flags verbatim, and (on Unix) replaces this process with
//! it.

use std::ffi::OsString;
use std::path::PathBuf;

/// Locate the `favetto-tui` executable.
///
/// `FAVETTO_TUI_BIN` wins, then the sibling of the running `favetto` binary
/// (`cargo install` and `cargo build` place both in the same directory), then a
/// bare name resolved on `PATH`.
fn tui_program() -> PathBuf {
    if let Some(path) = std::env::var_os("FAVETTO_TUI_BIN") {
        return PathBuf::from(path);
    }
    let name = format!("favetto-tui{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name(&name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(name)
}

/// The arguments after the `tui` subcommand verbatim.
fn forwarded_args(raw: &[OsString]) -> &[OsString] {
    match raw.iter().position(|a| a.to_str() == Some("tui")) {
        Some(pos) => &raw[pos + 1..],
        None => &[],
    }
}

/// Forward the original invocation to the `favetto-tui` binary.
pub fn run(raw: &[OsString]) -> anyhow::Result<()> {
    let program = tui_program();
    // The flags are forwarded verbatim rather than re-encoded, so the standalone
    // client parses exactly what the user typed.
    let forwarded = forwarded_args(raw);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&program).args(forwarded).exec();
        Err(anyhow::anyhow!(
            "failed to launch {}: {err}",
            program.display()
        ))
    }

    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&program)
            .args(forwarded)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to launch {}: {e}", program.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn forwards_every_flag_after_the_subcommand() {
        let raw = argv(&["favetto", "tui", "--remote", "ws://h:1", "--no-sound"]);
        assert_eq!(
            forwarded_args(&raw),
            argv(&["--remote", "ws://h:1", "--no-sound"]).as_slice()
        );
    }

    #[test]
    fn forwards_nothing_when_there_are_no_flags() {
        assert!(forwarded_args(&argv(&["favetto", "tui"])).is_empty());
    }

    #[test]
    fn missing_subcommand_verb_is_empty() {
        assert!(forwarded_args(&argv(&["favetto", "daemon"])).is_empty());
    }
}
