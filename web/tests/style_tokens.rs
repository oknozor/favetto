//! Static guards for the token layer in `web/assets/style.css` (issue #278).
//!
//! These tests do not parse CSS; they assert structural properties that keep
//! the stylesheet a single token-driven design layer: every colour literal
//! lives in a custom-property declaration, the de-duplicated component shells
//! have exactly one base rule, every task/run status word has a badge rule,
//! and the reduced-motion media query is honoured.

const CSS: &str = include_str!("../assets/style.css");

/// Blank out `/* … */` comment contents while preserving newlines, so comment
/// text mentioning colours can never trip the literal-colour scan.
fn without_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut in_comment = false;
    let mut chars = css.chars().peekable();
    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_comment = false;
            } else if c == '\n' {
                out.push('\n');
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_comment = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// True when the line contains a colour literal: `rgb(`/`rgba(`/`hsl(`/`hsla(`
/// or a `#` followed by 3, 4, 6 or 8 hexadecimal digits. A `#favetto-app` id
/// selector yields only two hex characters, so it is not a colour.
fn has_literal_colour(line: &str) -> bool {
    if ["rgb(", "rgba(", "hsl(", "hsla("]
        .iter()
        .any(|prefix| line.contains(prefix))
    {
        return true;
    }

    for (idx, ch) in line.char_indices() {
        if ch != '#' {
            continue;
        }
        let hex_len = line[idx + 1..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .count();
        if !matches!(hex_len, 3 | 4 | 6 | 8) {
            continue;
        }
        // The run must end at a token boundary, so `#abc` is a colour but a
        // longer identifier such as `#abcdefg` is not (and has no valid length
        // anyway). Hex digits are ASCII, so this slice is a char boundary.
        let boundary = line[idx + 1 + hex_len..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        if boundary {
            return true;
        }
    }
    false
}

#[test]
fn no_literal_colours_outside_tokens() {
    let css = without_comments(CSS);
    for (i, raw) in css.lines().enumerate() {
        let line = raw.trim_start();
        if has_literal_colour(line) {
            assert!(
                line.starts_with("--"),
                "literal colour outside a token at line {}: {}",
                i + 1,
                raw.trim()
            );
        }
    }
}

fn rule_count(selector: &str) -> usize {
    let needle = format!("{selector} {{");
    CSS.lines().filter(|l| l.trim() == needle).count()
}

#[test]
fn deduped_selectors_have_a_single_base_rule() {
    assert_eq!(
        rule_count(".status-badge"),
        1,
        "duplicate .status-badge rule"
    );
    assert_eq!(rule_count(".back-link"), 1, "duplicate .back-link rule");
    assert_eq!(
        rule_count(".connection-banner"),
        1,
        "duplicate .connection-banner rule"
    );
}

#[test]
fn every_status_word_has_a_badge_rule() {
    for word in [
        "pending",
        "running",
        "awaiting_input",
        "succeeded",
        "failed",
        "cancelled",
        "interrupted",
        "timed_out",
    ] {
        assert!(
            CSS.contains(&format!(".status-badge.status-{word}")),
            "missing .status-badge.status-{word}"
        );
    }
}

#[test]
fn reduced_motion_is_honoured() {
    let block = CSS
        .split("@media (prefers-reduced-motion: reduce)")
        .nth(1)
        .expect("missing reduced-motion block");
    assert!(block.contains(".spinner") && block.contains("animation: none"));
    assert!(block.contains(".skip-link") && block.contains("transition: none"));
}
