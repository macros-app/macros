//! Per-buffer view state for a grid: cursor and selection, scroll position,
//! column/row geometry, zoom, and the in-progress cell edit.
//!
//! Pure data and arithmetic (no GPUI), so the geometry that both the renderer
//! and mouse hit-testing depend on can be unit-tested directly. Every length
//! here is in *unzoomed* pixels (100% zoom); the renderer multiplies by
//! [`GridView::zoom`] when it paints. Positions are `f64` because a million
//! 20 px rows is 2·10⁷ px, past where `f32` can still address a single pixel.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::formula::refs;

/// Excel's default column width at 100% zoom: 8.43 characters of Calibri 11,
/// which Excel renders as 64 px.
pub const DEFAULT_COL_WIDTH: f64 = 64.0;
/// Excel's default row height at 100% zoom: 15 pt = 20 px.
pub const DEFAULT_ROW_HEIGHT: f64 = 20.0;
/// Dragging a border below this hides the column/row (stored as size 0), the
/// way Excel does.
pub const MIN_SIZE: f64 = 4.0;
/// The widest column / tallest row (100% px): sizes past it buy nothing on
/// screen and break text layout arithmetic.
pub const MAX_SIZE: f64 = 100_000.0;
/// Zoom steps for `grid-zoom-in`/`-out`, Excel's preset ladder widened to the
/// 10%–400% range.
pub const ZOOM_STEPS: &[f32] = &[
    0.10, 0.25, 0.40, 0.50, 0.67, 0.75, 0.85, 1.00, 1.10, 1.25, 1.50, 1.75, 2.00, 2.50, 3.00, 4.00,
];
/// Below this zoom cells are painted as tinted blocks instead of text: the
/// glyphs would be unreadable anyway, and skipping shaping keeps a zoomed-out
/// overview of a huge file fast.
pub const BLOCK_ZOOM: f32 = 0.40;
/// Keeps `f64` positions exact: 2³¹ rows × even a 400 px row is far inside
/// 2⁵³. Not a data limit — the model is unbounded — just where the view stops.
pub const MAX_INDEX: usize = 1 << 31;

/// One axis (columns or rows): a default size plus sparse per-index overrides.
///
/// Only sizes that differ from the default are stored, so the start offset of
/// any index — even millions deep in the empty area past the data — is
/// `i × default` plus the deltas of the overrides before it, with no per-index
/// table.
///
/// Two more ways to take an index off screen, both keeping its size for when
/// it comes back: `hidden` (explicitly hidden columns, `grid-hide-column`) and
/// `only` (a row filter / narrowing: inside its range, only the listed indices
/// show). Both stay cheap at a million rows — the filter is a sorted list
/// searched by bisection, never a per-index override.
#[derive(Debug, Clone, PartialEq)]
pub struct Axis {
    pub default: f64,
    overrides: BTreeMap<usize, f64>,
    hidden: BTreeSet<usize>,
    only: Option<Arc<Only>>,
}

/// An axis's per-index layout — sizes set by hand and hidden indices — which
/// follows its rows or columns through structural edits and their undo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sizing {
    overrides: BTreeMap<usize, f64>,
    hidden: BTreeSet<usize>,
}

/// A filter over an axis: indices below `end` show only when listed in
/// `shown` (sorted, deduplicated); indices from `end` on are unaffected.
#[derive(Debug, Clone, PartialEq)]
pub struct Only {
    pub shown: Vec<usize>,
    pub end: usize,
}

impl Axis {
    pub fn new(default: f64) -> Self {
        Self {
            default,
            overrides: BTreeMap::new(),
            hidden: BTreeSet::new(),
            only: None,
        }
    }

    /// Size of index `i` (0 when hidden, filtered out, or dragged shut).
    pub fn size(&self, i: usize) -> f64 {
        if !self.shown(i) {
            return 0.0;
        }
        self.base(i)
    }

    /// The stored size, ignoring hiding and filtering.
    pub fn base(&self, i: usize) -> f64 {
        self.overrides.get(&i).copied().unwrap_or(self.default)
    }

    /// Not hidden and not filtered out (it may still be dragged to 0).
    pub fn shown(&self, i: usize) -> bool {
        !self.filtered_out(i) && !self.hidden.contains(&i)
    }

    fn filtered_out(&self, i: usize) -> bool {
        self.only
            .as_ref()
            .is_some_and(|o| i < o.end && o.shown.binary_search(&i).is_err())
    }

    /// Set index `i`'s size; a size equal to the default drops the override.
    pub fn set(&mut self, i: usize, size: f64) {
        let size = if size < MIN_SIZE {
            0.0
        } else {
            size.min(MAX_SIZE)
        };
        if (size - self.default).abs() < f64::EPSILON {
            self.overrides.remove(&i);
        } else {
            self.overrides.insert(i, size);
        }
    }

