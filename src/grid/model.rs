//! `Workbook`: the owner of a file's sheets, the shared string pool, and the
//! editing API with undo/redo.
//!
//! A CSV opens as a workbook with exactly one sheet; `.xlsx` (later) opens one
//! per tab. Cross-sheet state — the pool, formulas, the undo history and the
//! modified flag — lives here so every sheet buffer of one file agrees on it.
//!
//! Every edit is one undo step, including bulk ones (paste, clearing a range,
//! a sort), and is stored compactly: cell edits keep old/new [`CellRef`]s,
//! deletes keep the removed cells, a sort keeps its row permutation. Undoing
//! back to the last save clears [`Workbook::is_modified`], as for text buffers.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};

use rustc_hash::FxHashSet;

use super::cell::CellRef;
use super::dialect::Dialect;
use super::infer::{ColType, infer_column};
use super::intern::StringPool;
use super::sheet::{Chunks, RowMeta, RowView, Sheet};

/// Formula text by `(sheet, row, col)`, kept apart from the cells so that
/// non-formula cells stay 8 bytes. Always empty in v1.
pub type FormulaTable = HashMap<(usize, usize, usize), u32>;

/// Most cells (rows × columns of the used rectangle) an edit may stretch a
/// sheet to. Cells are stored densely over that rectangle, 8 bytes each, so
/// this bounds what one far-away write can allocate (512 MiB).
pub const MAX_CELLS: usize = 64 << 20;

/// Whether a `rows × cols` rectangle is within [`MAX_CELLS`].
pub fn fits(rows: usize, cols: usize) -> bool {
    rows.checked_mul(cols).is_some_and(|n| n <= MAX_CELLS)
}

/// Whether growing a sheet from `old` to `new` (rows, cols) adds at most
/// [`MAX_CELLS`] cells — so a sheet already that large (a big file) can
/// still gain a row, but no single write stretches it far.
pub fn fits_growth(old: (usize, usize), new: (usize, usize)) -> bool {
    new.0
        .checked_mul(new.1)
        .is_some_and(|n| n.saturating_sub(old.0 * old.1) <= MAX_CELLS)
}

/// The file format a workbook was loaded from and saves back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Format {
    Csv(Dialect),
}

#[derive(Clone, Copy, Debug)]
struct CellChange {
    row: usize,
    col: usize,
    old: CellRef,
    new: CellRef,
}

#[derive(Debug)]
enum EditKind {
    SetCells {
        changes: Vec<CellChange>,
        /// Each touched row's metadata before the edit (restores clean rows).
        old_meta: Vec<(usize, RowMeta)>,
        /// `(rows, cols)` before the edit: undo shrinks back to it, so writing
        /// past the data and undoing leaves no trace.
        extent: (usize, usize),
    },
    InsertRows {
        at: usize,
        n: usize,
    },
    DeleteRows {
        at: usize,
        n: usize,
        /// The removed rows while the edit is applied (moved back on undo).
        cells: Vec<Vec<CellRef>>,
        metas: Vec<RowMeta>,
    },
    InsertCols {
        at: usize,
        n: usize,
        min_width: usize,
        /// Dirty flags before the edit (a column op dirties every row).
        dirty: Vec<u64>,
    },
    DeleteCols {
        at: usize,
        n: usize,
        cols: Vec<Chunks<CellRef>>,
        types: Vec<ColType>,
        min_width: usize,
        dirty: Vec<u64>,
    },
    Sort {
        start: usize,
        col: usize,
        /// New row `start + i` is old row `perm[i]`.
        perm: Vec<usize>,
    },
    /// Column `from` moved to index `to` (the others shift to make room).
    MoveCol {
        from: usize,
        to: usize,
        dirty: Vec<u64>,
    },
}

impl EditKind {
    fn for_each_cell_mut(&mut self, f: &mut impl FnMut(&mut CellRef)) {
        match self {
            EditKind::SetCells { changes, .. } => {
                for ch in changes {
                    f(&mut ch.old);
                    f(&mut ch.new);
                }
            }
            EditKind::DeleteRows { cells, .. } => cells.iter_mut().flatten().for_each(f),
            EditKind::DeleteCols { cols, .. } => cols.iter_mut().flatten().flatten().for_each(f),
            _ => {}
        }
    }
}

#[derive(Debug)]
struct Edit {
    /// Unique per workbook; the saved point is remembered as the id on top of
    /// the undo stack at save time.
    id: u64,
    sheet: usize,
    kind: EditKind,
}

/// How a sort compares a column's values (org-table's `C-c ^` choices).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKind {
    /// By the column's inferred type: numbers by value, everything else as text.
    Auto,
    /// As text, whatever the column looks like.
    Alpha,
    /// By numeric value; values that aren't numbers sort after the numbers.
    Numeric,
    /// By date/time (`2024-03-01`, `2024-03-01 14:30`, `3/1/2024`); values
    /// that aren't dates sort after them.
    Time,
}

/// Sort key for one cell.
enum Key<'a> {
    Num(f64),
    Text(&'a str, Option<u32>),
    Empty,
}

