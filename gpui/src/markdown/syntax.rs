//! Fenced-code tokenisation for GPUI Markdown code blocks.
//!
//! Ported from `crates/favetto-tui/src/tui/syntax.rs` and
//! `crates/favetto-tui/src/tui/json.rs` (same language table and token rules),
//! but returning semantic [`TokenKind`]s instead of ratatui styles so the GPUI
//! client stays independent of the terminal UI. Tokens always concatenate to
//! the source exactly, including whitespace and newlines.

/// The semantic class of one token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
    Type,
    Function,
    Key,
    Operator,
    Punctuation,
}

/// One highlighted run of a fenced code block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub kind: TokenKind,
}

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

/// Highlight a fenced code block. Unknown languages stay plain, preserving the
/// source exactly.
pub fn highlight(language: &str, source: &str) -> Vec<Token> {
    let name = language_name(language);
    match name.as_str() {
        "diff" | "patch" => highlight_diff(source),
        "json" => highlight_json(source),
        "toml" => highlight_toml(source),
        _ => match spec(&name) {
            Some(spec) => highlight_code(source, spec),
            None => vec![Token {
                text: source.to_string(),
                kind: TokenKind::Plain,
            }],
        },
    }
}

// ---------------------------------------------------------------------------
// Language specs (kept in sync with the TUI tokenizer)
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
// Generic tokenizer
// ---------------------------------------------------------------------------

fn highlight_code(source: &str, spec: &Spec) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut in_block_comment = false;
    let mut i = 0;

    while i < chars.len() {
        if in_block_comment {
            let end = spec.block_comment.map(|(_, end)| end).unwrap_or_default();
            if let Some(pos) = find(&chars, i, end) {
                let text: String = chars[i..pos + end.chars().count()].iter().collect();
                push(&mut tokens, &text, TokenKind::Comment);
                i = pos + end.chars().count();
                in_block_comment = false;
            } else {
                let text: String = chars[i..].iter().collect();
                push(&mut tokens, &text, TokenKind::Comment);
                i = chars.len();
            }
            continue;
        }

        if spec
            .line_comments
            .iter()
            .any(|marker| starts_with(&chars, i, marker))
        {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            let end = chars[i..]
                .iter()
                .position(|c| *c == '\n')
                .map(|pos| i + pos)
                .unwrap_or(chars.len());
            let text: String = chars[i..end].iter().collect();
            push(&mut tokens, &text, TokenKind::Comment);
            i = end;
            continue;
        }

        if let Some((start, end)) = spec.block_comment {
            if starts_with(&chars, i, start) {
                flush(&mut buf, &mut tokens, TokenKind::Plain);
                if let Some(pos) = find(&chars, i + start.chars().count(), end) {
                    let text: String = chars[i..pos + end.chars().count()].iter().collect();
                    push(&mut tokens, &text, TokenKind::Comment);
                    i = pos + end.chars().count();
                } else {
                    let text: String = chars[i..].iter().collect();
                    push(&mut tokens, &text, TokenKind::Comment);
                    in_block_comment = true;
                    i = chars.len();
                }
                continue;
            }
        }

        if spec.quotes.contains(&chars[i]) {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            let (text, next) = read_string(&chars, i, chars[i]);
            push(&mut tokens, &text, TokenKind::String);
            i = next;
            continue;
        }

        if chars[i].is_ascii_digit() {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            let next = read_number(&chars, i);
            let text: String = chars[i..next].iter().collect();
            push(&mut tokens, &text, TokenKind::Number);
            i = next;
            continue;
        }

        if is_ident_start(chars[i]) {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            let next = read_ident(&chars, i);
            let word: String = chars[i..next].iter().collect();
            let kind = word_kind(&word, &chars, next, spec);
            push(&mut tokens, &word, kind);
            i = next;
            continue;
        }

        if is_operator(chars[i]) {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            push(&mut tokens, &chars[i].to_string(), TokenKind::Operator);
            i += 1;
            continue;
        }

        if is_punctuation(chars[i]) {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            push(&mut tokens, &chars[i].to_string(), TokenKind::Punctuation);
            i += 1;
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }

    flush(&mut buf, &mut tokens, TokenKind::Plain);
    tokens
}

fn word_kind(word: &str, chars: &[char], next: usize, spec: &Spec) -> TokenKind {
    if spec.keywords.contains(&word) {
        return TokenKind::Keyword;
    }
    if matches!(word, "true" | "false" | "null" | "none" | "nil") {
        return TokenKind::Keyword;
    }
    let after = chars[next..].iter().find(|c| !c.is_whitespace()).copied();
    if after == Some('(') {
        return TokenKind::Function;
    }
    if word.chars().next().is_some_and(char::is_uppercase) {
        return TokenKind::Type;
    }
    TokenKind::Plain
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn highlight_json(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            let (string, next) = read_string(&chars, i, '"');
            let mut j = next;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let kind = if chars.get(j) == Some(&':') {
                TokenKind::Key
            } else {
                TokenKind::String
            };
            push(&mut tokens, &string, kind);
            i = next;
            continue;
        }
        if c == '-' || c.is_ascii_digit() {
            flush(&mut buf, &mut tokens, TokenKind::Plain);
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || matches!(chars[j], '.' | '-' | '+' | 'e' | 'E'))
            {
                j += 1;
            }
            let number: String = chars[i..j].iter().collect();
            push(&mut tokens, &number, TokenKind::Number);
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let word: String = chars[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            if matches!(word.as_str(), "true" | "false" | "null") {
                flush(&mut buf, &mut tokens, TokenKind::Plain);
                push(&mut tokens, &word, TokenKind::Keyword);
                i += word.chars().count();
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }

    flush(&mut buf, &mut tokens, TokenKind::Plain);
    tokens
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

fn highlight_toml(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            push(&mut tokens, line, TokenKind::Comment);
            continue;
        }
        if trimmed.starts_with('[') {
            push(&mut tokens, line, TokenKind::Keyword);
            continue;
        }
        match line.split_once('=') {
            Some((key, value)) => {
                push(&mut tokens, key, TokenKind::Key);
                push(&mut tokens, "=", TokenKind::Operator);
                toml_value(value, &mut tokens);
            }
            None => push(&mut tokens, line, TokenKind::Plain),
        }
    }
    tokens
}