    /// The overrides, for persisting.
    pub fn overrides(&self) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.overrides.iter().map(|(&i, &s)| (i, s))
    }

    /// Hide or show index `i` (its size is kept for when it shows again).
    pub fn set_hidden(&mut self, i: usize, hidden: bool) {
        if hidden {
            self.hidden.insert(i);
        } else {
            self.hidden.remove(&i);
        }
    }

    /// Explicitly hidden indices, in order.
    pub fn hidden(&self) -> impl Iterator<Item = usize> + '_ {
        self.hidden.iter().copied()
    }

    /// The per-index layout (sizes and hidden indices), for undo.
    pub fn sizing(&self) -> Sizing {
        Sizing {
            overrides: self.overrides.clone(),
            hidden: self.hidden.clone(),
        }
    }

    pub fn set_sizing(&mut self, sizing: Sizing) {
        self.overrides = sizing.overrides;
        self.hidden = sizing.hidden;
    }

    /// Whether any index has its own size or is hidden.
    pub fn has_sizing(&self) -> bool {
        !self.overrides.is_empty() || !self.hidden.is_empty()
    }

    /// Move each index's size and hidden flag to `map(index)`, dropping
    /// those whose index was deleted (`None`).
    pub fn remap(&mut self, map: impl Fn(usize) -> Option<usize>) {
        self.overrides = std::mem::take(&mut self.overrides)
            .into_iter()
            .filter_map(|(i, s)| Some((map(i)?, s)))
            .collect();
        self.hidden = std::mem::take(&mut self.hidden)
            .into_iter()
            .filter_map(&map)
            .collect();
    }

    /// The active filter, if any.
    pub fn only(&self) -> Option<&Arc<Only>> {
        self.only.as_ref()
    }

    /// Install (or with `None`, drop) a filter.
    pub fn set_only(&mut self, only: Option<Only>) {
        self.only = only.map(|mut o| {
            o.shown.sort_unstable();
            o.shown.dedup();
            Arc::new(o)
        });
    }

    /// How many indices below `i` are filtered out or hidden.
    fn invisible_below(&self, i: usize) -> usize {
        let filtered = self.only.as_ref().map_or(0, |o| {
            let lim = i.min(o.end);
            lim - o.shown.partition_point(|&s| s < lim)
        });
        let hidden = self
            .hidden
            .range(..i)
            .filter(|&&h| !self.filtered_out(h))
            .count();
        filtered + hidden
    }

    /// Start offset of index `i`.
    pub fn start(&self, i: usize) -> f64 {
        let delta: f64 = self
            .overrides
            .range(..i)
            .filter(|&(&k, _)| self.shown(k))
            .map(|(_, &s)| s - self.default)
            .sum();
        (i - self.invisible_below(i)) as f64 * self.default + delta
    }

    /// The index whose span contains `pos` (clamped to 0 for negatives).
    /// Hidden (zero-size) indices are never returned.
    pub fn index_at(&self, pos: f64) -> usize {
        if pos <= 0.0 {
            return self.next_visible(0);
        }
        if self.hidden.is_empty() && self.only.is_none() {
            // The common case: walk the overrides in order, accumulating
            // their deltas, until `pos` falls before the next one; between
            // overrides sizes are uniform.
            let mut delta = 0.0;
            for (&k, &s) in &self.overrides {
                let k_start = k as f64 * self.default + delta;
                if pos < k_start {
                    break;
                }
                if pos < k_start + s {
                    return k;
                }
                delta += s - self.default;
            }
            let i = ((pos - delta) / self.default).floor().max(0.0) as usize;
            return self.next_visible(i.min(MAX_INDEX));
        }
        // With hidden or filtered indices: bisect for the last index starting
        // at or before `pos` (`start` is monotonic and cheap).
        let (mut lo, mut hi) = (0usize, MAX_INDEX);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if self.start(mid) <= pos {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        self.next_visible(lo)
    }

    /// `i`, or the first index after it that has a size.
    pub fn next_visible(&self, mut i: usize) -> usize {
        while i < MAX_INDEX {
            if let Some(o) = &self.only
                && i < o.end
                && o.shown.binary_search(&i).is_err()
            {
                // Jump straight to the next listed index (or the filter's end).
                let k = o.shown.partition_point(|&s| s < i);
                i = o.shown.get(k).copied().unwrap_or(o.end);
                continue;
            }
            if self.size(i) > 0.0 {
                break;
            }
            i += 1;
        }
        i
    }

    /// `i`, or the last index before it that has a size.
    pub fn prev_visible(&self, mut i: usize) -> Option<usize> {
        loop {
            if let Some(o) = &self.only
                && i < o.end
                && o.shown.binary_search(&i).is_err()
            {
                let k = o.shown.partition_point(|&s| s < i);
                i = *o.shown.get(k.checked_sub(1)?)?;
                continue;
            }
            if self.size(i) > 0.0 {
                return Some(i);
            }
            i = i.checked_sub(1)?;
        }
    }

    /// Position of `i` among the shown indices (for zebra striping that
    /// alternates on screen even when a filter hides rows in between).
    pub fn rank(&self, i: usize) -> usize {
        i - self.invisible_below(i)
    }
}

/// What a range selection covers: a cell rectangle, whole rows (`V`), or whole
/// columns (`C-v`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    Cells,
    Rows,
    Cols,
}

/// The in-progress edit of one cell. The same text is shown in the cell and
/// in the formula bar; `in_bar` only says which one draws the caret.
#[derive(Debug, Clone, PartialEq)]
pub struct CellEdit {
    pub row: usize,
    pub col: usize,
    pub text: String,
    /// Caret byte offset into `text` (always a char boundary).
    pub caret: usize,
    /// The cell's value when the edit began — restored by `C-g`.
    pub original: String,
    pub in_bar: bool,
    /// The function-name dropdown while typing a formula, if open.
    pub completion: Option<FormulaCompletion>,
    /// The text the dropdown was dismissed (Escape) at: it stays closed
    /// until the text changes.
    pub completion_dismissed: Option<String>,
    /// Point mode: the reference the arrow keys are steering, inserted at
    /// `span` in `text` (see [`CellEdit::point_eligible`]).
    pub point: Option<PointRef>,
}

