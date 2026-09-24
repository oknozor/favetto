//! Minimal Markdown + TOML highlighting for the task preview pane.
//!
//! A task file is TOML front-matter followed by a Markdown prompt. This module
//! renders such a file into styled [`Line`]s: the front-matter is highlighted as
//! TOML and the prompt as Markdown (headings, lists, blockquotes, fenced code,
//! and inline code/bold/italic/links). It is intentionally small — no external
//! renderer or highlighter.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// Render a task file (TOML front-matter + Markdown body) into styled lines.
pub fn render_task(source: &str, theme: &Theme) -> Vec<Line<'static>> {
    let lines: Vec<&str> = source.lines().collect();
    let separator = lines.iter().position(|l| l.trim() == "---");

    let Some(sep) = separator else {
        // No front-matter: treat the whole file as Markdown.
        return render_markdown(&lines, theme);
    };

    let mut out = Vec::new();
    for line in &lines[..sep] {
        out.push(highlight_toml(line, theme));
    }
    out.push(Line::from(Span::styled(
        "---",
        Style::default().fg(theme.syn_comment),
    )));
    out.extend(render_markdown(&lines[sep + 1..], theme));
    out
}

fn plain(text: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), style))
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

fn highlight_toml(line: &str, theme: &Theme) -> Line<'static> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return plain(line, Style::default().fg(theme.syn_comment));
    }
    if trimmed.starts_with('[') {
        return plain(
            line,
            Style::default()
                .fg(theme.syn_heading)
                .add_modifier(Modifier::BOLD),
        );
    }
    match line.split_once('=') {
        Some((key, value)) => {
            let mut spans = vec![
                Span::styled(key.to_string(), Style::default().fg(theme.syn_key)),
                Span::styled("=".to_string(), Style::default().fg(theme.syn_comment)),
            ];
            spans.extend(toml_value(value, theme));
            Line::from(spans)
        }
        None => plain(line, Style::default()),
    }
}

fn toml_value(value: &str, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                flush(&mut buf, &mut spans, Style::default().fg(theme.syn_number));
                let mut s = String::from('"');
                i += 1;
                while i < chars.len() {
                    s.push(chars[i]);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                        s.push(chars[i]);
                    } else if chars[i] == '"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                spans.push(Span::styled(s, Style::default().fg(theme.syn_string)));
            }
            '[' | ']' | '{' | '}' | ',' => {
                flush(&mut buf, &mut spans, Style::default().fg(theme.syn_number));
                spans.push(Span::styled(
                    chars[i].to_string(),
                    Style::default().fg(theme.syn_comment),
                ));
                i += 1;
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, &mut spans, Style::default().fg(theme.syn_number));
    spans
}

fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), style));
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn render_markdown(lines: &[&str], theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            out.push(plain(line, Style::default().fg(theme.syn_comment)));
            continue;
        }
        if in_code {
            out.push(plain(line, Style::default().fg(theme.syn_code)));
            continue;
        }
        out.push(render_markdown_line(line, theme));
    }
    out
}

fn render_markdown_line(line: &str, theme: &Theme) -> Line<'static> {
    let trimmed = line.trim_start();

    // Headings.
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes > 0 && hashes <= 6 && trimmed[hashes..].starts_with(' ') {
        return plain(
            trimmed,
            Style::default()
                .fg(theme.syn_heading)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Horizontal rule.
    if matches!(trimmed, "---" | "***" | "___") {
        return plain(line, Style::default().fg(theme.syn_comment));
    }

    // Blockquote.
    if let Some(rest) = trimmed.strip_prefix("> ") {
        let mut spans = vec![Span::styled("> ", Style::default().fg(theme.syn_comment))];
        spans.extend(inline(
            rest,
            Style::default().fg(theme.fg).add_modifier(Modifier::ITALIC),
            theme,
        ));
        return Line::from(spans);
    }

    // Bullet / numbered list.
    if let Some((marker, content)) = strip_bullet(trimmed) {
        let mut spans = vec![Span::styled(marker, Style::default().fg(theme.syn_comment))];
        spans.extend(inline(content, Style::default().fg(theme.fg), theme));
        return Line::from(spans);
    }

    let indent = line.len() - trimmed.len();
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(line[..indent].to_string()));
    }
    spans.extend(inline(trimmed, Style::default().fg(theme.fg), theme));
    Line::from(spans)
}