fn toml_value(value: &str, tokens: &mut Vec<Token>) {
    let chars: Vec<char> = value.chars().collect();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' | '\'' => {
                flush(&mut buf, tokens, TokenKind::Number);
                let (text, next) = read_string(&chars, i, chars[i]);
                push(tokens, &text, TokenKind::String);
                i = next;
            }
            '[' | ']' | '{' | '}' | ',' => {
                flush(&mut buf, tokens, TokenKind::Number);
                push(tokens, &chars[i].to_string(), TokenKind::Punctuation);
                i += 1;
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush(&mut buf, tokens, TokenKind::Number);
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

fn highlight_diff(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for line in source.split_inclusive('\n') {
        let kind = if line.starts_with("+++") || line.starts_with("---") {
            TokenKind::Comment
        } else if line.starts_with("@@") {
            TokenKind::Function
        } else if line.starts_with('+') {
            TokenKind::String
        } else if line.starts_with('-') {
            TokenKind::Keyword
        } else if line.starts_with("diff ") {
            TokenKind::Type
        } else {
            TokenKind::Plain
        };
        push(&mut tokens, line, kind);
    }
    tokens
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn flush(buf: &mut String, tokens: &mut Vec<Token>, kind: TokenKind) {
    if !buf.is_empty() {
        push(tokens, &std::mem::take(buf), kind);
    }
}

fn push(tokens: &mut Vec<Token>, text: &str, kind: TokenKind) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = tokens.last_mut() {
        if last.kind == kind {
            last.text.push_str(text);
            return;
        }
    }
    tokens.push(Token {
        text: text.to_string(),
        kind,
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn concat(tokens: &[Token]) -> String {
        tokens.iter().map(|token| token.text.as_str()).collect()
    }

    fn kind_of(tokens: &[Token], text: &str) -> Option<TokenKind> {
        tokens
            .iter()
            .find(|token| token.text == text)
            .map(|token| token.kind)
    }

    #[test]
    fn language_names_are_normalised() {
        assert_eq!(language_name("Rust"), "rust");
        assert_eq!(language_name("rust,no_run"), "rust");
        assert_eq!(language_name("{.json}"), "json");
        assert_eq!(language_name("  TypeScript title=x"), "typescript");
        assert_eq!(language_name(""), "");
    }

    #[test]
    fn json_keys_strings_and_numbers_tokenize_exactly() {
        let source = "{\n  \"name\": \"foo\",\n  \"n\": 42,\n  \"ok\": true\n}";
        let tokens = highlight("json", source);
        assert_eq!(concat(&tokens), source);
        assert_eq!(kind_of(&tokens, "\"name\""), Some(TokenKind::Key));
        assert_eq!(kind_of(&tokens, "\"foo\""), Some(TokenKind::String));
        assert_eq!(kind_of(&tokens, "42"), Some(TokenKind::Number));
        assert_eq!(kind_of(&tokens, "true"), Some(TokenKind::Keyword));
    }

    #[test]
    fn rust_keywords_strings_and_comments_tokenize_exactly() {
        let source = "fn main() {\n    // greeting\n    let name = \"world\";\n}";
        let tokens = highlight("rust", source);
        assert_eq!(concat(&tokens), source);
        assert_eq!(kind_of(&tokens, "fn"), Some(TokenKind::Keyword));
        assert_eq!(kind_of(&tokens, "let"), Some(TokenKind::Keyword));
        assert_eq!(kind_of(&tokens, "\"world\""), Some(TokenKind::String));
        assert_eq!(kind_of(&tokens, "// greeting"), Some(TokenKind::Comment));
        assert_eq!(kind_of(&tokens, "main"), Some(TokenKind::Function));
    }

    #[test]
    fn unknown_and_empty_languages_are_one_plain_token() {
        for language in ["invented", ""] {
            let source = "let x = **not markdown**;\n<not html>";
            let tokens = highlight(language, source);
            assert_eq!(tokens.len(), 1);
            assert_eq!(tokens[0].kind, TokenKind::Plain);
            assert_eq!(tokens[0].text, source);
        }
    }

    #[test]
    fn unterminated_strings_and_comments_consume_the_remainder() {
        let string = highlight("rust", "\"abc");
        assert_eq!(concat(&string), "\"abc");
        assert_eq!(kind_of(&string, "\"abc"), Some(TokenKind::String));

        let comment = highlight("rust", "/* abc");
        assert_eq!(concat(&comment), "/* abc");
        assert_eq!(kind_of(&comment, "/* abc"), Some(TokenKind::Comment));
    }

    #[test]
    fn toml_and_diff_preserve_the_source() {
        let toml = "agent = \"opencode\"\n[task]\nenabled = true\n";
        assert_eq!(concat(&highlight("toml", toml)), toml);

        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(concat(&highlight("diff", diff)), diff);
    }

    #[test]
    fn block_comments_span_lines_and_crlf_is_preserved() {
        let source = "/* start\r\nmiddle */\r\nconst x = 1;\r\n";
        let tokens = highlight("javascript", source);
        assert_eq!(concat(&tokens), source);
        assert_eq!(
            kind_of(&tokens, "/* start\r\nmiddle */"),
            Some(TokenKind::Comment)
        );
    }
}
