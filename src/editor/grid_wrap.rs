//! Wrap Text in grid buffers: the text a wrapped cell shows, and the pass
//! that grows rows to fit their wrapped cells.
//!
//! Row heights are measured lazily: each frame the grid element asks for
//! the rows it is about to draw ([`GridDoc::fit_wrapped_rows`]), so a
//! million-row sheet only ever measures what is on screen. A row's height
//! is cached under a hash of everything it depends on (its wrapped cells'
//! text and column widths, and the font's metrics at this zoom), so an
//! unchanged row costs one hash per frame and resizing a column, zooming or
//! editing a cell measures it again. A row whose height was set by hand
//! keeps it.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};

use super::grid::{GridDoc, SHEET};
use crate::formula::value::Value;
use crate::grid::cellfmt::CellFmt;
use crate::grid::wrap::{self, Metrics};

/// Cached row heights kept before the cache is dropped and refilled.
const CACHE_LIMIT: usize = 10_000;

impl GridDoc {
    /// What cell `(r, c)` shows with formatting `fmt` (number formats and
    /// Scheme column formats applied), and whether that is a number — which
    /// Wrap Text leaves on one line, as Excel does.
    pub(super) fn wrap_text(&self, r: usize, c: usize, fmt: CellFmt) -> (Cow<'_, str>, bool) {
        let value = self.shown_text(r, c);
        if let Some((text, numeric)) = fmt.num.and_then(|code| self.number_formatted(r, c, code)) {
            return (Cow::Owned(text), numeric);
        }
        let header_row = r == 0 && self.wb.header(SHEET);
        let text = match (!header_row && self.style.touches(c))
            .then(|| self.style.display(c, value))
            .flatten()
        {
            Some(t) => Cow::Owned(t),
            None => Cow::Borrowed(value),
        };
        let numeric = match self.formula_value(r, c) {
            Some(v) => matches!(v, Value::Number(_)),
            None => crate::grid::infer::cell_number(&text).is_some(),
        } && !header_row;
        (text, numeric)
    }

