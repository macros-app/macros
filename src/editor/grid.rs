//! Grid buffers for [`Editor`]: CSV/TSV files shown and edited as a
//! spreadsheet (see `docs/plans/grid-buffer.md`).
//!
//! A grid buffer is an ordinary [`Document`] in the buffer list — so the
//! buffer menu, perspectives, the mode line, `kill-buffer`, and session
//! restore treat it like any file — whose `grid` field carries the
//! [`Workbook`] (the data) and [`GridView`] (cursor, scroll, geometry). Its
//! rope stays empty; every place that writes a buffer to disk asks
//! [`Document::save_bytes`], which serializes the workbook for a grid.
//!
//! Keys: grid normal resolves against the Scheme keymaps in
//! `scheme/grid.scm`; cell insert is a small native line editor (see
//! [`Editor::key_grid`] and [`Editor::grid_action_key`]).

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use gpui::{Context, Keystroke, MouseButton, Pixels, Point, SharedString};

use super::input::Handled;
use super::*;
use crate::formula::refs::Axis;
use crate::grid::cellfmt::{self, CellFmt, CellFormats, NumCode};
use crate::grid::cmd::{Dir, GridCmd, GridInvocation, GridOp};
use crate::grid::infer::ColType;
use crate::grid::model::{CellsSnapshot, Change, Workbook};
use crate::grid::sidecar::Sidecar;
use crate::grid::style::RuleTest;
use crate::grid::view::{
    self, CellEdit, CellRange, GridView, Pos, SelKind, Spread, cell_name, parse_address, range_name,
};

/// The grid has a single sheet until `.xlsx` support lands.
pub(super) const SHEET: usize = 0;

/// Files at most this big parse on the UI thread in one go; bigger ones show
/// their first rows at once and finish loading in the background.
const SYNC_PARSE_LIMIT: usize = 2 * 1024 * 1024;
/// Rows parsed up front for the instant first paint of a large file.
const PREFIX_ROWS: usize = 200;
/// Files over this ask before opening as a grid.
const LARGE_FILE: u64 = 1024 * 1024 * 1024;
/// Width of the formula bar's name box, in screen px.
pub(super) const NAME_BOX_WIDTH: f32 = 200.0;
/// Height of the formula bar, in screen px (not zoomed — it's chrome).
pub(super) const BAR_HEIGHT: f32 = 26.0;
/// Half-width of the grabbable band on a header border.
const BORDER_GRAB: f32 = 3.0;
/// Rows sampled by the quick auto-fit (visible rows are always included).
const AUTOFIT_SAMPLE: usize = 1000;
/// Grids up to this many cells publish a copy for Scheme's synchronous
/// `grid-cell` reads (re-copied only after an edit).
const SYNC_READ_CELLS: usize = 1_000_000;

/// Workbook replacements (load, revert) get a fresh epoch so caches keyed by
/// the workbook's generation counters never confuse the new one with the old.
fn next_epoch() -> u64 {
    static EPOCH: AtomicU64 = AtomicU64::new(1);
    EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Editor-wide grid settings (see [`crate::grid::cmd::GridOption`]).
#[derive(Debug, Clone)]
pub(super) struct GridOptions {
    pub default_col_width: f64,
    pub max_autofit_width: f64,
    pub auto_header: Option<bool>,
    pub truncate_indicator: String,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            default_col_width: view::DEFAULT_COL_WIDTH,
            max_autofit_width: 2000.0,
            auto_header: None,
            truncate_indicator: String::new(),
        }
    }
}

/// Editor-wide grid state (one per editor, not per grid buffer).
pub(super) struct GridUi {
    /// File extensions that open as a grid (`grid-extensions`, lower case).
    pub extensions: Vec<String>,
    /// Painted geometry per grid pane (set by `GridElement` each frame).
    pub geom: HashMap<usize, GridGeom>,
    /// A mouse drag (selection or header resize) on a grid pane.
    pub drag: Option<GridDrag>,
    /// The echo the grid last showed (formula-bar fallback), so it only
    /// replaces its own text, never someone else's message.
    pub echo: Option<SharedString>,
    /// Editor-wide grid settings (`grid-default-column-width`, …).
    pub opts: GridOptions,
    /// An open `ge` popup.
    pub cell_popup: Option<CellPopup>,
    /// A background slice loop is finishing a large recalculation.
    pub recalc_scheduled: bool,
    /// The formulas of the last grid copy (see `FormulaClip`).
    pub formula_clip: Option<super::grid_formula::FormulaClip>,
    /// Where the grid's function-name dropdown was painted (for clicks).
    pub completion_hit: Option<PopupHit>,
}

impl Default for GridUi {
    fn default() -> Self {
        Self {
            extensions: ["csv", "tsv", "tab", "psv", "xlsx"]
                .map(String::from)
                .to_vec(),
            geom: HashMap::new(),
            drag: None,
            echo: None,
            opts: GridOptions::default(),
            cell_popup: None,
            recalc_scheduled: false,
            formula_clip: None,
            completion_hit: None,
        }
    }
}

/// An open `ge` popup: a text buffer editing one grid cell.
#[derive(Debug, Clone)]
pub(super) struct CellPopup {
    /// The popup buffer (`*cell C7*`).
    pub buffer: String,
    /// The grid buffer the cell belongs to.
    pub grid: String,
    pub row: usize,
    pub col: usize,
}

/// One stage of a row filter. Filters stack: each keeps only rows the ones
/// before it kept.
#[derive(Debug, Clone)]
pub(super) enum RowFilter {
    /// Rows whose `col` value passes `test`.
    Test {
        col: usize,
        test: RuleTest,
        label: String,
    },
    /// Exactly these rows (`C-x n n`). Positional, so it can't follow rows
    /// that move: a structural change drops it.
    Rows { rows: Vec<usize>, label: String },
}

impl RowFilter {
    fn label(&self) -> &str {
        match self {
            RowFilter::Test { label, .. } | RowFilter::Rows { label, .. } => label,
        }
    }
}

/// Cursor, anchor and selection kind: what `grid-selection-changed-hook`
/// compares to decide whether to fire.
type SelState = (Pos, Option<Pos>, SelKind);
/// A cached cell copy and the `(epoch, generation)` it was made at.
type PublishedCells = Option<((u64, u64), Arc<CellsSnapshot>)>;

/// The paste `M-y` may replace.
#[derive(Debug, Clone)]
pub(super) struct YankState {
    /// Kill-ring index that was pasted.
    pub index: usize,
    /// Grid history depth right after the paste.
    pub depth: usize,
    /// Cursor and anchor before the paste.
    pub cursor: Pos,
    pub anchor: Option<Pos>,
}

/// One changed cell: `(row, col, old, new)`.
type CellChange = (usize, usize, String, String);

/// One step of the grid's undo history. Data edits are delegated to the
/// workbook's own undo stack; what lives only in the grid (formatting set by
/// hand, column and row sizes, hidden columns) is recorded here.
#[derive(Debug, Clone)]
enum Hist {
    /// `steps` workbook undo steps, with the grid's [`Layout`] before and
    /// after when the edit moved it (a sort, inserted rows, a moved column).
    Model {
        steps: usize,
        layout: Option<Box<(Layout, Layout)>>,
    },
    /// Formatting set by hand: `(cell, before, after)` per cell.
    Format(Vec<(Pos, CellFmt, CellFmt)>),
    /// A column's width or a row's height set.
    Resize {
        axis: Axis,
        idx: usize,
        old: f64,
        new: f64,
    },
    /// Columns hidden (`hidden`) or shown.
    Hide { cols: Vec<usize>, hidden: bool },
    /// A sheet added, renamed, deleted or moved: replayed by the editor
    /// across the workbook (see `grid_sheets.rs`).
    Sheet(super::grid_sheets::SheetStep),
    /// Everything one Scheme command (or one native command made of
    /// several edits) did: undone last-first, as one step.
    Group(Vec<Hist>),
}

/// A grid's undo history. Data edits are delegated to the workbook's own
/// undo stack (see [`Hist::Model`]); this orders them with the grid-only
/// steps and groups the edits of one command into one step.
#[derive(Debug)]
struct History {
    undo: Vec<Hist>,
    redo: Vec<Hist>,
    /// The Scheme command batch now applying, if any: model edits made under
    /// the same batch merge into one undo step.
    batch: Option<u64>,
    /// The batch the last history entry belongs to.
    last_batch: Option<u64>,
    /// Source of local undo batches (grouping several model edits made by
    /// one native command, e.g. deleting the visible rows of a filter).
    local_batch: u64,
}

impl Default for History {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            batch: None,
            last_batch: None,
            // Far from the Scheme batches' sequence numbers.
            local_batch: u64::MAX / 2,
        }
    }
}

impl History {
    /// Push an entry; within one batch (a Scheme command, or a
    /// [`GridDoc::grouped`] native one) it joins the batch's single step.
    fn record(&mut self, entry: Hist) {
        match (self.batch, self.undo.last_mut()) {
            (Some(b), Some(last)) if self.last_batch == Some(b) => match last {
                Hist::Group(entries) => entries.push(entry),
                _ => {
                    let prev = std::mem::replace(last, Hist::Group(Vec::new()));
                    *last = Hist::Group(vec![prev, entry]);
                }
            },
            _ => self.undo.push(entry),
        }
        self.last_batch = self.batch;
        self.redo.clear();
    }

    /// The entry an undo (`undo`) or redo replays next, taken off its stack.
    fn take(&mut self, undo: bool) -> Option<Hist> {
        if undo {
            self.undo.pop()
        } else {
            self.redo.pop()
        }
    }

    /// File `entry`, just replayed, on the other stack. The next edit starts
    /// a step of its own.
    fn replayed(&mut self, entry: Hist, undo: bool) {
        self.last_batch = None;
        if undo {
            self.redo.push(entry);
        } else {
            self.undo.push(entry);
        }
    }

    /// Open a local batch; returns the batch to restore when it ends.
    fn begin_local(&mut self) -> Option<u64> {
        self.local_batch += 1;
        self.batch.replace(self.local_batch)
    }

    /// How many steps undo can take.
    fn len(&self) -> usize {
        self.undo.len()
    }

