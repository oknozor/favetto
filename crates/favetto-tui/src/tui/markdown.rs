//! Minimal Markdown + TOML highlighting for the task preview pane and the ACP
//! feed.
//!
//! A task file is TOML front-matter followed by a Markdown prompt. This module
//! renders such a file into styled [`Line`]s: the front-matter is highlighted as
//! TOML and the prompt as Markdown (headings, lists, blockquotes, fenced code,
//! and inline code/bold/italic/links). It is intentionally small — no external
//! renderer or highlighter. Fenced code is highlighted for a handful of common
//! languages by [`super::syntax`]; unsupported languages stay readable in the
//! plain code colour.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// Render a task file (TOML front-matter + Markdown body) into styled lines.
pub fn render_task(source: &str, theme: &Theme) -> Vec<Line<'static>> {
    let source = super::sanitize::text(source);
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
        Style::default().fg(theme.syn_comment()),
    )));
    out.extend(render_markdown(&lines[sep + 1..], theme));
    out
}

/// Render a Markdown document (assistant prose, a catalog prompt, …).
pub fn render(source: &str, theme: &Theme) -> Vec<Line<'static>> {
    let source = super::sanitize::text(source);
    let lines: Vec<&str> = source.lines().collect();
    render_markdown(&lines, theme)
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
        return plain(line, Style::default().fg(theme.syn_comment()));
    }
    if trimmed.starts_with('[') {
        return plain(
            line,
            Style::default()
                .fg(theme.syn_heading())
                .add_modifier(Modifier::BOLD),
        );
    }
    match line.split_once('=') {
        Some((key, value)) => {
            let mut spans = vec![
                Span::styled(key.to_string(), Style::default().fg(theme.syn_key())),
                Span::styled("=".to_string(), Style::default().fg(theme.syn_comment())),
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
                flush(
                    &mut buf,
                    &mut spans,
                    Style::default().fg(theme.syn_number()),
                );
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
                spans.push(Span::styled(s, Style::default().fg(theme.syn_string())));
            }
            '[' | ']' | '{' | '}' | ',' => {
                flush(
                    &mut buf,
                    &mut spans,
                    Style::default().fg(theme.syn_number()),
                );
                spans.push(Span::styled(
                    chars[i].to_string(),
                    Style::default().fg(theme.syn_comment()),
                ));
                i += 1;
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(
        &mut buf,
        &mut spans,
        Style::default().fg(theme.syn_number()),
    );
    spans
}

fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), style));
    }
}

// ---------------------------------------------------------------------------
// Markdown blocks
// ---------------------------------------------------------------------------

struct Fence {
    marker: char,
    len: usize,
    info: String,
}

impl Fence {
    fn open(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next()?;
        if !matches!(marker, '`' | '~') {
            return None;
        }
        let len = trimmed.chars().take_while(|c| *c == marker).count();
        if len < 3 {
            return None;
        }
        Some(Self {
            marker,
            len,
            info: trimmed[len..].trim().to_string(),
        })
    }

    fn closes(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let count = trimmed.chars().take_while(|c| *c == self.marker).count();
        count >= self.len && trimmed[count..].trim().is_empty()
    }
}

fn render_markdown(lines: &[&str], theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(fence) = Fence::open(lines[i]) {
            let start = i;
            i += 1;
            let mut end = lines.len();
            while i < lines.len() {
                if fence.closes(lines[i]) {
                    end = i;
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(plain(
                lines[start],
                Style::default().fg(theme.syn_comment()),
            ));
            out.extend(render_fence_code(
                &fence.info,
                &lines[start + 1..end],
                theme,
            ));
            if end < lines.len() {
                out.push(plain(lines[end], Style::default().fg(theme.syn_comment())));
            }
            continue;
        }

        // Setext headings: a text line underlined by `===` or `---`.
        if i + 1 < lines.len() && !lines[i].trim().is_empty() {
            if let Some(level) = setext_level(lines[i + 1]) {
                out.push(setext_heading(lines[i], level, theme));
                i += 2;
                continue;
            }
        }

        out.push(render_markdown_line(lines[i], theme));
        i += 1;
    }
    out
}

fn setext_level(line: &str) -> Option<u8> {
    let trimmed = line.trim();
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '=' | '-') || !trimmed.chars().all(|c| c == marker) {
        return None;
    }
    Some(if marker == '=' { 1 } else { 2 })
}

fn setext_heading(line: &str, level: u8, theme: &Theme) -> Line<'static> {
    let prefix = if level == 1 { "# " } else { "## " };
    let mut spans = vec![Span::styled(
        prefix,
        Style::default()
            .fg(theme.syn_heading())
            .add_modifier(Modifier::BOLD),
    )];
    spans.extend(inline(
        line.trim(),
        Style::default()
            .fg(theme.syn_heading())
            .add_modifier(Modifier::BOLD),
        theme,
    ));
    Line::from(spans)
}

