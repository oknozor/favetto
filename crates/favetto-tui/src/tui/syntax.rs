//! Small, dependency-free syntax highlighting for fenced code blocks.
//!
//! The TUI renders code inside a feed that must stay readable even when a
//! language is not recognised. Rather than pull in a full grammar engine, this
//! module tokenises the handful of constructs that matter in a terminal:
//! comments, strings, numbers, keywords, types, and function calls. JSON is
//! delegated to the existing [`super::json`] highlighter, and TOML to the task
//! preview highlighter, so those two keep their established colours.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// The language name from a fence info string, lower-cased and stripped of
/// decoration such as `{.rust}`. Only the first token is significant.
pub fn language_name(info: &str) -> String {
    let token = info
        .trim()
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '{' | '}'))
        .find(|token| !token.is_empty())
        .unwrap_or_default();
    token
        .trim_start_matches('.')
        .trim_matches('"')
        .to_ascii_lowercase()
}

/// Highlight a fenced code block. Unknown languages fall back to the plain code
/// colour, preserving the source exactly.
pub fn highlight(language: &str, source: &str, theme: &Theme) -> Vec<Line<'static>> {
    let name = language_name(language);
    match name.as_str() {
        "diff" | "patch" => highlight_diff(source, theme),
        _ => match spec(&name) {
            Some(spec) => {
                let mut in_block_comment = false;
                source
                    .lines()
                    .map(|line| highlight_line(line, spec, theme, &mut in_block_comment))
                    .collect()
            }
            None => source
                .lines()
                .map(|line| plain(line, theme.syn_code()))
                .collect(),
        },
    }
}

fn plain(text: &str, color: ratatui::style::Color) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(color)))
}

// ---------------------------------------------------------------------------
// Language specs
// ---------------------------------------------------------------------------

struct Spec {
    keywords: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "yield",
];

const RUST: Spec = Spec {
    keywords: RUST_KEYWORDS,
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"'],
};

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "match", "case",
];

const PYTHON: Spec = Spec {
    keywords: PYTHON_KEYWORDS,
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
};

const JAVASCRIPT_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "namespace",
    "new",
    "null",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "satisfies",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const JAVASCRIPT: Spec = Spec {
    keywords: JAVASCRIPT_KEYWORDS,
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
};

const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
    "true",
    "false",
    "nil",
    "iota",
];

const GO: Spec = Spec {
    keywords: GO_KEYWORDS,
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
};

const C_KEYWORDS: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "constexpr",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "explicit",
    "extern",
    "false",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "nullptr",
    "operator",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];

const C: Spec = Spec {
    keywords: C_KEYWORDS,
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
};

const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
];

const JAVA: Spec = Spec {
    keywords: JAVA_KEYWORDS,
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
};

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "in", "select", "time", "return", "export", "local", "readonly", "declare",
    "unset", "shift", "source", "alias", "true", "false",
];

const SHELL: Spec = Spec {
    keywords: SHELL_KEYWORDS,
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\'', '`'],
};

const YAML_KEYWORDS: &[&str] = &["true", "false", "null", "yes", "no", "on", "off"];

const YAML: Spec = Spec {
    keywords: YAML_KEYWORDS,
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
};

const SQL_KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "case",
    "create",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "foreign",
    "from",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "right",
    "select",
    "set",
    "table",
    "then",
    "union",
    "unique",
    "update",
    "values",
    "when",
    "where",
];

const SQL: Spec = Spec {
    keywords: SQL_KEYWORDS,
    line_comments: &["--"],
    block_comment: Some(("/*", "*/")),
    quotes: &['\'', '"'],
};