    /// Fold every entry pushed since the history was `depth` deep into one
    /// step (`undo-amalgamate-end`: a query-replace session). Untouched when
    /// it is shallower now (an undo mid-way).
    fn squash_since(&mut self, depth: usize) {
        if self.undo.len() > depth + 1 {
            let tail = self.undo.split_off(depth);
            self.undo.push(Hist::Group(tail));
        }
        // The next edit starts a step of its own.
        self.last_batch = None;
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// What an `.xlsx` grid carries besides its cells (all empty for CSV).
#[derive(Debug, Default)]
pub(super) struct XlsxState {
    /// What the file holds that a rewrite would drop (charts, styles, hidden
    /// sheets, …), and so saving over the file is refused.
    pub lossy: Vec<String>,
    /// The sheet names, in workbook order.
    pub sheets: Vec<String>,
    /// Which of them this buffer shows.
    pub sheet: usize,
    /// The workbook a sheet buffer (`report.xlsx<Costs>`) was opened from;
    /// such a buffer visits no file itself.
    pub source: Option<PathBuf>,
    /// The workbook's other sheets' values, by lower-case name, for
    /// `Costs!B2` references: from the file, then live from open sheet
    /// buffers ([`Editor::grid_sync_sheets_of`]).
    pub other_sheets: HashMap<String, crate::formula::engine::SheetValues>,
    /// Cells the file stored as text that would otherwise be saved as
    /// another type (`"12"` as a number), with that text: saved as text while
    /// they still hold it (an edit, then undo, included).
    pub text: HashMap<(usize, usize), String>,
    /// `(data generation, formula evaluations)` when this sheet's values
    /// were last handed to the other open sheets.
    pub sheet_published: Option<(u64, u64)>,
    /// The workbook this sheet belongs to, shared with its other sheets'
    /// buffers, once it has several sheets (see `grid_sheets.rs`).
    pub book: Option<super::grid_xlsx::Book>,
    /// Sheet operations an undo or redo just replayed here, for the editor
    /// to carry out across the workbook (`true`: undone).
    pub sheet_replays: Vec<(super::grid_sheets::SheetStep, bool)>,
}

/// A grid's state against the file on disk.
#[derive(Debug, Default)]
struct SaveState {
    /// Hash of the bytes last read from / written to disk, so auto-revert can
    /// tell our own save apart from an outside change.
    disk_hash: u64,
    /// The last save failed: stay modified even if undo returns to the
    /// workbook's saved point.
    failed: bool,
    /// `formats` as last saved (or loaded): formatting differing from it
    /// makes the buffer modified, since an `.xlsx` saves it.
    formats: CellFormats,
    /// String-pool size after the last load/compaction: edits leave orphaned
    /// strings in the append-only pool, reclaimed on save once they pile up.
    pool_baseline: usize,
}

/// What of a grid follows its cells through structural edits: formatting
/// set by hand, and the columns' and rows' sizes and hidden flags.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    formats: CellFormats,
    cols: view::Sizing,
    rows: view::Sizing,
    xlsx_text: HashMap<(usize, usize), String>,
}

/// What only a grid holds, kept while it's shown as raw text.
pub(super) struct GridStash {
    text: String,
    formats: CellFormats,
    style: crate::grid::style::GridStyle,
    choices: HashMap<usize, Vec<String>>,
}

/// What `.` repeats.
#[derive(Debug, Clone)]
enum RepeatOp {
    Cmd(GridCmd),
    /// A committed cell edit: set the cursor cell to this text.
    SetText(String),
}

/// A background parse in flight for a large file.
pub(super) struct GridLoading {
    cancel: Arc<AtomicBool>,
    pub progress: Arc<AtomicUsize>,
    pub total: usize,
    /// A save was requested mid-load; run it when the load lands.
    save_after: bool,
}

/// The grid half of a [`Document`].
pub(super) struct GridDoc {
    pub wb: Workbook,
    pub view: GridView,
    /// Undo and redo, and the batches edits group into.
    history: History,
    pub loading: Option<GridLoading>,
    /// A `v`/`V`/`C-v`/`C-SPC` selection: motions extend it until Escape.
    /// (Shift-arrow selections aren't sticky — the next plain motion drops
    /// them, as in a spreadsheet.)
    sticky_sel: bool,
    last_repeat: Option<RepeatOp>,
    search: Option<String>,
    /// What was last read from or written to disk.
    save: SaveState,
    /// Lower-cased file extension, the dialect hint for re-parses.
    pub(super) ext: Option<String>,
    /// The mode line's range summary, recomputed after each command (not
    /// per frame — a whole-column selection can span a million cells).
    pub summary: String,
    /// Declarative styling set from Scheme (faces, number formats, rules).
    pub style: crate::grid::style::GridStyle,
    /// Bold and colors set by hand (`grid-toggle-bold`, …); session only.
    pub formats: CellFormats,
    /// See [`next_epoch`].
    pub(super) epoch: u64,
    /// Values `grid-complete-cell` offers per column (`grid-column-choices`).
    pub(super) choices: HashMap<usize, Vec<String>>,
    /// The row filters in force (`grid-filter`, `C-x n n`).
    pub(super) filters: Vec<RowFilter>,
    /// `(epoch, layout generation)` the filters were last evaluated at.
    filter_layout: (u64, u64),
    pub(super) yank: Option<YankState>,
    /// `C-l` cycles middle → top → bottom while repeated.
    pub(super) recenter_phase: u8,
    /// What `grid-selection-changed-hook` last saw.
    hook_sel: Option<SelState>,
    /// The cell copy published for Scheme, keyed by `(epoch, generation)`.
    published: RefCell<PublishedCells>,
    /// The formula engine: which cells are formulas, their results, and
    /// recalculation (see `grid_formula.rs`).
    pub(super) formulas: crate::formula::engine::Engine,
    /// A formula has existed in this buffer, so structural edits must
    /// re-index (a plain CSV never pays for the scan).
    pub(super) had_formulas: bool,
    /// What an `.xlsx` brought along: its sheets, what a rewrite would
    /// drop, and cells stored as text.
    pub(super) xlsx: XlsxState,
    /// A structural edit moved column widths, hidden columns or row heights
    /// (to persist with the file's layout).
    pub(super) layout_moved: bool,
    /// Cells the user changed in the current command, for
    /// `grid-cell-changed-hook` (drained by `grid_fire_changes`).
    changed: Vec<CellChange>,
}

impl Drop for GridDoc {
    fn drop(&mut self) {
        // Killing the buffer mid-load stops the parser at its next batch.
        if let Some(l) = &self.loading {
            l.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl GridDoc {
    pub(super) fn new(
        wb: Workbook,
        ext: Option<String>,
        disk_hash: u64,
        opts: &GridOptions,
    ) -> Self {
        // Nothing is frozen by default; `grid-toggle-freeze` opts in.
        let mut view = GridView::default();
        view.cols.default = opts.default_col_width.max(view::MIN_SIZE);
        let mut wb = wb;
        if let Some(h) = opts.auto_header
            && h != wb.header(SHEET)
        {
            wb.set_header(SHEET, h);
        }
        let pool_baseline = wb.pool().len();
        Self {
            wb,
            view,
            history: History::default(),
            loading: None,
            sticky_sel: false,
            last_repeat: None,
            search: None,
            save: SaveState {
                disk_hash,
                pool_baseline,
                ..SaveState::default()
            },
            ext,
            summary: String::new(),
            style: Default::default(),
            formats: CellFormats::default(),
            epoch: next_epoch(),
            choices: HashMap::new(),
            filters: Vec::new(),
            filter_layout: (0, 0),
            yank: None,
            recenter_phase: 0,
            hook_sel: None,
            published: RefCell::new(None),
            formulas: crate::formula::engine::Engine::new(),
            had_formulas: false,
            xlsx: XlsxState::default(),
            layout_moved: false,
            changed: Vec::new(),
        }
    }

    /// `wb` was just replaced by a fresh parse.
    pub(super) fn replaced_workbook(&mut self, opts: &GridOptions) {
        self.epoch = next_epoch();
        if let Some(h) = opts.auto_header
            && h != self.wb.header(SHEET)
        {
            self.wb.set_header(SHEET, h);
        }
        self.yank = None;
        // Positions mean nothing in the new data: drop hand formatting and
        // positional narrowing (`C-x n n`); filters by value re-apply.
        self.formats.clear();
        self.save.formats.clear();
        self.filters.retain(|f| matches!(f, RowFilter::Test { .. }));
        self.rebuild_formulas();
        self.refresh_derived();
    }

    /// Run `f`, merging the model edits it makes into one undo step.
    pub(super) fn grouped(&mut self, f: impl FnOnce(&mut Self)) {
        let saved = self.history.begin_local();
        f(self);
        self.history.batch = saved;
    }

    /// Whether row `r` is shown (not filtered out).
    pub(super) fn row_shown(&self, r: usize) -> bool {
        self.view.rows.shown(r)
    }

    /// Rows the filters always keep: the header and any frozen rows.
    fn pinned_rows(&self) -> usize {
        self.view
            .frozen_rows
            .max(usize::from(self.wb.header(SHEET)))
    }

    /// Re-run the filter stack over the current rows and install the result.
    /// Returns the number of data rows shown.
    pub(super) fn apply_filters(&mut self) -> usize {
        let (rows, _) = self.used();
        self.filter_layout = (self.epoch, self.wb.layout_generation());
        if self.filters.is_empty() {
            self.view.rows.set_only(None);
            return rows;
        }
        let pinned = self.pinned_rows().min(rows);
        let mut keep: Vec<usize> = (pinned..rows).collect();
        for f in &self.filters {
            match f {
                RowFilter::Test { col, test, .. } => {
                    keep.retain(|&r| test.test_value(self.cell(r, *col)));
                }
                RowFilter::Rows { rows: set, .. } => {
                    keep.retain(|r| set.binary_search(r).is_ok());
                }
            }
        }
        let shown_data = keep.len();
        let mut shown: Vec<usize> = (0..pinned).collect();
        shown.extend(keep);
        self.view
            .rows
            .set_only(Some(view::Only { shown, end: rows }));
        shown_data
    }

    /// Rows changed shape since the filters ran: re-run them (dropping a
    /// positional narrowing, which can't follow moved rows). Returns a
    /// message when something was dropped.
    fn refilter_if_reshaped(&mut self) -> Option<&'static str> {
        if self.filters.is_empty()
            || self.filter_layout == (self.epoch, self.wb.layout_generation())
        {
            return None;
        }
        let before = self.filters.len();
        self.filters.retain(|f| matches!(f, RowFilter::Test { .. }));
        self.apply_filters();
        (self.filters.len() < before)
            .then_some("Narrowing dropped: rows were added, removed, or moved")
    }

    /// The filter description for the mode line: `C>100 · 12/1,000 rows`.
    pub(super) fn filter_status(&self) -> String {
        if self.filters.is_empty() {
            return String::new();
        }
        let labels: Vec<&str> = self.filters.iter().map(RowFilter::label).collect();
        let (rows, _) = self.used();
        let pinned = self.pinned_rows().min(rows);
        let shown = self
            .view
            .rows
            .only()
            .map_or(rows, |o| o.shown.len())
            .saturating_sub(pinned);
        format!(
            "{} · {}/{} rows",
            labels.join(" & "),
            group_thousands(shown as f64),
            group_thousands(rows.saturating_sub(pinned) as f64)
        )
    }

    /// Recompute the columnar highlight rules when the data changed.
    pub(super) fn refresh_derived(&mut self) {
        let generation = self.epoch.wrapping_shl(32) ^ self.wb.generation();
        if !self.style.needs_refresh(generation) {
            return;
        }
        let rows = self.wb.used_rows(SHEET);
        let start = usize::from(self.wb.header(SHEET)).min(rows);
        let wb = &self.wb;
        self.style.refresh(generation, |col, f| {
            if col < wb.used_cols(SHEET) {
                wb.for_each_in_column(SHEET, col, start..rows, |_, v| f(v));
            }
        });
    }

    /// The cell copy for Scheme's synchronous reads (cached per edit).
    fn published_cells(&self) -> Option<Arc<CellsSnapshot>> {
        let (rows, cols) = self.used();
        if self.loading.is_some() || rows.saturating_mul(cols) > SYNC_READ_CELLS {
            return None;
        }
        let key = (self.epoch, self.wb.generation());
        let mut slot = self.published.borrow_mut();
        if let Some((k, cells)) = slot.as_ref()
            && *k == key
        {
            return Some(cells.clone());
        }
        let cells = Arc::new(self.wb.cells_snapshot(SHEET));
        *slot = Some((key, cells.clone()));
        Some(cells)
    }

    /// The cursor moved one shown cell `d`-ward (staying put at the edge).
    fn stepped(&self, d: Dir) -> Pos {
        let mut p = self.view.cursor;
        let i = view::coord_mut(&mut p, d.axis());
        *i = self.step(d.axis(), *i, d.forward());
        p
    }

    /// One shown row (column) on from `i` — down (right) when `forward`,
    /// else up (left) — staying put at the edge.
    pub(super) fn step(&self, axis: Axis, i: usize, forward: bool) -> usize {
        let a = self.view.axis(axis);
        if forward {
            a.next_visible(i + 1)
        } else {
            i.checked_sub(1)
                .and_then(|p| a.prev_visible(p))
                .unwrap_or(i)
        }
    }

    /// `i` if shown, else the nearest shown index — forward, or back when
    /// forward would leave the data.
    fn snap(axis: &view::Axis, i: usize, used: usize) -> usize {
        if axis.size(i) > 0.0 {
            return i;
        }
        let next = axis.next_visible(i);
        match axis.prev_visible(i) {
            Some(prev) if next >= used.max(1) => prev,
            _ => next,
        }
    }

    /// The cells of `sel` inside the data, skipping filtered-out rows.
    pub(super) fn visible_cells(&self, sel: &CellRange) -> Vec<Pos> {
        let (rows, cols) = self.used();
        let cols = sel.c0..sel.c1.saturating_add(1).min(cols);
        (sel.r0..sel.r1.saturating_add(1).min(rows))
            .filter(|&r| self.row_shown(r))
            .flat_map(|r| cols.clone().map(move |c| (r, c)))
            .collect()
    }

    /// The selection's shown rows as a block of text (empty past the data).
    pub(super) fn visible_block(&self, sel: &CellRange) -> Vec<Vec<String>> {
        let (rows, cols) = self.used();
        (sel.r0..=sel.r1)
            .filter(|&r| r >= rows || self.row_shown(r))
            .map(|r| {
                (sel.c0..=sel.c1)
                    .map(|c| {
                        if r < rows && c < cols {
                            self.cell(r, c).to_string()
                        } else {
                            String::new()
                        }
                    })
                    .collect()
            })
            .collect()
    }

    pub fn modified(&self) -> bool {
        self.wb.is_modified()
            || self.save.failed
            || self.formats != self.save.formats
            || self
                .xlsx
                .book
                .as_ref()
                .is_some_and(|b| b.borrow().structure_modified())
    }

    /// Record a sheet operation as an undo step (with the rest of a batch).
    pub(super) fn record_sheet_step(&mut self, step: super::grid_sheets::SheetStep) {
        self.record(Hist::Sheet(step));
    }

    /// Hash of the file as last read or written.
    pub(super) fn disk_hash(&self) -> u64 {
        self.save.disk_hash
    }

    /// The file was just written with bytes hashing to `hash` (a workbook
    /// saved from another of its sheets' buffers): record the saved point.
    pub(super) fn saved_as(&mut self, hash: u64) {
        self.mark_saved();
        self.save.failed = false;
        self.save.disk_hash = hash;
    }

    /// Record the data and the formatting as saved.
    pub fn mark_saved(&mut self) {
        self.wb.mark_saved();
        self.save.formats = self.formats.clone();
    }

    pub fn used(&self) -> (usize, usize) {
        (self.wb.used_rows(SHEET), self.wb.used_cols(SHEET))
    }

    /// What a cell shows: a formula's result, anything else as stored. (For
    /// the text to *edit*, see [`GridDoc::source`].)
    pub fn cell(&self, r: usize, c: usize) -> &str {
        let raw = self.wb.cell(SHEET, r, c);
        if crate::formula::engine::is_formula(raw) {
            self.formulas.shown(r, c).unwrap_or("#CALC!")
        } else {
            raw
        }
    }

    pub fn selection(&self) -> CellRange {
        self.view.selection(self.used())
    }

    /// Bytes for a save-as to `target`: a `.tsv`/`.csv`/`.psv` extension
    /// that implies another delimiter rewrites every row with it.
    pub fn bytes_for(&self, target: &Path) -> std::io::Result<Vec<u8>> {
        let ext = ext_of(target);
        if ext.as_deref() == Some("xlsx") {
            return Ok(self.xlsx_bytes());
        }
        if let Some(what) = self.csv_would_lose() {
            return Err(std::io::Error::other(format!(
                "this grid has {what}, which CSV can't store — save it as .xlsx"
            )));
        }
        let current = self.wb.dialect();
        if let Some(e) = self.encoding_error() {
            return Err(std::io::Error::other(e));
        }
        Ok(
            match ext.and_then(|e| crate::grid::dialect::Dialect::delimiter_for_extension(&e)) {
                Some(d) if d != current.delimiter => {
                    crate::grid::write::to_bytes_with_dialect(&self.wb, &current.with_delimiter(d))
                }
                // CSV-type target: CSV, even from an .xlsx grid.
                _ => crate::grid::write::to_bytes(&self.wb),
            },
        )
    }

    /// Why the file can't be written in its encoding, if it can't: a typed
    /// character that a Windows-1252 file can't hold.
    pub fn encoding_error(&self) -> Option<String> {
        if self.is_xlsx() {
            return None;
        }
        let ch = crate::grid::write::unencodable(&self.wb, self.wb.dialect())?;
        Some(format!(
            "{ch:?} can't be stored in this file's Windows-1252 encoding — remove it to save"
        ))
    }

    /// A synchronous save path (save-all on quit, `save-some-buffers`) just
    /// wrote [`Self::bytes`]: record the saved point.
    pub fn saved_externally(&mut self) {
        self.mark_saved();
        self.save.failed = false;
        self.save.disk_hash = bytes_hash(&self.bytes());
    }

    /// The file's bytes as they should be written now (CSV, or `.xlsx` for
    /// a grid visiting one).
    pub fn bytes(&self) -> Vec<u8> {
        if self.is_xlsx() {
            self.xlsx_bytes()
        } else {
            crate::grid::write::to_bytes(&self.wb)
        }
    }

    /// Run a workbook mutation, recording it in the grid history only when
    /// the workbook actually took an undo step (a no-op edit must not make
    /// the next `u` undo something older).
    pub(super) fn model_op<R>(&mut self, f: impl FnOnce(&mut Workbook) -> R) -> R {
        let before = self.wb.undo_depth();
        // Formulas and layout as they were, in case this edit moves cells
        // around. The formula snapshot is the engine's, which hasn't seen
        // the log yet (`absorb_changes` takes it after the command).
        let snap = self.formula_snapshot();
        let mark = self.wb.change_mark();
        let out = f(&mut self.wb);
        let mut layout = None;
        if snap.is_some() || self.has_layout() {
            // Peek at what the edit did; a structural one rewrites the
            // references it moved (one more model step, same undo entry)
            // and carries the layout along with its cells. Cell changes
            // move nothing, so only the structural ones are kept.
            let structural = |changes: &[Change]| -> Vec<Change> {
                changes
                    .iter()
                    .filter(|c| !matches!(c, Change::Cells(_)))
                    .cloned()
                    .collect()
            };
            let moved = structural(self.wb.changes_since(mark));
            if !moved.is_empty() {
                if self.has_layout() {
                    // `f` only touched the workbook: this is still the
                    // layout from before the edit (earlier edits in this
                    // command already carried it along theirs).
                    let before = self.layout();
                    self.remap_layout(&moved);
                    let after = self.layout();
                    if before != after {
                        layout = Some(Box::new((before, after)));
                        self.layout_moved = true;
                    }
                }
                if let Some(snap) = snap {
                    // The snapshot predates every change in the log, so
                    // all of them apply to it.
                    let all = structural(self.wb.changes_since(0));
                    self.rewrite_after(&all, snap);
                }
            }
        }
        let steps = self.wb.undo_depth().saturating_sub(before);
        if steps > 0 {
            self.record(Hist::Model { steps, layout });
        }
        out
    }

    /// Push a history entry (see [`History::record`]).
    fn record(&mut self, entry: Hist) {
        self.history.record(entry);
    }

    /// Undo (`undo`) or redo one history entry. Returns the cell to put the
    /// cursor on, and whether column/row layout changed (to persist it).
    fn replay(&mut self, entry: &Hist, undo: bool) -> (Option<Pos>, bool) {
        match entry {
            Hist::Model { steps, layout } => {
                let mut pos = None;
                for _ in 0..*steps {
                    pos = if undo { self.wb.undo() } else { self.wb.redo() }.or(pos);
                }
                if let Some(l) = layout {
                    self.set_layout(if undo { &l.0 } else { &l.1 }.clone());
                }
                (pos, layout.is_some())
            }
            Hist::Format(cells) => {
                if undo {
                    for &((r, c), before, _) in cells.iter().rev() {
                        self.formats.set(r, c, before);
                    }
                } else {
                    for &((r, c), _, after) in cells {
                        self.formats.set(r, c, after);
                    }
                }
                (cells.first().map(|&(pos, ..)| pos), false)
            }
            Hist::Resize {
                axis,
                idx,
                old,
                new,
            } => {
                self.view
                    .axis_mut(*axis)
                    .set(*idx, if undo { *old } else { *new });
                (None, true)
            }
            Hist::Hide { cols, hidden } => {
                for &c in cols {
                    self.view.cols.set_hidden(c, *hidden != undo);
                }
                (None, true)
            }
            Hist::Sheet(step) => {
                self.xlsx.sheet_replays.push((step.clone(), undo));
                (None, false)
            }
            Hist::Group(entries) => {
                let (mut pos, mut layout) = (None, false);
                let mut step = |g: &mut Self, e: &Hist| {
                    let (p, l) = g.replay(e, undo);
                    pos = p.or(pos);
                    layout |= l;
                };
                if undo {
                    entries.iter().rev().for_each(|e| step(self, e));
                } else {
                    entries.iter().for_each(|e| step(self, e));
                }
                (pos, layout)
            }
        }
    }

    fn layout(&self) -> Layout {
        Layout {
            formats: self.formats.clone(),
            cols: self.view.cols.sizing(),
            rows: self.view.rows.sizing(),
            xlsx_text: self.xlsx.text.clone(),
        }
    }

    fn set_layout(&mut self, layout: Layout) {
        self.formats = layout.formats;
        self.view.cols.set_sizing(layout.cols);
        self.view.rows.set_sizing(layout.rows);
        self.xlsx.text = layout.xlsx_text;
    }

    /// Whether anything would need to follow cells through an edit.
    fn has_layout(&self) -> bool {
        !self.formats.is_empty()
            || !self.xlsx.text.is_empty()
            || self.view.cols.has_sizing()
            || self.view.rows.has_sizing()
    }

    /// Change the hand-set formatting of `cells` with `f`, as one undo step
    /// (merged with the rest of a Scheme batch). Returns how many cells
    /// changed.
    pub(super) fn format_cells(
        &mut self,
        cells: impl IntoIterator<Item = Pos>,
        f: impl Fn(CellFmt) -> CellFmt,
    ) -> usize {
        let mut changed = Vec::new();
        for (r, c) in cells {
            let before = self.formats.get(r, c);
            let after = f(before);
            if after != before {
                self.formats.set(r, c, after);
                changed.push(((r, c), before, after));
            }
        }
        let n = changed.len();
        if n > 0 {
            self.record(Hist::Format(changed));
        }
        n
    }

    /// Depth of the grid's undo history.
    pub(super) fn hist_len(&self) -> usize {
        self.history.len()
    }

    /// Merge the undo steps taken since the history was `depth` deep into
    /// one.
    pub(super) fn squash_undo_since(&mut self, depth: usize) {
        self.history.squash_since(depth);
    }

    /// Set column (or row) `idx`'s size, as an undo step.
    fn resize(&mut self, axis: Axis, idx: usize, size: f64) {
        let sizes = self.view.axis_mut(axis);
        // The stored size, not the shown one: a hidden column shows as 0,
        // and its new size must still be recorded (and undo to the old).
        let old = sizes.base(idx);
        sizes.set(idx, size);
        let new = sizes.base(idx);
        if (old - new).abs() > f64::EPSILON {
            self.record(Hist::Resize {
                axis,
                idx,
                old,
                new,
            });
        }
    }

    /// Hide (or show) columns as one undo step. Returns how many changed.
    pub(super) fn set_cols_hidden(
        &mut self,
        cols: impl IntoIterator<Item = usize>,
        hidden: bool,
    ) -> usize {
        let changed: Vec<usize> = cols
            .into_iter()
            .filter(|&c| self.view.cols.shown(c) == hidden)
            .collect();
        for &c in &changed {
            self.view.cols.set_hidden(c, hidden);
        }
        let n = changed.len();
        if n > 0 {
            self.record(Hist::Hide {
                cols: changed,
                hidden,
            });
        }
        n
    }

    /// Drop the selection (and its stickiness).
    pub(super) fn deselect(&mut self) {
        self.view.anchor = None;
        self.sticky_sel = false;
    }

    /// Up past the first shown row selects the cursor's column; left past
    /// the first shown column selects its row — as if the cursor stepped
    /// onto the header. A sticky (visual) selection is left alone.
    fn select_line_at_edge(&mut self, kind: SelKind) {
        if !self.sticky_sel {
            self.view.anchor = Some(self.view.cursor);
            self.view.sel_kind = kind;
        }
        self.view.reveal_cursor();
    }

    /// Whether whole rows or columns (`kind`) are selected.
    fn line_selected(&self, kind: SelKind) -> bool {
        self.view.anchor.is_some() && self.view.sel_kind == kind
    }

    /// Stepping back off the header: Down from selected columns (Right from
    /// selected rows) at the edge drops the selection and leaves the cursor
    /// where it was. True when it did.
    fn leave_line_at_edge(&mut self, kind: SelKind) -> bool {
        let at_edge = match kind {
            SelKind::Cols => self.step(Axis::Rows, self.view.cursor.0, false) == self.view.cursor.0,
            _ => self.step(Axis::Cols, self.view.cursor.1, false) == self.view.cursor.1,
        };
        let line = !self.sticky_sel && self.line_selected(kind);
        if at_edge && line {
            self.view.anchor = None;
        }
        at_edge && line
    }

    /// Move the cursor, keeping or dropping the selection anchor.
    pub(super) fn move_to(&mut self, r: usize, c: usize, extend: bool) {
        let (rows, cols) = self.used();
        let r = Self::snap(&self.view.rows, r.min(view::MAX_INDEX), rows);
        let c = Self::snap(&self.view.cols, c.min(view::MAX_INDEX), cols);
        if extend {
            if self.view.anchor.is_none() {
                self.view.anchor = Some(self.view.cursor);
                self.view.sel_kind = SelKind::Cells;
            }
        } else if !self.sticky_sel {
            self.view.anchor = None;
        }
        self.view.cursor = (r, c);
        self.view.reveal_cursor();
    }

    /// Excel's Ctrl+Arrow: from a filled cell with a filled neighbour, go to
    /// the end of the filled run; otherwise to the next filled cell; with
    /// nothing further, to the edge of the data.
    fn edge(&self, dr: isize, dc: isize) -> (usize, usize) {
        let (mut r, mut c) = self.view.cursor;
        let (rows, cols) = self.used();
        let in_data = |r: usize, c: usize| r < rows && c < cols;
        let step = |r: usize, c: usize| -> Option<(usize, usize)> {
            let nr = r.checked_add_signed(dr)?;
            let nc = c.checked_add_signed(dc)?;
            (nr <= view::MAX_INDEX && nc <= view::MAX_INDEX).then_some((nr, nc))
        };
        let filled = |r: usize, c: usize| in_data(r, c) && !self.cell(r, c).is_empty();
        let Some((nr, nc)) = step(r, c) else {
            return (r, c);
        };
        if filled(r, c) && filled(nr, nc) {
            (r, c) = (nr, nc);
            while let Some((nr, nc)) = step(r, c) {
                if !filled(nr, nc) {
                    break;
                }
                (r, c) = (nr, nc);
            }
            return (r, c);
        }
        let (mut tr, mut tc) = (nr, nc);
        loop {
            if filled(tr, tc) {
                return (tr, tc);
            }
            // Past the data in the direction of travel: stop at its edge.
            let past = (dr > 0 && tr + 1 >= rows) || (dc > 0 && tc + 1 >= cols);
            if past || !(in_data(tr, tc) || (dr < 0 || dc < 0)) {
                let er = if dr > 0 {
                    rows.saturating_sub(1).max(r)
                } else {
                    tr
                };
                let ec = if dc > 0 {
                    cols.saturating_sub(1).max(c)
                } else {
                    tc
                };
                return (er, ec);
            }
            match step(tr, tc) {
                Some(p) => (tr, tc) = p,
                None => return (tr, tc),
            }
        }
    }

    fn last_filled_col(&self, r: usize) -> usize {
        (0..self.wb.used_cols(SHEET))
            .rev()
            .find(|&c| !self.cell(r, c).is_empty())
            .unwrap_or(0)
    }

    /// Approximate pixel width (unzoomed) needed to show column `c`, from the
    /// widest text among `rows`. The editor font is monospace, so a char count
    /// times the advance is exact for most text.
    fn fit_width(&self, c: usize, rows: impl Iterator<Item = usize>, char_w: f64, max: f64) -> f64 {
        // Measure what the cell shows: a number format can make `249.00`
        // draw as `$249.00` (the header row is never formatted).
        let header = self.wb.header(SHEET);
        let chars = rows
            .map(|r| {
                let raw = self.cell(r, c);
                let shown = if header && r == 0 {
                    None
                } else {
                    self.style.display(c, raw)
                };
                shown
                    .as_deref()
                    .unwrap_or(raw)
                    .lines()
                    .map(|l| l.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        if chars == 0 {
            return self.view.cols.default;
        }
        (chars as f64 * char_w + 10.0).clamp(view::MIN_SIZE, max.max(view::MIN_SIZE))
    }

    /// Name box and contents for the formula bar (or the echo fallback).
    pub fn bar_text(&self) -> (String, String) {
        if let Some(e) = &self.view.edit {
            return (self.name_box_for(e.row, e.col), e.text.clone());
        }
        let sel = self.selection();
        if !sel.is_single() {
            return (range_name(&sel), String::new());
        }
        let (r, c) = self.view.cursor;
        (self.name_box_for(r, c), self.source(r, c).to_string())
    }

    fn name_box_for(&self, r: usize, c: usize) -> String {
        let name = cell_name(r, c);
        if self.wb.header(SHEET) && r > 0 {
            let header = self.cell(0, c);
            if !header.is_empty() {
                return format!("{name} · {header}");
            }
        }
        name
    }

    /// `24 cells · Sum 1,204 · Avg 50.2` for a multi-cell selection.
    pub fn range_summary(&self) -> String {
        let sel = self.selection();
        if sel.is_single() {
            return String::new();
        }
        let (rows, cols) = self.used();
        let (mut count, mut nums, mut sum) = (0usize, 0usize, 0f64);
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        let filtered = self.view.rows.only().is_some();
        if sel.r0 < rows {
            let row_end = (sel.r1 + 1).min(rows);
            // Column-wise bulk iteration: a whole-column selection walks the
            // chunks once instead of looking up every cell.
            for c in sel.c0..=sel.c1.min(cols.saturating_sub(1)) {
                if c >= cols {
                    break;
                }
                self.wb
                    .for_each_in_column(SHEET, c, sel.r0..row_end, |r, v| {
                        if v.is_empty() || (filtered && !self.view.rows.shown(r)) {
                            return;
                        }
                        count += 1;
                        if let Some(n) = crate::grid::infer::cell_number(v) {
                            nums += 1;
                            sum += n;
                            lo = lo.min(n);
                            hi = hi.max(n);
                        }
                    });
            }
        }
        let mut out = format!("{} cells", group_thousands(count as f64));
        if nums > 0 {
            out.push_str(&format!(
                " · Sum {} · Avg {} · Min {} · Max {}",
                fmt_num(sum),
                fmt_num(sum / nums as f64),
                fmt_num(lo),
                fmt_num(hi)
            ));
        }
        out
    }

    /// Set cells as one edit made by the user, queueing `(row, col, old,
    /// new)` for each cell that actually changed — what
    /// `grid-cell-changed-hook` receives (see [`Self::changed`]).
    pub(super) fn set_cells_tracked(&mut self, edits: &[(usize, usize, String)]) {
        // What the user typed becomes what's stored (`=…` → a formula).
        let edits: Vec<(usize, usize, String)> = edits
            .iter()
            .map(|(r, c, t)| (*r, *c, super::grid_formula::stored_input(t)))
            .collect();
        let changes: Vec<CellChange> = edits
            .iter()
            .filter_map(|(r, c, new)| {
                let old = self.wb.cell(SHEET, *r, *c);
                (old != new).then(|| {
                    let shown = |t: &str| crate::formula::engine::display_text(t).to_string();
                    (*r, *c, shown(old), shown(new))
                })
            })
            .collect();
        match edits.as_slice() {
            _ if changes.is_empty() => {}
            [(r, c, text)] => {
                self.model_op(|wb| wb.set_cell(SHEET, *r, *c, text));
            }
            edits => {
                self.model_op(|wb| wb.set_cells(SHEET, edits));
            }
        }
        self.changed.extend(changes);
    }

    /// What Scheme's `grid-*` reads see (published with every snapshot).
    pub fn snapshot(&self) -> crate::script::GridSnapshot {
        let (rows, cols) = self.used();
        let sel = self.selection();
        let header = self.wb.header(SHEET);
        crate::script::GridSnapshot {
            cursor: self.view.cursor,
            selection: (sel.r0, sel.c0, sel.r1, sel.c1),
            rows,
            cols,
            // What the cursor cell shows (its result, for a formula), or the
            // text being typed.
            value: match &self.view.edit {
                Some(e) => e.text.clone(),
                None => self
                    .cell(self.view.cursor.0, self.view.cursor.1)
                    .to_string(),
            },
            formula: {
                let (r, c) = self.view.cursor;
                self.is_formula_at(r, c)
                    .then(|| self.source(r, c).to_string())
            },
            header,
            header_names: if header {
                (0..cols).map(|c| self.cell(0, c).to_string()).collect()
            } else {
                Vec::new()
            },
            col_types: (0..cols)
                .map(|c| self.wb.col_type(SHEET, c).name().to_string())
                .collect(),
            frozen_rows: self.view.frozen_rows,
            zoom: f64::from(self.view.zoom) * 100.0,
            editing: self.view.edit.is_some(),
            cells: self.published_cells(),
            filter: self.view.rows.only().cloned(),
            visible_rows: self.view.rows.only().map_or(rows, |o| o.shown.len()),
            sheets: self.xlsx.sheets.clone(),
            sheet: self.xlsx.sheets.get(self.xlsx.sheet).cloned(),
        }
    }

    /// `C7 · int` (or `B2:D9`) for the mode line.
    pub fn cell_status(&self) -> String {
        let sel = self.selection();
        if !sel.is_single() {
            return range_name(&sel);
        }
        let (r, c) = self.view.cursor;
        let ty = self.wb.col_type(SHEET, c).name();
        format!("{} · {ty}", cell_name(r, c))
    }

    /// Whether column `c` should be right-aligned (numbers, like Excel).
    pub fn right_aligned(&self, c: usize) -> bool {
        match self.style.align(c) {
            Some(a) => a == crate::grid::style::Align::Right,
            None => matches!(self.wb.col_type(SHEET, c), ColType::Int | ColType::Float),
        }
    }
}

/// What a grid command did, for the editor to report.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Outcome {
    /// For the echo area.
    pub msg: Option<Cow<'static, str>>,
    /// Column/row layout changed: save the view sidecar.
    pub persist_view: bool,
}

impl Outcome {
    fn msg(msg: impl Into<Cow<'static, str>>) -> Self {
        Self {
            msg: Some(msg.into()),
            persist_view: false,
        }
    }

    fn persist() -> Self {
        Self {
            msg: None,
            persist_view: true,
        }
    }

    fn and_persist(self) -> Self {
        Self {
            persist_view: true,
            ..self
        }
    }
}

/// What a grid command reads from the editor besides the grid.
#[derive(Debug, Clone, Copy)]
pub(super) struct CmdEnv {
    /// The typed count (0 = none).
    pub count: usize,
    /// A `C-u` flag (see [`GridCmd::prefix_is_flag`]).
    pub flag: bool,
    /// One monospace advance at 100% zoom, for column widths.
    pub char_w: f64,
    /// `grid-max-autofit-width`.
    pub max_fit: f64,
}

impl Default for CmdEnv {
    fn default() -> Self {
        Self {
            count: 0,
            flag: false,
            char_w: 7.0,
            max_fit: GridOptions::default().max_autofit_width,
        }
    }
}

impl GridDoc {
    /// Apply a grid command that only touches this grid (motion, selection,
    /// structure, sizes, view toggles, undo). `Err` is a refusal to report,
    /// with nothing changed. Commands that need the editor (the clipboard,
    /// prompts, Scheme) are [`Editor::grid_command`]'s; here they do nothing.
    pub(super) fn apply(
        &mut self,
        cmd: GridCmd,
        env: &CmdEnv,
    ) -> Result<Outcome, Cow<'static, str>> {
        use GridCmd::*;
        if let Some(done) = self.apply_cursors(cmd) {
            return done.map(|()| Outcome::default());
        }
        let (r, c) = self.view.cursor;
        let (rows, cols) = self.used();
        match cmd {
            // Across a whole-line selection, step it to take in the next line.
            Move(d) if self.line_selected(line_kind(d.axis())) => {
                let (nr, nc) = self.stepped(d);
                self.move_to(nr, nc, true);
            }
            Move(Dir::Left) if self.stepped(Dir::Left) == (r, c) => {
                self.select_line_at_edge(SelKind::Rows)
            }
            Move(Dir::Right) if self.leave_line_at_edge(SelKind::Rows) => {}
            Move(Dir::Up) if self.stepped(Dir::Up) == (r, c) => {
                self.select_line_at_edge(SelKind::Cols)
            }
            Move(Dir::Down) if self.leave_line_at_edge(SelKind::Cols) => {}
            Move(d) | Extend(d) => {
                let (nr, nc) = self.stepped(d);
                self.move_to(nr, nc, matches!(cmd, Extend(_)));
            }
            NextCell => {
                if cols > 0 && c + 1 >= cols && r + 1 < rows.max(r + 2) {
                    self.move_to(self.step(Axis::Rows, r, true), 0, false);
                } else {
                    self.move_to(r, self.step(Axis::Cols, c, true), false);
                }
            }
            PreviousCell => {
                if c == 0 && r > 0 {
                    let last = cols.saturating_sub(1);
                    self.move_to(self.step(Axis::Rows, r, false), last, false);
                } else {
                    self.move_to(r, self.step(Axis::Cols, c, false), false);
                }
            }
            Edge(d) => {
                let (dr, dc) = d.delta();
                let (nr, nc) = self.edge(dr, dc);
                self.move_to(nr, nc, false);
            }
            RowStart => self.move_to(r, 0, false),
            RowFirstNonEmpty => {
                let first = (0..cols)
                    .find(|&k| !self.cell(r, k).is_empty())
                    .unwrap_or(0);
                self.move_to(r, first, false);
            }
            RowEnd => {
                let last = self.last_filled_col(r);
                self.move_to(r, last, false);
            }
            GotoFirstRow => self.move_to(0, c, false),
            GotoLastRow => {
                let target = if env.count > 0 {
                    env.count - 1
                } else {
                    rows.saturating_sub(1)
                };
                self.move_to(target, c, false);
            }
            GotoColumn => self.move_to(r, env.count.saturating_sub(1), false),
            GotoFirstCell => self.move_to(0, 0, false),
            GotoLastCell => self.move_to(rows.saturating_sub(1), cols.saturating_sub(1), false),
            PageDown | PageUp | HalfPageDown | HalfPageUp => self.page(cmd),
            SelectCells | SelectRows | SelectColumns => self.toggle_select(match cmd {
                SelectCells => SelKind::Cells,
                SelectRows => SelKind::Rows,
                _ => SelKind::Cols,
            }),
            SelectAll => {
                self.view.anchor = Some((0, 0));
                self.view.cursor = (rows.saturating_sub(1), cols.saturating_sub(1));
                self.view.sel_kind = SelKind::Cells;
                self.sticky_sel = true;
            }
            SelectCancel => self.deselect(),
            EditStart | EditEnd => {
                let text = self.source(r, c).to_string();
                self.view.edit = Some(CellEdit::new(r, c, &text, text.clone(), cmd == EditEnd));
                self.view.spread = if cmd == EditEnd {
                    Spread::Append
                } else {
                    Spread::Insert
                };
                self.view.reveal_cursor();
            }
            EditReplace => {
                let text = self.source(r, c).to_string();
                self.view.edit = Some(CellEdit::new(r, c, &text, String::new(), true));
                self.view.spread = Spread::Replace;
                self.view.reveal_cursor();
            }
            Clear => self.clear_selection(env.flag),
            DeleteRow => self.delete_selected_rows(),
            DeleteColumn => self.delete_selected_cols(),
            InsertRowBelow | InsertRowAbove => {
                let at = if cmd == InsertRowBelow { r + 1 } else { r };
                if at <= rows {
                    self.model_op(|wb| wb.insert_rows(SHEET, at, 1));
                }
                self.move_to(at, c, false);
            }
            InsertColumnRight | InsertColumnLeft => {
                let at = if cmd == InsertColumnRight { c + 1 } else { c };
                if at <= cols {
                    self.model_op(|wb| wb.insert_cols(SHEET, at, 1));
                }
                self.move_to(r, at, false);
            }
            SortAscending | SortDescending => return Ok(self.sort(cmd == SortDescending)),
            Undo | Redo => return self.undo_redo(cmd == Undo),
            Repeat => {
                // (Repeating a command is the editor's: it may need it.)
                if let Some(RepeatOp::SetText(text)) = self.last_repeat.clone() {
                    self.set_cells_tracked(&[(r, c, text)]);
                }
            }
            WidenColumn | NarrowColumn => {
                let w = self.view.cols.size(c);
                let step = env.char_w.max(1.0);
                let nw = if cmd == WidenColumn {
                    w + step
                } else {
                    (w - step).max(view::MIN_SIZE)
                };
                self.resize(Axis::Cols, c, nw);
                return Ok(Outcome::persist());
            }
            AutofitColumn | AutofitColumnFull => {
                self.autofit(c, cmd == AutofitColumnFull, env);
                return Ok(Outcome::persist());
            }
            TallerRow | ShorterRow => {
                let h = self.view.rows.size(r);
                let nh = if cmd == TallerRow {
                    h + 5.0
                } else {
                    (h - 5.0).max(view::MIN_SIZE)
                };
                self.resize(Axis::Rows, r, nh);
                return Ok(Outcome::persist());
            }
            ZoomIn | ZoomOut | ZoomReset => {
                match cmd {
                    ZoomIn => self.view.step_zoom(1),
                    ZoomOut => self.view.step_zoom(-1),
                    _ => self.view.zoom = 1.0,
                }
                self.view.reveal_cursor();
                let pct = (self.view.zoom * 100.0).round();
                return Ok(Outcome::msg(format!("Zoom {pct}%")).and_persist());
            }
            ToggleHeader => {
                let h = !self.wb.header(SHEET);
                self.wb.set_header(SHEET, h);
                let msg = if h {
                    "Row 1 is a header"
                } else {
                    "Row 1 is data"
                };
                return Ok(Outcome::msg(msg).and_persist());
            }
            ToggleFreeze => return Ok(self.toggle_freeze().and_persist()),
            FormulaBar => self.view.bar_visible = !self.view.bar_visible,
            NextProblem => return Ok(self.next_problem()),
            ShowFormulas => {
                self.view.show_formulas = !self.view.show_formulas;
                return Ok(Outcome::msg(if self.view.show_formulas {
                    "Showing formulas"
                } else {
                    "Showing results"
                }));
            }
            // The editor's (see `Editor::grid_command`).
            EditPopup | Cut | Yank | Paste | YankPop | Goto | SearchForward | SearchBackward
            | SearchNext | SearchPrevious | ViewAsText | ExchangeAnchor | Recenter
            | JumpToColumn | KillToRowEnd | Upcase | Downcase | Capitalize | CopyDown
            | CompleteCell | MoveLine(_) | HideColumn | ShowColumns | NarrowToSelection
            | Filter | FilterByValue | Widen | CopyAsOrg | CopyAsMarkdown | CopyAsJson
            | ToggleBold | CycleTextColor | CycleFillColor | ClearFormat | FormatGeneral
            | FormatNumber | FormatCurrency | FormatPercent | FormatDate | FormatText
            | IncreaseDecimals | DecreaseDecimals | TracePrecedents | TraceDependents
            | GridMode | ToggleView | CellPopupCommit | CellPopupAbort | OrgTableToGrid
            | Recalculate | SelectMatching => {}
            // [`Self::apply_cursors`]'s.
            AddCursorBelow
            | AddCursorAbove
            | KeepPrimaryCursor
            | RemovePrimaryCursor
            | RotateCursorsForward
            | RotateCursorsBackward => {}
        }
        Ok(Outcome::default())
    }

    /// A page (or half a page) up or down: the view scrolls with the cursor.
    fn page(&mut self, cmd: GridCmd) {
        use GridCmd::*;
        let (r, c) = self.view.cursor;
        let page = self.view.page_rows();
        let n = if matches!(cmd, PageDown | PageUp) {
            page
        } else {
            (page / 2).max(1)
        };
        let down = matches!(cmd, PageDown | HalfPageDown);
        let mut nr = r;
        for _ in 0..n {
            nr = self.step(Axis::Rows, nr, down);
        }
        let dy = self.view.rows.start(nr) - self.view.rows.start(r);
        self.view.scroll_y += dy;
        self.view.clamp_scroll();
        self.move_to(nr, c, false);
    }

    /// `v` / `V` / `C-v`: start a sticky selection of `kind`, or end one.
    fn toggle_select(&mut self, kind: SelKind) {
        if self.sticky_sel && self.view.sel_kind == kind {
            self.deselect();
        } else {
            self.view.anchor.get_or_insert(self.view.cursor);
            self.view.sel_kind = kind;
            self.sticky_sel = true;
        }
    }

    /// Empty the selection's shown cells; with `whole_rows` (`C-u C-d`),
    /// the selection's whole rows.
    fn clear_selection(&mut self, whole_rows: bool) {
        let mut sel = self.selection();
        if whole_rows {
            sel.c0 = 0;
            sel.c1 = self.used().1.saturating_sub(1);
        }
        let mut edits: Vec<(usize, usize, String)> = self
            .visible_cells(&sel)
            .into_iter()
            .map(|(r, c)| (r, c, String::new()))
            .collect();
        // Helix's extra cursors are cleared with it.
        edits.extend(
            self.view
                .cursors
                .iter()
                .map(|&(r, c)| (r, c, String::new())),
        );
        if !edits.is_empty() {
            self.set_cells_tracked(&edits);
        }
    }

    /// Delete the selected rows (or the cursor's).
    fn delete_selected_rows(&mut self) {
        let (r, c) = self.view.cursor;
        let rows = self.used().0;
        let sel = self.selection();
        let (r0, n) = if self.view.anchor.is_some() {
            (sel.r0, sel.rows())
        } else {
            (r, 1)
        };
        if r0 < rows {
            let n = n.min(rows - r0);
            if self.view.rows.only().is_some() {
                // Under a filter, only the rows on screen go (as in a
                // spreadsheet): delete them bottom-up, one step.
                let doomed: Vec<usize> = (r0..r0 + n).filter(|&k| self.row_shown(k)).collect();
                self.grouped(|g| {
                    for &k in doomed.iter().rev() {
                        g.model_op(|wb| wb.delete_rows(SHEET, k, 1));
                    }
                });
            } else {
                self.model_op(|wb| wb.delete_rows(SHEET, r0, n));
            }
        }
        self.deselect();
        self.move_to(r0, c, false);
    }

    /// Delete the selected columns (or the cursor's).
    fn delete_selected_cols(&mut self) {
        let (r, c) = self.view.cursor;
        let cols = self.used().1;
        let sel = self.selection();
        let (c0, n) = if self.view.anchor.is_some() {
            (sel.c0, sel.cols())
        } else {
            (c, 1)
        };
        if c0 < cols {
            let n = n.min(cols - c0);
            self.model_op(|wb| wb.delete_cols(SHEET, c0, n));
        }
        self.deselect();
        self.move_to(r, c0, false);
    }

    /// Sort the rows by the cursor's column.
    fn sort(&mut self, descending: bool) -> Outcome {
        let c = self.view.cursor.1;
        if c >= self.used().1 {
            return Outcome::default();
        }
        let header = self.wb.header(SHEET);
        self.model_op(|wb| wb.sort_by_column(SHEET, c, descending, header));
        let dir = if descending {
            "descending"
        } else {
            "ascending"
        };
        Outcome::msg(format!("Sorted by column {} ({dir})", view::col_name(c)))
    }

    /// Undo (or redo) one history entry, putting the cursor where it
    /// happened.
    fn undo_redo(&mut self, undo: bool) -> Result<Outcome, Cow<'static, str>> {
        let entry = self.history.take(undo);
        let Some(entry) = entry else {
            let what = if undo { "undo" } else { "redo" };
            return Err(format!("No further {what} information").into());
        };
        let (pos, layout) = self.replay(&entry, undo);
        if let Some((pr, pc)) = pos {
            self.deselect();
            self.move_to(pr, pc, false);
        }
        self.history.replayed(entry, undo);
        Ok(Outcome {
            msg: None,
            persist_view: layout,
        })
    }

    /// Fit column `c` to its text: every row (`full`), or a sample plus the
    /// rows on screen.
    fn autofit(&mut self, c: usize, full: bool, env: &CmdEnv) {
        let rows = self.used().0;
        let w = if full {
            self.fit_width(c, 0..rows, env.char_w, env.max_fit)
        } else {
            let top = self.view.top_row();
            let visible = top..top + self.view.page_rows() + 1;
            let sample = 0..rows.min(AUTOFIT_SAMPLE);
            let rows_iter = sample.chain(visible.filter(|&x| x < rows));
            self.fit_width(c, rows_iter, env.char_w, env.max_fit)
        };
        self.resize(Axis::Cols, c, w);
    }

    /// Freeze panes, by rows: the rows above the cursor, or just row 1 when
    /// the cursor is on it. Again unfreezes.
    fn toggle_freeze(&mut self) -> Outcome {
        if self.view.frozen_rows > 0 {
            self.view.frozen_rows = 0;
            return Outcome::msg("Unfrozen");
        }
        // The rows above the cursor, within the data.
        let n = self.view.cursor.0.max(1).min(self.used().0.max(1));
        self.view.frozen_rows = n;
        self.view.scroll_y = 0.0;
        self.view.reveal_cursor();
        Outcome::msg(if n == 1 {
            "Froze row 1".to_string()
        } else {
            format!("Froze rows 1–{n}")
        })
    }

    /// Go to the next malformed row (wrapping).
    fn next_problem(&mut self) -> Outcome {
        let r = self.view.cursor.0;
        let problems = self.wb.problems(SHEET);
        let next = problems
            .iter()
            .copied()
            .find(|&p| p > r)
            .or_else(|| problems.first().copied());
        match next {
            Some(p) => {
                let n = problems.len();
                self.move_to(p, 0, false);
                Outcome::msg(format!("Malformed row {} ({n} total)", p + 1))
            }
            None => Outcome::msg("No malformed rows"),
        }
    }
}

/// `1234567.0` → `1,234,567`; keeps up to two decimals.
fn fmt_num(n: f64) -> String {
    if n.fract().abs() < 1e-9 {
        group_thousands(n)
    } else {
        let s = format!("{:.2}", n);
        let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
        let int: f64 = int.parse().unwrap_or(0.0);
        let sign = if n < 0.0 && int == 0.0 { "-" } else { "" };
        format!("{sign}{}.{frac}", group_thousands(int))
    }
}

/// `n` rounded to a whole number, grouped: `-1234567.0` → `-1,234,567`.
fn group_thousands(n: f64) -> String {
    let out = cellfmt::group_digits(&format!("{:.0}", n.abs()));
    if n < 0.0 { format!("-{out}") } else { out }
}

pub(super) use crate::grid::bytes_hash;

/// Painted geometry of one grid pane, reported by the element every frame so
/// mouse events can be hit-tested against what's actually on screen.
#[derive(Debug, Clone, Copy)]
pub(super) struct GridGeom {
    /// Top-left of the whole pane content (formula bar included).
    pub origin: Point<Pixels>,
    pub width: f32,
    pub height: f32,
    /// Height of the formula bar (0 when hidden).
    pub bar_h: f32,
    pub header_w: f32,
    pub header_h: f32,
    /// One monospace advance at the current zoom.
    pub char_w: f32,
    /// The formula bar's text: where it starts (from the pane's left), how
    /// far it's scrolled, and its font's advance (unzoomed chrome font).
    pub bar_text_x: f32,
    pub bar_shift: f32,
    pub bar_char_w: f32,
}

impl GridGeom {
    /// Top-left of the cells area (past bar and headers).
    pub fn cells_origin(&self) -> (f32, f32) {
        (
            f32::from(self.origin.x) + self.header_w,
            f32::from(self.origin.y) + self.bar_h + self.header_h,
        )
    }
}

/// A mouse drag in progress on a grid pane.
#[derive(Debug, Clone, Copy)]
pub(super) enum GridDrag {
    /// Extending a cell/row/column selection.
    Select { pane: usize },
    /// Resizing a column or row: index, its start (screen px), and the live
    /// size (unzoomed) shown by the guide line.
    Resize {
        pane: usize,
        axis: Axis,
        idx: usize,
        start: f32,
        size: f64,
    },
}

/// Where a point in a grid pane landed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GridHit {
    NameBox,
    Bar { x: f32 },
    Corner,
    ColHeader(usize),
    ColBorder(usize),
    RowHeader(usize),
    RowBorder(usize),
    Cell(usize, usize),
}

fn load_sidecar(path: &Path, doc: &mut GridDoc) {
    if let Some(sc) = Sidecar::load(path) {
        doc.apply_sidecar(sc);
    }
}

fn store_sidecar(path: &Path, doc: &GridDoc) {
    doc.sidecar().store(path);
}

impl GridDoc {
    /// Apply view settings saved for this grid's file.
    fn apply_sidecar(&mut self, sc: Sidecar) {
        for (i, w) in sc.cols {
            self.view.cols.set(i, w);
        }
        for (i, h) in sc.rows {
            self.view.rows.set(i, h);
        }
        if let Some(z) = sc.zoom {
            self.view.zoom = z.clamp(0.1, 4.0);
        }
        if let Some(h) = sc.header
            && h != self.wb.header(SHEET)
        {
            self.wb.set_header(SHEET, h);
        }
        self.view.frozen_rows = sc.freeze.unwrap_or(0);
        for c in sc.hidden_cols {
            self.view.cols.set_hidden(c, true);
        }
    }

