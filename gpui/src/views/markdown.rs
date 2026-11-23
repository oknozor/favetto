//! The shared GPUI Markdown paint path.
//!
//! Both the task Output feed and the Catalog source preview render the same
//! [`MarkdownDoc`] model from [`crate::markdown`]. This module owns the styling
//! (headings, prose, lists, tables, quotes, rules) and fenced-code syntax
//! colouring so the two surfaces cannot drift apart. It also exposes the
//! highlighted-source primitive the Catalog uses for its TOML front matter.
//!
//! Element ids are seeded by the caller: a feed row uses its index stride while
//! the single Catalog preview starts at zero.

use std::ops::Range;

use gpui::{
    div, prelude::*, px, AnyElement, Div, ElementId, FontStyle, FontWeight, Hsla, InteractiveText,
    IntoElement, StrikethroughStyle, StyledText, TextRun, TextStyle, UnderlineStyle,
};

use crate::markdown::syntax::{self, Token, TokenKind};
use crate::markdown::{self, Align, Block, Inline, MarkdownDoc};
use crate::theme::Theme;

/// Which visual treatment a Markdown body uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Prose,
    Reasoning,
}

/// How many element ids each feed row may consume before the next row starts.
pub(crate) const MARKDOWN_ROW_STRIDE: u64 = 100_000;

/// Render a parsed Markdown document with the given tone and id seed.
pub(crate) fn markdown_body(
    theme: &Theme,
    base: &TextStyle,
    doc: &MarkdownDoc,
    tone: Tone,
    seed: u64,
) -> Div {
    let mut ids = seed;
    render_blocks(theme, base, &doc.blocks, tone, 0, &mut ids)
}

/// Render already-tokenised source, preserving the original whitespace.
pub(crate) fn highlighted_tokens(
    theme: &Theme,
    base: &TextStyle,
    tokens: &[Token],
    size: f32,
) -> Div {
    let (text, runs) = syntax_styled(theme, base, tokens, size);
    div()
        .min_w_0()
        .whitespace_normal()
        .child(StyledText::new(text).with_runs(runs))
}

fn syntax_styled(
    theme: &Theme,
    base: &TextStyle,
    tokens: &[Token],
    size: f32,
) -> (String, Vec<TextRun>) {
    let mut text = String::new();
    let mut runs = Vec::new();
    for token in tokens {
        let mut style = base.clone();
        style.font_family = "monospace".into();
        style.font_size = px(size).into();
        style.color = Hsla::from(syntax_color(theme, token.kind));
        text.push_str(&token.text);
        runs.push(style.to_run(token.text.len()));
    }
    (text, runs)
}

fn render_blocks(
    theme: &Theme,
    base: &TextStyle,
    blocks: &[Block],
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    let mut column = div().flex().flex_col().gap(px(theme.spacing.s2)).min_w_0();
    for block in blocks {
        column = column.child(render_block(theme, base, block, tone, depth, ids));
    }
    column
}

fn render_block(
    theme: &Theme,
    base: &TextStyle,
    block: &Block,
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    match block {
        Block::Heading { level, content } => {
            let (size, weight, color) = heading_style(theme, *level);
            let mut heading = div().min_w_0().child(prose_colored(
                theme, base, content, tone, size, weight, color, ids,
            ));
            if *level <= 2 {
                heading = heading.pt(px(theme.spacing.s1));
            }
            heading
        }
        Block::Paragraph(content) => div().min_w_0().child(prose(
            theme,
            base,
            content,
            tone,
            theme.font.sm,
            base.font_weight,
            ids,
        )),
        Block::Code { language, code } => render_code_block(theme, base, language, code),
        Block::List {
            ordered,
            start,
            items,
        } => render_list(theme, base, *ordered, *start, items, tone, depth, ids),
        Block::Quote(blocks) => div()
            .flex()
            .flex_col()
            .min_w_0()
            .pl(px(theme.spacing.s2))
            .border_l_2()
            .border_color(theme.colors.border)
            .child(render_blocks(
                theme,
                base,
                blocks,
                Tone::Reasoning,
                depth + 1,
                ids,
            )),
        Block::Rule => div()
            .w_full()
            .py(px(theme.spacing.s1))
            .child(div().w_full().h(px(1.0)).bg(theme.colors.border)),
        Block::Table { align, head, rows } => {
            render_table(theme, base, align, head, rows, tone, depth, ids)
        }
    }
}

