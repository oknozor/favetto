//! Dependency-light JSON unwrapping and syntax highlighting for the agent feed.
//!
//! Agent tool input/output is an arbitrary [`serde_json::Value`] that is often
//! double-encoded (a JSON document stored inside a JSON string) or wrapped in a
//! provider envelope such as `{"content":[{"type":"text","text":"…"}]}` or
//! `{"stdout":"…","stderr":"…"}`. [`readable_json`] unwraps those envelopes to
//! plain text when it can and otherwise keeps the value as structured JSON;
//! [`highlight`] tokenizes pretty-printed JSON into coloured [`Token`]s for the
//! feed, mirroring the TUI's `tui/json.rs` highlighter.

use serde_json::{Map, Value};

use crate::components::output_viewer::strip_ansi;

/// The maximum number of nested string/envelope unwraps before giving up, so a
/// self-referential payload cannot recurse without bound.
const MAX_UNWRAP_DEPTH: usize = 6;

/// How a tool input/output value should be rendered in the feed.
#[derive(Debug, Clone, PartialEq)]
pub enum Readable {
    /// Plain, human-readable text (already unescaped).
    Text(String),
    /// Genuinely structured JSON; pretty-print and highlight it.
    Json(Value),
}

impl Readable {
    /// Whether the value has nothing worth showing.
    pub fn is_empty(&self) -> bool {
        match self {
            Readable::Text(text) => text.trim().is_empty(),
            Readable::Json(value) => value.is_null(),
        }
    }
}

/// Turn a tool input/output value into something human-readable.
///
/// Recognizable envelopes are unwrapped to text; anything genuinely structured
/// is kept as JSON for [`highlight`].
pub fn readable_json(value: &Value) -> Readable {
    normalize(value, 0)
}

/// Pretty-print a JSON value, degrading to an empty string on failure.
pub fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn normalize(value: &Value, depth: usize) -> Readable {
    match value {
        Value::Null => Readable::Text(String::new()),
        Value::Bool(value) => Readable::Text(value.to_string()),
        Value::Number(value) => Readable::Text(value.to_string()),
        Value::String(raw) => {
            if depth >= MAX_UNWRAP_DEPTH {
                return Readable::Text(strip_ansi(raw));
            }
            match embedded_json(raw) {
                Some(inner) => normalize(&inner, depth + 1),
                None => Readable::Text(strip_ansi(raw)),
            }
        }
        Value::Array(blocks) => match flatten_blocks(blocks) {
            Some(text) => normalize(&Value::String(text), depth + 1),
            None => Readable::Json(value.clone()),
        },
        Value::Object(map) => unwrap_object(map, value, depth),
    }
}

/// Unwrap the provider envelopes the feed knows how to read; anything else is
/// kept as structured JSON.
fn unwrap_object(map: &Map<String, Value>, original: &Value, depth: usize) -> Readable {
    if let Some(content) = map.get("content") {
        if let Some(text) = content_text(content) {
            return normalize(&Value::String(text), depth + 1);
        }
    }

    let stdout = map.get("stdout").and_then(Value::as_str);
    let stderr = map.get("stderr").and_then(Value::as_str);
    if stdout.is_some() || stderr.is_some() {
        let joined = [stdout, stderr]
            .into_iter()
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return normalize(&Value::String(joined), depth + 1);
    }

    Readable::Json(original.clone())
}

fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => flatten_blocks(blocks),
        _ => None,
    }
}