/// The LSP-style dropdown of function names for the partial name before the
/// caret (`=SU|` offers `SUM`, `SUMIF`, …).
#[derive(Debug, Clone, PartialEq)]
pub struct FormulaCompletion {
    /// Byte offset in the edit text where the partial name starts.
    pub start: usize,
    /// Matching function names, upper case, sorted.
    pub names: Vec<String>,
    pub selected: usize,
}

/// A reference inserted by pointing (arrows or a click) while typing a
/// formula: where it sits in the text and the cells it names.
#[derive(Debug, Clone, PartialEq)]
pub struct PointRef {
    pub span: std::ops::Range<usize>,
    /// The fixed corner (moves with plain arrows; stays put with Shift).
    pub anchor: Pos,
    /// The moving corner.
    pub at: Pos,
}

impl CellEdit {
    /// Whether the arrows should point at cells rather than move the caret:
    /// a formula whose caret sits right after an operator, `(`, `,`, or `=`
    /// — or at the end of a reference pointing just inserted.
    pub fn point_eligible(&self) -> bool {
        if !self.text.starts_with('=') {
            return false;
        }
        if let Some(p) = &self.point
            && p.span.end == self.caret
        {
            return true;
        }
        self.text[..self.caret]
            .trim_end()
            .chars()
            .next_back()
            .is_some_and(|c| "=(,+-*/^&<>:;".contains(c))
    }

    /// Insert (or replace the pointed) reference text for `anchor`..`at`.
    pub fn set_point(&mut self, anchor: Pos, at: Pos) {
        let text = if anchor == at {
            cell_name(at.0, at.1)
        } else {
            let (r0, r1) = (anchor.0.min(at.0), anchor.0.max(at.0));
            let (c0, c1) = (anchor.1.min(at.1), anchor.1.max(at.1));
            format!("{}:{}", cell_name(r0, c0), cell_name(r1, c1))
        };
        let span = match &self.point {
            Some(p) if p.span.end == self.caret => p.span.clone(),
            _ => self.caret..self.caret,
        };
        self.text.replace_range(span.clone(), &text);
        let end = span.start + text.len();
        self.caret = end;
        self.point = Some(PointRef {
            span: span.start..end,
            anchor,
            at,
        });
    }
    pub fn new(row: usize, col: usize, original: &str, text: String, caret_at_end: bool) -> Self {
        let caret = if caret_at_end { text.len() } else { 0 };
        Self {
            row,
            col,
            text,
            caret,
            original: original.to_string(),
            in_bar: false,
            completion: None,
            completion_dismissed: None,
            point: None,
        }
    }

    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.caret]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.caret..]
            .graphemes(true)
            .next()
            .map_or(self.caret, |g| self.caret + g.len())
    }

    /// Start of the word before the caret (skipping any whitespace first).
    fn word_back(&self) -> usize {
        let before = &self.text[..self.caret];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .char_indices()
            .rev()
            .find(|&(_, c)| !c.is_alphanumeric())
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    /// End of the word after the caret (skipping any non-word chars first).
    fn word_forward(&self) -> usize {
        let after = &self.text[self.caret..];
        let skip = after.len()
            - after
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .len();
        let rest = &after[skip..];
        let word = rest
            .char_indices()
            .find(|&(_, c)| !c.is_alphanumeric())
            .map_or(rest.len(), |(i, _)| i);
        self.caret + skip + word
    }

    pub fn backspace(&mut self) {
        let start = self.prev_boundary();
        self.text.replace_range(start..self.caret, "");
        self.caret = start;
    }

    pub fn delete(&mut self) {
        let end = self.next_boundary();
        self.text.replace_range(self.caret..end, "");
    }

    pub fn left(&mut self) {
        self.caret = self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.caret = self.next_boundary();
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.text.len();
    }

    pub fn word_left(&mut self) {
        self.caret = self.word_back();
    }

    pub fn word_right(&mut self) {
        self.caret = self.word_forward();
    }

    /// `C-w`: delete the word before the caret.
    pub fn kill_word_back(&mut self) {
        let start = self.word_back();
        self.text.replace_range(start..self.caret, "");
        self.caret = start;
    }

    /// `C-u`: delete everything before the caret.
    pub fn kill_to_start(&mut self) {
        self.text.replace_range(..self.caret, "");
        self.caret = 0;
    }

    /// `C-k`: delete everything after the caret.
    pub fn kill_to_end(&mut self) {
        self.text.truncate(self.caret);
    }

    /// Place the caret at char index `chars` (clamped), for clicks.
    pub fn set_caret_chars(&mut self, chars: usize) {
        self.caret = self
            .text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(i, _)| i);
    }
}