impl<'a> Key<'a> {
    fn of(cell: &'a CellRef, source: &'a str, pool: &'a StringPool, kind: SortKind) -> Key<'a> {
        if cell.is_empty() {
            return Key::Empty;
        }
        let s = cell.get(source, pool);
        let num = match kind {
            SortKind::Numeric => super::infer::cell_number(s),
            SortKind::Time => time_key(s),
            SortKind::Auto | SortKind::Alpha => None,
        };
        match num {
            Some(v) => Key::Num(v),
            None => Key::Text(s, cell.interned_id()),
        }
    }
}

/// A date/time as a sortable number `YYYYMMDD.hhmmss`, for ISO dates (with an
/// optional `T`/space time) and `M/D/YYYY` / `D.M.YYYY` short dates.
pub fn time_key(s: &str) -> Option<f64> {
    let s = s.trim();
    let num = |t: &str| t.parse::<u32>().ok();
    let (date, time) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let (y, m, d) = if date.len() >= 10 && matches!(date.as_bytes()[4], b'-' | b'/') {
        let sep = date.as_bytes()[4] as char;
        let mut it = date.split(sep);
        (num(it.next()?)?, num(it.next()?)?, num(it.next()?)?)
    } else {
        let sep = if date.contains('/') { '/' } else { '.' };
        let parts: Vec<&str> = date.split(sep).collect();
        let [a, b, y] = parts[..] else {
            return None;
        };
        let y = num(y)?;
        let y = if y < 100 { 2000 + y } else { y };
        // US order for `/`, European for `.`.
        if sep == '/' {
            (y, num(a)?, num(b)?)
        } else {
            (y, num(b)?, num(a)?)
        }
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut hms = 0f64;
    if !time.is_empty() {
        let clock: String = time
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ':')
            .collect();
        let mut parts = clock.split(':').map(|p| p.parse::<u32>().unwrap_or(0));
        let (h, mi, se) = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        // Out-of-range clock parts aren't a time (and would overflow).
        if h > 99 || mi > 59 || se > 60 {
            return None;
        }
        hms = f64::from(h * 10_000 + mi * 100 + se) / 1_000_000.0;
    }
    // In f64: a year as large as a u32 holds would overflow `y * 10_000`.
    Some(f64::from(y) * 10_000.0 + f64::from(m * 100 + d) + hms)
}

/// Numbers before text; empty cells last in either direction (as Excel does).
fn compare_keys(a: &Key, b: &Key, descending: bool) -> Ordering {
    let ord = match (a, b) {
        (Key::Empty, Key::Empty) => return Ordering::Equal,
        (Key::Empty, _) => return Ordering::Greater,
        (_, Key::Empty) => return Ordering::Less,
        (Key::Num(x), Key::Num(y)) => x.total_cmp(y),
        (Key::Num(_), Key::Text(..)) => Ordering::Less,
        (Key::Text(..), Key::Num(_)) => Ordering::Greater,
        (Key::Text(x, ix), Key::Text(y, iy)) => {
            if ix.is_some() && ix == iy {
                Ordering::Equal
            } else {
                x.cmp(y)
            }
        }
    };
    if descending { ord.reverse() } else { ord }
}

pub struct Workbook {
    pub format: Format,
    pool: StringPool,
    sheets: Vec<Sheet>,
    #[allow(dead_code)] // model API kept for multi-sheet / `.xlsx`
    pub formulas: FormulaTable,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    next_id: u64,
    /// Id of the undo-stack top when last saved (0: the empty stack).
    saved_id: u64,
    complete: bool,
    /// Bumped by every applied edit, undo and redo (anything that can change
    /// what a cell reads as) — for caches of derived data.
    generation: u64,
    /// Bumped when rows are added, removed, or reordered.
    layout_generation: u64,
    /// What edits did since the last [`Workbook::take_changes`], for the
    /// formula engine (which cells to re-read, when to re-index).
    changes: Vec<Change>,
    /// An edit was refused for stretching the sheet past [`MAX_CELLS`].
    refused_growth: bool,
}

/// What an edit did, as the formula engine needs to know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// These cells' contents changed.
    Cells(Vec<(usize, usize)>),
    RowsInserted {
        at: usize,
        n: usize,
    },
    RowsDeleted {
        at: usize,
        n: usize,
    },
    ColsInserted {
        at: usize,
        n: usize,
    },
    ColsDeleted {
        at: usize,
        n: usize,
    },
    /// A sort: new row `start + i` is old row `perm[i]`.
    Sorted {
        start: usize,
        perm: Vec<usize>,
    },
    /// Rows moved as cells (org-table `M-up`/`M-down`): same shape as
    /// `Sorted`, but references to the moved cells follow them.
    RowsMoved {
        start: usize,
        perm: Vec<usize>,
    },
    ColMoved {
        from: usize,
        to: usize,
    },
    /// An undo/redo changed the sheet's shape: positions shifted, and any
    /// reference rewriting is itself undone/redone as cell changes.
    Reshaped,
}

/// A read-only copy of one sheet's cells, for Scheme's synchronous reads.
pub struct CellsSnapshot {
    sheet: Sheet,
    pool: StringPool,
}

impl CellsSnapshot {
    /// A cell's text; a formula reads as its source (`=SUM(A1:A3)`).
    /// A cell as Scheme reads it: what typing recreates it (a formula as
    /// `=…`, text starting with `=` as `'=…`), so writing a value read back
    /// keeps it what it was.
    pub fn cell(&self, row: usize, col: usize) -> std::borrow::Cow<'_, str> {
        crate::formula::engine::input_text(self.sheet.cell(&self.pool, row, col))
    }

    pub fn rows(&self) -> usize {
        self.sheet.rows()
    }

    pub fn cols(&self) -> usize {
        self.sheet.cols()
    }
}

impl Workbook {
    /// A new, empty one-sheet workbook (a file that doesn't exist yet).
    #[allow(dead_code)] // model API kept for multi-sheet / `.xlsx`
    pub fn new(format: Format) -> Workbook {
        Workbook::from_parts(
            format,
            StringPool::new(),
            vec![Sheet::new("Sheet1", "".into())],
            true,
        )
    }

