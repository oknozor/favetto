//! The shared, grapheme-aware text buffer behind every GPUI input.
//!
//! GPUI's published crate has no reusable `InputState` (that lives in Zed's
//! separate `ui` crate), so the client owns one small editor model. Every
//! surface — the palette, the repo picker, the search fields, the catalog and
//! schedule sheets, Settings, the follow-up composer and ACP form fields —
//! embeds this type instead of appending to a `String`.
//!
//! Offsets are byte offsets into [`TextInput::value`], always snapped to a
//! grapheme-cluster boundary so emoji, combining marks and IME text are never
//! split. The platform talks UTF-16 (see [`TextInput::text_for_range_utf16`]
//! and friends); those conversions live here so the views stay pure.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// A caret plus optional selection over a UTF-8 string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    /// The selected byte range. An empty range is a plain caret.
    selected_range: Range<usize>,
    /// Whether the selection anchor is the range's end (the caret is at start).
    selection_reversed: bool,
    /// The IME composition range, underlined while the platform is composing.
    marked_range: Option<Range<usize>>,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl From<String> for TextInput {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for TextInput {
    fn from(value: &str) -> Self {
        Self::new(value.to_string())
    }
}

impl std::fmt::Display for TextInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.value)
    }
}

impl PartialEq<str> for TextInput {
    fn eq(&self, other: &str) -> bool {
        self.value == other
    }
}

impl PartialEq<&str> for TextInput {
    fn eq(&self, other: &&str) -> bool {
        self.value == *other
    }
}

impl PartialEq<String> for TextInput {
    fn eq(&self, other: &String) -> bool {
        &self.value == other
    }
}

