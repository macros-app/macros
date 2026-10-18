//! Wrap Text layout: how a wrapped cell's text breaks into lines, and how
//! tall its row grows to show them.
//!
//! The grid's font is monospaced, so a column's width is a number of
//! characters and breaking is plain arithmetic, the same in the painter and
//! in the row-height pass (which needs no text shaping). Explicit newlines
//! (`M-RET` in the cell editor) always break; each line then word-wraps at
//! spaces, and a word longer than the column breaks wherever it must.

use super::view::DEFAULT_ROW_HEIGHT;

/// The lines `text` shows as in a cell `width` characters wide (at least
/// one character). A break at a space drops the spaces there; an empty
/// line (`"a\n\nb"`) stays one.
pub fn wrap_lines(text: &str, width: usize) -> Vec<&str> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut rest = para.strip_suffix('\r').unwrap_or(para);
        loop {
            // Byte offset of the first character that doesn't fit.
            let Some((cut, _)) = rest.char_indices().nth(width) else {
                out.push(rest.trim_end_matches(' '));
                break;
            };
            let head = &rest[..cut];
            let (line, next) = if rest[cut..].starts_with(' ') {
                (head, &rest[cut..])
            } else {
                match head.rfind(' ') {
                    // Break after the last word that fits, unless that leaves
                    // only the line's leading spaces.
                    Some(sp) if !head[..sp].trim_start_matches(' ').is_empty() => {
                        (&head[..sp], &rest[sp..])
                    }
                    _ => (head, &rest[cut..]),
                }
            };
            out.push(line.trim_end_matches(' '));
            rest = next.trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
        }
    }
    out
}

/// A row's height (100% px) for its tallest wrapped cell's `lines`: the
/// default height holds one line, and each further line adds `line_px`.
pub fn row_height(lines: usize, line_px: f64) -> f64 {
    DEFAULT_ROW_HEIGHT + lines.saturating_sub(1) as f64 * line_px
}

/// What the row-height pass needs from the font, at the current zoom: one
/// character's advance and a line's pitch, both in 100% px. Zooming changes
/// the advance slightly (glyphs snap to pixels), so heights are measured
/// again at each zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub char_px: f64,
    pub line_px: f64,
    /// Horizontal padding on each side of a cell's text, 100% px.
    pub pad_px: f64,
}

impl Metrics {
    /// How many characters fit on one line of a column `col_px` wide.
    pub fn chars(&self, col_px: f64) -> usize {
        ((col_px - 2.0 * self.pad_px) / self.char_px.max(0.01))
            .floor()
            .max(1.0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaks_at_spaces_within_the_width() {
        assert_eq!(
            wrap_lines("the quick brown fox", 10),
            ["the quick", "brown fox"]
        );
        // A space exactly at the edge breaks there.
        assert_eq!(wrap_lines("abcde fgh", 5), ["abcde", "fgh"]);
        // Runs of spaces at a break vanish.
        assert_eq!(wrap_lines("ab    cd", 3), ["ab", "cd"]);
        assert_eq!(wrap_lines("fits", 10), ["fits"]);
        assert_eq!(wrap_lines("", 10), [""]);
    }

    #[test]
    fn long_words_break_anywhere() {
        assert_eq!(wrap_lines("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap_lines("a bcdefgh", 4), ["a", "bcde", "fgh"]);
        // Characters, not bytes.
        assert_eq!(wrap_lines("ééééé", 2), ["éé", "éé", "é"]);
        assert_eq!(wrap_lines("xyz", 0), ["x", "y", "z"]);
    }

    #[test]
    fn explicit_newlines_always_break() {
        assert_eq!(wrap_lines("a\nb", 10), ["a", "b"]);
        assert_eq!(wrap_lines("a\r\n\nb", 10), ["a", "", "b"]);
        assert_eq!(
            wrap_lines("one two\nthree four", 5),
            ["one", "two", "three", "four"]
        );
    }

    #[test]
    fn heights_and_widths() {
        assert_eq!(row_height(1, 17.0), DEFAULT_ROW_HEIGHT);
        assert_eq!(row_height(0, 17.0), DEFAULT_ROW_HEIGHT);
        assert_eq!(row_height(3, 17.0), DEFAULT_ROW_HEIGHT + 34.0);
        let m = Metrics {
            char_px: 8.0,
            line_px: 17.0,
            pad_px: 3.0,
        };
        assert_eq!(m.chars(64.0), 7);
        assert_eq!(m.chars(2.0), 1);
    }
}