fn spec(language: &str) -> Option<&'static Spec> {
    Some(match language {
        "rust" | "rs" => &RUST,
        "python" | "py" => &PYTHON,
        "javascript" | "js" | "jsx" | "typescript" | "ts" | "tsx" => &JAVASCRIPT,
        "go" | "golang" => &GO,
        "c" | "h" | "cpp" | "c++" | "cc" | "cxx" | "hpp" => &C,
        "java" => &JAVA,
        "bash" | "sh" | "shell" | "zsh" => &SHELL,
        "yaml" | "yml" => &YAML,
        "sql" => &SQL,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Tokeniser
// ---------------------------------------------------------------------------

fn highlight_line(
    line: &str,
    spec: &Spec,
    theme: &Theme,
    in_block_comment: &mut bool,
) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let base = Style::default().fg(theme.syn_code());
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        if *in_block_comment {
            let end = spec.block_comment.map(|(_, end)| end).unwrap_or_default();
            if let Some(pos) = find(&chars, i, end) {
                let text: String = chars[i..pos + end.chars().count()].iter().collect();
                spans.push(Span::styled(text, comment_style(theme)));
                i = pos + end.chars().count();
                *in_block_comment = false;
            } else {
                let text: String = chars[i..].iter().collect();
                spans.push(Span::styled(text, comment_style(theme)));
                i = chars.len();
            }
            continue;
        }

        if spec
            .line_comments
            .iter()
            .any(|marker| starts_with(&chars, i, marker))
        {
            flush(&mut buf, &mut spans, base);
            let text: String = chars[i..].iter().collect();
            spans.push(Span::styled(text, comment_style(theme)));
            i = chars.len();
            continue;
        }

        if let Some((start, end)) = spec.block_comment {
            if starts_with(&chars, i, start) {
                flush(&mut buf, &mut spans, base);
                if let Some(pos) = find(&chars, i + start.chars().count(), end) {
                    let text: String = chars[i..pos + end.chars().count()].iter().collect();
                    spans.push(Span::styled(text, comment_style(theme)));
                    i = pos + end.chars().count();
                } else {
                    let text: String = chars[i..].iter().collect();
                    spans.push(Span::styled(text, comment_style(theme)));
                    *in_block_comment = true;
                    i = chars.len();
                }
                continue;
            }
        }

        if spec.quotes.contains(&chars[i]) {
            flush(&mut buf, &mut spans, base);
            let (text, next) = read_string(&chars, i, chars[i]);
            spans.push(Span::styled(text, Style::default().fg(theme.syn_string())));
            i = next;
            continue;
        }

        if chars[i].is_ascii_digit() {
            flush(&mut buf, &mut spans, base);
            let next = read_number(&chars, i);
            let text: String = chars[i..next].iter().collect();
            spans.push(Span::styled(text, Style::default().fg(theme.syn_number())));
            i = next;
            continue;
        }

        if is_ident_start(chars[i]) {
            flush(&mut buf, &mut spans, base);
            let next = read_ident(&chars, i);
            let word: String = chars[i..next].iter().collect();
            let style = word_style(&word, &chars, next, spec, theme);
            spans.push(Span::styled(word, style));
            i = next;
            continue;
        }

        if is_operator(chars[i]) {
            flush(&mut buf, &mut spans, base);
            spans.push(Span::styled(
                chars[i].to_string(),
                Style::default().fg(theme.syntax.operator),
            ));
            i += 1;
            continue;
        }

        if is_punctuation(chars[i]) {
            flush(&mut buf, &mut spans, base);
            spans.push(Span::styled(
                chars[i].to_string(),
                Style::default().fg(theme.syntax.punctuation),
            ));
            i += 1;
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush(&mut buf, &mut spans, base);
    Line::from(spans)
}

fn word_style(word: &str, chars: &[char], next: usize, spec: &Spec, theme: &Theme) -> Style {
    if spec.keywords.contains(&word) {
        return Style::default()
            .fg(theme.syntax.keyword)
            .add_modifier(Modifier::BOLD);
    }
    if matches!(word, "true" | "false" | "null" | "none" | "nil") {
        return Style::default()
            .fg(theme.syn_bool())
            .add_modifier(Modifier::BOLD);
    }
    let after = chars[next..].iter().find(|c| !c.is_whitespace()).copied();
    if after == Some('(') {
        return Style::default().fg(theme.syntax.function);
    }
    if word.chars().next().is_some_and(char::is_uppercase) {
        return Style::default().fg(theme.syntax.type_);
    }
    Style::default().fg(theme.syn_code())
}

fn comment_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.syn_comment())
        .add_modifier(Modifier::ITALIC)
}

fn read_string(chars: &[char], start: usize, quote: char) -> (String, usize) {
    let mut text = String::new();
    let mut i = start;
    while i < chars.len() {
        text.push(chars[i]);
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            text.push(chars[i]);
        } else if chars[i] == quote && i > start {
            i += 1;
            break;
        }
        i += 1;
    }
    (text, i)
}

fn read_number(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() || c == '_' {
            i += 1;
            continue;
        }
        if c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
            i += 1;
            continue;
        }
        break;
    }
    i
}