    pub(super) fn from_parts(
        format: Format,
        pool: StringPool,
        sheets: Vec<Sheet>,
        complete: bool,
    ) -> Workbook {
        Workbook {
            format,
            pool,
            sheets,
            formulas: FormulaTable::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 0,
            saved_id: 0,
            complete,
            generation: 0,
            layout_generation: 0,
            changes: Vec::new(),
            refused_growth: false,
        }
    }

    /// Everything edits did since the last call (see [`Change`]).
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// How long the change log is now: a mark for [`Self::changes_since`].
    pub fn change_mark(&self) -> usize {
        self.changes.len()
    }

    /// The changes logged since `mark` (from [`Self::change_mark`]; 0 for
    /// everything since the last [`Self::take_changes`]), left in the log.
    pub fn changes_since(&self, mark: usize) -> &[Change] {
        &self.changes[mark.min(self.changes.len())..]
    }

    /// Whether the last edit was refused for stretching the sheet past
    /// [`MAX_CELLS`] (and forget it).
    pub fn take_refused_growth(&mut self) -> bool {
        std::mem::take(&mut self.refused_growth)
    }

    /// A one-sheet workbook holding `cells` (`(row, col, stored text)`),
    /// as loaded from a format other than CSV (`.xlsx`). It saves through
    /// its own writer, so the CSV dialect is just the default.
    pub fn from_cells(name: &str, cells: Vec<(usize, usize, String)>) -> Workbook {
        let mut wb = Workbook::from_parts(
            Format::Csv(Dialect::default()),
            StringPool::new(),
            vec![Sheet::new(name, "".into())],
            true,
        );
        let s = &mut wb.sheets[0];
        for (r, c, v) in cells {
            s.grow_rows(r + 1);
            s.grow_cols(c + 1);
            *s.cell_mut(r, c) = CellRef::from_value(&v, &mut wb.pool);
        }
        wb.infer_types(0);
        wb
    }

    /// See the `generation` field.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// See the `layout_generation` field.
    pub fn layout_generation(&self) -> u64 {
        self.layout_generation
    }

    /// A read-only copy of `sheet`'s cells (costs a copy of every cell).
    pub fn cells_snapshot(&self, sheet: usize) -> CellsSnapshot {
        CellsSnapshot {
            sheet: self.sheets[sheet].clone(),
            pool: self.pool.read_copy(),
        }
    }

    pub fn pool(&self) -> &StringPool {
        &self.pool
    }

    #[allow(dead_code)] // model API kept for multi-sheet / `.xlsx`
    pub fn sheets(&self) -> &[Sheet] {
        &self.sheets
    }

    pub fn sheet(&self, sheet: usize) -> &Sheet {
        &self.sheets[sheet]
    }

    /// Rename a sheet (the workbook's tab; not an undoable cell edit).
    pub fn rename_sheet(&mut self, sheet: usize, name: &str) {
        self.sheets[sheet].name = name.to_string();
    }

    /// The CSV dialect the workbook saves with.
    pub fn dialect(&self) -> &Dialect {
        match &self.format {
            Format::Csv(d) => d,
        }
    }

    /// False for a [`parse_prefix`](super::parse::parse_prefix) preview, which holds
    /// only the first rows and must not be saved.
    #[allow(dead_code)] // model API kept for multi-sheet / `.xlsx`
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    // ---- reading ------------------------------------------------------------

    /// The text at `(row, col)`. The grid is unbounded: anywhere outside the
    /// used area reads as `""`.
    pub fn cell(&self, sheet: usize, row: usize, col: usize) -> &str {
        self.sheets[sheet].cell(&self.pool, row, col)
    }

    pub fn used_rows(&self, sheet: usize) -> usize {
        self.sheets[sheet].rows()
    }

    pub fn used_cols(&self, sheet: usize) -> usize {
        self.sheets[sheet].cols()
    }

    /// Visit column `col`'s text over `rows` in order — one chunk lookup for
    /// the whole range, not one per cell.
    pub fn for_each_in_column(
        &self,
        sheet: usize,
        col: usize,
        rows: Range<usize>,
        mut f: impl FnMut(usize, &str),
    ) {
        let s = &self.sheets[sheet];
        s.try_for_each_in_column(col, rows, |row, cell| {
            f(row, cell.get(s.source(), &self.pool));
            ControlFlow::Continue(())
        });
    }

    /// Visit `rows` in order, each positioned once (see [`RowView`]).
    pub fn for_each_row<'a>(
        &'a self,
        sheet: usize,
        rows: Range<usize>,
        f: impl FnMut(usize, RowView<'a>),
    ) {
        self.sheets[sheet].for_each_row(&self.pool, rows, f);
    }

    /// Sorted row indices of malformed records (an unclosed quote, or a field
    /// count differing from the first record's), as numbered at load time.
    pub fn problems(&self, sheet: usize) -> &[usize] {
        self.sheets[sheet].problems()
    }

    pub fn header(&self, sheet: usize) -> bool {
        self.sheets[sheet].header
    }

    /// Mark row 1 as a header (or not). Display and sort/inference behaviour
    /// only — the row is still ordinary data — so this is not an edit.
    pub fn set_header(&mut self, sheet: usize, header: bool) {
        if self.sheets[sheet].header != header {
            self.sheets[sheet].header = header;
            self.infer_types(sheet);
        }
    }

    pub fn col_type(&self, sheet: usize, col: usize) -> ColType {
        self.sheets[sheet].col_type(col)
    }

    /// (Re)infer every column's type from a sample (see [`super::infer`]).
    pub fn infer_types(&mut self, sheet: usize) {
        let s = &self.sheets[sheet];
        let types = (0..s.cols())
            .map(|c| infer_column(s, &self.pool, c))
            .collect();
        self.sheets[sheet].col_types = types;
    }

    // ---- editing ------------------------------------------------------------