/// Flatten content blocks (`[{ "type": "text", "text": "…" }]`) to plain text.
fn flatten_blocks(blocks: &[Value]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            Value::String(text) => parts.push(text.clone()),
            Value::Object(map) => {
                if let Some(Value::String(text)) = map.get("text") {
                    parts.push(text.clone());
                } else if let Some(content) = map.get("content") {
                    if let Some(text) = content_text(content) {
                        parts.push(text);
                    }
                }
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Parse a string that looks like an embedded JSON document.
fn embedded_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if !matches!(trimmed.as_bytes().first(), Some(b'{') | Some(b'[')) {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// A syntax role for a highlighted token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRole {
    /// An object key.
    Key,
    /// A string value.
    String,
    /// A number literal.
    Number,
    /// `true`, `false` or `null`.
    Bool,
    /// Punctuation, whitespace and any unrecognized text.
    Punct,
}

impl TokenRole {
    /// The CSS class the feed uses to colour this role.
    pub fn class(self) -> &'static str {
        match self {
            TokenRole::Key => "syn-key",
            TokenRole::String => "syn-string",
            TokenRole::Number => "syn-number",
            TokenRole::Bool => "syn-bool",
            TokenRole::Punct => "syn-punct",
        }
    }
}

/// One coloured fragment of highlighted JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The literal text.
    pub text: String,
    /// How to colour it.
    pub role: TokenRole,
}

/// Tokenize (pretty-printed) JSON for syntax highlighting.
///
/// The scan is deliberately forgiving: it never fails and folds anything it
/// does not recognize into a punctuation token, so a truncated preview still
/// highlights cleanly.
pub fn highlight(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut punct = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            flush(&mut punct, &mut tokens);
            let (literal, next) = read_string(&chars, i);
            let mut j = next;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            // A string followed by ':' is an object key.
            let role = if chars.get(j) == Some(&':') {
                TokenRole::Key
            } else {
                TokenRole::String
            };
            tokens.push(Token {
                text: literal,
                role,
            });
            i = next;
            continue;
        }
        if c == '-' || c.is_ascii_digit() {
            flush(&mut punct, &mut tokens);
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || matches!(chars[j], '.' | '-' | '+' | 'e' | 'E'))
            {
                j += 1;
            }
            tokens.push(Token {
                text: chars[i..j].iter().collect(),
                role: TokenRole::Number,
            });
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let word: String = chars[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            if matches!(word.as_str(), "true" | "false" | "null") {
                flush(&mut punct, &mut tokens);
                tokens.push(Token {
                    text: word.clone(),
                    role: TokenRole::Bool,
                });
                i += word.len();
                continue;
            }
        }
        punct.push(c);
        i += 1;
    }
    flush(&mut punct, &mut tokens);
    tokens
}

fn flush(punct: &mut String, tokens: &mut Vec<Token>) {
    if !punct.is_empty() {
        tokens.push(Token {
            text: std::mem::take(punct),
            role: TokenRole::Punct,
        });
    }
}

/// Read a JSON string starting at `start` (a `"`), returning it with its quotes
/// and the index just past the closing quote.
fn read_string(chars: &[char], start: usize) -> (String, usize) {
    let mut literal = String::from('"');
    let mut i = start + 1;
    while i < chars.len() {
        literal.push(chars[i]);
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            literal.push(chars[i]);
        } else if chars[i] == '"' {
            i += 1;
            break;
        }
        i += 1;
    }
    (literal, i)
}

/// Rewrite `<pre><code class="language-json">` blocks in rendered HTML so their
/// contents are syntax-highlighted with [`highlight`].
///
/// `pulldown-cmark` escapes only `<`, `>` and `&` in code blocks, so the JSON
/// quotes survive unescaped and only those three entities need reversing.
pub fn highlight_fenced_json(html: &str) -> String {
    const OPEN: &str = "<pre><code class=\"language-json\">";
    const CLOSE: &str = "</code></pre>";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(OPEN) {
        let (head, tail) = rest.split_at(start + OPEN.len());
        out.push_str(head);
        let Some(end) = tail.find(CLOSE) else {
            out.push_str(tail);
            return out;
        };
        let (code, after) = tail.split_at(end);
        for token in highlight(&unescape_body(code)) {
            out.push_str("<span class=\"");
            out.push_str(token.role.class());
            out.push_str("\">");
            out.push_str(&escape_body(&token.text));
            out.push_str("</span>");
        }
        out.push_str(CLOSE);
        rest = after;
    }
    out.push_str(rest);
    out
}

