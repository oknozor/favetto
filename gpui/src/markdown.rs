//! Markdown parsing for the task output feed.
//!
//! The desktop client renders agent messages and reasoning as Markdown, but the
//! GPUI paint path must stay cheap: only the trailing streaming block changes
//! between events, and completed blocks must not be re-parsed on every frame.
//! This module is therefore a pure parser + intermediate model with no `gpui`
//! dependency, plus a small content-addressed [`MarkdownCache`] the view opens
//! once per feed revision.
//!
//! Raw HTML is never interpreted: block and inline HTML events are dropped, and
//! a document that would otherwise render nothing falls back to its literal
//! source so a message can never become invisible.

pub mod syntax;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Maximum container nesting. Deeper structures are flattened to text so
/// adversarial input cannot overflow the parser or renderer stack.
pub const MAX_NESTING: usize = 12;

/// A parsed Markdown document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkdownDoc {
    pub blocks: Vec<Block>,
}

/// One block-level Markdown element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Code {
        language: String,
        code: String,
    },
    List {
        ordered: bool,
        start: u64,
        items: Vec<ListItem>,
    },
    Quote(Vec<Block>),
    Rule,
    Table {
        align: Vec<Align>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
}

/// One list item, optionally a GFM task-list item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// Column alignment of a GFM table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// One inline Markdown element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Code(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strike(Vec<Inline>),
    Link {
        label: Vec<Inline>,
        url: String,
    },
    Image {
        alt: Vec<Inline>,
        url: String,
    },
    SoftBreak,
    HardBreak,
    /// Internal: lifted into [`ListItem::task`] while parsing an item.
    TaskMarker(bool),
}

/// The style flags one flattened inline span carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub strong: bool,
    pub emphasis: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
}

/// A run of equal-styled text, as a byte range into [`InlineText::text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineSpan {
    pub range: Range<usize>,
    pub style: InlineStyle,
}

/// Flattened inline content ready to become GPUI text runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineText {
    pub text: String,
    pub spans: Vec<InlineSpan>,
}

/// Split a catalog task file into its TOML front matter and Markdown body.
///
/// The separator is the first line whose trimmed content is exactly `---`
/// (matching the TUI preview). A file with no separator is entirely
/// body, and an empty header is reported as no header. The returned slices keep
/// the original bytes apart from the separator line itself.
pub fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let mut offset = 0usize;
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let header = raw[..offset].trim_end_matches(['\r', '\n']);
            let header = (!header.is_empty()).then_some(header);
            return (header, &raw[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, raw)
}

/// Parse a Markdown source into the renderer's block model.
pub fn parse(source: &str) -> MarkdownDoc {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(source, options);
    let mut it = parser.peekable();
    let mut task = None;
    let blocks = parse_blocks(&mut it, 0, &mut task);

    if blocks.is_empty() && !source.trim().is_empty() {
        // A whole-message HTML block (or another construct we intentionally
        // drop) must still show its source; never render an invisible row.
        MarkdownDoc {
            blocks: vec![Block::Paragraph(vec![Inline::Text(source.to_string())])],
        }
    } else {
        MarkdownDoc { blocks }
    }
}

fn parse_blocks(
    it: &mut std::iter::Peekable<Parser<'_>>,
    depth: usize,
    task: &mut Option<bool>,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    while let Some(event) = it.peek() {
        if matches!(event, Event::End(_)) {
            break;
        }
        blocks.extend(parse_one_block(it, depth, task));
    }
    blocks
}

