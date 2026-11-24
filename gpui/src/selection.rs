//! A GPUI-free read-only text-selection model.
//!
//! [`crate::selectable_text`] owns shaping, hit-testing and painting. Everything
//! that can be decided from the text alone (anchor/cursor, word/line expansion,
//! clamping to grapheme-safe byte boundaries) lives here so it can be unit
//! tested without a window and without depending on `gpui`.

use std::collections::HashMap;
use std::ops::Range;

/// An anchor plus the moving edge of a selection.
///
/// `anchor` stays where the drag started; `cursor` follows the pointer. The
/// visible range is always the two offsets in ascending order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextSelection {
    anchor: usize,
    cursor: usize,
}

impl TextSelection {
    /// A collapsed selection at `offset`.
    pub fn at(offset: usize) -> Self {
        Self {
            anchor: offset,
            cursor: offset,
        }
    }

    /// A selection from `anchor` to `cursor`.
    pub fn new(anchor: usize, cursor: usize) -> Self {
        Self { anchor, cursor }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    pub fn range(&self) -> Range<usize> {
        let start = self.anchor.min(self.cursor);
        let end = self.anchor.max(self.cursor);
        start..end
    }

    /// The selected substring, or `None` when the selection is collapsed.
    pub fn text<'a>(&self, text: &'a str) -> Option<&'a str> {
        let range = self.range();
        if range.is_empty() {
            None
        } else {
            text.get(range)
        }
    }

    pub fn extend_to(&mut self, offset: usize) {
        self.cursor = offset;
    }

    pub fn select_all(&mut self, len: usize) {
        self.anchor = 0;
        self.cursor = len;
    }

    pub fn clamp(&mut self, len: usize) {
        self.anchor = self.anchor.min(len);
        self.cursor = self.cursor.min(len);
    }

    /// The word under `offset` (double-click). A whitespace run selects the run
    /// itself, so double-clicking a gap still gives a visible range.
    pub fn word(text: &str, offset: usize) -> Self {
        if text.is_empty() {
            return Self::at(0);
        }
        let offset = snap(text, offset);
        let start_index = match text[offset..].chars().next() {
            Some(_) => offset,
            None => text
                .char_indices()
                .next_back()
                .map(|(index, _)| index)
                .unwrap_or(0),
        };
        let Some(first) = text[start_index..].chars().next() else {
            return Self::at(start_index);
        };
        let class = char_class(first);

        let mut start = start_index;
        for (index, ch) in text[..start_index].char_indices().rev() {
            if char_class(ch) != class {
                break;
            }
            start = index;
        }

        let mut end = start_index + first.len_utf8();
        for ch in text[end..].chars() {
            if char_class(ch) != class {
                break;
            }
            end += ch.len_utf8();
        }

        Self::new(start, end)
    }

    /// The paragraph under `offset` (triple-click). A blank-line boundary wins;
    /// a single-line paragraph is therefore the whole line.
    pub fn paragraph(text: &str, offset: usize) -> Self {
        if text.is_empty() {
            return Self::at(0);
        }
        let offset = snap(text, offset);
        let start = text[..offset]
            .rfind("\n\n")
            .map(|index| index + 2)
            .unwrap_or(0);
        let end = text[offset..]
            .find("\n\n")
            .map(|index| offset + index)
            .unwrap_or(text.len());
        Self::new(start, end)
    }
}