fn render_fence_code(info: &str, lines: &[&str], theme: &Theme) -> Vec<Line<'static>> {
    let language = super::syntax::language_name(info);
    match language.as_str() {
        "" | "text" | "txt" | "plaintext" => lines
            .iter()
            .map(|line| plain(line, Style::default().fg(theme.syn_code())))
            .collect(),
        "json" => super::json::render_source(&lines.join("\n"), theme),
        "toml" => lines
            .iter()
            .map(|line| highlight_toml(line, theme))
            .collect(),
        _ => super::syntax::highlight(&language, &lines.join("\n"), theme),
    }
}

fn render_markdown_line(line: &str, theme: &Theme) -> Line<'static> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    // Headings.
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes > 0 && hashes <= 6 && trimmed[hashes..].starts_with(' ') {
        return plain(
            trimmed,
            Style::default()
                .fg(theme.syn_heading())
                .add_modifier(Modifier::BOLD),
        );
    }

    // Horizontal rule.
    if matches!(trimmed, "---" | "***" | "___") {
        return plain(line, Style::default().fg(theme.syn_comment()));
    }

    // Blockquote, including nested `>` markers.
    if let Some((prefix, rest)) = strip_blockquote(trimmed) {
        let mut spans = vec![Span::styled(
            prefix,
            Style::default().fg(theme.markdown.block_quote),
        )];
        spans.extend(inline(
            rest,
            Style::default()
                .fg(theme.markdown.block_quote)
                .add_modifier(Modifier::ITALIC),
            theme,
        ));
        return Line::from(spans);
    }

    // Bullet / numbered list.
    if let Some((marker, content)) = strip_bullet(trimmed) {
        let mut spans = Vec::new();
        if indent > 0 {
            spans.push(Span::raw(line[..indent].to_string()));
        }
        let color = if marker.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            theme.markdown.list_enumeration
        } else {
            theme.markdown.list_item
        };
        spans.push(Span::styled(marker, Style::default().fg(color)));
        spans.extend(inline(content, Style::default().fg(theme.fg()), theme));
        return Line::from(spans);
    }

    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(line[..indent].to_string()));
    }
    spans.extend(inline(trimmed, Style::default().fg(theme.fg()), theme));
    Line::from(spans)
}

fn strip_blockquote(line: &str) -> Option<(String, &str)> {
    let mut prefix = String::new();
    let mut rest = line;
    while let Some(after) = rest.strip_prefix('>') {
        prefix.push_str("> ");
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    (!prefix.is_empty()).then_some((prefix, rest))
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

// ---------------------------------------------------------------------------
// Inline Markdown
// ---------------------------------------------------------------------------

/// Expand inline Markdown (`code`, **bold**, *italic*, [text](url), images).
/// Unmatched markers and unsupported constructs are left as literal text.
fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        // Backslash escapes the next punctuation character.
        if chars[i] == '\\' && chars.get(i + 1).is_some_and(|c| is_escapable(*c)) {
            buf.push(chars[i + 1]);
            i += 2;
            continue;
        }

        if chars[i] == '`' {
            let ticks = chars[i..].iter().take_while(|c| **c == '`').count();
            if let Some(end) = find_run(&chars, i + ticks, '`', ticks) {
                flush(&mut buf, &mut spans, base);
                let code: String = chars[i + ticks..end].iter().collect();
                spans.push(Span::styled(code, Style::default().fg(theme.syn_code())));
                i = end + ticks;
                continue;
            }
        }

        if matches!(chars[i], '*' | '_') && emphasis_allowed(&chars, i) {
            if chars.get(i + 1) == Some(&chars[i]) {
                if let Some(end) = find_pair(&chars, i + 2, chars[i]) {
                    flush(&mut buf, &mut spans, base);
                    let inner: String = chars[i + 2..end].iter().collect();
                    spans.push(Span::styled(inner, base.add_modifier(Modifier::BOLD)));
                    i = end + 2;
                    continue;
                }
            } else if let Some(end) = find_single(&chars, i + 1, chars[i]) {
                flush(&mut buf, &mut spans, base);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(inner, base.add_modifier(Modifier::ITALIC)));
                i = end + 1;
                continue;
            }
        }

        if chars[i] == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((label, url, next)) = parse_link(&chars, i, true) {
                flush(&mut buf, &mut spans, base);
                spans.push(Span::styled(
                    "image ",
                    base.fg(theme.markdown.image).add_modifier(Modifier::ITALIC),
                ));
                spans.push(Span::styled(
                    label.clone(),
                    base.fg(theme.markdown.image_text)
                        .add_modifier(Modifier::ITALIC),
                ));
                push_url(&mut spans, &label, &url, base, theme);
                i = next;
                continue;
            }
        }

        if chars[i] == '[' {
            if let Some((label, url, next)) = parse_link(&chars, i, false) {
                flush(&mut buf, &mut spans, base);
                spans.push(Span::styled(
                    label.clone(),
                    base.fg(theme.markdown.link_text)
                        .add_modifier(Modifier::UNDERLINED),
                ));
                push_url(&mut spans, &label, &url, base, theme);
                i = next;
                continue;
            }
        }

        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut spans, base);
    spans
}