fn parse_one_block(
    it: &mut std::iter::Peekable<Parser<'_>>,
    depth: usize,
    task: &mut Option<bool>,
) -> Vec<Block> {
    let Some(event) = it.peek().cloned() else {
        return Vec::new();
    };
    match event {
        Event::Start(Tag::Paragraph) => {
            it.next();
            let content = strip_task_markers(parse_inlines(it, task, depth + 1));
            if content.is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph(content)]
            }
        }
        Event::Start(Tag::Heading { level, .. }) => {
            it.next();
            let content = strip_task_markers(parse_inlines(it, task, depth + 1));
            vec![Block::Heading {
                level: heading_level(level),
                content,
            }]
        }
        Event::Start(Tag::CodeBlock(kind)) => {
            let language = match kind {
                CodeBlockKind::Fenced(info) => syntax::language_name(&info),
                CodeBlockKind::Indented => String::new(),
            };
            it.next();
            let mut code = String::new();
            for event in it.by_ref() {
                match event {
                    Event::Text(text) => code.push_str(&text),
                    Event::End(TagEnd::CodeBlock) => break,
                    _ => {}
                }
            }
            // pulldown-cmark appends one newline to a closed fenced block; the
            // renderer owns the container padding instead.
            if code.ends_with('\n') {
                code.pop();
            }
            vec![Block::Code { language, code }]
        }
        Event::Start(Tag::List(start)) => {
            it.next();
            if depth >= MAX_NESTING {
                let text = collect_container_text(it);
                if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![Block::Paragraph(vec![Inline::Text(text)])]
                }
            } else {
                vec![parse_list(it, depth + 1, start)]
            }
        }
        Event::Start(Tag::BlockQuote(_)) => {
            it.next();
            if depth >= MAX_NESTING {
                let text = collect_container_text(it);
                if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![Block::Paragraph(vec![Inline::Text(text)])]
                }
            } else {
                let inner = parse_blocks(it, depth + 1, task);
                if matches!(it.peek(), Some(Event::End(TagEnd::BlockQuote(_)))) {
                    it.next();
                }
                vec![Block::Quote(inner)]
            }
        }
        Event::Start(Tag::Table(align)) => {
            let align = align.into_iter().map(align_of).collect();
            it.next();
            if depth >= MAX_NESTING {
                let text = collect_container_text(it);
                if text.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![Block::Paragraph(vec![Inline::Text(text)])]
                }
            } else {
                vec![parse_table(it, align, depth + 1, task)]
            }
        }
        Event::Rule => {
            it.next();
            vec![Block::Rule]
        }
        Event::Start(Tag::HtmlBlock) => {
            it.next();
            let _ = collect_container_text(it);
            Vec::new()
        }
        Event::Start(_) => {
            // Stray inline tag at block level (for example a bare emphasis
            // run). Treat it as a paragraph rather than losing it.
            let content = strip_task_markers(parse_inlines(it, task, depth + 1));
            if content.is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph(content)]
            }
        }
        Event::Text(_)
        | Event::Code(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_)
        | Event::SoftBreak
        | Event::HardBreak => {
            let content = strip_task_markers(collect_inline_run(it, task, depth + 1));
            if content.is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph(content)]
            }
        }
        Event::Html(_) | Event::InlineHtml(_) => {
            it.next();
            Vec::new()
        }
        Event::TaskListMarker(checked) => {
            it.next();
            *task = Some(checked);
            Vec::new()
        }
        Event::FootnoteReference(_) => {
            it.next();
            Vec::new()
        }
        Event::End(_) => Vec::new(),
    }
}

fn parse_list(it: &mut std::iter::Peekable<Parser<'_>>, depth: usize, start: Option<u64>) -> Block {
    let mut items = Vec::new();
    loop {
        match it.peek() {
            None => break,
            Some(Event::Start(Tag::Item)) => {
                it.next();
                items.push(parse_item(it, depth));
            }
            Some(Event::End(TagEnd::List(_))) => {
                it.next();
                break;
            }
            Some(Event::End(_)) => {
                it.next();
                break;
            }
            Some(_) => {
                it.next();
            }
        }
    }
    Block::List {
        ordered: start.is_some(),
        start: start.unwrap_or(1),
        items,
    }
}

fn parse_item(it: &mut std::iter::Peekable<Parser<'_>>, depth: usize) -> ListItem {
    let mut task = None;
    let mut blocks = Vec::new();
    loop {
        match it.peek() {
            None => break,
            Some(Event::End(TagEnd::Item)) => {
                it.next();
                break;
            }
            Some(Event::End(_)) => {
                it.next();
                break;
            }
            Some(Event::TaskListMarker(checked)) => {
                task = Some(*checked);
                it.next();
            }
            Some(_) => blocks.extend(parse_one_block(it, depth, &mut task)),
        }
    }
    ListItem { task, blocks }
}

