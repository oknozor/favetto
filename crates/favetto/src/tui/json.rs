//! Pretty-printed, syntax-highlighted JSON for the event payload pane.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render a JSON value as pretty-printed, highlighted lines.
pub fn render(value: &serde_json::Value) -> Vec<Line<'static>> {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    pretty.lines().map(highlight_line).collect()
}

fn highlight_line(line: &str) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            flush(&mut buf, &mut spans);
            let (string, next) = read_string(&chars, i);
            // A string followed by ':' is an object key.
            let mut j = next;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let color = if chars.get(j) == Some(&':') {
                Color::LightBlue
            } else {
                Color::Green
            };
            spans.push(Span::styled(string, Style::default().fg(color)));
            i = next;
            continue;
        }
        if c == '-' || c.is_ascii_digit() {
            flush(&mut buf, &mut spans);
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || matches!(chars[j], '.' | '-' | '+' | 'e' | 'E'))
            {
                j += 1;
            }
            let number: String = chars[i..j].iter().collect();
            spans.push(Span::styled(number, Style::default().fg(Color::Yellow)));
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let word: String = chars[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            if matches!(word.as_str(), "true" | "false" | "null") {
                flush(&mut buf, &mut spans);
                spans.push(Span::styled(
                    word.clone(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
                i += word.len();
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    Line::from(spans)
}

/// Read a JSON string starting at `start` (a `"`), returning it (quotes included)
/// and the index just past the closing quote.
fn read_string(chars: &[char], start: usize) -> (String, usize) {
    let mut s = String::from('"');
    let mut i = start + 1;
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
    (s, i)
}

/// Punctuation and whitespace (whitespace's colour is invisible).
fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        spans.push(Span::styled(
            std::mem::take(buf),
            Style::default().fg(Color::DarkGray),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(value: &serde_json::Value) -> Vec<Span<'static>> {
        render(value).into_iter().flat_map(|l| l.spans).collect()
    }

    #[test]
    fn keys_are_blue_and_values_green() {
        let spans = spans(&serde_json::json!({ "name": "foo" }));
        let key = spans.iter().find(|s| s.content == "\"name\"").unwrap();
        assert_eq!(key.style.fg, Some(Color::LightBlue));
        let value = spans.iter().find(|s| s.content == "\"foo\"").unwrap();
        assert_eq!(value.style.fg, Some(Color::Green));
    }

    #[test]
    fn numbers_and_booleans_are_highlighted() {
        let spans = spans(&serde_json::json!({ "n": 42, "ok": true, "nil": null }));
        assert!(spans
            .iter()
            .any(|s| s.content == "42" && s.style.fg == Some(Color::Yellow)));
        assert!(spans
            .iter()
            .any(|s| s.content == "true" && s.style.fg == Some(Color::Magenta)));
        assert!(spans
            .iter()
            .any(|s| s.content == "null" && s.style.fg == Some(Color::Magenta)));
    }
}
