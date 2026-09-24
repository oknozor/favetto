//! A small grapheme-aware text buffer with a caret.
//!
//! Favetto's TUI text inputs (the `[[vars]]` form, the step-by-step forms and
//! the one-shot wizard) share this editor so caret movement, word jumps and
//! mid-string edits behave the same everywhere. All movement and deletion
//! operate on grapheme clusters, so emoji and combining marks are never split.

use unicode_segmentation::UnicodeSegmentation;

/// Editable text plus a caret stored as a byte offset into [`value`](Self::value).
///
/// The caret always sits on a grapheme boundary; the editing methods keep that
/// invariant. Use [`TextBuffer::new`] to place the caret at the end (the usual
/// starting point for a pre-filled value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextBuffer {
    value: String,
    caret: usize,
}

impl TextBuffer {
    /// Create a buffer with the caret at the end of `value`.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let caret = value.len();
        Self { value, caret }
    }

    /// The buffer's text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Consume the buffer and return its text.
    pub fn into_string(self) -> String {
        self.value
    }

    /// The caret's byte offset.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Insert one character at the caret and advance it.
    pub fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
    }

    /// Insert a string at the caret and advance it.
    pub fn insert_str(&mut self, s: &str) {
        self.value.insert_str(self.caret, s);
        self.caret += s.len();
    }

    /// Delete the grapheme before the caret.
    pub fn backspace(&mut self) {
        if let Some(start) = self.prev_boundary() {
            self.value.replace_range(start..self.caret, "");
            self.caret = start;
        }
    }

    /// Delete the grapheme at the caret.
    pub fn delete(&mut self) {
        if let Some(end) = self.next_boundary() {
            self.value.replace_range(self.caret..end, "");
        }
    }

    /// Move the caret one grapheme to the left.
    pub fn move_left(&mut self) {
        if let Some(start) = self.prev_boundary() {
            self.caret = start;
        }
    }

    /// Move the caret one grapheme to the right.
    pub fn move_right(&mut self) {
        if let Some(end) = self.next_boundary() {
            self.caret = end;
        }
    }

    /// Move the caret to the start of the word before it, skipping separators.
    pub fn move_word_left(&mut self) {
        let mut pos = self.caret;
        while let Some(start) = self.prev_grapheme_start(pos) {
            if !self.is_separator(start, pos) {
                break;
            }
            pos = start;
        }
        while let Some(start) = self.prev_grapheme_start(pos) {
            if self.is_separator(start, pos) {
                break;
            }
            pos = start;
        }
        self.caret = pos;
    }

    /// Move the caret to the start of the next word, skipping separators.
    pub fn move_word_right(&mut self) {
        let mut pos = self.caret;
        while let Some(end) = self.next_grapheme_end(pos) {
            if self.is_separator(pos, end) {
                break;
            }
            pos = end;
        }
        while let Some(end) = self.next_grapheme_end(pos) {
            if !self.is_separator(pos, end) {
                break;
            }
            pos = end;
        }
        self.caret = pos;
    }

    /// Delete the word before the caret (`Alt+Backspace` / `Ctrl+W`).
    pub fn delete_word_before(&mut self) {
        let end = self.caret;
        self.move_word_left();
        self.value.replace_range(self.caret..end, "");
    }

    /// Byte offset of the start of the caret's line.
    pub fn line_start(&self) -> usize {
        self.value[..self.caret]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    /// Byte offset of the end of the caret's line (its `\n` or the buffer end).
    pub fn line_end(&self) -> usize {
        self.value[self.caret..]
            .find('\n')
            .map(|i| self.caret + i)
            .unwrap_or(self.value.len())
    }

    /// Move the caret to the start of its line (`Home` / `Ctrl+A`).
    pub fn home(&mut self) {
        self.caret = self.line_start();
    }

    /// Move the caret to the end of its line (`End` / `Ctrl+E`).
    pub fn end(&mut self) {
        self.caret = self.line_end();
    }

    /// Delete from the caret to the start of its line (`Ctrl+U`).
    pub fn delete_to_line_start(&mut self) {
        let start = self.line_start();
        self.value.replace_range(start..self.caret, "");
        self.caret = start;
    }

    /// Delete from the caret to the end of its line (`Ctrl+K`).
    pub fn delete_to_line_end(&mut self) {
        let end = self.line_end();
        self.value.replace_range(self.caret..end, "");
    }

    /// Move the caret to the same column on the previous line, clamped to its end.
    pub fn line_up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            self.caret = 0;
            return;
        }
        // `start - 1` is the `\n` that ends the previous line.
        let prev_end = start - 1;
        let prev_start = self.value[..prev_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.caret = self.column(prev_start, prev_end);
    }

    /// Move the caret to the same column on the next line, clamped to its end.
    pub fn line_down(&mut self) {
        let end = self.line_end();
        if end >= self.value.len() {
            self.caret = self.value.len();
            return;
        }
        let next_start = end + 1;
        let next_end = self.value[next_start..]
            .find('\n')
            .map(|i| next_start + i)
            .unwrap_or(self.value.len());
        self.caret = self.column(next_start, next_end);
    }

    /// Byte offset `target` graphemes into `[start, end)`, capped at `end`.
    fn column(&self, start: usize, end: usize) -> usize {
        let target = self.value[self.line_start()..self.caret]
            .graphemes(true)
            .count();
        let mut pos = start;
        let mut count = 0;
        while pos < end && count < target {
            let Some(g) = self.value[pos..end].graphemes(true).next() else {
                break;
            };
            pos += g.len();
            count += 1;
        }
        pos
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.prev_grapheme_start(self.caret)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.next_grapheme_end(self.caret)
    }

    fn prev_grapheme_start(&self, from: usize) -> Option<usize> {
        self.value[..from]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_grapheme_end(&self, from: usize) -> Option<usize> {
        self.value[from..]
            .graphemes(true)
            .next()
            .map(|g| from + g.len())
    }

    /// A grapheme is a separator when none of its characters is a word
    /// character (letters, digits or `_`). Punctuation and whitespace separate
    /// words, so `Alt+←`/`Alt+→` stop at path and argument boundaries too.
    fn is_separator(&self, start: usize, end: usize) -> bool {
        self.value[start..end]
            .chars()
            .all(|c| !c.is_alphanumeric() && c != '_')
    }
}