impl TextInput {
    /// A buffer with the caret at the end of `value`.
    pub fn new(value: String) -> Self {
        let end = value.len();
        Self {
            value,
            selected_range: end..end,
            selection_reversed: false,
            marked_range: None,
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replace the whole value and move the caret to its end. Used by
    /// programmatic updates (seeded defaults, completed slash commands).
    pub fn set_value(&mut self, value: String) {
        self.value = value;
        self.selected_range = self.value.len()..self.value.len();
        self.selection_reversed = false;
        self.marked_range = None;
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn len(&self) -> usize {
        self.value.len()
    }

    pub fn selected_range(&self) -> &Range<usize> {
        &self.selected_range
    }

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn selection_reversed(&self) -> bool {
        self.selection_reversed
    }

    pub fn marked_range(&self) -> Option<&Range<usize>> {
        self.marked_range.as_ref()
    }

    pub fn has_selection(&self) -> bool {
        !self.selected_range.is_empty()
    }

    /// The caret byte offset, taking a reversed selection into account.
    pub fn caret(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// The selected text (empty for a plain caret).
    pub fn selection_text(&self) -> &str {
        &self.value[self.selected_range.clone()]
    }

    /// Clear the selection, placing the caret at `offset`.
    pub fn move_to(&mut self, offset: usize) {
        let offset = self.snap_boundary(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
    }

    /// Extend the selection to `offset`. Movement by grapheme/word passes a
    /// boundary already; mouse positions are snapped here.
    pub fn select_to(&mut self, offset: usize) {
        let offset = self.snap_boundary(offset);
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
    }

    pub fn select_all(&mut self) {
        self.selected_range = 0..self.value.len();
        self.selection_reversed = false;
    }

    // -- editing -----------------------------------------------------------

    /// Insert text at the caret (or replace the selection/marked range).
    pub fn insert(&mut self, text: &str) {
        let range = self
            .marked_range
            .take()
            .unwrap_or_else(|| self.selected_range.clone());
        self.value.replace_range(range.clone(), text);
        let caret = range.start + text.len();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
    }

    /// Insert one typed character.
    pub fn push_char(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        let mut buffer = [0u8; 4];
        self.insert(character.encode_utf8(&mut buffer));
    }

    /// Remove the selection, or the grapheme before the caret.
    pub fn backspace(&mut self) {
        if !self.selected_range.is_empty() {
            self.delete_range(self.selected_range.clone());
            return;
        }
        let caret = self.caret();
        let start = self.previous_boundary(caret);
        self.delete_range(start..caret);
    }

    /// Remove the selection, or the grapheme at the caret.
    pub fn delete(&mut self) {
        if !self.selected_range.is_empty() {
            self.delete_range(self.selected_range.clone());
            return;
        }
        let caret = self.caret();
        let end = self.next_boundary(caret);
        self.delete_range(caret..end);
    }

    pub fn delete_selection(&mut self) {
        if self.selected_range.is_empty() {
            return;
        }
        self.delete_range(self.selected_range.clone());
    }

    /// Delete the whole word before the caret (`Alt+Backspace`).
    pub fn delete_word_backwards(&mut self) {
        if !self.selected_range.is_empty() {
            self.delete_selection();
            return;
        }
        let caret = self.caret();
        let start = self.previous_word_boundary(caret);
        self.delete_range(start..caret);
    }

    /// Delete the whole word after the caret (`Alt+Delete`).
    pub fn delete_word_forwards(&mut self) {
        if !self.selected_range.is_empty() {
            self.delete_selection();
            return;
        }
        let caret = self.caret();
        let end = self.next_word_boundary(caret);
        self.delete_range(caret..end);
    }

    fn delete_range(&mut self, range: Range<usize>) {
        self.value.replace_range(range.clone(), "");
        self.selected_range = range.start..range.start;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    // -- movement ----------------------------------------------------------

    pub fn move_left(&mut self) {
        if self.selected_range.is_empty() {
            let offset = self.previous_boundary(self.caret());
            self.move_to(offset);
        } else {
            self.move_to(self.selected_range.start);
        }
    }

    pub fn move_right(&mut self) {
        if self.selected_range.is_empty() {
            let offset = self.next_boundary(self.caret());
            self.move_to(offset);
        } else {
            self.move_to(self.selected_range.end);
        }
    }

    pub fn move_word_left(&mut self) {
        let offset = if self.selected_range.is_empty() {
            self.previous_word_boundary(self.caret())
        } else {
            self.selected_range.start
        };
        self.move_to(offset);
    }

    pub fn move_word_right(&mut self) {
        let offset = if self.selected_range.is_empty() {
            self.next_word_boundary(self.caret())
        } else {
            self.selected_range.end
        };
        self.move_to(offset);
    }

    pub fn move_home(&mut self) {
        let offset = self.line_start(self.caret());
        self.move_to(offset);
    }

    pub fn move_end(&mut self) {
        let offset = self.line_end(self.caret());
        self.move_to(offset);
    }

    pub fn move_line_up(&mut self) {
        if let Some(offset) = self.line_offset(self.caret(), -1) {
            self.move_to(offset);
        }
    }

    pub fn move_line_down(&mut self) {
        if let Some(offset) = self.line_offset(self.caret(), 1) {
            self.move_to(offset);
        }
    }

    pub fn select_left(&mut self) {
        let offset = self.previous_boundary(self.caret());
        self.select_to(offset);
    }

    pub fn select_right(&mut self) {
        let offset = self.next_boundary(self.caret());
        self.select_to(offset);
    }

    pub fn select_word_left(&mut self) {
        let offset = self.previous_word_boundary(self.caret());
        self.select_to(offset);
    }

    pub fn select_word_right(&mut self) {
        let offset = self.next_word_end(self.caret());
        self.select_to(offset);
    }

    pub fn select_home(&mut self) {
        let offset = self.line_start(self.caret());
        self.select_to(offset);
    }

    pub fn select_end(&mut self) {
        let offset = self.line_end(self.caret());
        self.select_to(offset);
    }

    pub fn select_line_up(&mut self) {
        if let Some(offset) = self.line_offset(self.caret(), -1) {
            self.select_to(offset);
        }
    }

    pub fn select_line_down(&mut self) {
        if let Some(offset) = self.line_offset(self.caret(), 1) {
            self.select_to(offset);
        }
    }

    // -- boundaries --------------------------------------------------------

    /// The grapheme boundary strictly before `offset`.
    pub fn previous_boundary(&self, offset: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    /// The grapheme boundary strictly after `offset`.
    pub fn next_boundary(&self, offset: usize) -> usize {
        self.value
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.value.len())
    }

    /// Snap an arbitrary byte offset (for example one returned by a line
    /// layout) down to the start of the containing grapheme.
    pub fn snap_boundary(&self, offset: usize) -> usize {
        if offset >= self.value.len() {
            return self.value.len();
        }
        let mut snapped = 0;
        for (index, _) in self.value.grapheme_indices(true) {
            if index > offset {
                break;
            }
            snapped = index;
        }
        snapped
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        let mut index = self.snap_boundary(offset);
        while index > 0 {
            let previous = self.previous_boundary(index);
            if WordClass::of(&self.value[previous..index]) == WordClass::Space {
                index = previous;
            } else {
                break;
            }
        }
        if index == 0 {
            return 0;
        }
        // A run of one non-space class is one stop: punctuation is separate
        // from the words around it (`beta.gamma` stops between `beta.` and
        // `gamma` in both directions).
        let previous = self.previous_boundary(index);
        let class = WordClass::of(&self.value[previous..index]);
        while index > 0 {
            let previous = self.previous_boundary(index);
            if WordClass::of(&self.value[previous..index]) != class {
                break;
            }
            index = previous;
        }
        index
    }

    /// The end of the word at or after `offset`, without its trailing
    /// separators. Shift+word selection uses this so a copy is just the word.
    fn next_word_end(&self, offset: usize) -> usize {
        let mut index = self.snap_boundary(offset);
        while index < self.value.len() {
            let next = self.next_boundary(index);
            if WordClass::of(&self.value[index..next]) == WordClass::Word {
                break;
            }
            index = next;
        }
        while index < self.value.len() {
            let next = self.next_boundary(index);
            if WordClass::of(&self.value[index..next]) != WordClass::Word {
                break;
            }
            index = next;
        }
        index
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        let mut index = self.snap_boundary(offset);
        // Consume the word under the caret, then its separators, and stop at
        // the start of the next word (`Ctrl+Right` / `Alt+Right`).
        while index < self.value.len() {
            let next = self.next_boundary(index);
            if WordClass::of(&self.value[index..next]) == WordClass::Word {
                index = next;
            } else {
                break;
            }
        }
        while index < self.value.len() {
            let next = self.next_boundary(index);
            if WordClass::of(&self.value[index..next]) == WordClass::Word {
                break;
            }
            index = next;
        }
        index
    }

    fn line_start(&self, offset: usize) -> usize {
        self.value[..self.snap_boundary(offset)]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        let offset = self.snap_boundary(offset);
        self.value[offset..]
            .find('\n')
            .map(|index| offset + index)
            .unwrap_or(self.value.len())
    }

    fn line_offset(&self, offset: usize, delta: isize) -> Option<usize> {
        let offset = self.snap_boundary(offset);
        let start = self.line_start(offset);
        let end = self.line_end(offset);
        let column = self.value[start..offset].graphemes(true).count();
        let (target_start, target_end) = if delta < 0 {
            if start == 0 {
                return None;
            }
            let previous_end = start - 1;
            let previous_start = self.line_start(previous_end);
            (previous_start, previous_end)
        } else {
            if end >= self.value.len() {
                return None;
            }
            let next_start = end + 1;
            let next_end = self.line_end(next_start);
            (next_start, next_end)
        };
        let mut result = target_start;
        for (count, (index, grapheme)) in self.value[target_start..target_end]
            .grapheme_indices(true)
            .enumerate()
        {
            if count >= column {
                break;
            }
            result = target_start + index + grapheme.len();
        }
        Some(self.snap_boundary(result))
    }

    // -- platform (UTF-16) -------------------------------------------------

    pub fn text_for_range_utf16(&self, range: Range<usize>) -> (String, Range<usize>) {
        let range = self.range_from_utf16(&range);
        (
            self.value[range.clone()].to_string(),
            self.range_to_utf16(&range),
        )
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.offset_from_utf16(range.start);
        let end = self.offset_from_utf16(range.end);
        start..end.max(start)
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for character in self.value.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += character.len_utf16();
            utf8_offset += character.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for character in self.value.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += character.len_utf8();
            utf16_offset += character.len_utf16();
        }
        utf16_offset
    }

    /// Replace a UTF-16 range (or the marked/selected range) with `text`.
    pub fn replace_text_in_range_utf16(&mut self, range: Option<Range<usize>>, text: &str) {
        let range = range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.value.replace_range(range.clone(), text);
        let caret = range.start + text.len();
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
    }

    /// Replace a range with composing text and keep the platform's marked range.
    pub fn replace_and_mark_text_in_range_utf16(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
    ) {
        let range = range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        self.value.replace_range(range.clone(), text);
        let start = range.start;
        self.marked_range = (!text.is_empty()).then_some(start..start + text.len());
        self.selected_range = selected_range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .map(|range| start + range.start..start + range.end)
            .unwrap_or_else(|| {
                let caret = start + text.len();
                caret..caret
            });
        self.selection_reversed = false;
    }

    pub fn unmark_text(&mut self) {
        self.marked_range = None;
    }

    pub fn selected_text_range_utf16(&self) -> (Range<usize>, bool) {
        (
            self.range_to_utf16(&self.selected_range),
            self.selection_reversed,
        )
    }

    pub fn marked_text_range_utf16(&self) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }
}

/// Which rendered input owns the keyboard/caret. The target is derived from the
/// same route and focus flags that route typed characters, so painting and
/// editing always agree about the active buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextTarget {
    /// The command palette query.
    Palette,
    /// The repository picker filter.
    RepoPicker,
    /// The Tasks search field.
    TasksSearch,
    /// The Catalog search field.
    CatalogSearch,
    /// One declared `[[vars]]` field in the launch sheet.
    CatalogLaunchVar(usize),
    /// The launch sheet's model override.
    CatalogLaunchModel,
    /// The one-shot sheet's working directory.
    CatalogOneShotCwd,
    /// The schedule editor's cron expression.
    ScheduleCron,
    /// The Settings daemon address.
    SettingsEndpoint,
    /// The Settings bearer token.
    SettingsToken,
    /// One generated control in the Settings daemon config form.
    SettingsDaemonControl(usize),
    /// The Settings advanced/raw daemon `config.toml` editor.
    SettingsDaemonRaw,
    /// The task detail's / Agent panel's follow-up composer.
    DetailPrompt,
    /// The Agent panel's model override before a session starts.
    AgentModel,
    /// One ACP elicitation text field.
    Elicitation(usize),
}

/// The classes word movement stops at. Runs of whitespace are skipped, and a
/// run of one class (word characters or punctuation) is treated as one stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordClass {
    Space,
    Word,
    Punctuation,
}

impl WordClass {
    fn of(grapheme: &str) -> Self {
        if grapheme.chars().all(char::is_whitespace) {
            Self::Space
        } else if grapheme
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
        {
            Self::Word
        } else {
            Self::Punctuation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: &str) -> TextInput {
        TextInput::new(value.to_string())
    }

    #[test]
    fn typed_characters_insert_at_the_caret_and_backspace_removes_the_previous_grapheme() {
        let mut input = input("abcd");
        input.move_to(2);
        input.push_char('X');
        assert_eq!(input.value(), "abXcd");
        assert_eq!(input.caret(), 3);
        input.backspace();
        assert_eq!(input.value(), "abcd");
        assert_eq!(input.caret(), 2);
        input.delete();
        assert_eq!(input.value(), "abd");
        assert_eq!(input.caret(), 2);
    }

    #[test]
    fn grapheme_clusters_are_never_split() {
        // A family emoji plus a combining mark: four grapheme clusters.
        let mut input = input("a👨‍👩‍👧‍👦e\u{301}z");
        assert_eq!(input.value().graphemes(true).count(), 4);
        input.move_end();
        input.backspace();
        assert_eq!(input.value(), "a👨‍👩‍👧‍👦e\u{301}");
        input.backspace();
        assert_eq!(input.value(), "a👨‍👩‍👧‍👦");
        input.backspace();
        assert_eq!(input.value(), "a");
        input.move_to(0);
        input.delete();
        assert_eq!(input.value(), "");
    }

    #[test]
    fn word_movement_skips_whitespace_and_punctuation_runs() {
        let mut input = input("alpha beta.gamma  delta");
        input.move_to(input.len());
        input.move_word_left();
        assert_eq!(input.caret(), "alpha beta.gamma  ".len());
        input.move_word_left();
        assert_eq!(input.caret(), "alpha beta.".len());
        input.move_word_left();
        assert_eq!(input.caret(), "alpha beta".len());
        input.move_word_left();
        assert_eq!(input.caret(), "alpha ".len());
        input.move_word_left();
        assert_eq!(input.caret(), 0);
        input.move_word_right();
        assert_eq!(input.caret(), "alpha ".len());
        input.move_word_right();
        assert_eq!(input.caret(), "alpha beta.".len());
    }

    #[test]
    fn word_deletion_removes_only_the_word_before_the_caret() {
        let mut input = input("alpha beta gamma");
        input.delete_word_backwards();
        assert_eq!(input.value(), "alpha beta ");
        input.delete_word_backwards();
        assert_eq!(input.value(), "alpha ");
        input.move_to(input.len());
        input.delete_word_forwards();
        assert_eq!(input.value(), "alpha ", "nothing follows the caret");

        let mut forward = TextInput::new("alpha beta gamma".to_string());
        forward.move_to("alpha ".len());
        forward.delete_word_forwards();
        assert_eq!(forward.value(), "alpha gamma");
    }

    #[test]
    fn selection_extends_in_both_directions_and_edits_replace_it() {
        let mut input = input("hello world");
        input.move_to(6);
        input.select_word_right();
        assert_eq!(input.selection_text(), "world");
        input.select_left();
        assert_eq!(input.selection_text(), "worl");
        assert!(!input.selection_reversed());
        input.insert("X");
        assert_eq!(input.value(), "hello Xd");
        assert_eq!(input.caret(), 7);

        input.select_all();
        assert_eq!(input.selection_text(), "hello Xd");
        input.backspace();
        assert_eq!(input.value(), "");

        let mut reversed = TextInput::new("hello world".to_string());
        reversed.move_to(6);
        reversed.select_word_left();
        assert_eq!(reversed.selection_text(), "hello ");
        assert!(reversed.selection_reversed());
        reversed.insert("X");
        assert_eq!(reversed.value(), "Xworld");
    }

    #[test]
    fn home_end_and_vertical_movement_track_lines() {
        let mut input = input("one\ntwo\nthree");
        input.move_to(input.len());
        input.move_home();
        assert_eq!(input.caret(), "one\ntwo\n".len());
        input.move_end();
        assert_eq!(input.caret(), input.len());
        input.move_line_up();
        assert_eq!(input.caret(), "one\ntwo".len());
        input.select_home();
        assert_eq!(input.selection_text(), "two");
        input.move_line_up();
        assert_eq!(input.caret(), 0);
        input.move_line_down();
        assert_eq!(input.caret(), "one\n".len());
    }

    #[test]
    fn newlines_insert_at_the_caret_for_multiline_buffers() {
        let mut input = input("hello world");
        input.move_to(5);
        input.insert("\n");
        assert_eq!(input.value(), "hello\n world");
        assert_eq!(input.caret(), 6);
        input.move_home();
        assert_eq!(input.caret(), 6);
        input.move_left();
        assert_eq!(input.caret(), 5);
    }

    #[test]
    fn utf16_ranges_round_trip_for_non_bmp_text() {
        let mut input = input("a😀b");
        let emoji_start = "a".len();
        let emoji_end = emoji_start + "😀".len();
        let range = input.range_to_utf16(&(emoji_start..emoji_end));
        assert_eq!(range, 1..3);
        assert_eq!(input.range_from_utf16(&range), emoji_start..emoji_end);

        input.replace_text_in_range_utf16(Some(1..3), "X");
        assert_eq!(input.value(), "aXb");
        assert_eq!(input.caret(), 2);
    }

    #[test]
    fn marked_composition_replaces_its_previous_range() {
        let mut input = input("hi");
        input.move_end();
        input.replace_and_mark_text_in_range_utf16(None, "e", None);
        assert_eq!(input.value(), "hie");
        assert_eq!(input.marked_range(), Some(&(2..3)));
        input.replace_and_mark_text_in_range_utf16(Some(2..3), "é", None);
        assert_eq!(input.value(), "hié");
        assert_eq!(input.marked_range(), Some(&(2..4)));
        input.unmark_text();
        assert_eq!(input.marked_range(), None);
    }

    #[test]
    fn copied_selection_text_round_trips_through_insertion() {
        let mut source = input("hello brave world");
        source.move_to(6);
        source.select_word_right();
        let copied = source.selection_text().to_string();
        assert_eq!(copied, "brave");

        let mut destination = input("start ");
        destination.move_end();
        destination.insert(&copied);
        destination.push_char('!');
        assert_eq!(destination.value(), "start brave!");
    }

    #[gpui::test]
    fn the_platform_clipboard_round_trips_selected_text(cx: &mut gpui::TestAppContext) {
        let mut source = input("copy this phrase");
        source.move_to(5);
        source.select_word_right();
        let copied = source.selection_text().to_string();
        assert_eq!(copied, "this");

        cx.update(|cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied.clone()));
        });
        let pasted = cx
            .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
            .expect("the test platform keeps the clipboard item");
        assert_eq!(pasted, copied);

        let mut destination = input("before ");
        destination.move_end();
        destination.insert(&pasted);
        assert_eq!(destination.value(), "before this");
    }

    #[test]
    fn a_programmatic_value_resets_the_caret_to_the_end() {
        let mut input = input("old");
        input.move_to(0);
        input.set_value("new value".to_string());
        assert_eq!(input.value(), "new value");
        assert_eq!(input.caret(), "new value".len());
        assert!(!input.has_selection());
    }
}
