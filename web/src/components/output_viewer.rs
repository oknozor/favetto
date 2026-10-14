//! Scrollable monospace output viewer.
//!
//! Task output is stored as a JSON envelope (`{ output, truncated, … }`, see
//! `executor::build_task_output`). This module turns that value into plain text
//! (stripping terminal escape sequences), caps it to a readable preview, and
//! renders it in a bounded, scrollable `<pre>` pane with a "Show more" toggle.

use dioxus::prelude::*;

/// Number of lines shown before the "Show more" expander is offered.
pub const COLLAPSED_LINES: usize = 200;

/// Extract the human-readable text and its `truncated` flag from a task output
/// value.
///
/// - `{ "output": "…", "truncated": bool }` (the executor envelope) → the text;
/// - a bare JSON string → the string itself;
/// - anything else → pretty-printed JSON.
pub fn extract_output_text(value: &serde_json::Value) -> (String, bool) {
    match value {
        serde_json::Value::String(text) => (text.clone(), false),
        serde_json::Value::Object(map) => match map.get("output") {
            Some(serde_json::Value::String(text)) => {
                let truncated = map
                    .get("truncated")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                (text.clone(), truncated)
            }
            _ => (pretty(value), false),
        },
        _ => (pretty(value), false),
    }
}

/// Pretty-print a JSON value, degrading to an empty string on failure.
pub fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Remove ANSI/VT100 escape sequences so the pane shows plain text.
///
/// Handles CSI/SGR sequences (`ESC [ … final`) and OSC sequences
/// (`ESC ] … BEL|ST`) plus lone two-byte escapes. This is the plain-text
/// fallback the issue allows, not a colour renderer.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // CSI: ESC [ params inter final
            Some('[') => {
                chars.next();
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … terminated by BEL or ST (ESC \)
            Some(']') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '\u{07}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // Lone escape: drop the escape and its single following byte.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Keep at most `max` lines of `text`, returning the preview and whether the
/// text was cut. A text that fits is returned unchanged.
pub fn truncate_lines(text: &str, max: usize) -> (String, bool) {
    let mut lines = text.lines();
    let mut kept: Vec<&str> = Vec::new();
    for _ in 0..max {
        match lines.next() {
            Some(line) => kept.push(line),
            None => return (text.to_string(), false),
        }
    }
    if lines.next().is_some() {
        (kept.join("\n"), true)
    } else {
        (text.to_string(), false)
    }
}

/// A scrollable, monospace output pane with a collapsed preview.
#[component]
pub fn OutputViewer(text: String, truncated: bool) -> Element {
    let mut expanded = use_signal(|| false);
    let clean = strip_ansi(&text);
    let (preview, over_cap) = truncate_lines(&clean, COLLAPSED_LINES);
    let expandable = truncated || over_cap;
    let shown = if expanded() { clean } else { preview };

    rsx! {
        div { class: "output-viewer",
            pre { class: "output-pane", "{shown}" }
            if expandable {
                if expanded() {
                    button {
                        class: "output-toggle",
                        onclick: move |_| expanded.set(false),
                        "Show less"
                    }
                } else {
                    button {
                        class: "output-toggle",
                        onclick: move |_| expanded.set(true),
                        "Show more"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_executor_envelope() {
        let value = serde_json::json!({
            "agent": "opencode",
            "truncated": true,
            "output": "hello\nworld",
        });
        let (text, truncated) = extract_output_text(&value);
        assert_eq!(text, "hello\nworld");
        assert!(truncated);
    }

    #[test]
    fn envelope_without_truncated_flag_is_not_truncated() {
        let value = serde_json::json!({ "output": "done" });
        assert_eq!(extract_output_text(&value), ("done".to_string(), false));
    }

    #[test]
    fn bare_string_and_arbitrary_object_fall_back_gracefully() {
        let (text, truncated) = extract_output_text(&serde_json::json!("plain"));
        assert_eq!(text, "plain");
        assert!(!truncated);

        let (text, _) = extract_output_text(&serde_json::json!({ "status": "ok" }));
        assert!(text.contains("\"status\""), "pretty JSON: {text}");
    }

    #[test]
    fn strips_sgr_colour_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("a\u{1b}[1;32mb\u{1b}[m c"), "ab c");
    }

    #[test]
    fn strips_cursor_movement_and_osc() {
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1Ghello"), "hello");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}body"), "body");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{1b}\\body"), "body");
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        let text = "line one\nline two\twith tab";
        assert_eq!(strip_ansi(text), text);
    }

    #[test]
    fn truncate_lines_caps_only_when_over_the_limit() {
        let (short, cut) = truncate_lines("a\nb\nc", 5);
        assert_eq!(short, "a\nb\nc");
        assert!(!cut);

        let (capped, cut) = truncate_lines("a\nb\nc\nd", 2);
        assert_eq!(capped, "a\nb");
        assert!(cut);
    }

    #[test]
    fn output_viewer_mounts_host_side() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                OutputViewer { text: "hello", truncated: false }
            }
        });
        dom.rebuild_in_place();
    }
}