fn parse_table(
    it: &mut std::iter::Peekable<Parser<'_>>,
    align: Vec<Align>,
    depth: usize,
    task: &mut Option<bool>,
) -> Block {
    let mut head = Vec::new();
    let mut rows = Vec::new();
    loop {
        match it.peek() {
            None => break,
            Some(Event::End(TagEnd::Table)) => {
                it.next();
                break;
            }
            Some(Event::Start(Tag::TableHead)) => {
                it.next();
                head = parse_cells(it, TagEnd::TableHead, depth, task);
            }
            Some(Event::Start(Tag::TableRow)) => {
                it.next();
                rows.push(parse_cells(it, TagEnd::TableRow, depth, task));
            }
            Some(_) => {
                it.next();
            }
        }
    }
    Block::Table { align, head, rows }
}

fn parse_cells(
    it: &mut std::iter::Peekable<Parser<'_>>,
    expected: TagEnd,
    depth: usize,
    task: &mut Option<bool>,
) -> Vec<Vec<Inline>> {
    let mut cells = Vec::new();
    loop {
        match it.peek() {
            None => break,
            Some(Event::End(end)) if *end == expected => {
                it.next();
                break;
            }
            Some(Event::Start(Tag::TableCell)) => {
                it.next();
                cells.push(parse_inlines(it, task, depth + 1));
            }
            Some(_) => {
                it.next();
            }
        }
    }
    cells
}

fn parse_inlines(
    it: &mut std::iter::Peekable<Parser<'_>>,
    task: &mut Option<bool>,
    depth: usize,
) -> Vec<Inline> {
    let mut out = Vec::new();
    while let Some(event) = it.peek() {
        if matches!(event, Event::End(_)) {
            it.next();
            break;
        }
        out.extend(parse_one_inline(it, task, depth));
    }
    out
}

fn parse_one_inline(
    it: &mut std::iter::Peekable<Parser<'_>>,
    task: &mut Option<bool>,
    depth: usize,
) -> Vec<Inline> {
    let Some(event) = it.next() else {
        return Vec::new();
    };
    match event {
        Event::Text(text) => vec![Inline::Text(text.into_string())],
        Event::Code(text) => vec![Inline::Code(text.into_string())],
        Event::InlineMath(text) | Event::DisplayMath(text) => {
            vec![Inline::Text(text.into_string())]
        }
        Event::SoftBreak => vec![Inline::SoftBreak],
        Event::HardBreak => vec![Inline::HardBreak],
        Event::Rule => vec![Inline::Text("—".to_string())],
        Event::TaskListMarker(checked) => {
            *task = Some(checked);
            vec![Inline::TaskMarker(checked)]
        }
        Event::Html(_) | Event::InlineHtml(_) => Vec::new(),
        Event::FootnoteReference(label) => vec![Inline::Text(format!("[^{label}]"))],
        Event::Start(Tag::Strong) => {
            if depth >= MAX_NESTING {
                vec![Inline::Text(collect_container_text(it))]
            } else {
                vec![Inline::Strong(parse_inlines(it, task, depth + 1))]
            }
        }
        Event::Start(Tag::Emphasis) => {
            if depth >= MAX_NESTING {
                vec![Inline::Text(collect_container_text(it))]
            } else {
                vec![Inline::Emphasis(parse_inlines(it, task, depth + 1))]
            }
        }
        Event::Start(Tag::Strikethrough) => {
            if depth >= MAX_NESTING {
                vec![Inline::Text(collect_container_text(it))]
            } else {
                vec![Inline::Strike(parse_inlines(it, task, depth + 1))]
            }
        }
        Event::Start(Tag::Link { dest_url, .. }) => {
            if depth >= MAX_NESTING {
                vec![Inline::Text(collect_container_text(it))]
            } else {
                let label = parse_inlines(it, task, depth + 1);
                vec![Inline::Link {
                    label,
                    url: dest_url.into_string(),
                }]
            }
        }
        Event::Start(Tag::Image { dest_url, .. }) => {
            if depth >= MAX_NESTING {
                vec![Inline::Text(collect_container_text(it))]
            } else {
                let alt = parse_inlines(it, task, depth + 1);
                vec![Inline::Image {
                    alt,
                    url: dest_url.into_string(),
                }]
            }
        }
        Event::Start(Tag::HtmlBlock) => {
            let _ = collect_container_text(it);
            Vec::new()
        }
        Event::Start(_) => {
            let text = collect_container_text(it);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Inline::Text(text)]
            }
        }
        Event::End(_) => Vec::new(),
    }
}