fn heading_style(theme: &Theme, level: u8) -> (f32, FontWeight, Hsla) {
    let weight = match level {
        1 | 2 => FontWeight::BOLD,
        3 | 5 => FontWeight::SEMIBOLD,
        _ => FontWeight::MEDIUM,
    };
    let color = if level >= 5 {
        Hsla::from(theme.colors.muted)
    } else {
        Hsla::from(theme.colors.text)
    };
    (theme.markdown_heading_size(level), weight, color)
}

fn prose(
    theme: &Theme,
    base: &TextStyle,
    content: &[Inline],
    tone: Tone,
    size: f32,
    weight: FontWeight,
    ids: &mut u64,
) -> AnyElement {
    let color = match tone {
        Tone::Prose => Hsla::from(theme.colors.text),
        Tone::Reasoning => Hsla::from(theme.colors.muted),
    };
    prose_colored(theme, base, content, tone, size, weight, color, ids)
}

#[allow(clippy::too_many_arguments)]
fn prose_colored(
    theme: &Theme,
    base: &TextStyle,
    content: &[Inline],
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
    ids: &mut u64,
) -> AnyElement {
    let inline = markdown::flatten(content);
    let id = *ids;
    *ids = (*ids).wrapping_add(1);
    inline_styled(theme, base, &inline, tone, size, weight, color, id)
}

#[allow(clippy::too_many_arguments)]
fn inline_styled(
    theme: &Theme,
    base: &TextStyle,
    inline: &markdown::InlineText,
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
    id: u64,
) -> AnyElement {
    let (runs, links) = inline_runs(theme, base, inline, tone, size, weight, color);
    let styled = StyledText::new(inline.text.clone()).with_runs(runs);
    if links.is_empty() {
        return div()
            .min_w_0()
            .whitespace_normal()
            .child(styled)
            .into_any_element();
    }

    let ranges: Vec<Range<usize>> = links.iter().map(|(range, _)| range.clone()).collect();
    let urls: Vec<String> = links.into_iter().map(|(_, url)| url).collect();
    let text = InteractiveText::new(ElementId::NamedInteger("md-prose".into(), id), styled)
        .on_click(ranges, move |index, _window, cx| {
            if let Some(url) = urls.get(index) {
                cx.open_url(url);
            }
        });
    div()
        .min_w_0()
        .whitespace_normal()
        .child(text)
        .into_any_element()
}

fn inline_runs(
    theme: &Theme,
    base: &TextStyle,
    inline: &markdown::InlineText,
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
) -> (Vec<TextRun>, Vec<(Range<usize>, String)>) {
    let mut runs = Vec::new();
    let mut links = Vec::new();
    let mut cursor = 0usize;

    for span in &inline.spans {
        if span.range.start > cursor {
            runs.push(
                base_style(base, tone, size, weight, color).to_run(span.range.start - cursor),
            );
        }
        let start = span.range.start.max(cursor);
        let end = span.range.end.max(start);
        cursor = end;

        let mut style = base_style(base, tone, size, weight, color);
        if span.style.code {
            style.font_family = "monospace".into();
            style.background_color = Some(Hsla::from(theme.colors.border).opacity(0.35));
        }
        if span.style.strong {
            style.font_weight = FontWeight::SEMIBOLD;
        }
        if span.style.emphasis {
            style.font_style = FontStyle::Italic;
        }
        if span.style.strike {
            style.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(Hsla::from(theme.colors.muted)),
            });
        }
        if let Some(url) = &span.style.link {
            style.color = Hsla::from(theme.colors.accent);
            style.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(Hsla::from(theme.colors.accent)),
                wavy: false,
            });
            if is_openable(url) {
                links.push((start..end, url.clone()));
            }
        }
        runs.push(style.to_run(end - start));
    }

    if cursor < inline.text.len() {
        runs.push(base_style(base, tone, size, weight, color).to_run(inline.text.len() - cursor));
    }
    if runs.is_empty() && !inline.text.is_empty() {
        runs.push(base_style(base, tone, size, weight, color).to_run(inline.text.len()));
    }
    debug_assert_eq!(
        runs.iter().map(|run| run.len).sum::<usize>(),
        inline.text.len()
    );
    (runs, links)
}