/// Everything about how one grid buffer is being looked at.
#[derive(Debug, Clone)]
pub struct GridView {
    /// `(row, col)` of the cursor cell.
    pub cursor: Pos,
    /// The other corner of a range selection, when one is active.
    pub anchor: Option<Pos>,
    pub sel_kind: SelKind,
    /// Scroll offsets into the scrollable area (past frozen rows), unzoomed px.
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub cols: Axis,
    pub rows: Axis,
    pub zoom: f32,
    pub edit: Option<CellEdit>,
    /// Rows frozen at the top (1 when the sheet has a header row).
    pub frozen_rows: usize,
    pub bar_visible: bool,
    /// Cells show formulas instead of their results (`grid-show-formulas`).
    pub show_formulas: bool,
    /// Cells outlined by `grid-trace-precedents` / `-dependents`; cleared
    /// by the next command.
    pub trace: Vec<CellRange>,
    /// The cells-area viewport (excluding headers and the formula bar) in
    /// *screen* px, as last painted. Used to keep the cursor in view.
    pub viewport: (f64, f64),
    /// Helix's extra cursors (`C`, `s`), each one cell, in no particular
    /// order; `cursor` is the primary. A cell edit spreads to all of them.
    pub cursors: Vec<Pos>,
    /// How the open edit spreads to [`Self::cursors`].
    pub spread: Spread,
}

/// How a cell edit made with several cursors applies to the other cursors'
/// cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Spread {
    /// `c` (or typing over a cell): every cell gets the typed text.
    #[default]
    Replace,
    /// `i`: what's typed before the cell's text goes before each cell's.
    Insert,
    /// `a`: what's typed after the cell's text goes after each cell's.
    Append,
}

impl Default for GridView {
    fn default() -> Self {
        Self {
            cursor: (0, 0),
            anchor: None,
            sel_kind: SelKind::Cells,
            scroll_x: 0.0,
            scroll_y: 0.0,
            cols: Axis::new(DEFAULT_COL_WIDTH),
            rows: Axis::new(DEFAULT_ROW_HEIGHT),
            zoom: 1.0,
            edit: None,
            frozen_rows: 0,
            bar_visible: true,
            show_formulas: false,
            trace: Vec::new(),
            viewport: (800.0, 600.0),
            cursors: Vec::new(),
            spread: Spread::Replace,
        }
    }
}

/// One column or row on screen: where it starts (screen px from the cells
/// area's left or top edge) and how wide or tall it draws.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleLine {
    pub idx: usize,
    pub pos: f32,
    pub size: f32,
    /// A frozen row (drawn over the scrolled ones).
    pub frozen: bool,
}

/// An inclusive cell rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRange {
    pub r0: usize,
    pub c0: usize,
    pub r1: usize,
    pub c1: usize,
}

impl CellRange {
    /// Just the cell `(r, c)`.
    pub fn cell((r, c): Pos) -> Self {
        Self {
            r0: r,
            c0: c,
            r1: r,
            c1: c,
        }
    }

    /// From `(r0, c0, r1, c1)` bounds (as [`crate::formula::ast::Area::bounds`]
    /// gives them).
    pub fn from_bounds((r0, c0, r1, c1): (usize, usize, usize, usize)) -> Self {
        Self { r0, c0, r1, c1 }
    }

    /// One cell only.
    pub fn is_single(&self) -> bool {
        self.r0 == self.r1 && self.c0 == self.c1
    }

    /// Every cell, row by row.
    pub fn cells(self) -> impl Iterator<Item = Pos> {
        (self.r0..=self.r1).flat_map(move |r| (self.c0..=self.c1).map(move |c| (r, c)))
    }

    pub fn contains(&self, r: usize, c: usize) -> bool {
        (self.r0..=self.r1).contains(&r) && (self.c0..=self.c1).contains(&c)
    }

    pub fn rows(&self) -> usize {
        self.r1 - self.r0 + 1
    }

    pub fn cols(&self) -> usize {
        self.c1 - self.c0 + 1
    }

    /// The first and last row, or column, it spans.
    pub fn span(&self, axis: refs::Axis) -> (usize, usize) {
        match axis {
            refs::Axis::Rows => (self.r0, self.r1),
            refs::Axis::Cols => (self.c0, self.c1),
        }
    }
}

impl GridView {
    /// The selected rectangle — the cursor cell alone when nothing is selected.
    /// Row/column selections are bounded to `used` (the data extent) so a
    /// "whole column" copy doesn't try to walk an unbounded range.
    pub fn selection(&self, used: (usize, usize)) -> CellRange {
        let (r, c) = self.cursor;
        let (ar, ac) = self.anchor.unwrap_or(self.cursor);
        let mut range = CellRange {
            r0: r.min(ar),
            c0: c.min(ac),
            r1: r.max(ar),
            c1: c.max(ac),
        };
        if self.anchor.is_some() {
            match self.sel_kind {
                SelKind::Cells => {}
                SelKind::Rows => {
                    range.c0 = 0;
                    range.c1 = used.1.saturating_sub(1).max(range.c1.min(used.1));
                }
                SelKind::Cols => {
                    range.r0 = 0;
                    range.r1 = used.0.saturating_sub(1).max(range.r1.min(used.0));
                }
            }
        }
        range
    }

    /// The rows' or the columns' geometry.
    pub fn axis(&self, which: refs::Axis) -> &Axis {
        match which {
            refs::Axis::Rows => &self.rows,
            refs::Axis::Cols => &self.cols,
        }
    }

    pub fn axis_mut(&mut self, which: refs::Axis) -> &mut Axis {
        match which {
            refs::Axis::Rows => &mut self.rows,
            refs::Axis::Cols => &mut self.cols,
        }
    }

    /// Zoomed size of column `c` / row `r`, in screen px.
    pub fn col_px(&self, c: usize) -> f64 {
        self.cols.size(c) * self.zoom as f64
    }

    pub fn row_px(&self, r: usize) -> f64 {
        self.rows.size(r) * self.zoom as f64
    }

    /// Height of the frozen rows band, unzoomed.
    pub fn frozen_height(&self) -> f64 {
        self.rows.start(self.frozen_rows)
    }