fn collect_inline_run(
    it: &mut std::iter::Peekable<Parser<'_>>,
    task: &mut Option<bool>,
    depth: usize,
) -> Vec<Inline> {
    let mut out = Vec::new();
    while let Some(event) = it.peek() {
        match event {
            Event::End(_) | Event::Rule => break,
            Event::Start(tag) if is_block_tag(tag) => break,
            Event::Html(_) | Event::InlineHtml(_) => {
                it.next();
            }
            _ => out.extend(parse_one_inline(it, task, depth)),
        }
    }
    out
}

fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::Item
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_)
    )
}

fn collect_container_text(it: &mut std::iter::Peekable<Parser<'_>>) -> String {
    let mut out = String::new();
    let mut depth = 1usize;
    for event in it.by_ref() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(text)
            | Event::Code(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => out.push_str(&text),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push('\n'),
            Event::Rule => out.push('—'),
            _ => {}
        }
    }
    out
}

fn strip_task_markers(mut content: Vec<Inline>) -> Vec<Inline> {
    content.retain(|inline| !matches!(inline, Inline::TaskMarker(_)));
    content
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn align_of(align: Alignment) -> Align {
    match align {
        Alignment::None => Align::None,
        Alignment::Left => Align::Left,
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Right,
    }
}

/// Flatten inline content into one text buffer plus equal-styled spans.
pub fn flatten(content: &[Inline]) -> InlineText {
    let mut out = InlineText::default();
    flatten_into(content, &InlineStyle::default(), &mut out, 0);
    out
}

fn flatten_into(content: &[Inline], style: &InlineStyle, out: &mut InlineText, depth: usize) {
    if depth >= MAX_NESTING {
        push_text(out, &plain_text(content), style.clone());
        return;
    }
    for inline in content {
        match inline {
            Inline::Text(text) => push_text(out, text, style.clone()),
            Inline::Code(code) => {
                let mut style = style.clone();
                style.code = true;
                push_text(out, code, style);
            }
            Inline::SoftBreak => push_text(out, " ", style.clone()),
            Inline::HardBreak => push_text(out, "\n", style.clone()),
            Inline::Strong(inner) => {
                let mut style = style.clone();
                style.strong = true;
                flatten_into(inner, &style, out, depth + 1);
            }
            Inline::Emphasis(inner) => {
                let mut style = style.clone();
                style.emphasis = true;
                flatten_into(inner, &style, out, depth + 1);
            }
            Inline::Strike(inner) => {
                let mut style = style.clone();
                style.strike = true;
                flatten_into(inner, &style, out, depth + 1);
            }
            Inline::Link { label, url } => {
                let mut style = style.clone();
                style.link = Some(url.clone());
                flatten_into(label, &style, out, depth + 1);
            }
            Inline::Image { alt, url } => {
                push_text(out, "🖼 ", style.clone());
                let mut style = style.clone();
                style.link = Some(url.clone());
                flatten_into(alt, &style, out, depth + 1);
            }
            Inline::TaskMarker(_) => {}
        }
    }
}

fn push_text(out: &mut InlineText, text: &str, style: InlineStyle) {
    if text.is_empty() {
        return;
    }
    let start = out.text.len();
    out.text.push_str(text);
    let end = out.text.len();
    if let Some(last) = out.spans.last_mut() {
        if last.style == style && last.range.end == start {
            last.range.end = end;
            return;
        }
    }
    out.spans.push(InlineSpan {
        range: start..end,
        style,
    });
}

fn plain_text(content: &[Inline]) -> String {
    let mut out = String::new();
    let mut stack: Vec<std::slice::Iter<'_, Inline>> = vec![content.iter()];
    while let Some(iter) = stack.last_mut() {
        let Some(inline) = iter.next() else {
            stack.pop();
            continue;
        };
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::SoftBreak => out.push(' '),
            Inline::HardBreak => out.push('\n'),
            Inline::Strong(inner) | Inline::Emphasis(inner) | Inline::Strike(inner) => {
                stack.push(inner.iter());
            }
            Inline::Link { label, .. } => stack.push(label.iter()),
            Inline::Image { alt, .. } => {
                out.push_str("🖼 ");
                stack.push(alt.iter());
            }
            Inline::TaskMarker(_) => {}
        }
    }
    out
}