    /// Set one cell, growing the sheet to include it. Returns whether anything
    /// changed (setting a cell to its current value records nothing).
    pub fn set_cell(&mut self, sheet: usize, row: usize, col: usize, value: &str) -> bool {
        self.write_cells(sheet, std::iter::once((row, col, value)))
    }

    /// Set many cells as one undo step (paste, clearing a range).
    pub fn set_cells(&mut self, sheet: usize, cells: &[(usize, usize, String)]) -> bool {
        self.write_cells(sheet, cells.iter().map(|(r, c, v)| (*r, *c, v.as_str())))
    }

    fn write_cells<'v>(
        &mut self,
        sheet: usize,
        cells: impl IntoIterator<Item = (usize, usize, &'v str)>,
    ) -> bool {
        let cells: Vec<(usize, usize, &str)> = cells.into_iter().collect();
        // Storage is dense over the used rectangle: refuse an edit that would
        // stretch it past MAX_CELLS (a far-away cell, a huge index).
        let s = &self.sheets[sheet];
        let (mut rows, mut cols) = (s.rows(), s.cols());
        for &(r, c, v) in &cells {
            if !v.is_empty() {
                rows = rows.max(r.saturating_add(1));
                cols = cols.max(c.saturating_add(1));
            }
        }
        if (rows, cols) != (s.rows(), s.cols()) && !fits_growth((s.rows(), s.cols()), (rows, cols))
        {
            self.refused_growth = true;
            return false;
        }
        let s = &mut self.sheets[sheet];
        let pool = &mut self.pool;
        let extent = (s.rows(), s.cols());
        let mut changes = Vec::new();
        let mut old_meta = Vec::new();
        let mut touched = FxHashSet::default();
        for (row, col, value) in cells {
            let old = s.cell_ref(row, col).copied().unwrap_or_default();
            if old.get(s.source(), pool) == value {
                continue;
            }
            s.grow_rows(row + 1);
            s.grow_cols(col + 1);
            let new = CellRef::from_value(value, pool);
            *s.cell_mut(row, col) = new;
            let meta = s.meta_mut(row);
            if touched.insert(row) {
                old_meta.push((row, *meta));
            }
            meta.set_dirty(true);
            changes.push(CellChange { row, col, old, new });
        }
        if changes.is_empty() {
            return false;
        }
        self.changes.push(Change::Cells(
            changes.iter().map(|c| (c.row, c.col)).collect(),
        ));
        if s.rows() != extent.0 {
            self.layout_generation += 1;
        }
        self.push_edit(
            sheet,
            EditKind::SetCells {
                changes,
                old_meta,
                extent,
            },
        );
        true
    }

    /// Insert `n` empty rows before `at`. Inserting at or past the end of the
    /// used rows changes nothing (the space past the data is already empty).
    pub fn insert_rows(&mut self, sheet: usize, at: usize, n: usize) -> bool {
        let s = &self.sheets[sheet];
        if n == 0 || at >= s.rows() {
            return false;
        }
        if !fits_growth((s.rows(), s.cols()), (s.rows().saturating_add(n), s.cols())) {
            self.refused_growth = true;
            return false;
        }
        self.record(
            sheet,
            Change::RowsInserted { at, n },
            EditKind::InsertRows { at, n },
        )
    }

    /// Delete rows `at..at + n` (clamped to the used rows).
    pub fn delete_rows(&mut self, sheet: usize, at: usize, n: usize) -> bool {
        let rows = self.sheets[sheet].rows();
        let n = n.min(rows.saturating_sub(at));
        if n == 0 {
            return false;
        }
        self.record(
            sheet,
            Change::RowsDeleted { at, n },
            EditKind::DeleteRows {
                at,
                n,
                cells: Vec::new(),
                metas: Vec::new(),
            },
        )
    }

    /// Insert `n` empty columns before `at` (a no-op past the used columns).
    /// Every row is re-serialized on save, since every record changes.
    pub fn insert_cols(&mut self, sheet: usize, at: usize, n: usize) -> bool {
        let s = &self.sheets[sheet];
        if n == 0 || at >= s.cols() {
            return false;
        }
        if !fits_growth((s.rows(), s.cols()), (s.rows(), s.cols().saturating_add(n))) {
            self.refused_growth = true;
            return false;
        }
        self.record(
            sheet,
            Change::ColsInserted { at, n },
            EditKind::InsertCols {
                at,
                n,
                min_width: 0,
                dirty: Vec::new(),
            },
        )
    }

    /// Delete columns `at..at + n` (clamped to the used columns).
    pub fn delete_cols(&mut self, sheet: usize, at: usize, n: usize) -> bool {
        let cols = self.sheets[sheet].cols();
        let n = n.min(cols.saturating_sub(at));
        if n == 0 {
            return false;
        }
        self.record(
            sheet,
            Change::ColsDeleted { at, n },
            EditKind::DeleteCols {
                at,
                n,
                cols: Vec::new(),
                types: Vec::new(),
                min_width: 0,
                dirty: Vec::new(),
            },
        )
    }

    /// Stable sort of the rows by column `col` (all rows, or all but row 1
    /// when `skip_header`). Numeric columns compare by value, others by text;
    /// empty cells go last either way. Rows keep their clean state — their
    /// source bytes are untouched, the writer just emits them in the new order.
    pub fn sort_by_column(
        &mut self,
        sheet: usize,
        col: usize,
        descending: bool,
        skip_header: bool,
    ) -> bool {
        self.sort_by_column_as(sheet, col, descending, skip_header, SortKind::Auto)
    }

