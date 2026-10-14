//! Markdown → Dioxus rendering via `pulldown-cmark` (wasm-safe).

use dioxus::prelude::*;
use pulldown_cmark::{html, Event, Options, Parser};

/// Split a catalog `.md` file into its TOML front-matter header and Markdown
/// body. The separator is the first line whose trimmed content is exactly `---`.
/// A file with no separator is all body.
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

/// Render CommonMark (with the GFM extensions the catalog uses) to an HTML
/// string. Raw HTML blocks/inlines are dropped rather than forwarded.
pub fn render_markdown(source: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(source, options)
        .filter(|event| !matches!(event, Event::Html(_) | Event::InlineHtml(_)));
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// A rendered Markdown block. The HTML is produced by [`render_markdown`],
/// which strips raw HTML, so the `dangerous_inner_html` sink is safe here.
#[component]
pub fn Markdown(source: String) -> Element {
    let html = render_markdown(&source);
    rsx! {
        div { class: "markdown-body", dangerous_inner_html: "{html}" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings_code_blocks_lists_and_links() {
        let source = "# H1\n\n```rust\nlet x = 1;\n```\n\n- a\n- b\n\n[x](https://e)\n";
        let html = render_markdown(source);
        assert!(html.contains("<h1"), "heading missing from {html}");
        assert!(
            html.contains("<pre><code class=\"language-rust\""),
            "fenced code block missing from {html}"
        );
        assert!(html.contains("<ul>"), "list missing from {html}");
        assert!(
            html.contains("<a href=\"https://e\""),
            "link missing from {html}"
        );
    }

    #[test]
    fn raw_html_is_not_passed_through() {
        let html = render_markdown("<script>alert(1)</script>\n\ntext\n");
        assert!(!html.contains("<script"), "raw html leaked into {html}");
        assert!(html.contains("text"));
    }

    #[test]
    fn frontmatter_is_split_from_the_body() {
        let (header, body) = split_frontmatter("agent = \"x\"\n---\n\n# Title\n");
        assert_eq!(header, Some("agent = \"x\""));
        assert!(body.contains("# Title"));
    }

    #[test]
    fn no_separator_returns_the_whole_body() {
        let (header, body) = split_frontmatter("# Title\n\nno front matter\n");
        assert_eq!(header, None);
        assert_eq!(body, "# Title\n\nno front matter\n");
    }
}