/// A content-addressed cache of parsed documents, reset once per feed rebuild.
///
/// Entries survive across generations until [`MarkdownCache::end_refresh`], so
/// a completed block whose source did not change keeps its `Rc` from the
/// previous revision. Only changed or new blocks are parsed again.
#[derive(Default)]
pub struct MarkdownCache {
    generation: u64,
    entries: HashMap<(u64, usize), (u64, Rc<MarkdownDoc>)>,
}

impl MarkdownCache {
    /// Open a new refresh generation.
    pub fn begin_refresh(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Return the parsed document for `source`, reusing the cached `Rc` when the
    /// same bytes were already parsed (in this or an earlier generation).
    pub fn doc(&mut self, source: &str) -> Rc<MarkdownDoc> {
        let key = content_key(source);
        if let Some((generation, doc)) = self.entries.get_mut(&key) {
            *generation = self.generation;
            return doc.clone();
        }
        let doc = Rc::new(parse(source));
        self.entries.insert(key, (self.generation, doc.clone()));
        doc
    }

    /// Drop entries that were not requested since the last `begin_refresh`.
    pub fn end_refresh(&mut self) {
        let generation = self.generation;
        self.entries.retain(|_, (entry, _)| *entry == generation);
    }
}

fn content_key(source: &str) -> (u64, usize) {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    (hasher.finish(), source.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_splits_at_the_first_separator_line() {
        let (header, body) = split_frontmatter("agent = \"x\"\nkind = \"task\"\n---\n\n# Title\n");
        assert_eq!(header, Some("agent = \"x\"\nkind = \"task\""));
        assert_eq!(body, "\n# Title\n");

        let (header, body) = split_frontmatter("---\n\n# Title\n");
        assert_eq!(header, None);
        assert_eq!(body, "\n# Title\n");

        let (header, body) = split_frontmatter("agent = \"x\"\r\n---\r\n\r\n# Title\r\n");
        assert_eq!(header, Some("agent = \"x\""));
        assert_eq!(body, "\r\n# Title\r\n");
    }

    #[test]
    fn frontmatter_without_a_separator_is_all_body() {
        let source = "# Title\n\nno front matter\n";
        assert_eq!(split_frontmatter(source), (None, source));
        assert_eq!(split_frontmatter(""), (None, ""));
    }

    #[test]
    fn frontmatter_only_looks_at_the_first_separator() {
        let source = "agent = \"x\"\n---\n## Body\n\n---\n\nmore\n";
        let (header, body) = split_frontmatter(source);
        assert_eq!(header, Some("agent = \"x\""));
        assert_eq!(body, "## Body\n\n---\n\nmore\n");

        // The split is line-based and does not try to understand fences: a
        // `---` line inside a fenced block still splits if it comes first.
        let (header, body) = split_frontmatter("```\n---\n```\n");
        assert_eq!(header, Some("```"));
        assert_eq!(body, "```\n");
    }

    fn first_paragraph(source: &str) -> Vec<Inline> {
        match parse(source).blocks.into_iter().next() {
            Some(Block::Paragraph(content)) => content,
            other => panic!("expected paragraph, got {other:?}"),
        }
    }

    fn heading_levels(source: &str) -> Vec<(u8, String)> {
        parse(source)
            .blocks
            .into_iter()
            .filter_map(|block| match block {
                Block::Heading { level, content } => Some((level, flatten(&content).text)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn headings_keep_their_levels_and_inline_content() {
        let source = "# One\n\n## Two\n\n### Three\n\n#### Four\n\n##### Five\n\n###### Six\n";
        assert_eq!(
            heading_levels(source),
            vec![
                (1, "One".to_string()),
                (2, "Two".to_string()),
                (3, "Three".to_string()),
                (4, "Four".to_string()),
                (5, "Five".to_string()),
                (6, "Six".to_string()),
            ]
        );
    }

    #[test]
    fn paragraphs_keep_nested_inline_emphasis() {
        let content = first_paragraph("plain **bold** *italic* ~~gone~~ `code`");
        assert!(content
            .iter()
            .any(|inline| matches!(inline, Inline::Strong(_))));
        assert!(content
            .iter()
            .any(|inline| matches!(inline, Inline::Emphasis(_))));
        assert!(content
            .iter()
            .any(|inline| matches!(inline, Inline::Strike(_))));
        assert!(content
            .iter()
            .any(|inline| matches!(inline, Inline::Code(_))));
    }

    #[test]
    fn unordered_ordered_and_nested_lists_parse() {
        let doc = parse("- a\n  - nested\n- b\n\n1. one\n2. two\n");
        assert_eq!(doc.blocks.len(), 2);
        match &doc.blocks[0] {
            Block::List {
                ordered,
                start,
                items,
            } => {
                assert!(!ordered);
                assert_eq!(*start, 1);
                assert_eq!(items.len(), 2);
                assert!(items[0]
                    .blocks
                    .iter()
                    .any(|block| matches!(block, Block::List { .. })));
            }
            other => panic!("expected unordered list, got {other:?}"),
        }
        match &doc.blocks[1] {
            Block::List {
                ordered,
                start,
                items,
            } => {
                assert!(ordered);
                assert_eq!(*start, 1);
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected ordered list, got {other:?}"),
        }
    }

    #[test]
    fn task_list_markers_are_lifted_out_of_the_paragraph() {
        let doc = parse("- [x] done\n- [ ] todo\n- [ ]\n");
        let Block::List { items, .. } = &doc.blocks[0] else {
            panic!("expected list, got {:?}", doc.blocks);
        };
        assert_eq!(items[0].task, Some(true));
        // The marker text lives in the paragraph, not as a literal "[x]".
        let first_text = match &items[0].blocks[0] {
            Block::Paragraph(content) => flatten(content).text,
            other => panic!("expected paragraph, got {other:?}"),
        };
        assert_eq!(first_text, "done");
        assert_eq!(items[1].task, Some(false));
        assert_eq!(items[2].task, Some(false));
        assert!(items[2].blocks.is_empty());
    }

    #[test]
    fn gfm_tables_capture_header_rows_and_alignment() {
        let doc = parse("| a | b |\n| :- | -: |\n| 1 | 2 |\n");
        match &doc.blocks[0] {
            Block::Table { align, head, rows } => {
                assert_eq!(align, &vec![Align::Left, Align::Right]);
                assert_eq!(head.len(), 2);
                assert_eq!(flatten(&head[0]).text, "a");
                assert_eq!(rows.len(), 1);
                assert_eq!(flatten(&rows[0][1]).text, "2");
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn empty_table_header_is_tolerated() {
        let doc = parse("| | |\n|---|---|\n| x | y |\n");
        let Block::Table { rows, .. } = &doc.blocks[0] else {
            panic!("expected table, got {:?}", doc.blocks);
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(flatten(&rows[0][0]).text, "x");
    }

    #[test]
    fn fenced_code_keeps_language_and_body_without_the_added_newline() {
        let doc = parse("```rust\nfn main() {}\n```\n");
        match &doc.blocks[0] {
            Block::Code { language, code } => {
                assert_eq!(language, "rust");
                assert_eq!(code, "fn main() {}");
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn unclosed_fence_consumes_to_eof() {
        let doc = parse("```rust\nfn main() {");
        match &doc.blocks[0] {
            Block::Code { language, code } => {
                assert_eq!(language, "rust");
                assert_eq!(code, "fn main() {");
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn raw_html_is_dropped_and_never_blanks_a_message() {
        let doc = parse("<script>alert(1)</script>\n\ntext\n");
        let text: String = doc
            .blocks
            .iter()
            .map(|block| match block {
                Block::Paragraph(content) => flatten(content).text,
                _ => String::new(),
            })
            .collect();
        assert!(!text.contains("<script"));
        assert!(text.contains("text"));

        let only_html = parse("<script>alert(1)</script>");
        match &only_html.blocks[0] {
            Block::Paragraph(content) => {
                assert_eq!(flatten(content).text, "<script>alert(1)</script>");
            }
            other => panic!("expected literal fallback paragraph, got {other:?}"),
        }
    }

    #[test]
    fn partial_markdown_does_not_panic_or_lose_text() {
        for source in [
            "**unclosed",
            "[link](https://exa",
            "```rust\nfn main() {",
            "",
        ] {
            let doc = parse(source);
            if source.is_empty() {
                assert!(doc.blocks.is_empty());
            } else {
                assert!(!doc.blocks.is_empty());
            }
        }
        assert_eq!(flatten(&first_paragraph("**unclosed")).text, "**unclosed");
        assert!(flatten(&first_paragraph("[link](https://exa"))
            .text
            .contains("[link]"));
    }

    #[test]
    fn deep_nesting_is_capped_without_panicking() {
        let quote = format!("{}deep", "> ".repeat(200));
        let doc = parse(&quote);
        assert!(flatten_doc(&doc).contains("deep"));

        let mut list = String::new();
        for level in 0..200 {
            list.push_str(&"  ".repeat(level));
            list.push_str("- item\n");
        }
        let doc = parse(&list);
        assert!(flatten_doc(&doc).contains("item"));
    }

    fn flatten_doc(doc: &MarkdownDoc) -> String {
        fn walk(blocks: &[Block], out: &mut String) {
            for block in blocks {
                match block {
                    Block::Paragraph(content) | Block::Heading { content, .. } => {
                        out.push_str(&flatten(content).text)
                    }
                    Block::Quote(inner) => walk(inner, out),
                    Block::List { items, .. } => {
                        for item in items {
                            walk(&item.blocks, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut out = String::new();
        walk(&doc.blocks, &mut out);
        out
    }

    #[test]
    fn flatten_handles_breaks_styles_and_unicode_ranges() {
        let soft = flatten(&[
            Inline::Text("a".into()),
            Inline::SoftBreak,
            Inline::Text("b".into()),
        ]);
        assert_eq!(soft.text, "a b");

        let hard = flatten(&[
            Inline::Text("a".into()),
            Inline::HardBreak,
            Inline::Text("b".into()),
        ]);
        assert_eq!(hard.text, "a\nb");

        let nested = flatten(&[Inline::Strong(vec![Inline::Link {
            label: vec![Inline::Text("docs".into())],
            url: "https://example.com".into(),
        }])]);
        assert_eq!(nested.text, "docs");
        assert_eq!(nested.spans.len(), 1);
        assert!(nested.spans[0].style.strong);
        assert_eq!(
            nested.spans[0].style.link.as_deref(),
            Some("https://example.com")
        );

        let merged = flatten(&[Inline::Text("a".into()), Inline::Text("b".into())]);
        assert_eq!(merged.spans.len(), 1);

        let unicode = flatten(&[
            Inline::Text("héllo → 世界 ".into()),
            Inline::Strong(vec![Inline::Text("🚀".into())]),
        ]);
        assert_eq!(unicode.text, "héllo → 世界 🚀");
        for span in &unicode.spans {
            assert!(unicode.text.is_char_boundary(span.range.start));
            assert!(unicode.text.is_char_boundary(span.range.end));
        }
    }

    #[test]
    fn cache_reuses_completed_blocks_and_evicts_stale_ones() {
        let mut cache = MarkdownCache::default();
        cache.begin_refresh();
        let first = cache.doc("# one");
        let again = cache.doc("# one");
        assert!(Rc::ptr_eq(&first, &again));
        let changed = cache.doc("# two");
        assert!(!Rc::ptr_eq(&first, &changed));
        cache.end_refresh();

        cache.begin_refresh();
        let reused = cache.doc("# one");
        assert!(Rc::ptr_eq(&first, &reused));
        let fresh = cache.doc("# three");
        cache.end_refresh();
        assert!(!Rc::ptr_eq(&changed, &fresh));
        // The untouched "# two" entry is gone after the refresh.
        cache.begin_refresh();
        let reparsed = cache.doc("# two");
        assert!(!Rc::ptr_eq(&changed, &reparsed));
        cache.end_refresh();
        assert!(cache.entries.len() <= 2);
    }
}