fn strip_bullet(line: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some((marker.to_string(), rest));
        }
    }
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return Some((line[..digits + 2].to_string(), &line[digits + 2..]));
    }
    None
}

/// Expand inline Markdown (`code`, **bold**, *italic*, [text](url)).
fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                flush(&mut buf, &mut spans, base);
                let code: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(code, Style::default().fg(theme.syn_code)));
                i = end + 1;
                continue;
            }
        }
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_pair(&chars, i + 2, '*') {
                flush(&mut buf, &mut spans, base);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(inner, base.add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '*' {
            if let Some(end) = find_char(&chars, i + 1, '*') {
                flush(&mut buf, &mut spans, base);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                i = end + 1;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(close) = find_char(&chars, i + 1, ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = find_char(&chars, close + 2, ')') {
                        flush(&mut buf, &mut spans, base);
                        let label: String = chars[i + 1..close].iter().collect();
                        spans.push(Span::styled(
                            label,
                            base.fg(theme.syn_link).add_modifier(Modifier::UNDERLINED),
                        ));
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, base));
    }
    spans
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|c| *c == needle)
        .map(|p| p + from)
}

/// Find a `**` closing pair starting at or after `from`.
fn find_pair(chars: &[char], from: usize, needle: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == needle && chars[i + 1] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn splits_front_matter_and_body() {
        let lines = render_task(
            "agent = \"opencode\"\n---\n# Title\n\n- item\n",
            &Theme::dark(),
        );
        let text: Vec<String> = lines.iter().map(text_of).collect();
        assert_eq!(text[0], "agent = \"opencode\"");
        assert_eq!(text[1], "---");
        assert_eq!(text[2], "# Title");
        assert_eq!(text[3], "");
        assert_eq!(text[4], "- item");
    }

    #[test]
    fn highlights_toml_key_and_string() {
        let theme = Theme::dark();
        let line = highlight_toml("agent = \"opencode\"", &theme);
        assert_eq!(text_of(&line), "agent = \"opencode\"");
        assert_eq!(line.spans[0].style.fg, Some(theme.syn_key));
        assert!(line
            .spans
            .iter()
            .any(|s| s.style.fg == Some(theme.syn_string) && s.content.contains("opencode")));
    }

    #[test]
    fn headings_are_bold() {
        let line = render_markdown_line("## Plan", &Theme::dark());
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_code_is_highlighted() {
        let theme = Theme::dark();
        let spans = inline("use `cargo test` now", Style::default(), &theme);
        assert!(spans
            .iter()
            .any(|s| s.content == "cargo test" && s.style.fg == Some(theme.syn_code)));
    }

    #[test]
    fn light_theme_uses_different_highlight_colors() {
        let dark = Theme::dark();
        let light = Theme::light();

        let light_toml = highlight_toml("agent = \"opencode\"", &light);
        assert_eq!(light_toml.spans[0].style.fg, Some(light.syn_key));
        assert_ne!(light.syn_key, dark.syn_key);
        assert!(light_toml
            .spans
            .iter()
            .any(|s| s.style.fg == Some(light.syn_string) && s.content.contains("opencode")));
        assert_ne!(light.syn_string, dark.syn_string);

        let light_code = inline("use `cargo test` now", Style::default(), &light);
        assert!(light_code
            .iter()
            .any(|s| s.content == "cargo test" && s.style.fg == Some(light.syn_code)));
        assert_ne!(light.syn_code, dark.syn_code);
    }
}