/// Compare a buffer's text with a string literal.
impl PartialEq<&str> for TextBuffer {
    fn eq(&self, other: &&str) -> bool {
        self.value == *other
    }
}

/// Compare a buffer's text with an owned string.
impl PartialEq<String> for TextBuffer {
    fn eq(&self, other: &String) -> bool {
        self.value == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_places_caret_at_end() {
        let buf = TextBuffer::new("abc");
        assert_eq!(buf.value(), "abc");
        assert_eq!(buf.caret(), 3);
    }

    #[test]
    fn insert_and_delete_around_the_caret() {
        let mut buf = TextBuffer::new("ac");
        buf.move_left();
        buf.insert_char('b');
        assert_eq!(buf.value(), "abc");
        assert_eq!(buf.caret(), 2);

        buf.delete();
        assert_eq!(buf.value(), "ab");
        buf.backspace();
        assert_eq!(buf.value(), "a");
        assert_eq!(buf.caret(), 1);
    }

    #[test]
    fn movement_uses_grapheme_clusters() {
        // "e\u{301}" is a single grapheme (e + combining acute accent).
        let mut buf = TextBuffer::new("ae\u{301}b");
        buf.move_left();
        assert_eq!(buf.caret(), "ae\u{301}".len());
        buf.move_left();
        assert_eq!(buf.caret(), 1);
        buf.backspace();
        assert_eq!(buf.value(), "e\u{301}b");
    }

    #[test]
    fn word_movement_and_deletion() {
        let mut buf = TextBuffer::new("hello brave world");
        buf.move_word_left();
        assert_eq!(buf.caret(), "hello brave ".len());
        buf.move_word_left();
        assert_eq!(buf.caret(), "hello ".len());

        buf.move_word_right();
        assert_eq!(buf.caret(), "hello brave ".len());

        buf.delete_word_before();
        assert_eq!(buf.value(), "hello world");
    }

    #[test]
    fn home_and_end_are_line_relative() {
        let mut buf = TextBuffer::new("one\ntwo\nthree");
        buf.home();
        assert_eq!(buf.caret(), "one\ntwo\n".len());
        buf.end();
        assert_eq!(buf.caret(), "one\ntwo\nthree".len());
    }

    #[test]
    fn delete_to_line_bounds() {
        let mut buf = TextBuffer::new("hello world");
        buf.move_left();
        buf.move_left();
        buf.delete_to_line_end();
        assert_eq!(buf.value(), "hello wor");
        buf.delete_to_line_start();
        assert_eq!(buf.value(), "");
    }

    #[test]
    fn line_up_and_down_keep_column() {
        let mut buf = TextBuffer::new("abcd\nxy\nabcdef");
        // Start at the end of line 0 (after "abcd"), then move down twice.
        buf.caret = 4;
        buf.line_down();
        assert_eq!(buf.caret(), "abcd\nxy".len());
        buf.line_down();
        assert_eq!(buf.caret(), "abcd\nxy\nab".len());
        buf.line_up();
        assert_eq!(buf.caret(), "abcd\nxy".len());
    }
}
