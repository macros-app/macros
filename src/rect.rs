//! Display-column math for Emacs rectangles (`C-x r …`, `C-x SPC`).
//!
//! A rectangle's edges are *display* columns, as in Emacs: a tab advances to
//! the next tab stop and a wide (CJK, emoji) character takes two columns. A
//! character that an edge cuts through (a tab spanning the left edge, say) is
//! split into spaces by the editing commands, exactly like Emacs's
//! `move-to-column` with FORCE. Shared by the `rectangle-row` / `display-column`
//! primitives (scheme/rectangles.scm) and the `rectangle-mark-mode` highlight.

use unicode_width::UnicodeWidthChar;

/// The display width of `c` when it starts at column `col`: a tab runs to the
/// next multiple of `tab`; control and combining characters take none.
pub fn char_width(c: char, col: usize, tab: usize) -> usize {
    if c == '\t' {
        let tab = tab.max(1);
        tab - col % tab
    } else {
        c.width().unwrap_or(0)
    }
}

/// Every character of `line` (one line, no newline) as
/// `(byte, char, start_col, end_col)`.
fn columns(line: &str, tab: usize) -> impl Iterator<Item = (usize, char, usize, usize)> + '_ {
    let mut col = 0;
    line.char_indices().map(move |(b, c)| {
        let start = col;
        col += char_width(c, col, tab);
        (b, c, start, col)
    })
}

/// The display width of `text` (one line) laid out from column 0.
pub fn display_width(text: &str, tab: usize) -> usize {
    columns(text, tab).last().map_or(0, |(_, _, _, end)| end)
}

/// The byte offset in `line` of the character covering display column `col`
/// (so a tab or wide char cut by `col` is included), or `line.len()` past the
/// end. The left edge of a highlighted strip.
pub fn byte_at_col_floor(line: &str, col: usize, tab: usize) -> usize {
    columns(line, tab)
        .find(|&(_, _, _, end)| end > col)
        .map_or(line.len(), |(b, ..)| b)
}

/// The byte offset in `line` of the first visible character starting at or
/// after display column `col`, or `line.len()`. The (exclusive) right edge of
/// a highlighted strip; combining marks stay with their base character.
pub fn byte_at_col_ceil(line: &str, col: usize, tab: usize) -> usize {
    columns(line, tab)
        .find(|&(_, _, start, end)| start >= col && end > start)
        .map_or(line.len(), |(b, ..)| b)
}

/// How one line meets the rectangle `[left, right)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowSpan {
    /// Byte range (within the line) of the characters the rectangle touches,
    /// widened to whole characters where an edge cuts through one.
    pub start: usize,
    pub end: usize,
    /// Columns of the first touched character that lie LEFT of `left` (a cut
    /// tab or wide char): editing commands put back this many spaces.
    pub before: usize,
    /// Columns of the last touched character RIGHT of `right`, likewise.
    pub after: usize,
    /// Columns the line falls short of `left` (0 when it reaches it):
    /// commands that must write at `left` (yank, string-rectangle) pad first.
    pub pad: usize,
    /// The rectangle's text on this line, exactly `right - left` columns wide:
    /// cut characters become spaces and a short line is space-padded (what
    /// `extract-rectangle` returns).
    pub inside: String,
}

/// Where `line` (one line, no newline) meets the display-column rectangle
/// `[left, right)`. `left == right` gives the split point at that column.
pub fn row_span(line: &str, left: usize, right: usize, tab: usize) -> RowSpan {
    let right = right.max(left);
    let width = display_width(line, tab);
    let first = columns(line, tab).find(|&(_, _, _, end)| end > left);
    let Some((start, _, first_col, _)) = first else {
        return RowSpan {
            start: line.len(),
            end: line.len(),
            before: 0,
            after: 0,
            pad: left.saturating_sub(width),
            inside: " ".repeat(right - left),
        };
    };
    let (end, end_col) = columns(line, tab)
        .find(|&(_, _, s, e)| s >= right && e > s)
        .map_or((line.len(), width), |(b, _, s, _)| (b, s));
    let mut inside = String::new();
    // Walk the whole line, not `line[start..end]`: a tab's width depends on
    // its absolute column.
    let touched = columns(line, tab).filter(|&(b, ..)| b >= start && b < end);
    for (_, c, s, e) in touched {
        if s >= left && e <= right {
            inside.push(c);
        } else {
            inside.push_str(&" ".repeat(e.min(right).saturating_sub(s.max(left))));
        }
    }
    inside.push_str(&" ".repeat(right.saturating_sub(end_col.max(left))));
    RowSpan {
        start,
        end,
        before: left - first_col,
        after: end_col.saturating_sub(right),
        pad: 0,
        inside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(line: &str, l: usize, r: usize) -> (usize, usize, usize, usize, usize, String) {
        let s = row_span(line, l, r, 8);
        (s.start, s.end, s.before, s.after, s.pad, s.inside)
    }

    #[test]
    fn plain_ascii_rows() {
        assert_eq!(span("abcdef", 1, 3), (1, 3, 0, 0, 0, "bc".into()));
        // Zero width: the split point.
        assert_eq!(span("abcdef", 2, 2), (2, 2, 0, 0, 0, String::new()));
        // A line ending inside the rectangle is padded.
        assert_eq!(span("abc", 1, 5), (1, 3, 0, 0, 0, "bc  ".into()));
        // A line ending before it touches nothing and reports the shortfall.
        assert_eq!(span("ab", 4, 6), (2, 2, 0, 0, 2, "  ".into()));
    }

    #[test]
    fn tabs_are_split_at_the_edges() {
        // "\tx": the tab covers columns 0..8, `x` is column 8.
        assert_eq!(span("\tx", 2, 9), (0, 2, 2, 0, 0, "      x".into()));
        // Both edges inside the one tab.
        assert_eq!(span("\tx", 2, 4), (0, 1, 2, 4, 0, "  ".into()));
        // Split point inside a tab: two columns before, six after.
        assert_eq!(span("\tx", 2, 2), (0, 1, 2, 6, 0, String::new()));
        // A tab after text reaches only the next stop.
        assert_eq!(display_width("ab\t", 8), 8);
        // A tab wholly inside the rectangle is kept as a tab.
        assert_eq!(span("ab\tc", 0, 9), (0, 4, 0, 0, 0, "ab\tc".into()));
    }

    #[test]
    fn wide_chars_take_two_columns() {
        // 日 is columns 0..2, 本 is 2..4.
        assert_eq!(display_width("日本", 8), 4);
        assert_eq!(span("日本x", 2, 5), (3, 7, 0, 0, 0, "本x".into()));
        // Cutting 日 in half: one column survives on the left.
        assert_eq!(span("日本x", 1, 4), (0, 6, 1, 0, 0, " 本".into()));
        assert_eq!(byte_at_col_floor("日本x", 3, 8), 3);
        assert_eq!(byte_at_col_ceil("日本x", 3, 8), 6);
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let line = "e\u{301}x"; // é as e + combining acute
        assert_eq!(span(line, 0, 1), (0, 3, 0, 0, 0, "e\u{301}".into()));
        assert_eq!(byte_at_col_ceil(line, 1, 8), 3);
    }
}