    /// The view settings to save for this grid's file.
    fn sidecar(&self) -> Sidecar {
        Sidecar {
            cols: self.view.cols.overrides().collect(),
            rows: self.view.rows.overrides().collect(),
            zoom: ((self.view.zoom - 1.0).abs() > 1e-3).then_some(self.view.zoom),
            header: Some(self.wb.header(SHEET)),
            freeze: (self.view.frozen_rows > 0).then_some(self.view.frozen_rows),
            hidden_cols: self.view.cols.hidden().collect(),
        }
    }
}

/// Lower-cased extension of `path`.
fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

impl Editor {
    // --- Opening ---------------------------------------------------------------

    /// Whether `path` should open as a grid (by `grid-extensions`).
    pub(super) fn is_grid_path(&self, path: &Path) -> bool {
        ext_of(path).is_some_and(|e| self.grid_ui.extensions.contains(&e))
    }

    /// Ask before opening a huge file as a grid (it all goes into memory).
    /// Returns true when the open was deferred to the confirm.
    pub(super) fn grid_guard_large(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if meta.len() <= LARGE_FILE {
            return false;
        }
        let gb = meta.len() as f64 / (1024.0 * 1024.0 * 1024.0);
        self.open_confirm(
            &format!("{:.1} GB file — load it all as a grid?", gb),
            PendingAction::OpenLargeGrid(path.to_string_lossy().into_owned()),
            cx,
        );
        true
    }