fn base_style(
    base: &TextStyle,
    tone: Tone,
    size: f32,
    weight: FontWeight,
    color: Hsla,
) -> TextStyle {
    let mut style = base.clone();
    style.font_size = px(size).into();
    style.font_weight = weight;
    style.color = color;
    if matches!(tone, Tone::Reasoning) {
        style.font_style = FontStyle::Italic;
    }
    style
}

fn is_openable(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn render_code_block(theme: &Theme, base: &TextStyle, language: &str, code: &str) -> Div {
    let mut container = div()
        .flex()
        .flex_col()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(Hsla::from(theme.colors.border).opacity(0.45))
        .bg(Hsla::from(theme.colors.border).opacity(0.12));

    let label = syntax::language_name(language);
    if !label.is_empty() {
        container = container.child(
            div()
                .px(px(theme.spacing.s2))
                .py(px(theme.spacing.s1))
                .border_b_1()
                .border_color(Hsla::from(theme.colors.border).opacity(0.35))
                .font_family("monospace")
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.muted)
                .child(label),
        );
    }

    let tokens = syntax::highlight(language, code);
    let (text, runs) = syntax_styled(theme, base, &tokens, theme.font.xs);

    if !text.is_empty() {
        container = container.child(
            div()
                .min_w_0()
                .whitespace_normal()
                .p(px(theme.spacing.s2))
                .child(StyledText::new(text).with_runs(runs)),
        );
    }
    container
}

fn syntax_color(theme: &Theme, kind: TokenKind) -> gpui::Rgba {
    match kind {
        TokenKind::Plain => theme.colors.text,
        TokenKind::Keyword => theme.syntax.keyword,
        TokenKind::String => theme.syntax.string,
        TokenKind::Number => theme.syntax.number,
        TokenKind::Comment => theme.syntax.comment,
        TokenKind::Type => theme.syntax.type_name,
        TokenKind::Function => theme.syntax.function,
        TokenKind::Key => theme.syntax.key,
        TokenKind::Operator | TokenKind::Punctuation => theme.syntax.punctuation,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    theme: &Theme,
    base: &TextStyle,
    ordered: bool,
    start: u64,
    items: &[markdown::ListItem],
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    let mut column = div().flex().flex_col().gap(px(theme.spacing.s1)).min_w_0();
    if depth > 0 {
        column = column.pl(px(theme.spacing.s3));
    }

    for (offset, item) in items.iter().enumerate() {
        let (marker, color) = match item.task {
            Some(true) => ("☑".to_string(), theme.colors.accent),
            Some(false) => ("☐".to_string(), theme.colors.accent),
            None if ordered => (format!("{}.", start + offset as u64), theme.colors.muted),
            None => ("•".to_string(), theme.colors.muted),
        };
        let content = if item.blocks.is_empty() {
            div()
        } else {
            render_blocks(theme, base, &item.blocks, tone, depth + 1, ids)
        };
        column = column.child(
            div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s2))
                .min_w_0()
                .child(
                    div()
                        .w(px(18.0))
                        .flex_shrink_0()
                        .text_size(px(theme.font.sm))
                        .text_color(color)
                        .child(marker),
                )
                .child(div().flex_1().min_w_0().child(content)),
        );
    }
    column
}