fn read_ident(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    while i < chars.len() && is_ident_continue(chars[i]) {
        i += 1;
    }
    i
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

fn is_operator(c: char) -> bool {
    matches!(
        c,
        '+' | '-' | '*' | '/' | '%' | '=' | '!' | '<' | '>' | '&' | '|' | '^' | '~' | '?' | ':'
    )
}

fn is_punctuation(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '.')
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    !needle.is_empty()
        && chars.len() >= at + needle.len()
        && chars[at..at + needle.len()] == needle[..]
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || from >= chars.len() || needle.len() > chars.len() {
        return None;
    }
    let last = chars.len() - needle.len();
    if from > last {
        return None;
    }
    (from..=last).find(|&i| chars[i..i + needle.len()] == needle[..])
}

fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), style));
    }
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

fn highlight_diff(source: &str, theme: &Theme) -> Vec<Line<'static>> {
    source
        .lines()
        .map(|line| {
            let style = if line.starts_with("+++") || line.starts_with("---") {
                Style::default()
                    .fg(theme.syn_comment())
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with("@@") {
                Style::default()
                    .fg(theme.info())
                    .add_modifier(Modifier::BOLD)
            } else if line.starts_with('+') {
                Style::default().fg(theme.success())
            } else if line.starts_with('-') {
                Style::default().fg(theme.danger())
            } else if line.starts_with("diff ") {
                Style::default()
                    .fg(theme.accent())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.syn_code())
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn text_of(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn concat(lines: &[Line<'static>]) -> String {
        lines.iter().map(text_of).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn language_names_are_normalised() {
        assert_eq!(language_name("Rust"), "rust");
        assert_eq!(language_name("rust,no_run"), "rust");
        assert_eq!(language_name("{.json}"), "json");
        assert_eq!(language_name("  TypeScript title=x"), "typescript");
    }

    #[test]
    fn rust_highlights_keywords_strings_and_comments() {
        let theme = Theme::dark();
        let lines = highlight(
            "rust",
            "fn main() {\n    // greeting\n    let name = \"world\";\n}",
            &theme,
        );
        assert_eq!(
            concat(&lines),
            "fn main() {\n    // greeting\n    let name = \"world\";\n}"
        );

        let spans: Vec<_> = lines.into_iter().flat_map(|l| l.spans).collect();
        let keyword = spans.iter().find(|s| s.content == "fn").unwrap();
        assert_eq!(keyword.style.fg, Some(theme.syntax.keyword));
        let comment = spans
            .iter()
            .find(|s| s.content.contains("greeting"))
            .unwrap();
        assert_eq!(comment.style.fg, Some(theme.syn_comment()));
        let string = spans.iter().find(|s| s.content == "\"world\"").unwrap();
        assert_eq!(string.style.fg, Some(theme.syn_string()));
        let function = spans.iter().find(|s| s.content == "main").unwrap();
        assert_eq!(function.style.fg, Some(theme.syntax.function));
    }

    #[test]
    fn unknown_languages_are_plain_but_preserved() {
        let theme = Theme::dark();
        let source = "let x = **not markdown**;\n<not html>";
        let lines = highlight("invented-language", source, &theme);
        assert_eq!(concat(&lines), source);
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.fg == Some(theme.syn_code())));
    }

    #[test]
    fn block_comments_span_lines() {
        let theme = Theme::dark();
        let lines = highlight("javascript", "/* start\nmiddle */\nconst x = 1;", &theme);
        assert_eq!(concat(&lines), "/* start\nmiddle */\nconst x = 1;");
        let spans: Vec<_> = lines.into_iter().flat_map(|l| l.spans).collect();
        assert!(spans
            .iter()
            .any(|s| s.content.contains("middle") && s.style.fg == Some(theme.syn_comment())));
        assert!(spans
            .iter()
            .any(|s| s.content == "const" && s.style.fg == Some(theme.syntax.keyword)));
    }

    #[test]
    fn diff_lines_use_status_colours() {
        let theme = Theme::dark();
        let lines = highlight("diff", "-old\n+new\n context", &theme);
        let spans: Vec<_> = lines.into_iter().flat_map(|l| l.spans).collect();
        assert!(spans
            .iter()
            .any(|s| s.content == "-old" && s.style.fg == Some(theme.danger())));
        assert!(spans
            .iter()
            .any(|s| s.content == "+new" && s.style.fg == Some(theme.success())));
    }

    #[test]
    fn unknown_span_colour_is_not_the_code_colour() {
        let theme = Theme::dark();
        assert_ne!(theme.syn_code(), Color::Reset);
    }
}