    /// [`Self::sort_by_column`] comparing values as `kind`.
    pub fn sort_by_column_as(
        &mut self,
        sheet: usize,
        col: usize,
        descending: bool,
        skip_header: bool,
        kind: SortKind,
    ) -> bool {
        let s = &self.sheets[sheet];
        let start = usize::from(skip_header).min(s.rows());
        let n = s.rows() - start;
        if n < 2 || col >= s.cols() {
            return false;
        }
        let kind = match kind {
            SortKind::Auto if s.col_type(col).is_numeric() => SortKind::Numeric,
            SortKind::Auto => SortKind::Alpha,
            k => k,
        };
        let mut keys = Vec::with_capacity(n);
        s.try_for_each_in_column(col, start..s.rows(), |_, cell| {
            keys.push(Key::of(cell, s.source(), &self.pool, kind));
            ControlFlow::Continue(())
        });
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| compare_keys(&keys[a], &keys[b], descending));
        if order.iter().enumerate().all(|(i, &o)| i == o) {
            return false;
        }
        let perm: Vec<usize> = order.into_iter().map(|i| start + i).collect();
        drop(keys);
        self.record(
            sheet,
            Change::Sorted {
                start,
                perm: perm.clone(),
            },
            EditKind::Sort { start, col, perm },
        )
    }

    /// Reorder rows `start..start + perm.len()` so new row `start + i` is old
    /// row `perm[i]` (a permutation of that range), as one undo step.
    /// Like a sort, rows keep their clean state, so a moved row is saved
    /// byte for byte. `col` is only where undo puts the cursor.
    pub fn permute_rows(
        &mut self,
        sheet: usize,
        start: usize,
        perm: Vec<usize>,
        col: usize,
    ) -> bool {
        let rows = self.sheets[sheet].rows();
        let range = start..start + perm.len();
        if range.end > rows
            || !perm.iter().all(|p| range.contains(p))
            || perm.iter().enumerate().all(|(i, &p)| p == start + i)
        {
            return false;
        }
        self.record(
            sheet,
            Change::RowsMoved {
                start,
                perm: perm.clone(),
            },
            EditKind::Sort { start, col, perm },
        )
    }

    /// Move column `from` to index `to`, shifting the columns between.
    pub fn move_col(&mut self, sheet: usize, from: usize, to: usize) -> bool {
        let cols = self.sheets[sheet].cols();
        if from == to || from >= cols || to >= cols {
            return false;
        }
        self.record(
            sheet,
            Change::ColMoved { from, to },
            EditKind::MoveCol {
                from,
                to,
                dirty: Vec::new(),
            },
        )
    }

    /// Log `change` for the formula engine, apply a new edit, and push it as
    /// one undo step.
    fn record(&mut self, sheet: usize, change: Change, kind: EditKind) -> bool {
        self.changes.push(change);
        let mut edit = Edit { id: 0, sheet, kind };
        self.apply(&mut edit, true);
        self.push_edit(sheet, edit.kind);
        true
    }

    fn push_edit(&mut self, sheet: usize, kind: EditKind) {
        // Cell writes apply in place before getting here (not via `apply`).
        self.generation += 1;
        self.next_id += 1;
        self.undo.push(Edit {
            id: self.next_id,
            sheet,
            kind,
        });
        self.redo.clear();
    }

    /// Do (`forward`) or undo an edit, returning `(row, col)` for the cursor.
    fn apply(&mut self, edit: &mut Edit, forward: bool) -> (usize, usize) {
        self.generation += 1;
        let rows_before = self.sheets[edit.sheet].rows();
        let reorders = matches!(edit.kind, EditKind::Sort { .. });
        let pos = self.apply_inner(edit, forward);
        if reorders || self.sheets[edit.sheet].rows() != rows_before {
            self.layout_generation += 1;
        }
        pos
    }

    fn apply_inner(&mut self, edit: &mut Edit, forward: bool) -> (usize, usize) {
        let s = &mut self.sheets[edit.sheet];
        let pos = |row: usize, col: usize| (row, col);
        match &mut edit.kind {
            EditKind::SetCells {
                changes,
                old_meta,
                extent,
            } => {
                if forward {
                    for ch in changes.iter() {
                        s.grow_rows(ch.row + 1);
                        s.grow_cols(ch.col + 1);
                        *s.cell_mut(ch.row, ch.col) = ch.new;
                        s.meta_mut(ch.row).set_dirty(true);
                    }
                } else {
                    for ch in changes.iter().rev() {
                        *s.cell_mut(ch.row, ch.col) = ch.old;
                    }
                    for (row, meta) in old_meta.iter() {
                        *s.meta_mut(*row) = *meta;
                    }
                    s.truncate_rows(extent.0);
                    s.truncate_cols(extent.1);
                }
                pos(changes[0].row, changes[0].col)
            }
            EditKind::InsertRows { at, n } => {
                if forward {
                    s.insert_rows(*at, None, vec![RowMeta::default(); *n]);
                } else {
                    s.remove_rows(*at, *n);
                }
                pos(*at, 0)
            }
            EditKind::DeleteRows {
                at,
                n,
                cells,
                metas,
            } => {
                if forward {
                    (*cells, *metas) = s.remove_rows(*at, *n);
                } else {
                    s.insert_rows(*at, Some(std::mem::take(cells)), std::mem::take(metas));
                }
                pos(*at, 0)
            }
            EditKind::InsertCols {
                at,
                n,
                min_width,
                dirty,
            } => {
                if forward {
                    *dirty = s.dirty_bits();
                    *min_width = s.min_width;
                    let cols = (0..*n).map(|_| s.empty_column()).collect();
                    s.insert_cols(*at, cols, vec![ColType::Text; *n]);
                    if *at < s.min_width {
                        s.min_width += *n;
                    }
                    s.mark_all_dirty();
                } else {
                    s.remove_cols(*at, *n);
                    s.min_width = *min_width;
                    s.restore_dirty_bits(dirty);
                }
                pos(0, *at)
            }
            EditKind::DeleteCols {
                at,
                n,
                cols,
                types,
                min_width,
                dirty,
            } => {
                if forward {
                    *dirty = s.dirty_bits();
                    *min_width = s.min_width;
                    (*cols, *types) = s.remove_cols(*at, *n);
                    let overlap = (*at + *n).min(s.min_width).saturating_sub(*at);
                    s.min_width -= overlap;
                    s.mark_all_dirty();
                } else {
                    s.insert_cols(*at, std::mem::take(cols), std::mem::take(types));
                    s.min_width = *min_width;
                    s.restore_dirty_bits(dirty);
                }
                pos(0, *at)
            }
            EditKind::Sort { start, col, perm } => {
                if forward {
                    s.permute_rows(*start, perm);
                } else {
                    let mut inverse = vec![0; perm.len()];
                    for (i, &p) in perm.iter().enumerate() {
                        inverse[p - *start] = *start + i;
                    }
                    s.permute_rows(*start, &inverse);
                }
                pos(*start, *col)
            }
            EditKind::MoveCol { from, to, dirty } => {
                let (a, b) = if forward { (*from, *to) } else { (*to, *from) };
                if forward {
                    *dirty = s.dirty_bits();
                }
                let (cols, types) = s.remove_cols(a, 1);
                s.insert_cols(b, cols, types);
                if forward {
                    // Every record's field order changed.
                    s.mark_all_dirty();
                } else {
                    s.restore_dirty_bits(dirty);
                }
                pos(0, b)
            }
        }
    }

    /// Steps on the undo stack. An operation that changed nothing (setting a
    /// cell to its current value, inserting past the data, an already-sorted
    /// sort) pushes no step, so comparing depths tells whether one recorded.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// Undo the last edit; returns `(row, col)` of the cell it touched first
    /// (its sheet is the one the edit was made on).
    pub fn undo(&mut self) -> Option<(usize, usize)> {
        self.replay(false)
    }

    /// Redo the last undone edit; returns `(row, col)` of the cell it touched
    /// first.
    pub fn redo(&mut self) -> Option<(usize, usize)> {
        self.replay(true)
    }

    /// Undo (`forward` false) or redo one edit, moving it to the other stack.
    fn replay(&mut self, forward: bool) -> Option<(usize, usize)> {
        let mut edit = if forward {
            self.redo.pop()
        } else {
            self.undo.pop()
        }?;
        self.note_replay(&edit);
        let pos = self.apply(&mut edit, forward);
        if forward {
            &mut self.undo
        } else {
            &mut self.redo
        }
        .push(edit);
        Some(pos)
    }

    /// Log what replaying `edit` (undo or redo) changes.
    fn note_replay(&mut self, edit: &Edit) {
        let change = match &edit.kind {
            EditKind::SetCells { changes, .. } => {
                Change::Cells(changes.iter().map(|c| (c.row, c.col)).collect())
            }
            _ => Change::Reshaped,
        };
        self.changes.push(change);
    }

    #[allow(dead_code)] // model API kept for multi-sheet / `.xlsx`
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[allow(dead_code)] // model API kept for multi-sheet / `.xlsx`
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Whether the workbook differs from its last save (or load). Undoing or
    /// redoing back to that point makes it unmodified again.
    pub fn is_modified(&self) -> bool {
        self.undo.last().map_or(0, |e| e.id) != self.saved_id
    }

    /// Record the current state as saved.
    ///
    /// Rows stay dirty: the sheet source still holds the bytes as loaded, not
    /// as saved, so an edited row must keep being re-serialized.
    pub fn mark_saved(&mut self) {
        self.saved_id = self.undo.last().map_or(0, |e| e.id);
    }

    /// Rebuild the pool keeping only strings still referenced by a cell or by
    /// the undo/redo history, reclaiming those orphaned by edits.
    pub fn compact(&mut self) {
        let old = std::mem::take(&mut self.pool);
        let mut new = StringPool::new();
        let mut map = vec![u32::MAX; old.len()];
        let mut remap = |cell: &mut CellRef| {
            if let Some(id) = cell.interned_id() {
                let slot = &mut map[id as usize];
                if *slot == u32::MAX {
                    *slot = new.intern(old.get(id));
                }
                *cell = CellRef::interned(*slot);
            }
        };
        for sheet in &mut self.sheets {
            sheet.for_each_cell_mut(&mut remap);
        }
        for edit in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            edit.kind.for_each_cell_mut(&mut remap);
        }
        self.pool = new;
    }
}