#[allow(clippy::too_many_arguments)]
fn render_table(
    theme: &Theme,
    base: &TextStyle,
    align: &[Align],
    head: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    tone: Tone,
    depth: usize,
    ids: &mut u64,
) -> Div {
    let border = Hsla::from(theme.colors.border).opacity(0.45);
    let mut container = div()
        .flex()
        .flex_col()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(border);

    if !head.is_empty() {
        container = container.child(render_table_row(
            theme, base, head, align, tone, depth, ids, true,
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        let mut rendered = render_table_row(theme, base, row, align, tone, depth, ids, false);
        if index > 0 || !head.is_empty() {
            rendered = rendered
                .border_t_1()
                .border_color(Hsla::from(theme.colors.border).opacity(0.35));
        }
        container = container.child(rendered);
    }
    container
}

#[allow(clippy::too_many_arguments)]
fn render_table_row(
    theme: &Theme,
    base: &TextStyle,
    cells: &[Vec<Inline>],
    align: &[Align],
    tone: Tone,
    _depth: usize,
    ids: &mut u64,
    header: bool,
) -> Div {
    let mut row = div()
        .flex()
        .w_full()
        .min_w_0()
        .when(header, |row| row.bg(theme.colors.surface));
    let last = cells.len().saturating_sub(1);
    for (index, cell) in cells.iter().enumerate() {
        let alignment = align.get(index).copied().unwrap_or(Align::None);
        let content = if cell.is_empty() {
            div().into_any_element()
        } else {
            let color = if header {
                Hsla::from(theme.colors.text)
            } else if matches!(tone, Tone::Reasoning) {
                Hsla::from(theme.colors.muted)
            } else {
                Hsla::from(theme.colors.text)
            };
            let weight = if header {
                FontWeight::SEMIBOLD
            } else {
                base.font_weight
            };
            prose_colored(theme, base, cell, tone, theme.font.xs, weight, color, ids)
        };
        let mut cell = div()
            .flex_1()
            .min_w_0()
            .px(px(theme.spacing.s1))
            .py(px(2.0))
            .when(index < last, |cell| {
                cell.border_r_1()
                    .border_color(Hsla::from(theme.colors.border).opacity(0.35))
            })
            .child(content);
        cell = match alignment {
            Align::Center => cell.text_center(),
            Align::Right => cell.text_right(),
            Align::Left | Align::None => cell.text_left(),
        };
        row = row.child(cell);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> &'static Theme {
        Theme::for_appearance(gpui::WindowAppearance::Dark)
    }

    fn inline_of(source: &str) -> markdown::InlineText {
        match markdown::parse(source).blocks.into_iter().next() {
            Some(Block::Paragraph(content)) => markdown::flatten(&content),
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    fn runs_of(source: &str) -> (Vec<TextRun>, Vec<(Range<usize>, String)>) {
        let theme = theme();
        inline_runs(
            theme,
            &TextStyle::default(),
            &inline_of(source),
            Tone::Prose,
            theme.font.sm,
            FontWeight::NORMAL,
            Hsla::from(theme.colors.text),
        )
    }

    #[test]
    fn inline_runs_apply_strong_emphasis_strike_and_code() {
        let (bold, _) = runs_of("**bold**");
        assert_eq!(bold.len(), 1);
        assert_eq!(bold[0].font.weight, FontWeight::SEMIBOLD);

        let (italic, _) = runs_of("*italic*");
        assert_eq!(italic[0].font.style, FontStyle::Italic);

        let (strike, _) = runs_of("~~gone~~");
        assert!(strike[0].strikethrough.is_some());

        let (code, _) = runs_of("`code`");
        assert_eq!(code[0].font.family.as_ref(), "monospace");
        assert!(code[0].background_color.is_some());
    }

    #[test]
    fn inline_runs_expose_openable_links() {
        let (runs, links) = runs_of("[docs](https://example.com)");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://example.com");
        let theme = theme();
        assert_eq!(runs[0].color, Hsla::from(theme.colors.accent));
        assert!(runs[0].underline.is_some());
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), "docs".len());
    }

    #[test]
    fn only_http_and_https_links_open() {
        assert!(is_openable("http://example.com"));
        assert!(is_openable("https://example.com/a?b=c"));
        assert!(!is_openable("file:///etc/passwd"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable("/relative/path"));
    }

    #[test]
    fn headings_follow_the_documented_scale_and_weights() {
        let theme = theme();
        let expected = [
            FontWeight::BOLD,
            FontWeight::BOLD,
            FontWeight::SEMIBOLD,
            FontWeight::MEDIUM,
            FontWeight::SEMIBOLD,
            FontWeight::MEDIUM,
        ];
        for (index, weight) in expected.into_iter().enumerate() {
            let level = index as u8 + 1;
            let (size, actual, _) = heading_style(theme, level);
            assert_eq!(size, theme.markdown_heading_size(level));
            assert_eq!(actual, weight);
        }
        assert_eq!(heading_style(theme, 5).2, Hsla::from(theme.colors.muted));
        assert_eq!(heading_style(theme, 1).2, Hsla::from(theme.colors.text));
    }

    #[test]
    fn highlighted_source_preserves_the_exact_toml_bytes() {
        let source = "agent = \"opencode\"\n\n[[vars]]\nname = \"repo\"\n";
        let tokens = syntax::highlight("toml", source);
        let (text, runs) = syntax_styled(theme(), &TextStyle::default(), &tokens, 11.0);
        assert_eq!(text, source);
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), source.len());
    }
}
