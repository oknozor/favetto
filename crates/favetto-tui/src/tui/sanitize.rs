//! Neutralize agent-authored text before it becomes a terminal cell.
//!
//! The ACP feed renders content with ratatui rather than through a terminal
//! emulator, but escape sequences still make a transcript confusing and any
//! future rendering path could pass them to a terminal. [`text`] removes
//! terminal control sequences, drops C0/C1 controls (except newline/tab), and
//! replaces bidirectional/invisible format characters with a visible `[U+XXXX]`
//! marker so a path or command cannot visually masquerade as another. [`line`]
//! additionally flattens newlines/tabs for fields that must stay on one row.

use std::fmt::Write as _;

/// Sanitize multi-line text. Newlines and tabs survive because the feed splits
/// text into lines before it builds ratatui spans.
pub fn text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            // C1 equivalents of CSI/OSC/DCS/SOS/PM/APC can appear in UTF-8
            // streams too; treat them like their ESC forms rather than leaving
            // their payload as ordinary text.
            '\u{009b}' => skip_csi(&mut chars),
            '\u{009d}' => skip_string(&mut chars),
            '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => skip_string(&mut chars),
            '\r' => {}
            '\n' | '\t' => out.push(c),
            c if is_invisible_format(c) => {
                // A visible marker is safer (and easier to audit) than silently
                // dropping a character that can reorder or hide a path.
                let _ = write!(out, "[U+{:04X}]", c as u32);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Sanitize a value that is rendered on a single row. Tabs/newlines become
/// spaces so they cannot move the cursor or break the row layout.
pub fn line(input: &str) -> String {
    text(input)
        .chars()
        .map(|c| match c {
            '\n' | '\t' => ' ',
            c => c,
        })
        .collect()
}

/// Skip one ESC-introduced sequence (CSI, OSC, DCS/SOS/PM/APC, or a short
/// designation escape) and leave the following text in place.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            skip_csi(chars);
        }
        Some(']') => {
            chars.next();
            skip_string(chars);
        }
        Some('P' | 'X' | '^' | '_') => {
            chars.next();
            skip_string(chars);
        }
        Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
            // ESC + intermediate byte(s) + final byte (e.g. `ESC ( B`).
            while let Some(&next) = chars.peek() {
                if ('\u{20}'..='\u{2f}').contains(&next) {
                    chars.next();
                } else {
                    chars.next();
                    break;
                }
            }
        }
        // A malformed sequence must not consume the line break; leave it for the
        // caller so one bad escape cannot merge two transcript lines.
        Some('\n' | '\r') | None => {}
        Some(_) => {
            chars.next();
        }
    }
}

/// Skip a CSI sequence up to and including its final byte. A malformed sequence
/// stops at a newline or the next ESC rather than swallowing the transcript.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if matches!(c, '\n' | '\r' | '\x1b') {
            return;
        }
        chars.next();
        if ('\u{40}'..='\u{7e}').contains(&c) {
            return;
        }
    }
}

/// Skip an OSC/DCS/SOS/PM/APC string terminated by BEL or ST (`ESC \`). A
/// malformed string stops at a newline or a non-ST ESC.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&c) = chars.peek() {
        if matches!(c, '\n' | '\r') {
            return;
        }
        chars.next();
        match c {
            '\x07' | '\u{009c}' => return,
            '\x1b' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                return;
            }
            _ => {}
        }
    }
}

/// Bidirectional overrides/isolates, invisible format characters, and Unicode
/// tag characters that can reorder or hide visible text. Emoji presentation
/// selectors and combining marks are intentionally left alone.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_osc_and_dcs_sequences() {
        let input = "\x1b[31mred\x1b[0m \x1b]0;title\x07ok \x1bPpayload\x1b\\done";
        assert_eq!(text(input), "red ok done");
    }

    #[test]
    fn strips_c1_equivalents() {
        assert_eq!(text("\u{009b}31mred\u{009b}0m"), "red");
        assert_eq!(text("\u{009d}0;title\u{009c}ok"), "ok");
    }

    #[test]
    fn terminal_hyperlinks_are_stripped_like_other_osc_sequences() {
        let input = "a\x1b]8;;https://example.com\x1b\\b\x1b]8;;\x1b\\c";
        assert_eq!(text(input), "abc");
        assert_eq!(
            text("a\x1b]8;;https://example.com\x07b\x1b]8;;\x07c"),
            "abc"
        );
    }

    #[test]
    fn malformed_sequences_do_not_swallow_the_next_line() {
        assert_eq!(text("bad \x1b[31\nnext"), "bad \nnext");
        assert_eq!(text("bad \x1b]0;title\nnext"), "bad \nnext");
        assert_eq!(text("bad \x1b\nnext"), "bad \nnext");
        assert_eq!(text("bad \x1b\r\nnext"), "bad \nnext");
    }

    #[test]
    fn preserves_newlines_and_tabs_but_line_flattens_them() {
        assert_eq!(text("a\nb\tc"), "a\nb\tc");
        assert_eq!(line("a\nb\tc"), "a b c");
    }

    #[test]
    fn bidi_and_zero_width_characters_become_visible() {
        let out = text("safe\u{202e}txet\u{2066}\u{200b}end");
        assert!(!out.contains('\u{202e}'));
        assert!(!out.contains('\u{2066}'));
        assert!(!out.contains('\u{200b}'));
        assert!(out.contains("[U+202E]"));
        assert!(out.contains("[U+2066]"));
        assert!(out.contains("[U+200B]"));
    }

    #[test]
    fn unicode_tag_characters_cannot_hide_path_text() {
        let out = text("report\u{e002e}\u{e0065}\u{e0078}\u{e0065}.pdf");
        assert!(!out.contains('\u{e002e}'));
        assert!(!out.contains('\u{e0065}'));
        assert!(out.contains("[U+E002E][U+E0065][U+E0078][U+E0065]"));
        assert!(out.ends_with(".pdf"));
    }

    #[test]
    fn ordinary_markdown_is_untouched() {
        let source = "# Heading\n\n- [docs](https://example.com)\n\n```rust\nfn main() {}\n```";
        assert_eq!(text(source), source);
    }
}