    /// Screen x (relative to the cells area) of column `c`'s left edge.
    pub fn col_x(&self, c: usize) -> f64 {
        (self.cols.start(c) - self.scroll_x) * self.zoom as f64
    }

    /// Screen y (relative to the cells area) of row `r`'s top edge. Frozen
    /// rows sit at the top; the rest scroll underneath them.
    pub fn row_y(&self, r: usize) -> f64 {
        let z = self.zoom as f64;
        if r < self.frozen_rows {
            self.rows.start(r) * z
        } else {
            (self.rows.start(r) - self.scroll_y) * z
        }
    }

    /// The column under screen x (relative to the cells area).
    pub fn col_at(&self, x: f64) -> usize {
        self.cols.index_at(x / self.zoom as f64 + self.scroll_x)
    }

    /// The row under screen y (relative to the cells area).
    pub fn row_at(&self, y: f64) -> usize {
        let z = self.zoom as f64;
        let unzoomed = y / z;
        if unzoomed < self.frozen_height() {
            return self.rows.index_at(unzoomed);
        }
        self.rows
            .index_at(unzoomed + self.scroll_y)
            .max(self.frozen_rows)
    }

    /// First scrollable row on screen.
    pub fn top_row(&self) -> usize {
        self.rows
            .index_at(self.frozen_height() + self.scroll_y)
            .max(self.frozen_rows)
    }

    /// Rows that fit in the viewport below the frozen band (at least 1).
    pub fn page_rows(&self) -> usize {
        let avail = self.viewport.1 / self.zoom as f64 - self.frozen_height();
        ((avail / self.rows.default).floor() as usize).max(1)
    }

    /// Clamp scroll to non-negative and the view's addressing limit. There is
    /// deliberately no upper bound tied to the data: the grid scrolls on into
    /// empty cells, the way Excel's does.
    pub fn clamp_scroll(&mut self) {
        let max_y = self.rows.start(MAX_INDEX);
        let max_x = self.cols.start(MAX_INDEX);
        self.scroll_y = self.scroll_y.clamp(0.0, max_y);
        self.scroll_x = self.scroll_x.clamp(0.0, max_x);
    }

    /// Scroll the minimum needed to show the cursor cell.
    pub fn reveal_cursor(&mut self) {
        let (r, c) = self.cursor;
        let z = self.zoom as f64;
        let (vw, vh) = (self.viewport.0 / z, self.viewport.1 / z);
        // Horizontal.
        let x0 = self.cols.start(c);
        let x1 = x0 + self.cols.size(c);
        if x0 < self.scroll_x {
            self.scroll_x = x0;
        } else if x1 > self.scroll_x + vw {
            self.scroll_x = (x1 - vw).min(x0);
        }
        // Vertical (frozen rows are always visible).
        if r >= self.frozen_rows {
            let frozen = self.frozen_height();
            let avail = (vh - frozen).max(0.0);
            let y0 = self.rows.start(r) - frozen;
            let y1 = y0 + self.rows.size(r);
            if y0 < self.scroll_y {
                self.scroll_y = y0;
            } else if y1 > self.scroll_y + avail {
                self.scroll_y = (y1 - avail).min(y0);
            }
        }
        self.clamp_scroll();
    }

    /// The columns on screen across `width` screen px of cells area, left
    /// to right (hidden ones skipped).
    pub fn visible_cols(&self, width: f32) -> Vec<VisibleLine> {
        let mut out = Vec::new();
        let mut c = self.col_at(0.0);
        loop {
            let x = self.col_x(c) as f32;
            if x >= width || c >= MAX_INDEX {
                break;
            }
            let w = self.col_px(c) as f32;
            if w > 0.0 {
                out.push(VisibleLine {
                    idx: c,
                    pos: x,
                    size: w,
                    frozen: false,
                });
            }
            c = self.cols.next_visible(c + 1);
        }
        out
    }

    /// The rows on screen down `height` screen px of cells area: the
    /// frozen ones first, then the scrolled ones (hidden ones skipped).
    pub fn visible_rows(&self, height: f32) -> Vec<VisibleLine> {
        let mut out = Vec::new();
        let mut push = |r: usize, y: f32, frozen: bool| {
            let h = self.row_px(r) as f32;
            if h > 0.0 {
                out.push(VisibleLine {
                    idx: r,
                    pos: y,
                    size: h,
                    frozen,
                });
            }
        };
        for r in 0..self.frozen_rows {
            let y = self.row_y(r) as f32;
            if y >= height {
                break; // the rest of the frozen band is off screen
            }
            push(r, y, true);
        }
        let mut r = self.top_row();
        loop {
            let y = self.row_y(r) as f32;
            if y >= height || r >= MAX_INDEX {
                break;
            }
            push(r, y, false);
            // Skips a filter's hidden rows in one jump, however many.
            r = self.rows.next_visible(r + 1);
        }
        out
    }

    /// Step the zoom one preset up (`dir > 0`) or down.
    pub fn step_zoom(&mut self, dir: i32) {
        let i = ZOOM_STEPS
            .iter()
            .position(|&z| z >= self.zoom - 1e-3)
            .unwrap_or(ZOOM_STEPS.len() - 1);
        let next = if dir > 0 {
            if ZOOM_STEPS[i] > self.zoom + 1e-3 {
                i
            } else {
                (i + 1).min(ZOOM_STEPS.len() - 1)
            }
        } else {
            i.saturating_sub(1)
        };
        self.zoom = ZOOM_STEPS[next];
    }
}