#[cfg(test)]
mod tests {

    use super::super::parse::parse_now;
    use super::super::write::to_bytes;
    use super::*;

    fn load(s: &str) -> Workbook {
        parse_now(s.as_bytes().to_vec(), None).unwrap()
    }

    fn column(wb: &Workbook, col: usize) -> Vec<String> {
        let mut out = Vec::new();
        wb.for_each_in_column(0, col, 0..wb.used_rows(0), |_, s| out.push(s.to_string()));
        out
    }

    fn check(wb: &Workbook) {
        for s in wb.sheets() {
            s.check_layout();
        }
    }

    #[test]
    fn grid_set_cell_grow_and_undo() {
        let src = "a,b\n1,2\n";
        let mut wb = load(src);
        assert!(!wb.is_modified());
        assert_eq!(wb.cell(0, 100, 100), "");
        assert!(!wb.set_cell(0, 0, 0, "a"), "same value is not an edit");
        assert!(
            !wb.set_cell(0, 50, 50, ""),
            "clearing empty space is not an edit"
        );
        assert!(!wb.is_modified());

        assert!(wb.set_cell(0, 5, 3, "far away"));
        check(&wb);
        assert_eq!((wb.used_rows(0), wb.used_cols(0)), (6, 4));
        assert_eq!(wb.cell(0, 5, 3), "far away");
        assert!(wb.is_modified());

        assert_eq!(wb.undo(), Some((5, 3)));
        check(&wb);
        assert_eq!((wb.used_rows(0), wb.used_cols(0)), (2, 2));
        assert!(!wb.is_modified());
        assert_eq!(to_bytes(&wb), src.as_bytes());

        wb.redo();
        assert_eq!(wb.cell(0, 5, 3), "far away");
        assert!(wb.is_modified());
    }