    /// Install a grid buffer for `bytes` read from `path`. Small files parse
    /// right here; large ones show their first rows now and swap in the full
    /// workbook when the background parse lands.
    pub(super) fn open_grid(
        &mut self,
        name: String,
        path: PathBuf,
        bytes: Vec<u8>,
        cx: &mut Context<Self>,
    ) {
        if self.switch_to_visiting(&super::buffers::file_identity(&path), cx) {
            return;
        }
        let ext = ext_of(&path);
        // Its sheets are open in buffers of their own: join that workbook.
        if ext.as_deref() == Some("xlsx") && self.grid_open_live_book(&path, cx) {
            return;
        }
        let name = self.unique_buffer_name(&name, &path);
        let hash = bytes_hash(&bytes);
        let total = bytes.len();
        let mut xlsx_lossy = Vec::new();
        let mut xlsx_sheets = Vec::new();
        let mut other_sheets = HashMap::new();
        let mut xlsx_text = Default::default();
        let mut xlsx_formats = Vec::new();
        let mut book = None;
        let (wb, background) = if ext.as_deref() == Some("xlsx") {
            match super::grid_xlsx::load(&bytes, 0, &self.grid_fn_names()) {
                Ok(l) => {
                    if l.sheets.len() > 1 {
                        book = Some(super::grid_xlsx::BookStore::new(
                            l.sheets.clone(),
                            l.data,
                            Some(path.clone()),
                        ));
                    }
                    xlsx_lossy = l.lossy;
                    xlsx_sheets = l.sheets;
                    other_sheets = l.others;
                    xlsx_text = l.text_cells;
                    xlsx_formats = l.formats;
                    (l.wb, None)
                }
                Err(err) => {
                    self.echo(format!("Could not open {name}: {err}"), cx);
                    return;
                }
            }
        } else if total <= SYNC_PARSE_LIMIT {
            match crate::grid::parse::parse_now(bytes, ext.as_deref()) {
                Ok(wb) => (wb, None),
                Err(err) => {
                    self.echo(format!("Could not open {name} as a grid: {err}"), cx);
                    return;
                }
            }
        } else {
            let prefix = crate::grid::parse::parse_prefix(&bytes, ext.as_deref(), PREFIX_ROWS);
            (prefix, Some(bytes))
        };
        let mut grid = GridDoc::new(wb, ext.clone(), hash, &self.grid_ui.opts);
        grid.xlsx.lossy = xlsx_lossy;
        grid.xlsx.sheets = xlsx_sheets;
        grid.xlsx.other_sheets = other_sheets;
        grid.xlsx.text = xlsx_text;
        grid.xlsx.book = book;
        for ((r, c), f) in xlsx_formats {
            grid.formats.set(r, c, f);
        }
        grid.save.formats = grid.formats.clone();
        if grid.is_xlsx() {
            grid.rebuild_formulas();
        }
        load_sidecar(&path, &mut grid);
        if let Some(bytes) = background {
            let cancel = Arc::new(AtomicBool::new(false));
            let progress = Arc::new(AtomicUsize::new(0));
            grid.loading = Some(GridLoading {
                cancel: cancel.clone(),
                progress: progress.clone(),
                total,
                save_after: false,
            });
            self.spawn_grid_parse(name.clone(), bytes, ext, cancel, progress, cx);
        }
        self.install_grid_doc(name.clone(), Some(path), grid, true, cx);
        let problems = self
            .active
            .grid
            .as_ref()
            .map_or(0, |g| g.wb.problems(SHEET).len());
        // An .xlsx opens quietly: what saving over it would drop is said
        // when a save is refused, and `g t` lists the other sheets.
        if problems > 0 {
            self.echo(
                format!("Visiting {name} — {problems} malformed rows (gp / C-c C-p to visit)"),
                cx,
            );
        } else {
            self.echo(format!("Visiting {name}"), cx);
        }
        self.run_hook("find-file-hook");
        self.reconcile_watches(cx);
        self.grid_after(cx);
        self.grid_fire_load();
    }

    /// Make `grid` the active buffer, `name` (visiting `path`, if any), and
    /// run its mode hooks — unlike other special buffers, a grid runs them
    /// (`grid-mode-hook`). With `keep_current`, the buffer shown now stays
    /// open behind it; otherwise the grid replaces it (a text buffer turned
    /// into a grid).
    pub(super) fn install_grid_doc(
        &mut self,
        name: String,
        path: Option<PathBuf>,
        grid: GridDoc,
        keep_current: bool,
        cx: &mut Context<Self>,
    ) {
        let mut doc = make_special_document(name, "grid-mode", "");
        doc.path = path;
        // Hooks see the buffer as it is (an imported table is unsaved).
        doc.modified = grid.modified();
        doc.grid = Some(Box::new(grid));
        doc.hooks_ran = false;
        if keep_current {
            let current = self.snapshot_active();
            self.inactive.push(current);
        }
        self.install_active(doc);
        self.run_mode_hooks_once(cx);
    }

    /// Install an already-built workbook as a new grid buffer that visits no
    /// file yet (`org-table-to-grid`); it counts as unsaved until written.
    pub(super) fn open_grid_workbook(
        &mut self,
        name: String,
        wb: Workbook,
        cx: &mut Context<Self>,
    ) {
        let mut grid = GridDoc::new(wb, Some("csv".into()), 0, &self.grid_ui.opts);
        grid.save.failed = true;
        self.install_grid_doc(name, None, grid, true, cx);
        self.grid_after(cx);
        self.grid_fire_load();
    }