// Column letters and A1 names, shared with the .xlsx code (which keeps them,
// being a self-contained module).
pub use crate::xlsx::{cell_name, col_name, parse_col_name};

/// Parse `C7` / `c7` / `$C$7` into `(row, col)`. Unlike [`crate::xlsx::parse_ref`]
/// this is not capped at Excel's limits: a CSV grid can hold far more than
/// 1,048,576 rows, and goto must reach all of them.
pub fn parse_cell_name(s: &str) -> Option<(usize, usize)> {
    let s = s.trim().replace('$', "");
    let split = s.find(|c: char| c.is_ascii_digit())?;
    let col = parse_col_name(&s[..split])?;
    let row: usize = s[split..].parse().ok()?;
    (row >= 1).then(|| (row - 1, col))
}

/// A `(row, col)` cell position.
pub type Pos = (usize, usize);

/// `p`'s coordinate along `axis`: its row or its column.
pub fn coord(p: Pos, axis: refs::Axis) -> usize {
    match axis {
        refs::Axis::Rows => p.0,
        refs::Axis::Cols => p.1,
    }
}

pub fn coord_mut(p: &mut Pos, axis: refs::Axis) -> &mut usize {
    match axis {
        refs::Axis::Rows => &mut p.0,
        refs::Axis::Cols => &mut p.1,
    }
}

/// Parse an address or range: `C7`, `B2:D9`. A range returns both corners.
pub fn parse_address(s: &str) -> Option<(Pos, Option<Pos>)> {
    match s.split_once(':') {
        Some((a, b)) => Some((parse_cell_name(a)?, Some(parse_cell_name(b)?))),
        None => Some((parse_cell_name(s)?, None)),
    }
}

/// Name of a range for the name box: `C7` alone, `B2:D9` for a rectangle.
pub fn range_name(range: &CellRange) -> String {
    if range.is_single() {
        cell_name(range.r0, range.c0)
    } else {
        format!(
            "{}:{}",
            cell_name(range.r0, range.c0),
            cell_name(range.r1, range.c1)
        )
    }
}

/// Split clipboard text into a block of cells. Tab-separated with line
/// breaks — the format Excel, Sheets, and Numbers all put on the clipboard —
/// with a single trailing line break dropped. A field that starts with `"`
/// is quoted the way Excel copies a cell holding a tab, newline, or quote
/// (`""` inside is one `"`), so [`to_tsv_block`] round-trips exactly.
pub fn parse_tsv_block(text: &str) -> Vec<Vec<String>> {
    split_delimited(text, '\t')
}

/// Clipboard text as cells, whatever app it came from. Tab-separated text is
/// TSV. Without tabs, two or more lines that all split into the same number
/// (> 1) of fields on `,` `;` or `|` — CSV copied from a text buffer or a
/// terminal — split on that delimiter (the grid's own `preferred` first).
/// Anything else is one column, one line per cell; a single line is never
/// split (so pasting `Smith, John` stays one cell).
pub fn parse_clipboard_block(text: &str, preferred: u8) -> Vec<Vec<String>> {
    if text.contains('\t') {
        return parse_tsv_block(text);
    }
    let mut candidates = vec![preferred as char];
    candidates.extend(
        [',', ';', '|']
            .into_iter()
            .filter(|&d| d != preferred as char),
    );
    for d in candidates {
        if d == '\t' || !text.contains(d) {
            continue;
        }
        let block = split_delimited(text, d);
        let lines = block.iter().filter(|r| r.iter().any(|c| !c.is_empty()));
        let widths: Vec<usize> = lines.map(Vec::len).collect();
        if widths.len() >= 2 && widths[0] > 1 && widths.iter().all(|&w| w == widths[0]) {
            return block;
        }
    }
    parse_tsv_block(text)
}

/// Split `text` into rows of fields on `delim`, with CSV quoting: a field
/// that starts with `"` runs to the closing `"` (which may span lines), `""`
/// inside it being a literal quote. A quote that never closes, or is followed
/// by stray text, leaves the field literal. One trailing line break is
/// dropped; `\r\n` and `\n` both end a row.
fn split_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let chars: Vec<char> = text.chars().collect();
    let mut rows = vec![Vec::new()];
    let mut i = 0;
    loop {
        let (field, next) = if chars.get(i) == Some(&'"') {
            match quoted_field(&chars, i, delim) {
                Some(done) => done,
                None => plain_field(&chars, i, delim),
            }
        } else {
            plain_field(&chars, i, delim)
        };
        rows.last_mut().expect("a row").push(field);
        i = next;
        match chars.get(i) {
            None => break,
            Some(&c) if c == delim => i += 1,
            Some('\r') => {
                i += if chars.get(i + 1) == Some(&'\n') {
                    2
                } else {
                    1
                };
                rows.push(Vec::new());
            }
            Some(_) => {
                // '\n'
                i += 1;
                rows.push(Vec::new());
            }
        }
    }
    rows
}

/// An unquoted field from `i`: up to the next delimiter or line break.
fn plain_field(chars: &[char], i: usize, delim: char) -> (String, usize) {
    let end = chars[i..]
        .iter()
        .position(|&c| c == delim || c == '\n' || c == '\r')
        .map_or(chars.len(), |p| i + p);
    let field: String = chars[i..end].iter().collect();
    (field.strip_suffix('\r').unwrap_or(&field).to_string(), end)
}

