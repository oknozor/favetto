//! Embedded terminal view backing the Agent tab.
//!
//! The daemon owns terminal emulation and streams self-contained full-screen
//! frames (`vt100`'s `state_formatted`). The TUI parses each frame into a grid
//! and renders it as ordinary ratatui cells — the local terminal is never
//! involved, and a dropped frame can't corrupt the display because the next frame
//! clears and redraws.

pub struct TerminalView {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
}

impl TerminalView {
    pub fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = (rows.max(1), cols.max(1));
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
        }
    }

    /// Parse a frame (a full formatted screen) into the grid.
    pub fn process(&mut self, frame: &[u8]) {
        self.parser.process(frame);
    }

    /// Resize the grid to match the panel (frames are regenerated for it).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if (rows, cols) != (self.rows, self.cols) {
            self.rows = rows;
            self.cols = cols;
            self.parser.screen_mut().set_size(rows, cols);
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }
}

impl Default for TerminalView {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(term: &TerminalView, rows: u16, cols: u16) -> String {
        let mut out = String::new();
        for row in 0..rows {
            for col in 0..cols {
                if let Some(cell) = term.screen().cell(row, col) {
                    out.push_str(cell.contents());
                }
            }
        }
        out
    }

    #[test]
    fn parses_plain_text() {
        let mut term = TerminalView::new(5, 20);
        term.process(b"hello");
        assert!(screen_text(&term, 5, 20).contains("hello"));
    }

    #[test]
    fn parses_a_formatted_frame() {
        // What the daemon emits: clear, position, write, then clear again.
        let mut term = TerminalView::new(5, 20);
        term.process(b"\x1b[2J\x1b[Hfirst\x1b[2J\x1b[Hsecond");
        let text = screen_text(&term, 5, 20);
        assert!(text.contains("second"));
        assert!(!text.contains("first"), "frame should clear previous content");
    }

    #[test]
    fn resize_changes_grid() {
        let mut term = TerminalView::new(5, 20);
        term.resize(10, 40);
        assert_eq!(term.size(), (10, 40));
        assert_eq!(term.screen().size(), (10, 40));
    }
}