    #[test]
    fn grid_empty_input_is_a_usable_workbook() {
        let mut wb = load("");
        assert_eq!((wb.used_rows(0), wb.used_cols(0)), (0, 0));
        assert!(wb.is_complete());
        assert!(wb.problems(0).is_empty());
        assert_eq!(to_bytes(&wb), b"");
        assert_eq!(wb.undo_depth(), 0);
        assert!(!wb.set_cell(0, 0, 0, ""));
        assert_eq!(wb.undo_depth(), 0, "a no-op edit pushes no step");
        assert!(!wb.sort_by_column(0, 0, false, false));
        assert!(wb.set_cell(0, 1, 1, "x"));
        assert_eq!(wb.undo_depth(), 1);
        assert_eq!(to_bytes(&wb), b",\n,x\n");
        wb.undo();
        assert_eq!(wb.undo_depth(), 0);
        assert_eq!(to_bytes(&wb), b"");
        assert!(!wb.is_modified());
        let prefix = super::super::parse::parse_prefix(b"", None, 200);
        assert_eq!(prefix.used_rows(0), 0);
    }

    #[test]
    fn grid_set_cells_is_one_undo_step() {
        let mut wb = load("a,b\n1,2\n3,4\n");
        let cells = vec![
            (1, 0, "x".to_string()),
            (1, 1, "a value longer than seven".to_string()),
            (2, 1, String::new()),
            (1, 0, "y".to_string()), // same cell twice: last wins, undo restores the first old
        ];
        assert!(wb.set_cells(0, &cells));
        assert_eq!(wb.cell(0, 1, 0), "y");
        assert_eq!(wb.cell(0, 1, 1), "a value longer than seven");
        assert_eq!(wb.cell(0, 2, 1), "");
        wb.undo();
        assert_eq!(column(&wb, 0), ["a", "1", "3"]);
        assert_eq!(column(&wb, 1), ["b", "2", "4"]);
        assert!(!wb.can_undo());
        assert!(!wb.is_modified());
    }

    #[test]
    fn grid_saved_point_tracking() {
        let mut wb = load("a\n");
        wb.set_cell(0, 0, 0, "b");
        wb.mark_saved();
        assert!(!wb.is_modified());
        wb.set_cell(0, 0, 0, "c");
        assert!(wb.is_modified());
        wb.undo();
        assert!(!wb.is_modified(), "undo back to the saved point");
        wb.undo();
        assert!(wb.is_modified(), "undo past the saved point");
        wb.redo();
        assert!(!wb.is_modified());
        // A new edit after undoing past the save makes it unreachable.
        wb.undo();
        wb.set_cell(0, 0, 0, "d");
        wb.undo();
        assert!(wb.is_modified());
    }

    #[test]
    fn grid_row_ops_and_undo_redo() {
        let src = "h\n0\n1\n2\n3\n";
        let mut wb = load(src);
        assert!(wb.insert_rows(0, 2, 3));
        check(&wb);
        assert_eq!(column(&wb, 0), ["h", "0", "", "", "", "1", "2", "3"]);
        assert!(
            !wb.insert_rows(0, 8, 1),
            "inserting past the data is a no-op"
        );
        assert!(wb.delete_rows(0, 1, 2));
        assert_eq!(column(&wb, 0), ["h", "", "", "1", "2", "3"]);
        assert!(wb.delete_rows(0, 4, 100), "clamped");
        assert_eq!(column(&wb, 0), ["h", "", "", "1"]);
        wb.undo();
        wb.undo();
        assert_eq!(column(&wb, 0), ["h", "0", "", "", "", "1", "2", "3"]);
        wb.undo();
        check(&wb);
        assert_eq!(to_bytes(&wb), src.as_bytes());
        assert!(!wb.is_modified());
        wb.redo();
        wb.redo();
        wb.redo();
        assert_eq!(column(&wb, 0), ["h", "", "", "1"]);
        check(&wb);
    }

    #[test]
    fn grid_col_ops_and_undo_redo() {
        let src = "a,b,c\n1,2,3\n";
        let mut wb = load(src);
        assert!(wb.insert_cols(0, 1, 2));
        check(&wb);
        assert_eq!(to_bytes(&wb), b"a,,,b,c\n1,,,2,3\n");
        assert!(wb.delete_cols(0, 0, 2));
        assert_eq!(to_bytes(&wb), b",b,c\n,2,3\n");
        assert!(!wb.delete_cols(0, 9, 1));
        wb.undo();
        assert_eq!(to_bytes(&wb), b"a,,,b,c\n1,,,2,3\n");
        wb.undo();
        check(&wb);
        assert_eq!(to_bytes(&wb), src.as_bytes());
        assert!(
            wb.sheet(0).row_meta(0).unwrap().is_clean(),
            "undo restores clean rows"
        );
        assert!(!wb.is_modified());
        wb.redo();
        wb.redo();
        assert_eq!(to_bytes(&wb), b",b,c\n,2,3\n");
    }