/// A quoted field opening at `i`, or `None` when the quoting is broken.
fn quoted_field(chars: &[char], i: usize, delim: char) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut j = i + 1;
    loop {
        match chars.get(j)? {
            '"' if chars.get(j + 1) == Some(&'"') => {
                out.push('"');
                j += 2;
            }
            '"' => {
                j += 1;
                return match chars.get(j) {
                    None => Some((out, j)),
                    Some(&c) if c == delim || c == '\n' || c == '\r' => Some((out, j)),
                    Some(_) => None,
                };
            }
            &c => {
                out.push(c);
                j += 1;
            }
        }
    }
}

/// The inverse of [`parse_tsv_block`]: tabs between cells, `\n` after rows.
/// Cell text containing a tab or newline is quoted, the way Excel copies it.
pub fn to_tsv_block(rows: &[Vec<String>]) -> String {
    super::export::to_delimited(rows, '\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_start_and_index_round_trip_with_overrides() {
        let mut a = Axis::new(64.0);
        assert_eq!(a.start(10), 640.0);
        a.set(2, 100.0); // +36
        a.set(5, 0.0); // hidden, -64
        assert_eq!(a.start(3), 3.0 * 64.0 + 36.0);
        assert_eq!(a.start(6), 6.0 * 64.0 + 36.0 - 64.0);
        for i in 0..20 {
            if a.size(i) > 0.0 {
                assert_eq!(a.index_at(a.start(i) + 1.0), i, "index {i}");
            }
        }
        // A hidden column is skipped by hit-testing.
        assert_eq!(a.index_at(a.start(5)), 6);
        // Deep into the empty area stays exact.
        assert_eq!(a.index_at(a.start(1_000_000) + 0.5), 1_000_000);
    }

    #[test]
    fn axis_hidden_and_filtered_indices() {
        let mut a = Axis::new(20.0);
        a.set(3, 40.0);
        a.set_hidden(1, true);
        assert_eq!(a.size(1), 0.0);
        assert_eq!(a.base(1), 20.0, "hiding keeps the size");
        assert_eq!(a.start(2), 20.0);
        assert_eq!(a.start(4), 20.0 + 20.0 + 40.0);
        assert_eq!(a.index_at(25.0), 2);
        assert_eq!(a.next_visible(1), 2);
        assert_eq!(a.prev_visible(1), Some(0));
        // A filter showing rows 0, 3 and 7 out of the first 10.
        a.set_hidden(1, false);
        a.set_only(Some(Only {
            shown: vec![7, 0, 3],
            end: 10,
        }));
        assert_eq!(a.size(5), 0.0);
        assert_eq!(a.start(3), 20.0);
        assert_eq!(a.start(7), 60.0);
        assert_eq!(a.start(10), 80.0, "past the filter, rows are normal");
        assert_eq!(a.start(12), 120.0);
        assert_eq!(a.index_at(10.0), 0);
        assert_eq!(a.index_at(30.0), 3);
        assert_eq!(a.index_at(65.0), 7);
        assert_eq!(a.index_at(85.0), 10);
        assert_eq!(a.next_visible(1), 3);
        assert_eq!(a.next_visible(8), 10);
        assert_eq!(a.prev_visible(6), Some(3));
        assert_eq!(a.prev_visible(9), Some(7));
        assert_eq!(a.rank(7), 2);
        assert_eq!(a.rank(11), 4);
        a.set_only(None);
        assert_eq!(a.start(5), 120.0);
        // A million-row filter stays fast (bisection, not a walk).
        let mut big = Axis::new(20.0);
        big.set_only(Some(Only {
            shown: vec![0, 999_998],
            end: 1_000_000,
        }));
        assert_eq!(big.index_at(25.0), 999_998);
        assert_eq!(big.next_visible(1), 999_998);
    }

    #[test]
    fn axis_set_to_default_drops_override() {
        let mut a = Axis::new(20.0);
        a.set(3, 40.0);
        a.set(3, 20.0);
        assert_eq!(a.overrides().count(), 0);
        a.set(4, 2.0); // below MIN_SIZE hides
        assert_eq!(a.size(4), 0.0);
    }

    #[test]
    fn reveal_cursor_scrolls_minimally_and_respects_frozen_rows() {
        let mut v = GridView {
            viewport: (640.0, 200.0), // 10 cols × 10 rows
            frozen_rows: 1,
            ..GridView::default()
        };
        v.cursor = (30, 0);
        v.reveal_cursor();
        // Row 30 should be the last visible row under one frozen row.
        let bottom = v.row_y(30) + v.row_px(30);
        assert!(bottom <= 200.0 + 1e-6 && bottom > 180.0, "bottom {bottom}");
        v.cursor = (1, 0);
        v.reveal_cursor();
        assert_eq!(v.scroll_y, 0.0);
        assert_eq!(v.row_at(5.0), 0, "frozen header row");
        assert_eq!(v.row_at(25.0), 1);
    }

    #[test]
    fn zoom_scales_hit_testing() {
        let mut v = GridView {
            zoom: 2.0,
            ..GridView::default()
        };
        assert_eq!(v.col_at(127.0), 0);
        assert_eq!(v.col_at(129.0), 1);
        v.step_zoom(1);
        assert_eq!(v.zoom, 2.5);
        v.step_zoom(-1);
        v.step_zoom(-1);
        assert_eq!(v.zoom, 1.75);
    }

    #[test]
    fn column_names_round_trip() {
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(16383), "XFD");
        assert_eq!(col_name(16384), "XFE");
        for c in [0, 1, 25, 26, 51, 52, 701, 702, 16383, 100_000] {
            assert_eq!(parse_col_name(&col_name(c)), Some(c));
        }
        assert_eq!(parse_cell_name("C7"), Some((6, 2)));
        assert_eq!(parse_cell_name("$c$7"), Some((6, 2)));
        assert_eq!(parse_cell_name("C0"), None);
        assert_eq!(parse_address("B2:D9"), Some(((1, 1), Some((8, 3)))));
    }

    #[test]
    fn cell_edit_motions() {
        let mut e = CellEdit::new(0, 0, "", "hello wide world".into(), true);
        e.word_left();
        assert_eq!(&e.text[e.caret..], "world");
        e.kill_word_back();
        assert_eq!(e.text, "hello world");
        e.home();
        e.word_right();
        assert_eq!(e.caret, 5);
        e.kill_to_end();
        assert_eq!(e.text, "hello");
        e.insert("é");
        e.left();
        e.delete();
        assert_eq!(e.text, "hello");
        e.backspace();
        assert_eq!(e.text, "hell");
        e.set_caret_chars(2);
        e.kill_to_start();
        assert_eq!(e.text, "ll");
    }

    #[test]
    fn tsv_block_round_trip() {
        let block = parse_tsv_block("a\tb\r\nc\td\n");
        assert_eq!(block, vec![vec!["a", "b"], vec!["c", "d"]]);
        assert_eq!(to_tsv_block(&block), "a\tb\nc\td\n");
        // Cells holding a quote, a tab, or a line break survive the trip.
        let tricky = vec![
            vec!["say \"hi\"".to_string(), "x\ty".to_string()],
            vec!["two\nlines".to_string(), "5\" screen".to_string()],
        ];
        assert_eq!(parse_tsv_block(&to_tsv_block(&tricky)), tricky);
        // A quote mid-field, or one that never closes, is literal.
        assert_eq!(
            parse_tsv_block("5\" disk\t\"open"),
            vec![vec!["5\" disk", "\"open"]]
        );
    }

    #[test]
    fn clipboard_delimiter_sniffing() {
        let csv = parse_clipboard_block("a,b,c\n1,\"2,5\",3\n", b',');
        assert_eq!(csv, vec![vec!["a", "b", "c"], vec!["1", "2,5", "3"]]);
        let semi = parse_clipboard_block("a;b\n1;2", b',');
        assert_eq!(semi, vec![vec!["a", "b"], vec!["1", "2"]]);
        let pipes = parse_clipboard_block("x|y\n1|2", b'\t');
        assert_eq!(pipes, vec![vec!["x", "y"], vec!["1", "2"]]);
        // One line is never split; ragged lines aren't a table.
        assert_eq!(
            parse_clipboard_block("Smith, John", b','),
            vec![vec!["Smith, John"]]
        );
        assert_eq!(
            parse_clipboard_block("a, b\nc", b','),
            vec![vec!["a, b"], vec!["c"]]
        );
        // Tabs always mean TSV.
        assert_eq!(
            parse_clipboard_block("a,b\tc", b','),
            vec![vec!["a,b", "c"]]
        );
    }

    #[test]
    fn visible_lines_skip_hidden_and_put_frozen_rows_first() {
        let mut v = GridView {
            frozen_rows: 1,
            scroll_y: 40.0,
            ..GridView::default()
        };
        v.cols.set_hidden(1, true);
        let line = |idx, pos, size, frozen| VisibleLine {
            idx,
            pos,
            size,
            frozen,
        };
        assert_eq!(
            v.visible_cols(200.0),
            [
                line(0, 0.0, 64.0, false),
                line(2, 64.0, 64.0, false),
                line(3, 128.0, 64.0, false),
                line(4, 192.0, 64.0, false),
            ]
        );
        // Row 0 frozen at the top; the scroll starts the rest at row 3.
        assert_eq!(
            v.visible_rows(70.0),
            [
                line(0, 0.0, 20.0, true),
                line(3, 20.0, 20.0, false),
                line(4, 40.0, 20.0, false),
                line(5, 60.0, 20.0, false),
            ]
        );
        v.zoom = 2.0;
        assert_eq!(v.visible_rows(30.0), [line(0, 0.0, 40.0, true)]);
    }

    #[test]
    fn cell_range_cells() {
        let r = CellRange::from_bounds((1, 2, 2, 3));
        assert!(!r.is_single());
        assert_eq!(
            r.cells().collect::<Vec<_>>(),
            [(1, 2), (1, 3), (2, 2), (2, 3)]
        );
        let one = CellRange::cell((4, 5));
        assert!(one.is_single());
        assert_eq!(one.cells().collect::<Vec<_>>(), [(4, 5)]);
        assert_eq!(range_name(&one), "F5");
    }

    #[test]
    fn selection_kinds() {
        let mut v = GridView {
            cursor: (3, 2),
            anchor: Some((1, 1)),
            ..GridView::default()
        };
        assert_eq!(
            v.selection((10, 5)),
            CellRange {
                r0: 1,
                c0: 1,
                r1: 3,
                c1: 2
            }
        );
        v.sel_kind = SelKind::Rows;
        assert_eq!(v.selection((10, 5)).c1, 4);
        v.sel_kind = SelKind::Cols;
        assert_eq!(v.selection((10, 5)).r1, 9);
    }
}