fn unescape_body(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn escape_body(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_strings_and_scalars_are_text() {
        assert_eq!(
            readable_json(&json!("hello")),
            Readable::Text("hello".into())
        );
        assert_eq!(readable_json(&json!(42)), Readable::Text("42".into()));
        assert_eq!(readable_json(&json!(true)), Readable::Text("true".into()));
        assert!(readable_json(&Value::Null).is_empty());
    }

    #[test]
    fn ansi_is_stripped_from_text() {
        assert_eq!(
            readable_json(&json!("\u{1b}[31mred\u{1b}[0m")),
            Readable::Text("red".into())
        );
    }

    #[test]
    fn unwraps_a_content_envelope_to_text() {
        let value = json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" },
            ]
        });
        assert_eq!(
            readable_json(&value),
            Readable::Text("first\nsecond".into())
        );
    }

    #[test]
    fn unwraps_a_string_content_envelope() {
        assert_eq!(
            readable_json(&json!({ "content": "hi" })),
            Readable::Text("hi".into())
        );
    }

    #[test]
    fn unwraps_a_stdout_stderr_envelope() {
        let value = json!({ "stdout": "out", "stderr": "err", "exit_code": 1 });
        assert_eq!(readable_json(&value), Readable::Text("out\nerr".into()));
    }

    #[test]
    fn unwraps_json_stored_inside_a_string() {
        let encoded = json!("{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}");
        assert_eq!(readable_json(&encoded), Readable::Text("hi".into()));
    }

    #[test]
    fn unwraps_a_bare_content_block_array() {
        let value = json!([{ "type": "text", "text": "only" }]);
        assert_eq!(readable_json(&value), Readable::Text("only".into()));
    }

    #[test]
    fn structured_json_is_kept_for_highlighting() {
        let value = json!({ "command": "ls", "args": ["-la"] });
        assert_eq!(readable_json(&value), Readable::Json(value.clone()));
    }

    #[test]
    fn deeply_nested_strings_do_not_recurse_forever() {
        // `{"content":"{"content":"…"}"}` nested past the depth cap: the call
        // must terminate rather than recurse without bound.
        let mut inner = String::from("{\"leaf\":true}");
        for _ in 0..MAX_UNWRAP_DEPTH + 3 {
            let quoted = serde_json::to_string(&inner).expect("encode nested string");
            inner = format!("{{\"content\":{quoted}}}");
        }
        let readable = readable_json(&Value::String(inner));
        // At the cap the innermost layer falls back to text.
        assert!(matches!(readable, Readable::Text(_)));
    }

    #[test]
    fn highlight_classifies_keys_values_numbers_and_bools() {
        let tokens = highlight("{\n  \"name\": \"foo\",\n  \"n\": 42,\n  \"ok\": true\n}");
        let find = |text: &str| {
            tokens
                .iter()
                .find(|token| token.text == text)
                .unwrap_or_else(|| panic!("missing token {text}: {tokens:?}"))
                .role
        };
        assert_eq!(find("\"name\""), TokenRole::Key);
        assert_eq!(find("\"foo\""), TokenRole::String);
        assert_eq!(find("42"), TokenRole::Number);
        assert_eq!(find("true"), TokenRole::Bool);
        assert_eq!(TokenRole::Key.class(), "syn-key");
    }

    #[test]
    fn highlight_never_fails_on_a_truncated_preview() {
        let tokens = highlight("{\n  \"name\": \"fo");
        assert!(!tokens.is_empty());
    }

    #[test]
    fn fenced_json_blocks_are_highlighted() {
        let html = "<pre><code class=\"language-json\">{\n  &quot;a&quot;: 1\n}</code></pre>";
        // Quotes are not entity-encoded by pulldown, but ensure the function is
        // robust even if a caller encodes them.
        let html = html.replace("&quot;", "\"");
        let out = highlight_fenced_json(&html);
        assert!(
            out.contains("<span class=\"syn-key\">\"a\"</span>"),
            "{out}"
        );
        assert!(out.contains("<span class=\"syn-number\">1</span>"), "{out}");
        assert!(out.starts_with("<pre><code class=\"language-json\">"));
    }

    #[test]
    fn other_code_blocks_are_left_untouched() {
        let html = "<pre><code class=\"language-rust\">let x = 1;</code></pre>";
        assert_eq!(highlight_fenced_json(html), html);
    }
}