    /// Parse `bytes` off the UI thread and swap the result into buffer `name`.
    fn spawn_grid_parse(
        &mut self,
        name: String,
        bytes: Vec<u8>,
        ext: Option<String>,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicUsize>,
        cx: &mut Context<Self>,
    ) {
        let parse_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            crate::grid::parse::parse(bytes, ext.as_deref(), &parse_cancel, &progress)
        });
        // A ticking repaint so the "Loading… 45%" indicator moves.
        let ticker_cancel = cancel.clone();
        let tick_name = name.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                if ticker_cancel.load(Ordering::Relaxed) {
                    break;
                }
                let alive = this
                    .update(cx, |editor, cx| {
                        let loading = editor
                            .grid_doc_named(&tick_name)
                            .is_some_and(|g| g.loading.is_some());
                        if loading {
                            cx.notify();
                        }
                        loading
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |editor, cx| {
                editor.grid_load_finished(&name, &cancel, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn grid_load_finished(
        &mut self,
        name: &str,
        cancel: &Arc<AtomicBool>,
        result: Result<Workbook, crate::grid::parse::ParseError>,
        cx: &mut Context<Self>,
    ) {
        let opts = self.grid_ui.opts.clone();
        let Some(grid) = self.grid_doc_named_mut(name) else {
            return; // killed meanwhile
        };
        // Only the load we started (a revert may have replaced it since).
        if !grid
            .loading
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(&l.cancel, cancel))
        {
            return;
        }
        let save_after = grid.loading.as_ref().is_some_and(|l| l.save_after);
        grid.loading = None;
        match result {
            Ok(mut wb) => {
                // Keep a header choice the user already made on the prefix.
                if wb.header(SHEET) != grid.wb.header(SHEET) {
                    let h = grid.wb.header(SHEET);
                    wb.set_header(SHEET, h);
                }
                grid.wb = wb;
                grid.replaced_workbook(&opts);
                grid.apply_filters();
                let rows = grid.wb.used_rows(SHEET);
                let problems = grid.wb.problems(SHEET).len();
                let mut msg = format!("Loaded {name} ({} rows)", group_thousands(rows as f64));
                if problems > 0 {
                    msg.push_str(&format!(" — {problems} malformed rows"));
                }
                self.echo(msg, cx);
                if save_after && self.active.name == name {
                    self.grid_save(cx);
                }
            }
            Err(err) => self.echo(format!("Loading {name} failed: {err}"), cx),
        }
        cx.notify();
    }

    /// `M-x grid-mode`: show the current text buffer as a grid (its current
    /// text, unsaved edits included).
    fn grid_mode_from_text(&mut self, cx: &mut Context<Self>) {
        if self.active.grid.is_some() {
            self.echo(
                "Already a grid — C-c C-t / grid-toggle-view switches to raw text",
                cx,
            );
            return;
        }
        let text = self.active.buffer.full_text();
        let ext = self.active.path.as_deref().and_then(ext_of);
        let modified = self.active.modified;
        match crate::grid::parse::parse_now(text.into_bytes(), ext.as_deref()) {
            Ok(wb) => {
                let disk = self
                    .active
                    .path
                    .as_deref()
                    .and_then(|p| std::fs::read(p).ok())
                    .map_or(0, |b| bytes_hash(&b));
                let mut grid = GridDoc::new(wb, ext, disk, &self.grid_ui.opts);
                grid.save.failed = modified; // carry the unsaved state over
                if let Some(path) = &self.active.path {
                    load_sidecar(path, &mut grid);
                }
                // Back from the raw view: styling by column always returns;
                // formatting by cell only while the text is as it was (an
                // edit may have moved the cells).
                if let Some(stash) = self.active.grid_stash.take() {
                    grid.style = stash.style;
                    grid.choices = stash.choices;
                    if stash.text == self.active.buffer.full_text() {
                        grid.formats = stash.formats;
                    } else if !stash.formats.is_empty() {
                        self.echo(
                            "Cell formatting dropped: the text changed in the raw view",
                            cx,
                        );
                    }
                }
                let (name, path) = (self.active.name.clone(), self.active.path.clone());
                self.install_grid_doc(name, path, grid, false, cx);
                self.grid_after(cx);
                self.grid_fire_load();
            }
            Err(err) => self.echo(format!("Not parseable as a grid: {err}"), cx),
        }
    }

    // --- Lookup -------------------------------------------------------------------

    pub(super) fn grid_active(&self) -> bool {
        self.active.grid.is_some()
    }

    /// The active grid is editing a cell.
    pub(super) fn grid_editing(&self) -> bool {
        self.active
            .grid
            .as_ref()
            .is_some_and(|g| g.view.edit.is_some())
    }

    pub(super) fn grid_doc_named(&self, name: &str) -> Option<&GridDoc> {
        self.doc(name)?.grid.as_deref()
    }

    pub(super) fn grid_doc_named_mut(&mut self, name: &str) -> Option<&mut GridDoc> {
        self.doc_mut(name)?.grid.as_deref_mut()
    }

    /// The grid shown in pane `pane`, if it shows one.
    pub(super) fn pane_grid(&self, pane: usize) -> Option<&GridDoc> {
        let name = self.pane(pane)?.buffer.clone();
        self.grid_doc_named(&name)
    }

    fn pane_grid_mut(&mut self, pane: usize) -> Option<&mut GridDoc> {
        let name = self.pane(pane)?.buffer.clone();
        self.grid_doc_named_mut(&name)
    }

    /// Report a grid pane's painted geometry (called by its element).
    pub(super) fn set_grid_geometry(&mut self, pane: usize, geom: GridGeom) {
        self.grid_ui.geom.insert(pane, geom);
        let (vw, vh) = (
            (geom.width - geom.header_w).max(1.0) as f64,
            (geom.height - geom.bar_h - geom.header_h).max(1.0) as f64,
        );
        if let Some(g) = self.pane_grid_mut(pane) {
            g.view.viewport = (vw, vh);
        }
    }

    // --- After every command --------------------------------------------------

    /// Bring everything that mirrors grid state up to date: the document's
    /// `modified` flag (mode line `**`, kill/quit prompts), the mode line's
    /// range summary, and — when the formula bar is hidden — the echo area.
    pub(super) fn grid_after(&mut self, cx: &mut Context<Self>) {
        // An undo or redo of a sheet operation: carry it out workbook-wide.
        self.grid_sheet_replays(cx);
        if self
            .active
            .grid
            .as_deref_mut()
            .is_some_and(|g| std::mem::take(&mut g.layout_moved))
        {
            self.grid_persist_view();
        }
        if let Some(g) = self.active.grid.as_deref_mut()
            && g.wb.take_refused_growth()
        {
            self.echo(
                format!(
                    "Not written: that would stretch the sheet past {} cells",
                    group_thousands(crate::grid::model::MAX_CELLS as f64)
                ),
                cx,
            );
        }
        self.grid_formulas_after(cx);
        // Other open sheets of a workbook read this one's values.
        self.grid_sync_sheets_from(vec![None], cx);
        self.grid_fire_changes();
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let dropped = g.refilter_if_reshaped();
        g.refresh_derived();
        if !g.row_shown(g.view.cursor.0) {
            let (r, c) = g.view.cursor;
            g.move_to(r, c, g.view.anchor.is_some());
        }
        g.summary = g.range_summary();
        let sel = (g.view.cursor, g.view.anchor, g.view.sel_kind);
        let sel_changed = g.hook_sel.replace(sel) != Some(sel);
        let modified = g.modified();
        let bar_visible = g.view.bar_visible;
        let (name, content) = g.bar_text();
        self.active.modified = modified;
        if !bar_visible {
            let content = content.replace('\n', "↵");
            self.grid_set_echo(if content.is_empty() {
                name.into()
            } else {
                format!("{name}  │  {content}").into()
            });
        } else if self
            .grid_ui
            .echo
            .as_ref()
            .is_some_and(|e| *e == self.status)
        {
            self.status = SharedString::default();
            self.grid_ui.echo = None;
        }
        if let Some(msg) = dropped {
            self.echo(msg, cx);
        }
        // Typing a formula: the name dropdown, and the argument being typed.
        self.grid_refresh_completion();
        if let Some(hint) = self.grid_formula_hint() {
            self.grid_set_echo(hint.into());
        }
        if sel_changed {
            self.grid_fire_selection_changed();
        }
        cx.notify();
    }

    /// Show `line` in the echo area as the grid's own. Only an echo the grid
    /// itself put there (or none) is replaced, so an error or other message
    /// isn't wiped by the next cursor move. Not logged to *Messages* —
    /// cursor moves would flood it.
    fn grid_set_echo(&mut self, line: SharedString) {
        let ours = self.status.is_empty()
            || self
                .grid_ui
                .echo
                .as_ref()
                .is_some_and(|e| *e == self.status);
        if ours {
            self.status = line.clone();
            self.grid_ui.echo = Some(line);
        }
    }

    /// Call `grid-selection-changed-hook` functions (no arguments; they read
    /// `(grid-cursor)` / `(grid-selection)`). Free when nothing is hooked.
    fn grid_fire_selection_changed(&mut self) {
        let Some(functions) = self
            .hooks
            .get("grid-selection-changed-hook")
            .filter(|f| !f.is_empty())
            .cloned()
        else {
            return;
        };
        self.publish_point();
        for function in functions {
            self.script.dispatch_action(&function, None);
        }
    }

    /// Call `grid-cell-changed-hook` functions with the cells the user just
    /// changed, as one list `((row col old new) …)` per command. Edits made
    /// by Scheme itself (`grid-set-cell`, …) don't fire it, so a hook that
    /// writes cells can't loop.
    fn grid_fire_changes(&mut self) {
        let Some(changes) = self
            .active
            .grid
            .as_deref_mut()
            .map(|g| std::mem::take(&mut g.changed))
            .filter(|c| !c.is_empty())
        else {
            return;
        };
        let Some(functions) = self
            .hooks
            .get("grid-cell-changed-hook")
            .filter(|f| !f.is_empty())
            .cloned()
        else {
            return;
        };
        // Bounded: a paste of a million cells shouldn't build a million-
        // element list on the Scheme thread.
        const MAX: usize = 10_000;
        let list = changes
            .iter()
            .take(MAX)
            .map(|(r, c, old, new)| {
                format!(
                    "(list {r} {c} {} {})",
                    super::vim::steel_quote(old),
                    super::vim::steel_quote(new)
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.publish_point();
        for function in functions {
            self.script
                .dispatch(&format!("({function} (list {list}))"), false);
        }
    }

    /// Fire `grid-load-hook` with the buffer name (after a grid opens).
    pub(super) fn grid_fire_load(&mut self) {
        let name = self.active.name.clone();
        self.publish_point();
        self.run_hook_arg("grid-load-hook", &name);
    }

    /// Apply a [`GridOp`] from the Scheme API to the active grid.
    pub(super) fn grid_op(&mut self, op: GridOp, cx: &mut Context<Self>) {
        use GridOp::*;
        let op = match op {
            OpenSheet(name) => return self.grid_open_sheet(&name, cx),
            AddSheet(name) => return self.grid_add_sheet(&name, cx),
            RenameSheet { from, to } => return self.grid_rename_sheet(&from, &to, cx),
            DeleteSheet(name) => return self.grid_delete_sheet(&name, cx),
            MoveSheet(by) => return self.grid_move_sheet(by, cx),
            NumberFormat(code) => return self.grid_format(GridCmd::FormatGeneral, Some(&code), cx),
            op => op,
        };
        let Some(g) = self.active.grid.as_deref_mut() else {
            self.echo("grid: the current buffer is not a grid", cx);
            return;
        };
        let data = matches!(
            op,
            SetCells(_)
                | InsertRows { .. }
                | DeleteRows { .. }
                | InsertCols { .. }
                | DeleteCols { .. }
                | Sort { .. }
        );
        if data && g.loading.is_some() {
            self.echo(
                "grid: still loading — editing is available once it finishes",
                cx,
            );
            return;
        }
        let mut error: Option<String> = None;
        let mut persist = false;
        match op {
            SetCells(cells) => {
                let total = cells.len();
                let edits: Vec<(usize, usize, String)> = cells
                    .into_iter()
                    .filter_map(|(r, c, t)| Some((r?, c?, super::grid_formula::stored_input(&t))))
                    .collect();
                if edits.len() < total {
                    error = Some(format!(
                        "grid-set-cell: {} cell(s) with an invalid row/column skipped",
                        total - edits.len()
                    ));
                }
                if !edits.is_empty() {
                    g.model_op(|wb| wb.set_cells(SHEET, &edits));
                }
            }
            InsertRows { at, n }
            | DeleteRows { at, n }
            | InsertCols { at, n }
            | DeleteCols { at, n }
                if at.is_none() || n == 0 =>
            {
                if at.is_none() {
                    error = Some("grid: invalid row/column index".into());
                }
            }
            InsertRows { at: Some(at), n } => {
                g.model_op(|wb| wb.insert_rows(SHEET, at, n));
            }
            DeleteRows { at: Some(at), n } => {
                g.model_op(|wb| wb.delete_rows(SHEET, at, n));
            }
            InsertCols { at: Some(at), n } => {
                g.model_op(|wb| wb.insert_cols(SHEET, at, n));
            }
            DeleteCols { at: Some(at), n } => {
                g.model_op(|wb| wb.delete_cols(SHEET, at, n));
            }
            InsertRows { .. } | DeleteRows { .. } | InsertCols { .. } | DeleteCols { .. } => {}
            Sort {
                col: Some(col),
                descending,
                kind,
            } => {
                let header = g.wb.header(SHEET);
                g.model_op(|wb| wb.sort_by_column_as(SHEET, col, descending, header, kind));
            }
            Sort { col: None, .. } => error = Some("grid-sort: invalid column".into()),
            MoveTo {
                row: Some(r),
                col: Some(c),
            } => {
                g.deselect();
                g.move_to(r, c, false);
            }
            MoveTo { .. } => error = Some("grid-move-to: invalid row/column".into()),
            Select {
                r0: Some(r0),
                c0: Some(c0),
                r1: Some(r1),
                c1: Some(c1),
            } => {
                g.deselect();
                g.move_to(r0, c0, false);
                g.view.anchor = Some((r0, c0));
                g.view.sel_kind = SelKind::Cells;
                g.move_to(r1, c1, true);
            }
            Select { .. } => error = Some("grid-select-range: invalid row/column".into()),
            ColumnWidth { px, .. } | RowHeight { px, .. } if !px.is_finite() => {
                error = Some("grid: size must be a number of pixels".into());
            }
            Zoom(pct) if !pct.is_finite() => {
                error = Some("grid-set-zoom: zoom must be a percentage".into());
            }
            ColumnWidth { col: Some(c), px } => {
                g.resize(Axis::Cols, c, px.max(0.0));
                persist = true;
            }
            RowHeight { row: Some(r), px } => {
                g.resize(Axis::Rows, r, px.max(0.0));
                persist = true;
            }
            ColumnWidth { .. } | RowHeight { .. } => {
                error = Some("grid: invalid row/column index".into());
            }
            Freeze(n) => {
                // No more rows than the data has.
                g.view.frozen_rows = n.min(g.used().0);
                g.view.scroll_y = 0.0;
                g.view.reveal_cursor();
                persist = true;
            }
            SetHeader(on) => {
                g.wb.set_header(SHEET, on);
                persist = true;
            }
            Zoom(pct) => {
                g.view.zoom = ((pct / 100.0) as f32).clamp(0.1, 4.0);
                g.view.reveal_cursor();
                persist = true;
            }
            ReadRange { range, callback } => {
                let Some((a, b)) = parse_address(&range) else {
                    self.echo(format!("grid-read-range: not a range: {range}"), cx);
                    return;
                };
                if callback.is_empty() {
                    self.echo("grid-read-range: needs a callback function name", cx);
                    return;
                }
                let b = b.unwrap_or(a);
                let (r0, r1) = (a.0.min(b.0), a.0.max(b.0));
                let (c0, c1) = (a.1.min(b.1), a.1.max(b.1));
                let cells = (r1 - r0 + 1).saturating_mul(c1 - c0 + 1);
                const MAX_CELLS: usize = 200_000;
                if cells > MAX_CELLS {
                    self.echo(
                        format!(
                            "grid-read-range: {cells} cells is over the {MAX_CELLS}-cell limit"
                        ),
                        cx,
                    );
                    return;
                }
                let rows = (r0..=r1)
                    .map(|r| {
                        let row = (c0..=c1)
                            .map(|c| super::vim::steel_quote(g.cell(r, c)))
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!("(list {row})")
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.publish_point();
                self.script
                    .dispatch(&format!("({callback} (list {rows}))"), false);
                return;
            }
            ColumnFace { col: Some(c), face } => g.style.set_column_face(c, &face),
            FormatColumn {
                col: Some(c),
                format,
            } => {
                if let Err(e) = g.style.set_format(c, &format) {
                    error = Some(format!("grid-format-column: {e}"));
                }
            }
            ColumnFace { col: None, .. } | FormatColumn { col: None, .. } => {
                error = Some("grid: invalid column".into());
            }
            HighlightRule {
                col,
                op,
                value,
                face,
                row,
            } => {
                let name = if row {
                    "grid-row-highlight-rule"
                } else {
                    "grid-highlight-rule"
                };
                let added = RuleTest::parse(&op, &value).and_then(|test| {
                    g.style.add_rule(crate::grid::style::Rule {
                        row,
                        ..crate::grid::style::Rule::new(col, test, face)
                    })
                });
                match added {
                    Ok(()) => g.refresh_derived(),
                    Err(e) => error = Some(format!("{name}: {e}")),
                }
            }
            ClearStyles => g.style.clear(),
            CellFace {
                row: Some(r),
                col: Some(c),
                face,
            } => g.style.set_cell_face(r, c, &face),
            CellFace { .. } => error = Some("grid-set-cell-face: invalid row/column".into()),
            ClearCellFaces => g.style.clear_cell_faces(),
            CellFormat {
                row: Some(r),
                col: Some(c),
                bold,
                text,
                fill,
                num,
                bad_color: None,
            } => {
                let num = num.map(|n| NumCode::intern(cellfmt::preset_code(&n)));
                g.format_cells([(r, c)], |f| CellFmt {
                    bold: bold.unwrap_or(f.bold),
                    text: text.unwrap_or(f.text),
                    fill: fill.unwrap_or(f.fill),
                    num: num.unwrap_or(f.num),
                });
            }
            CellFormat {
                bad_color: Some(name),
                ..
            } => {
                let names = crate::grid::cellfmt::COLORS.join(" ");
                error = Some(format!(
                    "grid: unknown color {name:?} (one of: {names}, or #f)"
                ));
            }
            CellFormat { .. } => error = Some("grid: invalid row/column".into()),
            OpenSheet(_)
            | AddSheet(_)
            | RenameSheet { .. }
            | DeleteSheet(_)
            | MoveSheet(_)
            | NumberFormat(_) => unreachable!("handled above"),
            ClearCellFormats => {
                let all: Vec<(usize, usize)> = g.formats.positions().collect();
                g.format_cells(all, |_| CellFmt::default());
            }
            Stripe(face) => g.style.stripe = face.filter(|f| !f.is_empty()),
            HeaderFace(face) => g.style.header_face = (!face.is_empty()).then_some(face),
            Align {
                col: Some(c),
                align,
            } => match crate::grid::style::Align::parse(&align) {
                Ok(a) => g.style.set_align(c, a),
                Err(e) => error = Some(format!("grid-column-align: {e}")),
            },
            Align { col: None, .. } => error = Some("grid-column-align: invalid column".into()),
            HideColumn {
                col: Some(c),
                hidden,
            } => {
                g.set_cols_hidden([c], hidden);
                if hidden && g.view.cursor.1 == c {
                    let (r, c) = g.view.cursor;
                    g.move_to(r, c, false);
                }
                persist = true;
            }
            HideColumn { col: None, .. } => error = Some("grid-hide-column: invalid column".into()),
            Filter {
                col: Some(c),
                op,
                value,
            } => match RuleTest::parse(&op, &value) {
                Ok(test) if test.columnar() => {
                    error = Some(format!("grid-filter: {op} can't filter rows"));
                }
                Ok(test) => {
                    let label = format!("{}{op}{value}", view::col_name(c));
                    g.filters.push(RowFilter::Test {
                        col: c,
                        test,
                        label,
                    });
                    g.apply_filters();
                    let (r, c) = g.view.cursor;
                    g.move_to(r, c, false);
                }
                Err(e) => error = Some(format!("grid-filter: {e}")),
            },
            Filter { col: None, .. } => error = Some("grid-filter: invalid column".into()),
            Choices {
                col: Some(c),
                values,
            } => {
                if values.is_empty() {
                    g.choices.remove(&c);
                } else {
                    g.choices.insert(c, values);
                }
            }
            Choices { col: None, .. } => error = Some("grid-column-choices: invalid column".into()),
        }
        if persist {
            self.grid_persist_view();
        }
        if let Some(e) = error {
            self.echo(e, cx);
        }
        self.grid_after(cx);
    }

    pub(super) fn grid_persist_view(&self) {
        if let (Some(path), Some(g)) = (&self.active.path, self.active.grid.as_deref()) {
            store_sidecar(path, g);
        }
    }

    // --- Commands -----------------------------------------------------------------

    /// Route grid commands, and intercept the generic commands that mean
    /// something different in a grid (undo, save, clipboard, …) so every way
    /// of invoking them — keys, `M-x`, menus — does the grid thing.
    pub(super) fn cmd_grid(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        // Edits made while a Scheme command batch applies share one undo
        // step (the batch opened an undo group; see `apply_script_outcome`).
        // Only for this command: an edit made later some other way (typing
        // in a cell) must not join the batch's step.
        let batch = self.undo_grouping.batch();
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.history.batch = batch;
        }
        let result = self.cmd_grid_inner(command, cx);
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.history.batch = None;
        }
        result
    }

    fn cmd_grid_inner(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand as E;
        match command {
            E::GridOp(op) => self.grid_op(op, cx),
            E::Grid(inv) => self.grid_invoked(inv, cx),
            E::GridRegisterFunction {
                name,
                source,
                helper,
            } => self.grid_register_function(name, source, helper, cx),
            E::GridOption(opt) => {
                use crate::grid::cmd::GridOption as O;
                match opt {
                    O::DefaultColumnWidth(px) if px.is_finite() && px > 0.0 => {
                        self.grid_ui.opts.default_col_width = px
                    }
                    O::MaxAutofitWidth(px) if px.is_finite() && px > 0.0 => {
                        self.grid_ui.opts.max_autofit_width = px
                    }
                    O::DefaultColumnWidth(_) | O::MaxAutofitWidth(_) => {
                        self.echo("grid: a width must be a positive number of pixels", cx)
                    }
                    O::AutoHeader(h) => self.grid_ui.opts.auto_header = h,
                    O::FunctionTimeout(secs) if secs.is_finite() && secs >= 1.0 => {
                        self.grid_set_function_timeout(secs as u64)
                    }
                    O::FunctionTimeout(_) => {
                        self.echo("grid-function-timeout: give a number of seconds (≥ 1)", cx)
                    }
                    O::TruncateIndicator(s) => self.grid_ui.opts.truncate_indicator = s,
                }
                cx.notify();
            }
            E::GridExtensions(exts) => {
                self.grid_ui.extensions = exts
                    .into_iter()
                    .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                    .collect();
            }
            E::GridGotoAddress(addr) => self.grid_goto_address(&addr, cx),
            other if self.active.grid.is_some() => match other {
                E::Undo => self.grid_command(GridCmd::Undo, cx),
                E::Redo => self.grid_command(GridCmd::Redo, cx),
                E::SaveBuffer => self.grid_save(cx),
                E::RevertBuffer => self.grid_revert(cx),
                E::ClipboardCopy | E::KillRingSave => self.grid_command(GridCmd::Yank, cx),
                E::YankPop => self.grid_command(GridCmd::YankPop, cx),
                E::Recenter => self.grid_command(GridCmd::Recenter, cx),
                E::ExchangePointAndMark => self.grid_command(GridCmd::ExchangeAnchor, cx),
                E::KillLine => self.grid_command(GridCmd::KillToRowEnd, cx),
                E::IsearchForward => self.grid_command(GridCmd::SearchForward, cx),
                E::IsearchBackward => self.grid_command(GridCmd::SearchBackward, cx),
                E::ClipboardCut | E::KillRegion => self.grid_command(GridCmd::Cut, cx),
                E::ClipboardPaste | E::Yank => {
                    if self.grid_editing() {
                        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                            self.grid_edit_input(&text, cx);
                        }
                    } else {
                        self.grid_command(GridCmd::Paste, cx);
                    }
                }
                E::MarkWholeBuffer => self.grid_command(GridCmd::SelectAll, cx),
                E::KeyboardQuit => {
                    if self.grid_editing() {
                        self.grid_cancel_edit(cx);
                    } else {
                        self.grid_command(GridCmd::SelectCancel, cx);
                    }
                    self.pending_keys.clear();
                    self.status = SharedString::default();
                }
                other => return Err(other),
            },
            other => return Err(other),
        }
        Ok(())
    }

    /// A grid command from the command router: the few that mean something
    /// outside a grid (or switch into one) here, the rest on the active grid.
    fn grid_invoked(&mut self, inv: GridInvocation, cx: &mut Context<Self>) {
        match inv.cmd {
            GridCmd::GridMode => self.grid_mode_from_text(cx),
            GridCmd::ToggleView => {
                if self.active.grid.is_some() {
                    self.grid_view_as_text(cx);
                } else {
                    self.grid_mode_from_text(cx);
                }
            }
            GridCmd::CellPopupCommit => self.grid_cell_popup_finish(true, cx),
            GridCmd::CellPopupAbort => self.grid_cell_popup_finish(false, cx),
            GridCmd::OrgTableToGrid => self.org_table_to_grid(cx),
            _ if self.active.grid.is_none() => self.echo("Not a grid buffer", cx),
            GridCmd::Recalculate => self.grid_recalculate(cx),
            _ => self.grid_command(inv, cx),
        }
    }

    /// Apply one grid command to the active grid. The invocation's count
    /// (0 = none) is read by count-aware commands; others repeat.
    pub(super) fn grid_command(&mut self, inv: impl Into<GridInvocation>, cx: &mut Context<Self>) {
        use GridCmd::*;
        let GridInvocation { cmd, count, flag } = inv.into();
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        if !matches!(cmd, Paste | YankPop) {
            g.yank = None;
        }
        if !matches!(cmd, TracePrecedents | TraceDependents) {
            g.view.trace.clear();
        }
        if cmd != Recenter {
            g.recenter_phase = 0;
        }
        if !cmd.keeps_cursors() {
            g.view.cursors.clear();
        }
        let opens_editor = matches!(cmd, EditStart | EditEnd | EditReplace | EditPopup);
        if g.loading.is_some() && (cmd.edits() || opens_editor) {
            self.echo("Still loading — editing is available once it finishes", cx);
            return;
        }
        // Count-aware: run once with the count as an argument.
        if !matches!(cmd, GotoLastRow | GotoColumn) && count > 1 {
            for _ in 0..count.min(10_000) {
                self.grid_command(cmd, cx);
            }
            return;
        }
        if cmd.edits() {
            g.last_repeat = Some(RepeatOp::Cmd(cmd));
        }
        // Commands that need the editor: the clipboard and kill ring,
        // prompts, Scheme, other buffers.
        if cmd == Repeat
            && let Some(RepeatOp::Cmd(repeat)) = g.last_repeat
        {
            return self.grid_command(repeat, cx);
        }
        match cmd {
            EditPopup => return self.grid_cell_popup_open(cx),
            SelectMatching => return self.grid_select_matching_prompt(cx),
            Cut => {
                self.grid_copy_selection(cx);
                self.grid_command(Clear, cx);
                if let Some(g) = self.active.grid.as_deref_mut() {
                    g.deselect();
                }
                return self.grid_after(cx);
            }
            Yank => {
                self.grid_copy_selection(cx);
                if let Some(g) = self.active.grid.as_deref_mut() {
                    g.deselect();
                }
                return self.grid_after(cx);
            }
            Paste => return self.grid_paste(cx),
            YankPop => return self.grid_yank_pop(cx),
            SortAscending | SortDescending if flag => {
                // `C-u C-c C-s`: ask how to compare (org-table's `C-c ^`).
                self.publish_point();
                self.script.dispatch("(grid-sort-prompt)", false);
                return;
            }
            Goto => {
                let (r, c) = g.view.cursor;
                let here = cell_name(r, c);
                self.open_minibuffer("Go to cell: ", MinibufferPurpose::GridGoto, &here, cx);
                return self.grid_after(cx);
            }
            SearchForward => {
                self.open_minibuffer(
                    "Search cells: ",
                    MinibufferPurpose::GridSearch(Direction::Forward),
                    "",
                    cx,
                );
                return;
            }
            SearchBackward => {
                self.open_minibuffer(
                    "Search cells backward: ",
                    MinibufferPurpose::GridSearch(Direction::Backward),
                    "",
                    cx,
                );
                return;
            }
            SearchNext | SearchPrevious => {
                let Some(pat) = g.search.clone() else {
                    return self.echo("No previous search", cx);
                };
                return self.grid_search(&pat, cmd == SearchNext, cx);
            }
            ViewAsText => return self.grid_view_as_text(cx),
            ExchangeAnchor => return self.grid_exchange_anchor(cx),
            Recenter => return self.grid_recenter(cx),
            JumpToColumn => return self.grid_jump_to_column(cx),
            KillToRowEnd => return self.grid_kill_to_row_end(cx),
            Upcase => return self.grid_change_case(super::grid_ops::CaseOp::Upper, cx),
            Downcase => return self.grid_change_case(super::grid_ops::CaseOp::Lower, cx),
            Capitalize => {
                return self.grid_change_case(super::grid_ops::CaseOp::Capitalize, cx);
            }
            CopyDown => return self.grid_copy_down(flag, cx),
            CompleteCell => return self.grid_complete_cell(cx),
            MoveLine(d) => {
                return match d.axis() {
                    Axis::Rows => self.grid_move_rows(d.forward(), cx),
                    Axis::Cols => self.grid_move_columns(d.forward(), cx),
                };
            }
            HideColumn => return self.grid_hide_columns(cx),
            ToggleBold | CycleTextColor | CycleFillColor | ClearFormat | FormatGeneral
            | FormatNumber | FormatCurrency | FormatPercent | FormatDate | FormatText
            | IncreaseDecimals | DecreaseDecimals => {
                return self.grid_format(cmd, None, cx);
            }
            ShowColumns => return self.grid_show_columns(cx),
            NarrowToSelection => return self.grid_narrow_to_selection(cx),
            Filter => return self.grid_filter_prompt(cx),
            FilterByValue => return self.grid_filter_by_value(cx),
            Widen => return self.grid_widen(cx),
            CopyAsOrg | CopyAsMarkdown | CopyAsJson => return self.grid_copy_as(cmd, cx),
            TracePrecedents | TraceDependents => {
                return self.grid_trace(cmd == TracePrecedents, cx);
            }
            _ => {}
        }
        // The rest only touch the grid.
        let char_w = self
            .grid_ui
            .geom
            .get(&self.selected_pane)
            .map_or(7.0, |geo| {
                geo.char_w as f64 / f64::from(g.view.zoom).max(0.1)
            });
        let env = CmdEnv {
            count,
            flag,
            char_w,
            max_fit: self.grid_ui.opts.max_autofit_width,
        };
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        match g.apply(cmd, &env) {
            Ok(out) => {
                if let Some(msg) = out.msg {
                    self.echo(msg, cx);
                }
                if out.persist_view {
                    self.grid_persist_view();
                }
            }
            Err(refused) => return self.echo(refused, cx),
        }
        self.grid_after(cx);
    }

    /// `(grid-goto-address "B2:D9")` / the `grid-goto` prompt.
    pub(super) fn grid_goto_address(&mut self, addr: &str, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        match parse_address(addr) {
            Some((a, b)) => {
                g.deselect();
                match b {
                    Some(b) => {
                        g.move_to(a.0, a.1, false);
                        g.view.anchor = Some(a);
                        g.view.sel_kind = SelKind::Cells;
                        g.sticky_sel = true;
                        g.move_to(b.0, b.1, true);
                    }
                    None => g.move_to(a.0, a.1, false),
                }
                self.grid_after(cx);
            }
            None => self.echo(format!("Not a cell address: {addr}"), cx),
        }
    }

    /// Ex commands with a grid meaning. `:12` goes to row 12 and `:$` to the
    /// last row. `:C7` / `:B2:D9` jump to or select cells; the column letters
    /// must be upper case, so `:b2` still means buffer 2. Line commands
    /// (`:%s`, `:g`, `:d`, …) are refused with a pointer to the raw view.
    /// Returns whether the command was handled here.
    pub(super) fn grid_ex(&mut self, cmd: &str, cx: &mut Context<Self>) -> bool {
        if !self.grid_active() || cmd.is_empty() {
            return false;
        }
        let Some(g) = self.active.grid.as_deref_mut() else {
            return false;
        };
        let col = g.view.cursor.1;
        if cmd.bytes().all(|b| b.is_ascii_digit()) {
            let row = cmd.parse::<usize>().unwrap_or(1).saturating_sub(1);
            g.deselect();
            g.move_to(row, col, false);
            self.grid_after(cx);
            return true;
        }
        if cmd == "$" {
            let last = g.wb.used_rows(SHEET).saturating_sub(1);
            g.deselect();
            g.move_to(last, col, false);
            self.grid_after(cx);
            return true;
        }
        let upper_letters = cmd.split(':').all(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .all(|c| c.is_ascii_uppercase())
        });
        if upper_letters && parse_address(cmd).is_some() {
            self.grid_goto_address(cmd, cx);
            return true;
        }
        let parsed = super::ex::parse_ex_line(cmd);
        if matches!(parsed.name.as_str(), "s" | "substitute") {
            self.grid_ex_substitute(&parsed, cmd, cx);
            return true;
        }
        const LINE_COMMANDS: &[&str] = &[
            "s",
            "substitute",
            "&",
            "&&",
            "g",
            "global",
            "v",
            "vglobal",
            "norm",
            "normal",
            "d",
            "delete",
            "m",
            "move",
            "t",
            "co",
            "copy",
            "j",
            "join",
            "sort",
            "y",
            "yank",
            ">",
            "<",
        ];
        if parsed.range.is_some() || LINE_COMMANDS.contains(&parsed.name.as_str()) {
            self.echo(
                format!(
                    ":{cmd} works on text lines, not grid cells — g r / C-c C-t shows the raw text"
                ),
                cx,
            );
            return true;
        }
        false
    }

    /// Find the next (or previous) cell matching `pat` (a regex; smart case)
    /// in reading order from the cursor, wrapping around the data.
    pub(super) fn grid_search(&mut self, pat: &str, forward: bool, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        g.search = Some(pat.to_string());
        let smart = super::grid_ops::smart_case(pat);
        let re = match regex::Regex::new(&format!("{smart}{pat}")) {
            Ok(re) => re,
            Err(e) => {
                self.echo(format!("Bad pattern: {e}"), cx);
                return;
            }
        };
        let (rows, cols) = g.used();
        let total = rows.saturating_mul(cols);
        if total == 0 {
            self.echo("No data", cx);
            return;
        }
        let (r, c) = g.view.cursor;
        let here = (r.min(rows - 1)) * cols + c.min(cols - 1);
        for step in 1..=total {
            let i = if forward {
                (here + step) % total
            } else {
                (here + total - step % total) % total
            };
            let (rr, cc) = (i / cols, i % cols);
            if g.row_shown(rr) && g.view.cols.shown(cc) && re.is_match(g.cell(rr, cc)) {
                g.move_to(rr, cc, false);
                if step >= total || (forward && i <= here) || (!forward && i >= here) {
                    self.echo("Search wrapped", cx);
                }
                self.grid_after(cx);
                return;
            }
        }
        self.echo(format!("No cell matches {pat}"), cx);
    }

    // --- Cell editing ------------------------------------------------------------

    /// Commit the open cell edit, then move by `(dr, dc)`.
    pub(super) fn grid_commit_edit(&mut self, dr: isize, dc: isize, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let Some(edit) = g.view.edit.take() else {
            return;
        };
        // With helix's extra cursors, every cursor's cell (one undo step).
        let writes = g.commit_writes(&edit);
        if !writes.is_empty() {
            g.set_cells_tracked(&writes);
            g.last_repeat = Some(RepeatOp::SetText(edit.text.clone()));
        }
        g.move_after_commit(&edit, dr, dc);
        self.grid_after(cx);
    }

    pub(super) fn grid_cancel_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.view.edit = None;
        }
        self.grid_after(cx);
    }

    /// Typed text while editing a cell.
    pub(super) fn grid_edit_input(&mut self, text: &str, cx: &mut Context<Self>) {
        self.grid_caret_edit(|e| e.insert(text), cx);
    }

    /// Apply `f` to the cell being edited. A caret edit ends any reference
    /// pointing.
    fn grid_caret_edit(&mut self, f: impl FnOnce(&mut CellEdit), cx: &mut Context<Self>) {
        if let Some(e) = self
            .active
            .grid
            .as_deref_mut()
            .and_then(|g| g.view.edit.as_mut())
        {
            e.point = None;
            f(e);
        }
        self.grid_after(cx);
    }

    /// Text arriving through the platform input handler (IME commits, and
    /// plain typing that wasn't already taken by [`Self::key_grid`]). Returns
    /// whether the grid consumed it.
    pub(super) fn grid_type_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if !self.grid_active() || !self.pending_keys.is_empty() {
            return self.grid_active();
        }
        if self.grid_editing() {
            self.grid_edit_input(text, cx);
        } else if !self.evil_intercepts() && !text.is_empty() {
            self.grid_begin_typing(text, cx);
        }
        true
    }

    /// Spreadsheet-style typing over a cell: replace its contents with `text`.
    fn grid_begin_typing(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        if g.loading.is_some() {
            self.echo("Still loading — editing is available once it finishes", cx);
            return;
        }
        let (r, c) = g.view.cursor;
        let original = g.source(r, c).to_string();
        g.deselect();
        g.view.edit = Some(CellEdit::new(r, c, &original, text.to_string(), true));
        g.view.spread = Spread::Replace;
        g.view.reveal_cursor();
        self.grid_after(cx);
    }

    /// Commit a pending edit (saves call this so a value on screen is never
    /// left out of the file).
    pub(super) fn grid_commit_pending(&mut self, cx: &mut Context<Self>) {
        if self.grid_editing() {
            self.grid_commit_edit(0, 0, cx);
        }
    }

    // --- Clipboard ---------------------------------------------------------------

    fn grid_copy_selection(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let sel = g.selection();
        // Under a filter only the rows on screen are copied.
        let block = g.visible_block(&sel);
        let n = block.len() * sel.cols();
        // A single cell copies as its bare text (no trailing newline), which
        // is what pasting into any other app expects.
        let text = if n == 1 {
            block[0][0].clone()
        } else {
            view::to_tsv_block(&block)
        };
        // The clipboard gets values (what other apps expect); remember the
        // formulas so a paste back into a grid pastes those instead.
        let cells = g.copied_formulas(&sel);
        self.grid_ui.formula_clip = (!cells.is_empty()).then(|| super::grid_formula::FormulaClip {
            text: text.clone(),
            cells,
        });
        self.set_clipboard(&text, cx);
        self.kill_append = false;
        self.push_kill(text);
        self.echo(
            if n == 1 {
                "Copied 1 cell".to_string()
            } else {
                format!("Copied {n} cells")
            },
            cx,
        );
    }

    pub(super) fn grid_paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        // (A copied formula whose result is "" copies as empty text.)
        let ours = self
            .grid_ui
            .formula_clip
            .as_ref()
            .is_some_and(|c| c.text == text);
        if text.is_empty() && !ours {
            self.echo("Clipboard is empty", cx);
            return;
        }
        let block = view::parse_clipboard_block(text, g.wb.dialect().delimiter);
        let block = if block.is_empty() {
            vec![vec![String::new()]]
        } else {
            block
        };
        let sel = g.selection();
        let single = block.len() == 1 && block[0].len() == 1;
        let mut edits = Vec::new();
        if single && !sel.is_single() {
            // One value pasted over a range fills the range (Excel) — the
            // cells shown, not the rows a filter hides.
            let v = block[0][0].clone();
            let rows = g.used().0;
            for r in sel.r0..=sel.r1 {
                if r < rows && !g.row_shown(r) {
                    continue;
                }
                for c in (sel.c0..=sel.c1).filter(|&c| g.view.cols.shown(c)) {
                    edits.push((r, c, v.clone()));
                }
            }
        } else {
            for (dr, row) in block.iter().enumerate() {
                for (dc, v) in row.iter().enumerate() {
                    edits.push((sel.r0 + dr, sel.c0 + dc, v.clone()));
                }
            }
        }
        // A grid copy of formulas pastes the formulas, references moved by
        // the paste offset (Excel's paste); other text pastes as typed.
        if let Some(clip) = self
            .grid_ui
            .formula_clip
            .as_ref()
            .filter(|c| c.text == text)
        {
            if single && let Some((_, _, sr, sc, f)) = clip.cells.first() {
                for e in &mut edits {
                    e.2 = super::grid_formula::translated(
                        f,
                        e.0 as isize - *sr as isize,
                        e.1 as isize - *sc as isize,
                    );
                }
            } else {
                for (bi, bc, sr, sc, f) in &clip.cells {
                    let (dr, dc) = (sel.r0 + bi, sel.c0 + bc);
                    if let Some(e) = edits.iter_mut().find(|e| e.0 == dr && e.1 == dc) {
                        e.2 = super::grid_formula::translated(
                            f,
                            dr as isize - *sr as isize,
                            dc as isize - *sc as isize,
                        );
                    }
                }
            }
        }
        let n = edits.len();
        g.set_cells_tracked(&edits);
        if !single {
            let h = block.len();
            let w = block.iter().map(Vec::len).max().unwrap_or(1);
            g.view.cursor = (sel.r0, sel.c0);
            g.view.anchor = Some((sel.r0 + h - 1, sel.c0 + w - 1));
            g.view.sel_kind = SelKind::Cells;
            g.sticky_sel = false;
        }
        g.last_repeat = Some(RepeatOp::Cmd(GridCmd::Paste));
        self.echo(format!("Pasted {n} cells"), cx);
        self.grid_after(cx);
    }

