//! Pretty-printed, syntax-highlighted JSON for the event payload pane.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// Render a JSON value as pretty-printed, highlighted lines.
pub fn render(value: &serde_json::Value, theme: &Theme) -> Vec<Line<'static>> {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    pretty
        .lines()
        .map(|line| highlight_line(line, theme))
        .collect()
}

fn highlight_line(line: &str, theme: &Theme) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            flush(&mut buf, &mut spans, theme);
            let (string, next) = read_string(&chars, i);
            // A string followed by ':' is an object key.
            let mut j = next;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let color = if chars.get(j) == Some(&':') {
                theme.syn_key
            } else {
                theme.syn_string
            };
            spans.push(Span::styled(string, Style::default().fg(color)));
            i = next;
            continue;
        }
        if c == '-' || c.is_ascii_digit() {
            flush(&mut buf, &mut spans, theme);
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_digit() || matches!(chars[j], '.' | '-' | '+' | 'e' | 'E'))
            {
                j += 1;
            }
            let number: String = chars[i..j].iter().collect();
            spans.push(Span::styled(number, Style::default().fg(theme.syn_number)));
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let word: String = chars[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            if matches!(word.as_str(), "true" | "false" | "null") {
                flush(&mut buf, &mut spans, theme);
                spans.push(Span::styled(
                    word.clone(),
                    Style::default()
                        .fg(theme.syn_bool)
                        .add_modifier(Modifier::BOLD),
                ));
                i += word.len();
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans, theme);
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
fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>, theme: &Theme) {
    if !buf.is_empty() {
        spans.push(Span::styled(
            std::mem::take(buf),
            Style::default().fg(theme.syn_comment),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(value: &serde_json::Value, theme: &Theme) -> Vec<Span<'static>> {
        render(value, theme)
            .into_iter()
            .flat_map(|l| l.spans)
            .collect()
    }

    #[test]
    fn keys_are_blue_and_values_green() {
        let theme = Theme::dark();
        let spans = spans(&serde_json::json!({ "name": "foo" }), &theme);
        let key = spans.iter().find(|s| s.content == "\"name\"").unwrap();
        assert_eq!(key.style.fg, Some(theme.syn_key));
        let value = spans.iter().find(|s| s.content == "\"foo\"").unwrap();
        assert_eq!(value.style.fg, Some(theme.syn_string));
    }

    #[test]
    fn numbers_and_booleans_are_highlighted() {
        let theme = Theme::dark();
        let spans = spans(
            &serde_json::json!({ "n": 42, "ok": true, "nil": null }),
            &theme,
        );
        assert!(spans
            .iter()
            .any(|s| s.content == "42" && s.style.fg == Some(theme.syn_number)));
        assert!(spans
            .iter()
            .any(|s| s.content == "true" && s.style.fg == Some(theme.syn_bool)));
        assert!(spans
            .iter()
            .any(|s| s.content == "null" && s.style.fg == Some(theme.syn_bool)));
    }

    #[test]
    fn light_theme_uses_different_token_colors() {
        let dark = Theme::dark();
        let light = Theme::light();
        let light_spans = spans(&serde_json::json!({ "name": "foo", "n": 1 }), &light);

        let fg = |spans: &[Span<'static>], content: &str| {
            spans
                .iter()
                .find(|s| s.content == content)
                .unwrap()
                .style
                .fg
        };
        assert_eq!(fg(&light_spans, "\"name\""), Some(light.syn_key));
        assert_eq!(fg(&light_spans, "\"foo\""), Some(light.syn_string));
        assert_eq!(fg(&light_spans, "1"), Some(light.syn_number));
        assert_ne!(light.syn_key, dark.syn_key);
        assert_ne!(light.syn_string, dark.syn_string);
        assert_ne!(light.syn_number, dark.syn_number);
    }
}