fn snap(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn char_class(ch: char) -> u8 {
    if ch.is_whitespace() {
        0
    } else if ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

/// One selectable text region: its source and current selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionEntry {
    pub text: String,
    pub selection: TextSelection,
}

/// Every read-only selection the window currently knows about.
///
/// The map is keyed by the stable numeric seed the Markdown/feed renderers
/// already use for element ids. Only one region is `active`; that is the region
/// `Cmd/Ctrl+C` and `Cmd/Ctrl+A` act on.
#[derive(Debug, Default)]
pub struct SelectionStore {
    entries: HashMap<u64, SelectionEntry>,
    active: Option<u64>,
    dragging: Option<u64>,
}

impl SelectionStore {
    /// Start a selection on `key` (a mouse-down).
    ///
    /// `click_count` expands to a word (2) or paragraph (3); a plain click
    /// collapses the caret. Shift extends the active selection when the click
    /// lands in the same region.
    pub fn begin(&mut self, key: u64, text: &str, offset: usize, click_count: usize, shift: bool) {
        let entry = self.entries.entry(key).or_insert_with(|| SelectionEntry {
            text: String::new(),
            selection: TextSelection::default(),
        });
        entry.text.clear();
        entry.text.push_str(text);

        if shift && self.active == Some(key) {
            entry.selection.extend_to(offset);
        } else {
            entry.selection = match click_count {
                2 => TextSelection::word(text, offset),
                3 => TextSelection::paragraph(text, offset),
                _ => TextSelection::at(offset),
            };
        }
        entry.selection.clamp(text.len());
        self.active = Some(key);
        self.dragging = Some(key);
    }

    /// Extend the in-progress drag on `key`.
    pub fn extend(&mut self, key: u64, offset: usize) {
        if self.dragging != Some(key) {
            return;
        }
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        let offset = snap(&entry.text, offset);
        entry.selection.extend_to(offset);
    }

    /// End a drag. The active region stays active so `Cmd/Ctrl+C` still works
    /// after the mouse-up.
    pub fn end_drag(&mut self, key: u64) {
        if self.dragging == Some(key) {
            self.dragging = None;
        }
    }

    /// Keep an entry's text in sync with a streaming render. The selection is
    /// clamped so a shrinking message never points past its buffer.
    pub fn set_text(&mut self, key: u64, text: &str) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        entry.text.clear();
        entry.text.push_str(text);
        entry.selection.clamp(text.len());
    }

    /// Drop every selection. Called when a real text field takes focus so its
    /// caret/selection wins the next `Cmd/Ctrl+C`.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.active = None;
        self.dragging = None;
    }

    pub fn selection(&self, key: u64) -> Option<&TextSelection> {
        self.entries.get(&key).map(|entry| &entry.selection)
    }

    pub fn dragging_key(&self) -> Option<u64> {
        self.dragging
    }

    /// The selected text of the active region, if the selection is non-empty.
    pub fn active_selected_text(&self) -> Option<String> {
        let entry = self.entries.get(&self.active?)?;
        entry
            .selection
            .text(&entry.text)
            .map(|text| text.to_string())
    }

    pub fn select_all_active(&mut self) -> bool {
        let Some(key) = self.active else {
            return false;
        };
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        let len = entry.text.len();
        entry.selection.select_all(len);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_selection_stops_at_the_class_boundary() {
        let text = "call render_code_block(...) now";
        let word = TextSelection::word(text, 7);
        assert_eq!(word.text(text), Some("render_code_block"));

        let punctuation = TextSelection::word(text, text.find('(').unwrap());
        assert_eq!(punctuation.text(text), Some("(...)"));

        let whitespace = TextSelection::word(text, text.find(" now").unwrap());
        assert_eq!(whitespace.text(text), Some(" "));
    }

    #[test]
    fn word_selection_handles_unicode_and_the_end_of_the_buffer() {
        let text = "héllo wörld";
        let word = TextSelection::word(text, 8);
        assert_eq!(word.text(text), Some("wörld"));

        let tail = TextSelection::word(text, text.len());
        assert_eq!(tail.text(text), Some("wörld"));
    }

    #[test]
    fn paragraph_selection_uses_blank_lines() {
        let text = "one\ntwo\n\nthree\nfour\n\nfive";
        let paragraph = TextSelection::paragraph(text, 10);
        assert_eq!(paragraph.text(text), Some("three\nfour"));
    }

    #[test]
    fn selection_range_normalizes_a_backwards_drag() {
        let mut selection = TextSelection::new(5, 2);
        assert_eq!(selection.range(), 2..5);
        assert_eq!(selection.text("abcdefgh"), Some("cde"));
        selection.extend_to(7);
        assert_eq!(selection.range(), 5..7);
    }

    #[test]
    fn the_store_activates_copies_and_selects_all() {
        let mut store = SelectionStore::default();
        store.begin(7, "hello world", 0, 1, false);
        store.extend(7, 5);
        store.end_drag(7);
        assert_eq!(store.active_selected_text().as_deref(), Some("hello"));
        assert!(store.select_all_active());
        assert_eq!(store.active_selected_text().as_deref(), Some("hello world"));

        store.clear();
        assert_eq!(store.active_selected_text(), None);
        assert!(store.selection(7).is_none());
    }

    #[test]
    fn a_streaming_shrink_clamps_the_selection() {
        let mut store = SelectionStore::default();
        store.begin(3, "a long streaming answer", 0, 1, false);
        store.extend(3, 10);
        store.set_text(3, "short");
        assert_eq!(store.active_selected_text().as_deref(), Some("short"));
    }
}