    // --- Save / revert ---------------------------------------------------------

    /// `C-x C-s` on a grid: commit any open edit, serialize, write atomically.
    pub(super) fn grid_save(&mut self, cx: &mut Context<Self>) {
        self.grid_commit_pending(cx);
        // A sheet of a workbook with several sheets saves them all.
        if let Some(book) = self.active_book()
            && book.borrow().saves_whole(self.active.path.is_some())
        {
            self.grid_save_book(&book, cx);
            return;
        }
        let Some(path) = self.active.path.clone() else {
            self.echo("Buffer is not visiting a file", cx);
            return;
        };
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        if let Some(l) = g.loading.as_mut() {
            l.save_after = true;
            self.echo("Saving after load completes…", cx);
            return;
        }
        if !g.xlsx.lossy.is_empty() {
            let what = g.xlsx.lossy.join(", ");
            self.echo(
                format!("Not saving over this file: it has {what}, which would be lost — :w copy.xlsx saves a copy"),
                cx,
            );
            return;
        }
        if g.csv_blocked() {
            // CSV can't store formulas or formatting: this save has to be a
            // Save As .xlsx.
            let what = g.csv_would_lose().unwrap_or("formulas");
            let target = path.with_extension("xlsx");
            self.open_minibuffer(
                &format!("CSV can't store {what} — save as (.xlsx): "),
                MinibufferPurpose::GridSaveAsXlsx,
                &target.to_string_lossy(),
                cx,
            );
            return;
        }
        let saving = self.active.name.clone();
        self.run_hook_blocking("before-save-hook", cx);
        // A hook may switch buffers: save the buffer the save was asked for.
        if self.active.name != saving {
            self.switch_to_buffer(&saving, cx);
            if self.active.name != saving {
                self.echo(
                    format!("Not saved: {saving} closed during before-save-hook"),
                    cx,
                );
                return;
            }
        }
        if self.active.path.as_deref() != Some(path.as_path()) {
            self.echo(
                format!("Not saved: {saving} changed files during before-save-hook"),
                cx,
            );
            return;
        }
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        if let Some(e) = g.encoding_error() {
            self.echo(format!("Not saved: {e}"), cx);
            return;
        }
        let pool = g.wb.pool().len();
        if pool > g.save.pool_baseline.saturating_mul(2) + 10_000 {
            g.wb.compact();
            g.save.pool_baseline = g.wb.pool().len();
        }
        let bytes = g.bytes();
        let rows = g.wb.used_rows(SHEET);
        // What's on disk until the write lands: a failed write restores it,
        // so external-change detection keeps comparing against the real file.
        let previous_hash = g.save.disk_hash;
        g.save.disk_hash = bytes_hash(&bytes);
        g.mark_saved();
        g.save.failed = false;
        self.active.modified = false;
        let name = self.active.name.clone();
        let ticket = crate::save::ticket(&path);
        let write_path = path.clone();
        let write = cx
            .background_executor()
            .spawn(async move { crate::save::write_sequenced(&ticket, &write_path, &bytes) });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |editor, cx| match result {
                Ok(crate::save::SaveOutcome::Written) => {
                    editor.echo(
                        format!("Wrote {name} ({} rows)", group_thousands(rows as f64)),
                        cx,
                    );
                    editor.run_hook("after-save-hook");
                    editor.rewatch_file(&path, cx);
                }
                Ok(_) => {}
                Err(err) => {
                    if let Some(d) = editor.doc_mut(&name) {
                        d.modified = true;
                        if let Some(g) = d.grid.as_deref_mut() {
                            g.save.failed = true;
                            g.save.disk_hash = previous_hash;
                        }
                    }
                    editor.echo(format!("Save of {name} failed: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// `M-x revert-buffer` on a grid: re-read and re-parse the file (for a
    /// sheet buffer, its workbook's).
    pub(super) fn grid_revert(&mut self, cx: &mut Context<Self>) {
        let source = self
            .active
            .grid
            .as_deref()
            .and_then(|g| g.xlsx.source.clone());
        let Some(path) = self.active.path.clone().or(source) else {
            return;
        };
        let name = self.active.name.clone();
        match std::fs::read(&path).map_err(|e| e.to_string()) {
            Ok(bytes) => match self.grid_reload(None, bytes, cx) {
                Ok(()) => self.echo(format!("Reverted {name}"), cx),
                Err(err) => self.echo(format!("Revert failed: {err} ({name} kept as it was)"), cx),
            },
            Err(err) => self.echo(format!("Revert failed: {err}"), cx),
        }
    }

    /// Replace a grid's contents with `bytes`, the file as it is now. The
    /// file is parsed first: one that won't parse leaves the buffer as it
    /// was (contents, undo history, saved state) and returns why.
    fn grid_reload(
        &mut self,
        inactive: Option<usize>,
        bytes: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        enum Parsed {
            Xlsx(Box<super::grid_xlsx::Loaded>),
            Csv(Box<Workbook>),
            /// Too big to parse here: a prefix now, the rest in the background.
            Big,
        }
        let hash = bytes_hash(&bytes);
        let total = bytes.len();
        let opts = self.grid_ui.opts.clone();
        let doc = match inactive {
            None => &self.active,
            Some(i) => &self.inactive[i],
        };
        let name = doc.name.clone();
        let Some(g) = doc.grid.as_deref() else {
            return Ok(());
        };
        let ext = g.ext.clone();
        let parsed = if ext.as_deref() == Some("xlsx") {
            Parsed::Xlsx(Box::new(super::grid_xlsx::load(
                &bytes,
                g.xlsx.sheet,
                &self.grid_fn_names(),
            )?))
        } else if total <= SYNC_PARSE_LIMIT {
            Parsed::Csv(Box::new(
                crate::grid::parse::parse_now(bytes.clone(), ext.as_deref())
                    .map_err(|e| e.to_string())?,
            ))
        } else {
            Parsed::Big
        };
        let doc = match inactive {
            None => &mut self.active,
            Some(i) => &mut self.inactive[i],
        };
        let workbook = doc.path.clone();
        let Some(g) = doc.grid.as_deref_mut() else {
            return Ok(());
        };
        g.view.edit = None;
        g.history.clear();
        g.save.disk_hash = hash;
        g.save.failed = false;
        let mut background = None;
        let mut formats = Vec::new();
        match parsed {
            Parsed::Xlsx(l) => {
                // The workbook as the file has it now, for every sheet.
                match &g.xlsx.book {
                    Some(book) => {
                        let fresh = super::grid_xlsx::BookStore::new(
                            l.sheets.clone(),
                            l.data,
                            book.borrow().path.clone(),
                        );
                        book.swap(&fresh);
                    }
                    None if l.sheets.len() > 1 => {
                        g.xlsx.book = Some(super::grid_xlsx::BookStore::new(
                            l.sheets.clone(),
                            l.data,
                            workbook.or_else(|| g.xlsx.source.clone()),
                        ));
                    }
                    None => {}
                }
                g.wb = l.wb;
                g.xlsx.lossy = l.lossy;
                g.xlsx.sheets = l.sheets;
                g.xlsx.other_sheets = l.others;
                g.xlsx.text = l.text_cells;
                g.xlsx.sheet_published = None;
                formats = l.formats;
                if let Some(old) = g.loading.take() {
                    old.cancel.store(true, Ordering::Relaxed);
                }
            }
            Parsed::Csv(wb) => {
                g.wb = *wb;
                if let Some(old) = g.loading.take() {
                    old.cancel.store(true, Ordering::Relaxed);
                }
            }
            Parsed::Big => {
                g.wb = crate::grid::parse::parse_prefix(&bytes, ext.as_deref(), PREFIX_ROWS);
                let cancel = Arc::new(AtomicBool::new(false));
                let progress = Arc::new(AtomicUsize::new(0));
                // A save requested during the previous load still applies.
                let save_after = g.loading.as_ref().is_some_and(|l| l.save_after);
                if let Some(old) = g.loading.replace(GridLoading {
                    cancel: cancel.clone(),
                    progress: progress.clone(),
                    total,
                    save_after,
                }) {
                    old.cancel.store(true, Ordering::Relaxed);
                }
                background = Some((cancel, progress));
            }
        }
        g.replaced_workbook(&opts);
        // After `replaced_workbook`, which drops the old formatting.
        for ((r, c), f) in formats {
            g.formats.set(r, c, f);
        }
        g.save.formats = g.formats.clone();
        g.apply_filters();
        doc.modified = false;
        if let Some((cancel, progress)) = background {
            self.spawn_grid_parse(name, bytes, ext, cancel, progress, cx);
        }
        self.grid_after(cx);
        Ok(())
    }

    /// Auto-revert for grids showing `path` (called from the file watcher):
    /// the grid visiting it, and the sheet buffers of a workbook. Returns
    /// whether any grid shows it (handled either way).
    pub(super) fn grid_auto_revert(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let id = super::buffers::file_identity(path);
        let shows = |d: &Document| {
            d.grid.as_deref().is_some_and(|g| {
                d.path
                    .as_deref()
                    .or(g.xlsx.source.as_deref())
                    .is_some_and(|p| p == path || super::buffers::file_identity(p) == id)
            })
        };
        let targets: Vec<Option<usize>> = std::iter::once(None)
            .filter(|_| shows(&self.active))
            .chain(
                self.inactive
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| shows(d))
                    .map(|(i, _)| Some(i)),
            )
            .collect();
        if targets.is_empty() {
            return false;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return true;
        };
        let hash = bytes_hash(&bytes);
        let mut reverted = Vec::new();
        for target in targets {
            let doc = match target {
                None => &self.active,
                Some(i) => &self.inactive[i],
            };
            let Some(g) = doc.grid.as_deref() else {
                continue;
            };
            let name = doc.name.clone();
            // A sheet buffer has no disk hash of its own: it reloads
            // whenever the workbook really changed.
            if g.save.disk_hash == hash {
                continue; // our own save, or no real change
            }
            if g.modified() || g.view.edit.is_some() {
                self.echo(format!("{name} changed on disk (unsaved edits kept)"), cx);
                continue;
            }
            match self.grid_reload(target, bytes.clone(), cx) {
                Ok(()) => reverted.push((target, name)),
                Err(err) => self.echo(format!("{name} changed on disk but won't load: {err}"), cx),
            }
        }
        if !reverted.is_empty() {
            let names: Vec<&str> = reverted.iter().map(|(_, n)| n.as_str()).collect();
            self.echo(
                format!("Reverted {} (changed on disk)", names.join(", ")),
                cx,
            );
            // The workbook's open sheets read each other's new values.
            let slots = reverted.into_iter().map(|(t, _)| t).collect();
            self.grid_sync_sheets_from(slots, cx);
        }
        true
    }

    /// `grid-view-as-text` (and the grid half of `grid-toggle-view`): turn
    /// this buffer into a plain text buffer over the same file, in place —
    /// same name, same file, unsaved changes carried over (as text). Undo
    /// history doesn't survive the switch in either direction.
    fn grid_view_as_text(&mut self, cx: &mut Context<Self>) {
        self.grid_commit_pending(cx);
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        if g.formulas.formula_count() > 0 || g.is_xlsx() {
            self.echo(
                "No raw text view for a grid with formulas or an .xlsx (its text form would lose them)",
                cx,
            );
            return;
        }
        if g.loading.is_some() {
            self.echo("Still loading — switch views once it finishes", cx);
            return;
        }
        // A text buffer saves UTF-8; a UTF-16 / Windows-1252 file would come
        // back re-encoded, so those stay grid-only.
        if g.wb.dialect().encoding != crate::grid::dialect::Encoding::Utf8 {
            self.echo(
                "Raw view needs a UTF-8 file (this one is re-encoded on save only by the grid)",
                cx,
            );
            return;
        }
        let Ok(text) = String::from_utf8(g.bytes()) else {
            self.echo("Raw view needs valid UTF-8", cx);
            return;
        };
        let modified = g.modified();
        let stash = GridStash {
            text: text.clone(),
            formats: g.formats.clone(),
            style: g.style.clone(),
            choices: g.choices.clone(),
        };
        let (name, path) = (self.active.name.clone(), self.active.path.clone());
        let mut doc = make_document(name, path, &text);
        doc.modes.push(crate::mode::Mode::new("grid-text-mode"));
        doc.modified = modified;
        doc.grid_stash = Some(Box::new(stash));
        self.install_active(doc);
        self.parse_full();
        self.move_to(0, cx);
        self.set_mode_line_slot("grid".into(), String::new());
        self.echo(
            "Raw text view (C-c C-t or M-x grid-toggle-view for the grid)",
            cx,
        );
    }

    // --- `ge`: edit a cell in a full buffer -----------------------------------

    fn grid_cell_popup_open(&mut self, cx: &mut Context<Self>) {
        self.grid_commit_pending(cx);
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let (r, c) = g.view.cursor;
        let text = g.source(r, c).to_string();
        let grid_name = self.active.name.clone();
        let name = format!("*cell {}*", cell_name(r, c));
        let mut doc = make_document(name.clone(), None, &text);
        doc.modes
            .push(crate::mode::Mode::new("grid-cell-edit-mode"));
        let current = self.snapshot_active();
        self.inactive.retain(|d| d.name != name);
        self.inactive.push(current);
        self.install_active(doc);
        self.grid_ui.cell_popup = Some(CellPopup {
            buffer: name,
            grid: grid_name,
            row: r,
            col: c,
        });
        self.move_to(self.active.buffer.len(), cx);
        self.echo(
            "Edit the cell; C-c C-c to save it back, C-c C-k to cancel",
            cx,
        );
    }

    fn grid_cell_popup_finish(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(CellPopup {
            buffer: popup,
            grid: grid_name,
            row: r,
            col: c,
        }) = self.grid_ui.cell_popup.clone()
        else {
            self.echo("Not editing a cell", cx);
            return;
        };
        if self.active.name != popup {
            self.switch_to_buffer(&popup, cx);
        }
        let text = self.active.buffer.full_text();
        self.grid_ui.cell_popup = None;
        self.switch_to_buffer(&grid_name, cx);
        self.kill_buffer_named(&popup, cx);
        if commit && self.active.name != grid_name {
            self.echo(format!("Not written: {grid_name} is no longer open"), cx);
            return;
        }
        if commit && let Some(g) = self.active.grid.as_deref_mut() {
            g.set_cells_tracked(&[(r, c, text)]);
            g.move_to(r, c, false);
        }
        self.grid_after(cx);
    }

    // --- Keys ---------------------------------------------------------------------

    /// Grid key layer (see `dispatch_keystroke`): cell insert first, then the
    /// grid keymaps. Falls through for chords the grid doesn't bind, so the
    /// global maps (`C-x C-s`, `M-x`) still work.
    pub(super) fn key_grid(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if !self.grid_active() || ks.key.is_empty() {
            return Handled::No;
        }
        if self.grid_editing() {
            return self.key_grid_edit(ks, cx);
        }
        // A `C-u` argument being typed: digits extend it; any other key ends
        // it (leaving the count pending for the command this key runs). The
        // universal-argument layer sits after this one, so it must be fed
        // from here.
        if self.uarg.reading && self.handle_prefix_arg(ks, cx) {
            cx.stop_propagation();
            return Handled::Yes;
        }
        let chord = chord_string(ks);
        let modal = self.evil_intercepts();
        // Vim count prefix (`5G`, `3j`).
        if modal
            && self.pending_keys.is_empty()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
            && let Some(d) = chord.parse::<usize>().ok().filter(|&d| d < 10)
            && (d != 0 || self.modal_count.is_some())
        {
            let n = self.modal_count.unwrap_or(0).saturating_mul(10) + d;
            self.modal_count = Some(n.min(1_000_000_000));
            self.status = format!("{n}").into();
            cx.notify();
            cx.stop_propagation();
            return Handled::Yes;
        }
        let pending = self.pending_keys.join(" ");
        let sequence = if pending.is_empty() {
            chord.clone()
        } else {
            format!("{pending} {chord}")
        };
        match self.grid_resolve(&sequence) {
            GridKey::Bound(expr) => {
                self.pending_keys.clear();
                if self.grid_ui.echo.as_ref() != Some(&self.status) {
                    self.status = SharedString::default();
                }
                self.grid_run(&expr, cx);
                cx.stop_propagation();
                Handled::Yes
            }
            GridKey::Prefix => {
                self.pending_keys.push(chord);
                self.status = format!("{}-", pretty_sequence(&sequence)).into();
                cx.notify();
                cx.stop_propagation();
                Handled::Yes
            }
            GridKey::Miss => {
                // A modal leader sequence (`SPC f f`): the leader layer's.
                if modal
                    && !ks.modifiers.platform
                    && !matches!(
                        self.keymap.resolve_leader(None, &sequence),
                        crate::mode::LeaderMatch::Miss
                    )
                {
                    return Handled::No;
                }
                // The global maps know this sequence (`C-x C-s` after the
                // grid's own `C-x g` made `C-x` a shared prefix): theirs.
                if self.keymap.resolve(&self.active.modes, &sequence).is_some()
                    || self.keymap.is_prefix(&self.active.modes, &sequence)
                {
                    return Handled::No;
                }
                let ours = !pending.is_empty() && self.grid_is_prefix(&pending);
                let plain = !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
                if ours || (pending.is_empty() && plain) {
                    // A dead grid sequence, or an unbound plain key: under
                    // Emacs bindings a printable key starts editing the cell
                    // (spreadsheet typing); under evil/helix it's swallowed.
                    self.pending_keys.clear();
                    self.modal_count = None;
                    if !modal && pending.is_empty() && plain {
                        if let Some(text) = ks.key_char.clone().filter(|t| !t.is_empty()) {
                            self.grid_begin_typing(&text, cx);
                        }
                    } else if ours {
                        self.status = format!("{} is undefined", pretty_sequence(&sequence)).into();
                    }
                    cx.notify();
                    cx.stop_propagation();
                    return Handled::Yes;
                }
                // A Ctrl/Alt chord or someone else's prefix: let the global
                // maps have it.
                Handled::No
            }
        }
    }

    /// Resolve against the active modal overlay, then the base `grid-mode` map.
    fn grid_resolve(&self, sequence: &str) -> GridKey {
        let overlay = self
            .evil_intercepts()
            .then(|| format!("{}:grid-mode", self.modal_collection_prefix()));
        if let Some(km) = overlay.as_deref() {
            if let Some(expr) = self.keymap.resolve_mode(km, sequence) {
                return GridKey::Bound(expr.to_string());
            }
            if self.keymap.is_prefix_mode(km, sequence) {
                return GridKey::Prefix;
            }
        }
        if let Some(expr) = self.keymap.resolve_mode("grid-mode", sequence) {
            return GridKey::Bound(expr.to_string());
        }
        if self.keymap.is_prefix_mode("grid-mode", sequence) {
            return GridKey::Prefix;
        }
        GridKey::Miss
    }

    fn grid_is_prefix(&self, sequence: &str) -> bool {
        matches!(self.grid_resolve(sequence), GridKey::Prefix)
    }

    /// Run a bound grid expression under the pending count.
    fn grid_run(&mut self, expr: &str, cx: &mut Context<Self>) {
        let count = self.modal_count.take().unwrap_or(0);
        let name = expr
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(char::is_whitespace)
            .next()
            .unwrap_or("")
            .to_string();
        let grid_cmd = GridCmd::from_name(&name);
        self.record_event(MacroEvent::Command(expr.to_string()));
        // An Emacs `C-u N` prefix: a flag for some commands, else a count.
        let prefix = self.uarg.pending.take();
        match grid_cmd {
            // Through the command router, so commands it handles itself
            // (`grid-toggle-view`, popup commit) work from keys too.
            Some(cmd) => {
                let mut inv = GridInvocation {
                    count,
                    ..cmd.into()
                };
                match prefix {
                    Some(_) if cmd.prefix_is_flag() => inv.flag = true,
                    Some(n) if count == 0 && n > 1 => inv.count = n as usize,
                    _ => {}
                }
                self.apply_command(EditorCommand::Grid(inv), cx);
            }
            None => {
                let count = match prefix {
                    Some(n) if count == 0 => n.max(1) as usize,
                    _ => count,
                };
                for _ in 0..count.clamp(1, 10_000) {
                    self.run_script(expr, cx);
                }
            }
        }
    }

    /// Keys while a cell is being edited.
    fn key_grid_edit(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if !self.pending_keys.is_empty() {
            return Handled::No; // finishing a global prefix (C-x C-s)
        }
        let chord = chord_string(ks);
        if let Some((dr, extend)) = match chord.as_str() {
            "shift-up" => Some((-1, true)),
            "shift-down" => Some((1, true)),
            _ => None,
        } && self.grid_point(dr, 0, extend, cx)
        {
            cx.stop_propagation();
            return Handled::Yes;
        }
        if self.grid_completion_open() {
            match chord.as_str() {
                "escape" | "ctrl-g" => {
                    self.grid_completion_dismiss(cx);
                    cx.stop_propagation();
                    return Handled::Yes;
                }
                "ctrl-n" | "ctrl-p" => {
                    self.grid_completion_move(if chord == "ctrl-n" { 1 } else { -1 }, cx);
                    cx.stop_propagation();
                    return Handled::Yes;
                }
                _ => {}
            }
        }
        match chord.as_str() {
            "escape" => self.grid_commit_edit(0, 0, cx),
            "ctrl-g" => self.grid_cancel_edit(cx),
            "shift-enter" => self.grid_commit_edit(-1, 0, cx),
            "shift-tab" => self.grid_commit_edit(0, -1, cx),
            "alt-enter" | "ctrl-enter" | "ctrl-j" => self.grid_edit_input("\n", cx),
            "ctrl-a" => self.grid_caret_edit(CellEdit::home, cx),
            "ctrl-e" => self.grid_caret_edit(CellEdit::end, cx),
            "ctrl-f" => self.grid_caret_edit(CellEdit::right, cx),
            "ctrl-b" => self.grid_caret_edit(CellEdit::left, cx),
            "alt-f" | "alt-right" => self.grid_caret_edit(CellEdit::word_right, cx),
            "alt-b" | "alt-left" => self.grid_caret_edit(CellEdit::word_left, cx),
            "ctrl-w" | "alt-backspace" => self.grid_caret_edit(CellEdit::kill_word_back, cx),
            "ctrl-u" => self.grid_caret_edit(CellEdit::kill_to_start, cx),
            "ctrl-k" => self.grid_caret_edit(CellEdit::kill_to_end, cx),
            "ctrl-d" => self.grid_caret_edit(CellEdit::delete, cx),
            "ctrl-h" => self.grid_caret_edit(CellEdit::backspace, cx),
            // Emacs `completion-at-point`: finish the value from the column.
            "alt-tab" | "ctrl-alt-i" | "alt-/" => self.grid_complete_cell(cx),
            _ => {
                let plain = !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
                match ks.key_char.clone().filter(|t| plain && !t.is_empty()) {
                    Some(text) => self.grid_edit_input(&text, cx),
                    None => return Handled::No, // C-x, M-x, cmd-v, … go global
                }
            }
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// OS-special keys (bound as gpui actions, so they never reach
    /// `on_key_down`) on a grid. Returns whether the grid took the key.
    pub(super) fn grid_action_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if !self.grid_active() {
            return false;
        }
        if self.grid_editing() {
            // The function-name dropdown takes the arrows, Tab and Enter.
            if self.grid_completion_open() {
                match key {
                    "up" => {
                        return {
                            self.grid_completion_move(-1, cx);
                            true
                        };
                    }
                    "down" => {
                        return {
                            self.grid_completion_move(1, cx);
                            true
                        };
                    }
                    "tab" | "enter" if self.grid_complete_function(None, cx) => return true,
                    _ => {}
                }
            }
            let point = match key {
                "up" => Some((-1, 0, false)),
                "down" => Some((1, 0, false)),
                "left" => Some((0, -1, false)),
                "right" => Some((0, 1, false)),
                "shift-left" => Some((0, -1, true)),
                "shift-right" => Some((0, 1, true)),
                _ => None,
            };
            if let Some((dr, dc, extend)) = point
                && self.grid_point(dr, dc, extend, cx)
            {
                return true;
            }
            match key {
                "enter" => self.grid_commit_edit(1, 0, cx),
                "tab" => self.grid_commit_edit(0, 1, cx),
                "up" => self.grid_commit_edit(-1, 0, cx),
                "down" => self.grid_commit_edit(1, 0, cx),
                "left" => self.grid_caret_edit(CellEdit::left, cx),
                "right" => self.grid_caret_edit(CellEdit::right, cx),
                "home" => self.grid_caret_edit(CellEdit::home, cx),
                "end" => self.grid_caret_edit(CellEdit::end, cx),
                "backspace" => self.grid_caret_edit(CellEdit::backspace, cx),
                "delete" => self.grid_caret_edit(CellEdit::delete, cx),
                _ => {}
            }
            return true;
        }
        // `C-u 5 ↓`: the argument ends here (see `key_grid`).
        if self.uarg.reading {
            self.uarg.pending = Some(super::prefix_arg_value(self.uarg.digits, self.uarg.cu));
            self.reset_uarg();
        }
        let pending = self.pending_keys.join(" ");
        let sequence = if pending.is_empty() {
            key.to_string()
        } else {
            format!("{pending} {key}")
        };
        match self.grid_resolve(&sequence) {
            GridKey::Bound(expr) => {
                self.pending_keys.clear();
                self.grid_run(&expr, cx);
            }
            GridKey::Prefix => {
                self.pending_keys.push(key.to_string());
                cx.notify();
            }
            GridKey::Miss => {
                self.pending_keys.clear();
                self.modal_count = None;
            }
        }
        true
    }

    // --- Mouse ---------------------------------------------------------------------

    fn grid_hit(&self, pane: usize, pos: Point<Pixels>) -> Option<GridHit> {
        let geo = self.grid_ui.geom.get(&pane)?;
        let g = self.pane_grid(pane)?;
        let x = f32::from(pos.x) - f32::from(geo.origin.x);
        let y = f32::from(pos.y) - f32::from(geo.origin.y);
        if y < geo.bar_h {
            return Some(if x < NAME_BOX_WIDTH {
                GridHit::NameBox
            } else {
                GridHit::Bar { x }
            });
        }
        let (cx0, cy0) = (geo.header_w, geo.bar_h + geo.header_h);
        let v = &g.view;
        if y < cy0 {
            if x < cx0 {
                return Some(GridHit::Corner);
            }
            let cxr = (x - cx0) as f64;
            let col = v.col_at(cxr);
            let right = v.col_x(col) + v.col_px(col);
            let left = v.col_x(col);
            if (cxr - right).abs() <= BORDER_GRAB as f64 {
                return Some(GridHit::ColBorder(col));
            }
            if col > 0 && left >= 1.0 && (cxr - left).abs() <= BORDER_GRAB as f64 {
                // The left border of `col` is the right border of the
                // previous visible column — when there is one on screen (a
                // hidden first column has no border to grab).
                let prev = v.col_at(left - 1.0);
                if prev != col {
                    return Some(GridHit::ColBorder(prev));
                }
            }
            return Some(GridHit::ColHeader(col));
        }
        let cyr = (y - cy0) as f64;
        let row = v.row_at(cyr);
        if x < cx0 {
            let bottom = v.row_y(row) + v.row_px(row);
            let top = v.row_y(row);
            if (cyr - bottom).abs() <= BORDER_GRAB as f64 {
                return Some(GridHit::RowBorder(row));
            }
            if row > 0 && top >= 1.0 && (cyr - top).abs() <= BORDER_GRAB as f64 {
                let prev = v.row_at(top - 1.0);
                if prev != row {
                    return Some(GridHit::RowBorder(prev));
                }
            }
            return Some(GridHit::RowHeader(row));
        }
        Some(GridHit::Cell(row, v.col_at((x - cx0) as f64)))
    }

    /// A mouse press on a grid pane. Returns whether a grid took it.
    pub(super) fn grid_mouse_down(
        &mut self,
        pos: Point<Pixels>,
        button: MouseButton,
        clicks: usize,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if button != MouseButton::Left {
            return false;
        }
        // A click on the function-name dropdown picks that name.
        if self.grid_completion_open()
            && let Some(hit) = self.grid_ui.completion_hit
            && hit.bounds.contains(&pos)
        {
            let row = ((pos.y - hit.bounds.origin.y) / hit.row_height) as usize;
            if row < hit.rows {
                self.grid_complete_function(Some(hit.start + row), cx);
            }
            return true;
        }
        let Some(pane) = self.pane_at_position(pos) else {
            return false;
        };
        if self.pane_grid(pane).is_none() {
            return false;
        }
        if pane != self.selected_pane {
            self.select_pane(pane, cx);
        }
        let Some(hit) = self.grid_hit(pane, pos) else {
            return true;
        };
        // A click anywhere but the open cell/bar commits the edit first.
        // Typing a formula after an operator: a click points at the cell,
        // inserting its reference (Excel's point mode).
        if let GridHit::Cell(r, c) = hit
            && let Some(e) = self
                .active
                .grid
                .as_deref_mut()
                .and_then(|g| g.view.edit.as_mut())
            && (e.row, e.col) != (r, c)
            && e.point_eligible()
        {
            let anchor = if shift {
                e.point.as_ref().map_or((r, c), |p| p.anchor)
            } else {
                (r, c)
            };
            e.set_point(anchor, (r, c));
            self.grid_after(cx);
            return true;
        }
        let keep_edit = matches!(hit, GridHit::Bar { .. })
            || matches!(hit, GridHit::Cell(r, c)
                if self.active.grid.as_ref().and_then(|g| g.view.edit.as_ref())
                    .is_some_and(|e| e.row == r && e.col == c));
        if !keep_edit {
            self.grid_commit_pending(cx);
        }
        let geo = self.grid_ui.geom.get(&pane).copied();
        let Some(g) = self.active.grid.as_deref_mut() else {
            return true;
        };
        let (rows, cols) = g.used();
        // A click elsewhere is a new selection: helix's extra cursors go.
        if !keep_edit && !matches!(hit, GridHit::ColBorder(_) | GridHit::RowBorder(_)) {
            g.view.cursors.clear();
        }
        match hit {
            GridHit::NameBox => {
                self.grid_command(GridCmd::Goto, cx);
                return true;
            }
            GridHit::Bar { x } => {
                if g.view.edit.is_none() && g.loading.is_none() {
                    let (r, c) = g.view.cursor;
                    let text = g.source(r, c).to_string();
                    g.view.edit = Some(CellEdit::new(r, c, &text, text.clone(), true));
                    g.view.spread = Spread::Append;
                }
                if let (Some(e), Some(geo)) = (g.view.edit.as_mut(), geo) {
                    e.in_bar = true;
                    let rel = (x - geo.bar_text_x + geo.bar_shift).max(0.0);
                    e.set_caret_chars((rel / geo.bar_char_w.max(1.0)).round() as usize);
                }
            }
            GridHit::Corner => {
                g.view.anchor = Some((0, 0));
                g.view.cursor = (rows.saturating_sub(1), cols.saturating_sub(1));
                g.view.sel_kind = SelKind::Cells;
            }
            GridHit::ColBorder(col) | GridHit::RowBorder(col) => {
                let is_col = matches!(hit, GridHit::ColBorder(_));
                if clicks >= 2 && is_col {
                    // Double-click a column border: auto-fit (every selected
                    // column when the border belongs to the selection).
                    let char_w = geo.map_or(7.0, |geo| {
                        geo.char_w as f64 / f64::from(g.view.zoom).max(0.1)
                    });
                    let top = g.view.top_row();
                    let page = g.view.page_rows();
                    for c in selected_or(g, col, Axis::Cols) {
                        let sample = (0..rows.min(AUTOFIT_SAMPLE))
                            .chain((top..top + page + 1).filter(|&x| x < rows));
                        let w = g.fit_width(c, sample, char_w, self.grid_ui.opts.max_autofit_width);
                        g.resize(Axis::Cols, c, w);
                    }
                    self.grid_persist_view();
                } else {
                    let (start, size) = if is_col {
                        (g.view.col_x(col) as f32, g.view.cols.size(col))
                    } else {
                        (g.view.row_y(col) as f32, g.view.rows.size(col))
                    };
                    self.grid_ui.drag = Some(GridDrag::Resize {
                        pane,
                        axis: if is_col { Axis::Cols } else { Axis::Rows },
                        idx: col,
                        start,
                        size,
                    });
                }
            }
            GridHit::ColHeader(col) => {
                if shift && g.view.anchor.is_some() && g.view.sel_kind == SelKind::Cols {
                    g.view.cursor.1 = col;
                } else {
                    g.view.anchor = Some((0, col));
                    g.view.cursor = (g.view.cursor.0, col);
                    g.view.sel_kind = SelKind::Cols;
                }
                g.sticky_sel = false;
                self.grid_ui.drag = Some(GridDrag::Select { pane });
            }
            GridHit::RowHeader(row) => {
                if shift && g.view.anchor.is_some() && g.view.sel_kind == SelKind::Rows {
                    g.view.cursor.0 = row;
                } else {
                    g.view.anchor = Some((row, 0));
                    g.view.cursor = (row, g.view.cursor.1);
                    g.view.sel_kind = SelKind::Rows;
                }
                g.sticky_sel = false;
                self.grid_ui.drag = Some(GridDrag::Select { pane });
            }
            GridHit::Cell(r, c) => {
                if keep_edit {
                    let col_x = g.view.col_x(c) as f32;
                    if let (Some(e), Some(geo)) = (g.view.edit.as_mut(), geo) {
                        e.in_bar = false;
                        let x0 = geo.header_w + col_x;
                        let rel = f32::from(pos.x) - f32::from(geo.origin.x) - x0 - 3.0;
                        e.set_caret_chars((rel.max(0.0) / geo.char_w.max(1.0)).round() as usize);
                    }
                } else if clicks >= 2 && g.loading.is_none() {
                    g.view.anchor = None;
                    g.view.cursor = (r, c);
                    let text = g.source(r, c).to_string();
                    g.view.edit = Some(CellEdit::new(r, c, &text, text.clone(), true));
                } else {
                    if shift {
                        g.view.anchor.get_or_insert(g.view.cursor);
                        g.view.sel_kind = SelKind::Cells;
                    } else {
                        g.view.anchor = Some((r, c));
                        g.view.sel_kind = SelKind::Cells;
                    }
                    g.view.cursor = (r, c);
                    g.sticky_sel = false;
                    self.grid_ui.drag = Some(GridDrag::Select { pane });
                }
            }
        }
        self.grid_after(cx);
        true
    }

    /// Mouse motion while a grid drag is in progress.
    pub(super) fn grid_mouse_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.grid_ui.drag else {
            return false;
        };
        match drag {
            GridDrag::Select { pane } => {
                let Some(hit) = self.grid_hit(pane, pos) else {
                    return true;
                };
                let Some(g) = self.pane_grid_mut(pane) else {
                    return true;
                };
                match (hit, g.view.sel_kind) {
                    (GridHit::Cell(r, c), SelKind::Cells) => g.view.cursor = (r, c),
                    (GridHit::Cell(r, _) | GridHit::RowHeader(r), SelKind::Rows) => {
                        g.view.cursor.0 = r
                    }
                    (GridHit::Cell(_, c) | GridHit::ColHeader(c), SelKind::Cols) => {
                        g.view.cursor.1 = c
                    }
                    _ => {}
                }
                self.grid_after(cx);
            }
            GridDrag::Resize {
                pane,
                axis,
                idx,
                start,
                ..
            } => {
                let cols = axis == Axis::Cols;
                let Some(geo) = self.grid_ui.geom.get(&pane).copied() else {
                    return true;
                };
                let zoom = self.pane_grid(pane).map_or(1.0, |g| g.view.zoom).max(0.1);
                let (cx0, cy0) = geo.cells_origin();
                let edge = if cols {
                    f32::from(pos.x) - cx0
                } else {
                    f32::from(pos.y) - cy0
                };
                let size = (((edge - start) / zoom) as f64).max(0.0);
                self.grid_ui.drag = Some(GridDrag::Resize {
                    pane,
                    axis,
                    idx,
                    start,
                    size,
                });
                let px_size = (size * zoom as f64).round();
                let label = if cols {
                    let chars = size / 7.0;
                    format!("Width: {chars:.2} ({px_size} px)")
                } else {
                    format!("Height: {:.2} ({px_size} px)", size * 0.75)
                };
                self.status = label.into();
                cx.notify();
            }
        }
        true
    }

    /// Mouse release: finish a resize (applied now, as one undo step).
    pub(super) fn grid_mouse_up(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.grid_ui.drag.take() else {
            return;
        };
        if let GridDrag::Resize {
            pane,
            axis,
            idx,
            size,
            ..
        } = drag
        {
            if let Some(g) = self.pane_grid_mut(pane) {
                for i in selected_or(g, idx, axis) {
                    g.resize(axis, i, size);
                }
            }
            self.status = SharedString::default();
            if self.pane_grid(pane).is_some() && self.selected_pane == pane {
                self.grid_persist_view();
            }
            self.grid_after(cx);
        }
    }

    /// The wheel over a grid pane scrolls it (Shift = horizontal; Cmd/Ctrl
    /// zooms). Returns whether a grid took it.
    pub(super) fn grid_scroll(
        &mut self,
        pos: Point<Pixels>,
        dx: f32,
        dy: f32,
        zoom: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_at_position(pos) else {
            return false;
        };
        let Some(g) = self.pane_grid_mut(pane) else {
            return false;
        };
        if zoom {
            if dy.abs() > 0.5 {
                g.view.step_zoom(if dy > 0.0 { 1 } else { -1 });
            }
        } else {
            let z = f64::from(g.view.zoom).max(0.1);
            let (dx, dy) = if shift && dx.abs() < 0.01 {
                (dy, 0.0)
            } else {
                (dx, dy)
            };
            g.view.scroll_x -= dx as f64 / z;
            g.view.scroll_y -= dy as f64 / z;
            g.view.clamp_scroll();
        }
        cx.notify();
        true
    }
}

/// Result of resolving a key sequence against the grid keymaps.
enum GridKey {
    Bound(String),
    Prefix,
    Miss,
}

/// Whole rows or whole columns: the selection kind of a line along `axis`.
fn line_kind(axis: Axis) -> SelKind {
    match axis {
        Axis::Cols => SelKind::Cols,
        Axis::Rows => SelKind::Rows,
    }
}

/// The columns (or rows) a header-border action applies to: every selected
/// one when `idx` is part of a whole-column (row) selection, else just `idx`.
fn selected_or(g: &GridDoc, idx: usize, axis: Axis) -> Vec<usize> {
    if g.view.anchor.is_some() && g.view.sel_kind == line_kind(axis) {
        let (a, b) = g.view.selection((usize::MAX, usize::MAX)).span(axis);
        if (a..=b).contains(&idx) {
            return (a..=b).collect();
        }
    }
    vec![idx]
}

#[cfg(test)]
mod tests {
    use super::{fmt_num, group_thousands};

    #[test]
    fn number_formatting() {
        assert_eq!(group_thousands(1204.0), "1,204");
        assert_eq!(group_thousands(-1234567.0), "-1,234,567");
        assert_eq!(fmt_num(50.25), "50.25");
        assert_eq!(fmt_num(1234.5), "1,234.50");
        assert_eq!(fmt_num(12.0), "12");
        assert_eq!(fmt_num(-0.4), "-0.40");
        assert_eq!(fmt_num(-1234.5), "-1,234.50");
        assert_eq!(group_thousands(0.0), "0");
        assert_eq!(group_thousands(999.0), "999");
    }

    /// Several structural edits in one command carry the formatting along
    /// each edit once (the change log still holds the earlier ones).
    #[test]
    fn grouped_deletes_move_formats_once() {
        use super::*;
        let csv: String = (1..=7).map(|i| format!("{i}\n")).collect();
        let wb = crate::grid::parse::parse_now(csv.into_bytes(), Some("csv")).unwrap();
        let mut g = GridDoc::new(wb, Some("csv".into()), 0, &Default::default());
        let bold = CellFmt {
            bold: true,
            ..CellFmt::default()
        };
        g.formats.set(5, 0, bold);
        g.grouped(|g| {
            for k in [4usize, 2] {
                g.model_op(|wb| wb.delete_rows(SHEET, k, 1));
            }
        });
        let at: Vec<_> = g.formats.positions().collect();
        assert_eq!(at, vec![(3, 0)]);
    }

    /// A grid over `csv`, with no editor around it.
    fn doc(csv: &str) -> super::GridDoc {
        let wb = crate::grid::parse::parse_now(csv.as_bytes().to_vec(), Some("csv")).unwrap();
        super::GridDoc::new(wb, Some("csv".into()), 0, &Default::default())
    }

    fn column(g: &super::GridDoc, c: usize) -> Vec<String> {
        (0..g.used().0).map(|r| g.cell(r, c).to_string()).collect()
    }

    #[test]
    fn apply_sorts_and_undoes_without_an_editor() {
        use super::{CmdEnv, GridCmd, Outcome};
        let mut g = doc("n\n3\n1\n2\n");
        let env = CmdEnv::default();
        assert!(g.apply(GridCmd::Undo, &env).is_err(), "nothing to undo yet");
        let out = g.apply(GridCmd::SortAscending, &env).unwrap();
        assert_eq!(out.msg.as_deref(), Some("Sorted by column A (ascending)"));
        assert_eq!(column(&g, 0), ["n", "1", "2", "3"]);
        assert_eq!(g.apply(GridCmd::Undo, &env), Ok(Outcome::default()));
        assert_eq!(column(&g, 0), ["n", "3", "1", "2"]);
        g.apply(GridCmd::Redo, &env).unwrap();
        assert_eq!(column(&g, 0), ["n", "1", "2", "3"]);
    }

    #[test]
    fn apply_resizes_persist_the_view_and_undo_as_layout() {
        use super::{CmdEnv, GridCmd};
        let mut g = doc("a,b\n1,2\n");
        let env = CmdEnv::default();
        let before = g.view.cols.size(0);
        let out = g.apply(GridCmd::WidenColumn, &env).unwrap();
        assert!(out.persist_view);
        assert_eq!(g.view.cols.size(0), before + env.char_w);
        let undone = g.apply(GridCmd::Undo, &env).unwrap();
        assert!(undone.persist_view, "a size change undoes as layout");
        assert_eq!(g.view.cols.size(0), before);
    }

    #[test]
    fn apply_deletes_only_shown_rows_under_a_filter() {
        use super::{CmdEnv, GridCmd, RowFilter};
        let mut g = doc("k\na\nb\na\nb\n");
        g.filters.push(RowFilter::Rows {
            rows: vec![1, 3],
            label: "rows".into(),
        });
        g.apply_filters();
        // Select rows 2–5 (all the data), then delete: the hidden b's stay.
        g.move_to(1, 0, false);
        g.move_to(4, 0, true);
        g.apply(GridCmd::DeleteRow, &CmdEnv::default()).unwrap();
        assert_eq!(column(&g, 0), ["k", "b", "b"]);
        // …as one undo step.
        g.apply(GridCmd::Undo, &CmdEnv::default()).unwrap();
        assert_eq!(column(&g, 0), ["k", "a", "b", "a", "b"]);
    }

    /// Resizing a hidden column (a drag across a column selection that
    /// spans it) is recorded, and undo restores its stored size.
    #[test]
    fn resizing_a_hidden_column_undoes() {
        use super::{Axis, CmdEnv, GridCmd};
        let mut g = doc("a,b,c\n1,2,3\n");
        let default = g.view.cols.default;
        g.view.cols.set_hidden(1, true);
        g.resize(Axis::Cols, 1, 100.0);
        assert_eq!(g.view.cols.base(1), 100.0);
        g.apply(GridCmd::Undo, &CmdEnv::default()).unwrap();
        assert_eq!(g.view.cols.base(1), default);
    }

    #[test]
    fn apply_freeze_toggles_with_a_message() {
        use super::{CmdEnv, GridCmd};
        let mut g = doc("h\n1\n2\n3\n");
        let env = CmdEnv::default();
        g.move_to(2, 0, false);
        let out = g.apply(GridCmd::ToggleFreeze, &env).unwrap();
        assert_eq!(out.msg.as_deref(), Some("Froze rows 1–2"));
        assert!(out.persist_view);
        assert_eq!(g.view.frozen_rows, 2);
        let out = g.apply(GridCmd::ToggleFreeze, &env).unwrap();
        assert_eq!(out.msg.as_deref(), Some("Unfrozen"));
        assert_eq!(g.view.frozen_rows, 0);
    }
}
