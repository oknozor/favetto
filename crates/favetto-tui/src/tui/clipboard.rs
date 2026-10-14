//! Clipboard integration: an OSC 52 terminal escape (works over SSH/tmux/
//! containers) plus a best-effort local clipboard tool. No new crate.

use std::io::Write;
use std::process::{Command, Stdio};

use base64::Engine as _;

/// Emit an OSC 52 set-clipboard sequence and flush.
pub fn write_osc52(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

/// Copy via the first available local clipboard tool. Returns true on success.
pub fn write_local(text: &str) -> bool {
    local_candidates()
        .iter()
        .any(|(program, args)| run_tool(program, args, text))
}

fn local_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[(&str, &[&str])] = &[("pbcopy", &[])];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[(&str, &[&str])] = &[("clip", &[])];
    #[cfg(all(unix, not(target_os = "macos")))]
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    CANDIDATES
}

fn run_tool(program: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    if !wrote {
        let _ = child.kill();
        return false;
    }
    matches!(child.wait(), Ok(status) if status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_base64_encodes_the_payload() {
        let mut out = Vec::new();
        write_osc52(&mut out, "hello").unwrap();
        assert_eq!(out, b"\x1b]52;c;aGVsbG8=\x07");
    }
}