/// Append a link's target as inert text. When the label already is the target,
/// showing it twice only adds noise, so the target is preserved in full only
/// when it differs from the label. Nothing here is clickable or auto-opened.
fn push_url(spans: &mut Vec<Span<'static>>, label: &str, url: &str, base: Style, theme: &Theme) {
    if !url.is_empty() && label.trim() != url {
        spans.push(Span::styled(format!(" ({url})"), base.fg(theme.syn_link())));
    }
}

fn is_escapable(c: char) -> bool {
    "\\`*_{}[]()#+-.!|>~".contains(c)
}

fn emphasis_allowed(chars: &[char], i: usize) -> bool {
    if chars[i] != '_' {
        return true;
    }
    // CommonMark does not treat intraword underscores as emphasis.
    let prev_word = i > 0 && chars[i - 1].is_alphanumeric();
    let next_word = chars.get(i + 1).is_some_and(|c| c.is_alphanumeric());
    !(prev_word && next_word)
}

fn parse_link(chars: &[char], start: usize, image: bool) -> Option<(String, String, usize)> {
    let open = if image { start + 1 } else { start };
    let close = find_matching(chars, open, '[', ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find_matching(chars, close + 1, '(', ')')?;
    let label: String = chars[open + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((label, url.trim().to_string(), end + 1))
}

fn find_matching(chars: &[char], open: usize, opening: char, closing: char) -> Option<usize> {
    if chars.get(open) != Some(&opening) {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        if chars[i] == opening {
            depth += 1;
        } else if chars[i] == closing {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn find_run(chars: &[char], from: usize, needle: char, len: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == needle {
            let run = chars[i..].iter().take_while(|c| **c == needle).count();
            if run >= len {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_single(chars: &[char], from: usize, needle: char) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == needle {
            if chars.get(i + 1) == Some(&needle) {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
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

    fn rendered(source: &str, theme: &Theme) -> (String, Vec<Line<'static>>) {
        let lines = render(source, theme);
        let text = lines.iter().map(text_of).collect::<Vec<_>>().join("\n");
        (text, lines)
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
        assert_eq!(line.spans[0].style.fg, Some(theme.syn_key()));
        assert!(line
            .spans
            .iter()
            .any(|s| s.style.fg == Some(theme.syn_string()) && s.content.contains("opencode")));
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
            .any(|s| s.content == "cargo test" && s.style.fg == Some(theme.syn_code())));
    }

    #[test]
    fn renders_common_markdown_blocks() {
        let theme = Theme::dark();
        let (text, lines) = rendered(
            "# Title\n\nA **bold** and *italic* paragraph with `code` and [docs](https://example.com).\n\n> quoted\n\n- first\n- second\n\n1. one\n2. two\n\n---\n",
            &theme,
        );
        assert!(text.contains("# Title"));
        assert!(
            text.contains("A bold and italic paragraph with code and docs (https://example.com).")
        );
        assert!(text.contains("> quoted"));
        assert!(text.contains("- first"));
        assert!(text.contains("1. one"));
        assert!(text.contains("---"));

        let spans: Vec<_> = lines.iter().flat_map(|line| line.spans.clone()).collect();
        assert!(spans
            .iter()
            .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans
            .iter()
            .any(|s| s.content == "italic" && s.style.add_modifier.contains(Modifier::ITALIC)));
        assert!(spans
            .iter()
            .any(|s| s.content == "code" && s.style.fg == Some(theme.syn_code())));
        assert!(spans
            .iter()
            .any(|s| s.content == "docs" && s.style.add_modifier.contains(Modifier::UNDERLINED)));
        assert!(spans
            .iter()
            .any(|s| s.content == "- " && s.style.fg == Some(theme.markdown.list_item)));
        assert!(spans
            .iter()
            .any(|s| s.content == "1. " && s.style.fg == Some(theme.markdown.list_enumeration)));
        assert!(spans
            .iter()
            .any(|s| s.content == "> " && s.style.fg == Some(theme.markdown.block_quote)));
    }

    #[test]
    fn link_target_is_kept_when_it_differs_from_the_label() {
        let (text, _) = rendered("[docs](https://example.com/a?b=c)", &Theme::dark());
        assert_eq!(text, "docs (https://example.com/a?b=c)");
    }

    #[test]
    fn link_target_is_not_duplicated_when_it_equals_the_label() {
        let (text, _) = rendered(
            "[https://example.com/docs](https://example.com/docs)",
            &Theme::dark(),
        );
        assert_eq!(text, "https://example.com/docs");
        assert_eq!(text.matches("https://example.com/docs").count(), 1);
    }

    #[test]
    fn control_sequences_in_markdown_are_neutralized() {
        let (text, _) = rendered(
            "\x1b[31mred\x1b[0m [x](https://e)\u{202e}txet",
            &Theme::dark(),
        );
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\u{202e}'));
        assert!(text.contains("red x (https://e)"));
        assert!(text.contains("[U+202E]"));
    }

    #[test]
    fn nested_list_indentation_is_preserved() {
        let line = render_markdown_line("    - nested", &Theme::dark());
        assert_eq!(text_of(&line), "    - nested");
    }

    #[test]
    fn setext_headings_render_as_headings() {
        let theme = Theme::dark();
        let (text, lines) = rendered("Title\n=====\n\nSubtitle\n---\n", &theme);
        assert_eq!(text, "# Title\n\n## Subtitle");
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.fg == Some(theme.syn_heading())));
    }

    #[test]
    fn fenced_rust_keeps_the_language_label_and_highlights_tokens() {
        let theme = Theme::dark();
        let source = "```rust\nfn main() {\n    // hello\n    let name = \"world\";\n}\n```";
        let (text, lines) = rendered(source, &theme);
        assert_eq!(
            text,
            "```rust\nfn main() {\n    // hello\n    let name = \"world\";\n}\n```"
        );
        let spans: Vec<_> = lines.into_iter().flat_map(|line| line.spans).collect();
        assert!(spans
            .iter()
            .any(|s| s.content == "fn" && s.style.fg == Some(theme.syntax.keyword)));
        assert!(spans
            .iter()
            .any(|s| s.content == "\"world\"" && s.style.fg == Some(theme.syn_string())));
    }

    #[test]
    fn fenced_json_reuses_the_json_highlighter() {
        let theme = Theme::dark();
        let (text, lines) = rendered("```json\n{\"name\": \"foo\"}\n```", &theme);
        assert_eq!(text, "```json\n{\"name\": \"foo\"}\n```");
        let spans: Vec<_> = lines.into_iter().flat_map(|line| line.spans).collect();
        assert!(spans
            .iter()
            .any(|s| s.content == "\"name\"" && s.style.fg == Some(theme.syn_key())));
        assert!(spans
            .iter()
            .any(|s| s.content == "\"foo\"" && s.style.fg == Some(theme.syn_string())));
    }

    #[test]
    fn unknown_fence_language_stays_readable_without_highlighting() {
        let theme = Theme::dark();
        let source = "```invented\nfn **not markdown**\n```";
        let (text, lines) = rendered(source, &theme);
        assert_eq!(text, source);
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| !span.content.starts_with("```"))
            .all(|span| span.style.fg == Some(theme.syn_code())));
    }

    #[test]
    fn unmatched_markup_and_html_stay_literal() {
        let theme = Theme::dark();
        let (text, _) = rendered(
            "literal *asterisk and [bracket] and <script>alert(1)</script>",
            &theme,
        );
        assert_eq!(
            text,
            "literal *asterisk and [bracket] and <script>alert(1)</script>"
        );
    }

    #[test]
    fn unclosed_fence_renders_the_remaining_lines_as_code() {
        let theme = Theme::dark();
        let (text, lines) = rendered("```rust\nlet x = 1;", &theme);
        assert_eq!(text, "```rust\nlet x = 1;");
        assert!(lines[1]
            .spans
            .iter()
            .any(|span| span.content == "let" && span.style.fg == Some(theme.syntax.keyword)));
    }

    #[test]
    fn light_theme_uses_different_highlight_colors() {
        let dark = Theme::dark();
        let light = Theme::light();

        let light_toml = highlight_toml("agent = \"opencode\"", &light);
        assert_eq!(light_toml.spans[0].style.fg, Some(light.syn_key()));
        assert_ne!(light.syn_key(), dark.syn_key());
        assert!(light_toml
            .spans
            .iter()
            .any(|s| s.style.fg == Some(light.syn_string()) && s.content.contains("opencode")));
        assert_ne!(light.syn_string(), dark.syn_string());

        let light_code = inline("use `cargo test` now", Style::default(), &light);
        assert!(light_code
            .iter()
            .any(|s| s.content == "cargo test" && s.style.fg == Some(light.syn_code())));
        assert_ne!(light.syn_code(), dark.syn_code());
    }
}