    #[test]
    fn grid_sort_keeps_rows_clean_and_undoes() {
        let src = "name,n\r\n\"b, quoted\",10\r\na,9\r\nc,\r\nd,100";
        let mut wb = load(src);
        assert!(wb.header(0));
        assert_eq!(wb.col_type(0, 1), ColType::Int);
        assert!(wb.sort_by_column(0, 1, false, true));
        check(&wb);
        assert_eq!(column(&wb, 0), ["name", "a", "b, quoted", "d", "c"]);
        let sheet = wb.sheet(0);
        assert!(
            (0..5).all(|r| sheet.row_meta(r).unwrap().is_clean()),
            "sorting dirties nothing"
        );
        // Raw rows in the new order; the last row's missing newline stays last.
        assert_eq!(
            to_bytes(&wb),
            b"name,n\r\na,9\r\n\"b, quoted\",10\r\nd,100\r\nc,"
        );
        wb.undo();
        assert_eq!(to_bytes(&wb), src.as_bytes());
        assert!(!wb.is_modified());

        // Descending: numbers reversed, empties still last.
        wb.sort_by_column(0, 1, true, true);
        assert_eq!(column(&wb, 1), ["n", "100", "10", "9", ""]);
        // Text sort and the id short-circuit; stable for equal keys.
        let mut wb = load("k,v\nlong value b,1\nlong value a,2\nlong value b,3\n");
        wb.sort_by_column(0, 0, false, true);
        assert_eq!(column(&wb, 1), ["v", "2", "1", "3"]);
        // Already sorted: no edit.
        assert!(!wb.sort_by_column(0, 0, false, true));
    }

    #[test]
    fn grid_sort_kinds_moves_and_undo() {
        // Numeric over a text column, and dates in mixed formats.
        let mut wb = load("v,d\n10,3/1/2024\n9,2024-02-15 08:00\nx,2024-02-15\n100,1.1.2023\n");
        assert!(wb.sort_by_column_as(0, 0, false, true, SortKind::Numeric));
        assert_eq!(column(&wb, 0), ["v", "9", "10", "100", "x"]);
        assert!(wb.sort_by_column_as(0, 1, false, true, SortKind::Time));
        assert_eq!(
            column(&wb, 1),
            [
                "d",
                "1.1.2023",
                "2024-02-15",
                "2024-02-15 08:00",
                "3/1/2024"
            ]
        );
        assert!(wb.sort_by_column_as(0, 0, false, true, SortKind::Alpha));
        assert_eq!(column(&wb, 0), ["v", "10", "100", "9", "x"]);

        // Moving rows is a permutation: rows stay clean, saves byte-exact.
        let src = "a,1\nb,2\nc,3\n";
        let mut wb = load(src);
        assert!(wb.permute_rows(0, 0, vec![1, 0, 2], 0));
        assert_eq!(to_bytes(&wb), b"b,2\na,1\nc,3\n");
        assert!(
            !wb.permute_rows(0, 0, vec![0, 1, 2], 0),
            "identity: no edit"
        );
        wb.undo();
        // A window in the middle (moving one row down).
        assert!(wb.permute_rows(0, 1, vec![2, 1], 0));
        assert_eq!(to_bytes(&wb), b"a,1\nc,3\nb,2\n");
        wb.undo();
        assert_eq!(to_bytes(&wb), src.as_bytes());

        // Moving a column rewrites the records and undoes cleanly.
        assert!(wb.move_col(0, 1, 0));
        check(&wb);
        assert_eq!(to_bytes(&wb), b"1,a\n2,b\n3,c\n");
        wb.undo();
        assert_eq!(to_bytes(&wb), src.as_bytes());
        assert!(!wb.is_modified());
        wb.redo();
        assert_eq!(column(&wb, 0), ["1", "2", "3"]);
        assert!(!wb.move_col(0, 0, 5), "out of range");
        assert_eq!(time_key("2024-03-01T14:30:05"), Some(20240301.143005));
        assert_eq!(time_key("Tuesday"), None);
    }

    #[test]
    fn grid_header_and_inference() {
        let mut wb =
            load("id,price,ok,when,name\n1,2.5,true,2024-01-02,x\n2,3,false,2024-02-03,y\n");
        assert!(wb.header(0));
        let types: Vec<ColType> = (0..5).map(|c| wb.col_type(0, c)).collect();
        assert_eq!(
            types,
            [
                ColType::Int,
                ColType::Float,
                ColType::Bool,
                ColType::Date,
                ColType::Text
            ]
        );
        wb.set_header(0, false);
        assert_eq!(wb.col_type(0, 0), ColType::Text, "header text now counts");
        assert!(!wb.is_modified());
    }

    #[test]
    fn grid_compact_drops_orphans() {
        let mut wb = load("a\n");
        for i in 0..100 {
            wb.set_cell(0, 0, 0, &format!("long orphan value {i}"));
        }
        wb.set_cell(0, 1, 0, "kept long value");
        wb.compact();
        // Undo history still references the old values, so nothing is lost.
        assert_eq!(wb.pool().len(), 101);
        wb.undo();
        assert_eq!(wb.cell(0, 0, 0), "long orphan value 99");
        // Drop the history by making a fresh workbook state, then compact.
        let mut wb2 = load("a\n");
        wb2.set_cell(0, 0, 0, "long orphan value 1");
        wb2.set_cell(0, 0, 0, "long orphan value 2");
        wb2.undo();
        wb2.undo();
        wb2.set_cell(0, 0, 0, "final long value");
        wb2.compact();
        assert_eq!(wb2.pool().len(), 1);
        assert_eq!(wb2.cell(0, 0, 0), "final long value");
    }
}
