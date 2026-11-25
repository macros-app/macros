//! [`HexView`]: where the cursor is, what's selected, and how rows are laid
//! out. Pure state and arithmetic; the element measures the pane and reports
//! how many rows and bytes fit (`rows_visible`, `fit_width`).

use super::Pattern;

/// Bytes per row the width cycle steps through (`0` = fit the window).
pub const WIDTHS: [usize; 5] = [8, 16, 32, 64, 0];
/// Group sizes the group cycle steps through.
pub const GROUPS: [usize; 4] = [1, 2, 4, 8];

#[derive(Debug, Clone)]
pub struct HexView {
    /// The byte at point; may equal the length (the append slot after the
    /// last byte).
    pub cursor: usize,
    /// In the hex column, the next typed digit sets the low nibble.
    pub low_nibble: bool,
    /// Focus is the ASCII column (typing writes characters), not the hex.
    pub ascii: bool,
    /// The selection's other end, when one is active.
    pub mark: Option<usize>,
    /// The first row on screen.
    pub top: usize,
    /// Bytes per row; `0` fits as many as the window allows.
    pub width: usize,
    /// Bytes per space-separated group in the hex column.
    pub group: usize,
    /// Typing inserts bytes instead of overwriting them.
    pub insert: bool,
    /// Under evil/helix: typing is armed (`i` / `R`); Escape disarms.
    pub editing: bool,
    /// Show the data inspector strip.
    pub inspector: bool,
    /// The inspector reads multi-byte values big-endian.
    pub big_endian: bool,
    /// Rows the pane shows (set by the element each frame).
    pub rows_visible: usize,
    /// Bytes per row that fit the pane (set by the element each frame).
    pub fit_width: usize,
    /// The last search, for next/previous.
    pub search: Option<(Pattern, String)>,
}

impl Default for HexView {
    fn default() -> Self {
        Self {
            cursor: 0,
            low_nibble: false,
            ascii: false,
            mark: None,
            top: 0,
            width: 16,
            group: 1,
            insert: false,
            editing: false,
            inspector: true,
            big_endian: false,
            rows_visible: 24,
            fit_width: 16,
            search: None,
        }
    }
}

impl HexView {
    /// Bytes per row in effect.
    pub fn row_width(&self) -> usize {
        let w = if self.width == 0 {
            // Fit: a whole number of groups.
            let g = self.group.max(1);
            (self.fit_width / g * g).max(g)
        } else {
            self.width
        };
        w.max(1)
    }

    /// Rows a buffer of `len` bytes takes, counting the append slot.
    pub fn row_count(&self, len: usize) -> usize {
        len / self.row_width() + 1
    }

    /// The selected byte range (`start..end`, end exclusive) — point and mark
    /// inclusive — when a selection is active.
    pub fn selection(&self, len: usize) -> Option<std::ops::Range<usize>> {
        let mark = self.mark?;
        let (a, b) = if mark <= self.cursor {
            (mark, self.cursor)
        } else {
            (self.cursor, mark)
        };
        let end = (b + 1).min(len);
        (a < end).then_some(a..end)
    }

    /// Put the cursor at `at` (clamped to `0..=len`), starting a fresh byte.
    pub fn set_cursor(&mut self, at: usize, len: usize) {
        self.cursor = at.min(len);
        self.low_nibble = false;
        self.scroll_to_cursor(len);
    }

    /// Scroll just enough to keep the cursor's row on screen.
    pub fn scroll_to_cursor(&mut self, len: usize) {
        let row = self.cursor / self.row_width();
        let rows = self.rows_visible.max(1);
        if row < self.top {
            self.top = row;
        } else if row >= self.top + rows {
            self.top = row + 1 - rows;
        }
        self.clamp_top(len);
    }

    /// Keep `top` within the buffer (the last row may scroll to the top).
    pub fn clamp_top(&mut self, len: usize) {
        let last = self.row_count(len).saturating_sub(1);
        self.top = self.top.min(last);
    }

    /// Scroll by `rows` (negative is up), dragging the cursor along when it
    /// would leave the screen.
    pub fn scroll(&mut self, rows: isize, len: usize) {
        self.top = self.top.saturating_add_signed(rows);
        self.clamp_top(len);
        let w = self.row_width();
        let row = self.cursor / w;
        let col = self.cursor % w;
        let rows_vis = self.rows_visible.max(1);
        let new_row = row.clamp(self.top, self.top + rows_vis - 1);
        if new_row != row {
            self.cursor = (new_row * w + col).min(len);
            self.low_nibble = false;
        }
    }

    /// Step to the next value in `options` after `current`.
    pub fn cycle(options: &[usize], current: usize, forward: bool) -> usize {
        let i = options.iter().position(|&o| o == current).unwrap_or(0);
        let n = options.len();
        options[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_inclusive_of_both_ends() {
        let mut v = HexView {
            cursor: 4,
            mark: Some(2),
            ..Default::default()
        };
        assert_eq!(v.selection(10), Some(2..5));
        v.cursor = 1;
        assert_eq!(v.selection(10), Some(1..3));
        // The append slot never selects a byte past the end.
        v.mark = Some(10);
        v.cursor = 10;
        assert_eq!(v.selection(10), None);
        v.mark = None;
        assert_eq!(v.selection(10), None);
    }

    #[test]
    fn scrolling_follows_the_cursor() {
        let mut v = HexView {
            rows_visible: 4,
            ..Default::default()
        };
        v.set_cursor(16 * 10, 1000);
        assert_eq!(v.top, 7);
        v.set_cursor(16, 1000);
        assert_eq!(v.top, 1);
        v.scroll(5, 1000);
        assert_eq!(v.top, 6);
        assert_eq!(v.cursor / 16, 6);
    }

    #[test]
    fn fit_width_rounds_to_groups() {
        let v = HexView {
            width: 0,
            group: 4,
            fit_width: 22,
            ..Default::default()
        };
        assert_eq!(v.row_width(), 20);
    }

    #[test]
    fn cycle_wraps() {
        assert_eq!(HexView::cycle(&WIDTHS, 16, true), 32);
        assert_eq!(HexView::cycle(&WIDTHS, 0, true), 8);
        assert_eq!(HexView::cycle(&WIDTHS, 8, false), 0);
    }
}