    /// The height row `r` needs for its wrapped cells (100% px), and the
    /// key it is cached under.
    fn wrapped_row_height(&self, r: usize, m: Metrics) -> (u64, f64) {
        let mut h = rustc_hash::FxHasher::default();
        m.char_px.to_bits().hash(&mut h);
        m.line_px.to_bits().hash(&mut h);
        let cells: Vec<(usize, f64)> = self
            .formats
            .wrapped_in_row(r)
            .map(|c| (c, self.view.cols.size(c)))
            .filter(|&(_, w)| w > 0.0)
            .collect();
        let texts: Vec<(Cow<'_, str>, bool, f64)> = cells
            .iter()
            .map(|&(c, w)| {
                let (text, numeric) = self.wrap_text(r, c, self.formats.get(r, c));
                (text, numeric, w)
            })
            .collect();
        for (text, numeric, w) in &texts {
            text.hash(&mut h);
            numeric.hash(&mut h);
            w.to_bits().hash(&mut h);
        }
        let key = h.finish();
        if let Some(&(k, height)) = self.wrap_cache.get(&r)
            && k == key
        {
            return (key, height);
        }
        let lines = texts
            .iter()
            .filter(|(text, numeric, _)| !numeric && !text.is_empty())
            .map(|(text, _, w)| wrap::wrap_lines(text, m.chars(*w)).len())
            .max()
            .unwrap_or(1);
        (key, wrap::row_height(lines, m.line_px))
    }

    /// Size `rows` (those about to be drawn) to fit their wrapped cells,
    /// leaving rows sized by hand alone. Returns whether any height changed
    /// (positions below it then moved).
    pub(super) fn fit_wrapped_rows(
        &mut self,
        rows: impl IntoIterator<Item = usize>,
        m: Metrics,
    ) -> bool {
        if !self.formats.any_wrapped() && !self.view.rows.has_auto() {
            return false;
        }
        if self.wrap_cache.len() > CACHE_LIMIT {
            self.wrap_cache.clear();
        }
        let mut changed = false;
        for r in rows {
            if self.view.rows.is_manual(r) {
                continue;
            }
            if self.formats.wrapped_in_row(r).next().is_none() {
                changed |= self.view.rows.set_auto(r, self.view.rows.default);
                continue;
            }
            let (key, height) = self.wrapped_row_height(r, m);
            self.wrap_cache.insert(r, (key, height));
            changed |= self.view.rows.set_auto(r, height);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::view::DEFAULT_ROW_HEIGHT;

    const M: Metrics = Metrics {
        char_px: 8.0,
        line_px: 18.0,
        pad_px: 3.0,
    };

    fn doc(csv: &str) -> GridDoc {
        let wb = crate::grid::parse::parse_now(csv.as_bytes().to_vec(), Some("csv")).unwrap();
        GridDoc::new(wb, Some("csv".into()), 0, &Default::default())
    }

    fn wrap(g: &mut GridDoc, r: usize, c: usize) {
        g.format_cells([(r, c)], |f| CellFmt { wrap: true, ..f });
    }

    /// 64 px columns hold (64 − 2·3) / 8 = 7 characters: a wrapped cell's
    /// row grows a line pitch per extra line, newlines included.
    #[test]
    fn rows_grow_to_fit_wrapped_cells() {
        let mut g = doc("k,v\na,one two three\nb,\"x\ny\"\nc,unwrapped text that is long\n");
        g.view.cols.set(1, 64.0);
        wrap(&mut g, 1, 1);
        wrap(&mut g, 2, 1);
        assert!(g.fit_wrapped_rows(0..5, M));
        assert_eq!(g.view.rows.size(0), DEFAULT_ROW_HEIGHT);
        // "one two" / "three".
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT + 18.0);
        // An explicit newline: "x" / "y".
        assert_eq!(g.view.rows.size(2), DEFAULT_ROW_HEIGHT + 18.0);
        assert_eq!(g.view.rows.size(3), DEFAULT_ROW_HEIGHT, "not wrapped");
        assert!(!g.fit_wrapped_rows(0..5, M), "nothing changed");
        // A wider column needs one line; a narrower one three.
        g.view.cols.set(1, 120.0);
        assert!(g.fit_wrapped_rows(0..5, M));
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT);
        g.view.cols.set(1, 40.0); // 4 characters: "one" / "two" / "thre" / "e"
        g.fit_wrapped_rows(0..5, M);
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT + 3.0 * 18.0);
        // Zooming changes the advance a little: measured again.
        let zoomed = Metrics { char_px: 9.5, ..M };
        g.view.cols.set(1, 64.0); // 6 characters: "one" / "two" / "three"
        g.fit_wrapped_rows(0..5, zoomed);
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT + 2.0 * 18.0);
        // Turning wrap off brings the row back to one line.
        g.format_cells([(1, 1)], |f| CellFmt { wrap: false, ..f });
        g.fit_wrapped_rows(0..5, M);
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT);
        // Only the rows asked about are measured.
        g.format_cells([(1, 1)], |f| CellFmt { wrap: true, ..f });
        g.fit_wrapped_rows(2..3, M);
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT);
    }

    /// A height set by hand wins over Wrap Text, and is what saves.
    #[test]
    fn a_height_set_by_hand_stays() {
        let mut g = doc("v\nalpha beta gamma\n");
        g.view.cols.set(0, 64.0);
        wrap(&mut g, 1, 0);
        g.view.rows.set(1, 25.0);
        assert!(!g.fit_wrapped_rows(0..2, M));
        assert_eq!(g.view.rows.size(1), 25.0);
        assert_eq!(g.view.rows.overrides().collect::<Vec<_>>(), [(1, 25.0)]);
        // Back to the default by hand: automatic again.
        g.view.rows.set(1, DEFAULT_ROW_HEIGHT);
        assert!(g.fit_wrapped_rows(0..2, M));
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT + 2.0 * 18.0);
        assert_eq!(g.view.rows.overrides().count(), 0, "not persisted");
    }

    /// Numbers stay on one line, as in Excel; a number format's text is
    /// what wraps.
    #[test]
    fn numbers_never_wrap() {
        let mut g = doc("v\n12345678901234\n");
        g.view.cols.set(0, 40.0);
        wrap(&mut g, 1, 0);
        g.fit_wrapped_rows(0..2, M);
        assert_eq!(g.view.rows.size(1), DEFAULT_ROW_HEIGHT);
        assert!(g.wrap_text(1, 0, g.formats.get(1, 0)).1);
    }
}
