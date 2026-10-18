//! The formula engine for one sheet: which cells hold formulas, what each one
//! reads, their results, and recalculation.
//!
//! **Formula cells.** A cell is a formula when its stored text starts with
//! [`MARK`] followed by `=`. The marker is a control character that never
//! comes from a CSV (CSV can't store formulas), so a loaded `=…` is plain
//! text, while the formula-ness of a typed formula travels with the cell
//! through undo, sort, copy, and row moves for free. [`display_text`] strips
//! it for anything shown to the user.
//!
//! **Dependencies.** Each formula's precedents are the rectangles its
//! references cover. A reverse index answers "which formulas read cell
//! (r, c)?": a hash map for single cells, per-column interval lists for
//! ranges up to [`WIDE`] columns wide, and a short list for wider ones
//! (`1:1`, `A1:ZZ9`).
//!
//! **Recalculation.** An edit marks the formulas downstream of the changed
//! cells dirty (transitively). Recalculation works through the dirty set
//! with an explicit stack: evaluating a formula that reads a still-dirty
//! formula records it as *pending*, re-queues itself behind it, and is
//! evaluated again once its inputs are done — so each formula runs about
//! twice at most, chains of any length use no recursion, and `SUM` over a
//! million dirty cells is one scan. Reading a formula that is itself waiting
//! on the reader is a cycle: it becomes `#CIRC!`, which then propagates.
//! A time budget lets a large recalculation continue across frames.
//!
//! **Spilling.** A formula whose result is an array (`=SEQUENCE(3)`,
//! `=SORT(A1:A9)`, `=A1:B3*2`) is a dynamic-array formula: its values fill
//! the cells down and right of it, which hold no text of their own. A spill
//! range covering any non-empty cell (or another formula's spill) doesn't
//! spill: the formula shows `#SPILL!`. Each formula's wanted spill range is
//! indexed like a precedent, so typing into it or clearing a blocker
//! recalculates the formula; a formula reading a spilled cell depends on the
//! formula that spills there, and a spill that changes shape marks the
//! readers of every cell it covered or now covers. `A1#` reads the range the
//! formula in `A1` spills into.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use super::ast::Expr;
use super::eval::{self, Outcome};
use super::func::{Arg, Array, Ctx, RangeRef};
use super::parse::{ParseError, parse};
use super::value::{ErrorKind, Value};

/// Prefix marking a stored cell text as a formula (followed by `=`).
pub const MARK: char = '\u{1}';

/// Ranges wider than this many columns go in the short "wide" list instead
/// of one interval entry per column.
const WIDE: usize = 64;

/// How many times one formula's spill range may change shape in one
/// recalculation before it's taken for a cycle (a spill feeding the
/// formula that makes it, so it never settles) and shows `#CIRC!`.
const MAX_RESPILLS: u32 = 32;

/// Whether stored cell text is a formula.
pub fn is_formula(stored: &str) -> bool {
    stored.starts_with(MARK) && stored[MARK.len_utf8()..].starts_with('=')
}

/// The text to show or edit for a stored cell: a formula without its
/// marker (`=SUM(A1:A3)`), anything else as-is.
pub fn display_text(stored: &str) -> &str {
    if is_formula(stored) {
        &stored[MARK.len_utf8()..]
    } else {
        stored
    }
}

/// The text that, typed into a cell, recreates `stored`: a formula as
/// `=…`; text that itself starts with `=` behind Excel's `'` escape (so it
/// stays text, not a formula, when it's edited, copied or transformed);
/// anything else as stored.
pub fn input_text(stored: &str) -> std::borrow::Cow<'_, str> {
    if is_formula(stored) {
        std::borrow::Cow::Borrowed(display_text(stored))
    } else if stored.len() > 1 && stored.starts_with('=') {
        std::borrow::Cow::Owned(format!("'{stored}"))
    } else {
        std::borrow::Cow::Borrowed(stored)
    }
}

/// Stored form of formula text `=…` (adds the marker).
pub fn store_formula(text: &str) -> String {
    format!("{MARK}{text}")
}

/// What the engine reads from the sheet. The grid implements it over the
/// workbook; tests implement it over a map.
pub trait Source {
    /// The stored text of a cell ("" outside the used area).
    fn text(&self, row: usize, col: usize) -> &str;
    /// `(rows, cols)` of the used area.
    fn extent(&self) -> (usize, usize);
    /// Visit column `col`'s stored text over `rows`, in order.
    fn for_each_in_column(
        &self,
        col: usize,
        rows: std::ops::Range<usize>,
        f: &mut dyn FnMut(usize, &str),
    );
    /// The sheet's own name (a `Name!A1` reference to it is a local one).
    fn sheet_name(&self) -> &str {
        "Sheet1"
    }
    /// Another sheet of the same workbook, by name (any case): the values a
    /// `Costs!B2` reference reads. `None` makes such a reference `#REF!`.
    fn other_sheet(&self, _name: &str) -> Option<&SheetValues> {
        None
    }
    /// Excel serial "now", for `NOW()`/`TODAY()`.
    fn now(&self) -> f64 {
        now_serial()
    }
}

/// The current local-ish time as an Excel 1900-system serial (UTC; time
/// zones would need the OS, and v1 keeps `std` only).
pub fn now_serial() -> f64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    // 1970-01-01 is serial 25569 in the 1900 system.
    25569.0 + secs / 86400.0
}

/// Another sheet's cell values, as formulas on this sheet see them (for a
/// formula cell there, its result). Stored by column.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SheetValues {
    cols: Vec<Vec<Value>>,
    rows: usize,
}

impl SheetValues {
    /// From `(row, col, value)` triples, any order; empty values may be left out.
    pub fn from_cells(cells: impl IntoIterator<Item = (usize, usize, Value)>) -> Self {
        let mut out = SheetValues::default();
        for (r, c, v) in cells {
            if matches!(v, Value::Empty) {
                continue;
            }
            if out.cols.len() <= c {
                out.cols.resize_with(c + 1, Vec::new);
            }
            let col = &mut out.cols[c];
            if col.len() <= r {
                col.resize(r + 1, Value::Empty);
            }
            col[r] = v;
            out.rows = out.rows.max(r + 1);
        }
        out
    }

    pub fn get(&self, row: usize, col: usize) -> Value {
        self.cols
            .get(col)
            .and_then(|c| c.get(row))
            .cloned()
            .unwrap_or(Value::Empty)
    }

    /// `(rows, cols)` of the used area.
    pub fn extent(&self) -> (usize, usize) {
        (self.rows, self.cols.len())
    }
}

type Fid = usize;

/// Cells `(r0, c0, r1, c1)`, inclusive.
type Rect = (usize, usize, usize, usize);

/// A formula's array result, spilled from its cell.
struct Spill {
    rows: usize,
    cols: usize,
    /// Row-major; the first is the formula's own cell.
    values: Vec<Value>,
    /// Each value as shown (kept so reads can borrow a `&str`).
    shown: Vec<String>,
}

impl Spill {
    fn new(a: Array) -> Spill {
        Spill {
            rows: a.rows,
            cols: a.cols,
            shown: a.data.iter().map(Value::to_string).collect(),
            values: a.data,
        }
    }

    /// The cells it covers when spilled from `(row, col)`.
    fn rect(&self, (row, col): (usize, usize)) -> Rect {
        (row, col, row + self.rows - 1, col + self.cols - 1)
    }

    /// Index of `(row, col)` in [`Spill::values`], for a spill from `at`.
    fn index(&self, at: (usize, usize), row: usize, col: usize) -> usize {
        (row - at.0) * self.cols + (col - at.1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Clean,
    Dirty,
    /// Re-queued behind inputs it found still dirty.
    Waiting,
    /// Being evaluated right now (reading it again is a cycle).
    Evaluating,
}

struct Slot {
    pos: (usize, usize),
    /// Shared, so evaluating doesn't copy the tree.
    expr: Result<Arc<Expr>, ParseError>,
    result: Value,
    /// `result` as shown in the cell (kept so reads can borrow a `&str`).
    shown: String,
    state: State,
    volatile: bool,
    /// Rectangles read: `(r0, c0, r1, c1)` inclusive; open ends are MAX.
    precedents: Vec<(usize, usize, usize, usize)>,
    /// Other sheets read (lower-case names): it recalculates when one
    /// changes ([`Engine::sheet_changed`]).
    sheets: Vec<String>,
    /// Its array result, when it spilled.
    spill: Option<Spill>,
    /// The range its last array result wanted to spill into, spilled or
    /// blocked (`None` for one value).
    wants: Option<Rect>,
    /// [`Slot::wants`] without the formula's own cell: indexed like
    /// precedents, so an edit in the range recalculates it.
    spill_deps: Vec<Rect>,
}

impl Slot {
    /// Done: `value` is the result.
    fn settle(&mut self, value: Value) {
        self.shown = value.to_string();
        self.result = value;
        self.state = State::Clean;
    }
}

/// A Scheme function call waiting to be run (see [`Engine::take_requests`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalCall {
    pub name: String,
    /// Each argument flattened: a value, or a range as rows of values.
    pub args: Vec<ExternalArg>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExternalArg {
    Value(Value),
    Rows(Vec<Vec<Value>>),
}

/// Where a Scheme call stands.
#[derive(Clone, Debug, PartialEq)]
enum CallState {
    Queued,
    Running,
    Done(Value),
}

#[derive(Default)]
pub struct Engine {
    slots: Vec<Option<Slot>>,
    free: Vec<Fid>,
    by_pos: HashMap<(usize, usize), Fid>,
    cell_deps: HashMap<(usize, usize), Vec<Fid>>,
    /// Per column: `(r0, r1, fid)` ranges read in that column.
    col_deps: HashMap<usize, Vec<(usize, usize, Fid)>>,
    wide_deps: Vec<(usize, usize, usize, usize, Fid)>,
    /// Work list for the next/ongoing recalculation.
    stack: Vec<Fid>,
    rng: u64,
    /// Names of Scheme functions (upper-case).
    externals: HashSet<String>,
    /// Memoized Scheme calls for the current inputs.
    calls: HashMap<ExternalCall, CallState>,
    /// Calls queued since the last [`Engine::take_requests`].
    requests: Vec<ExternalCall>,
    /// Formulas showing `#CALC!` because a call they made isn't done.
    waiting_on_calls: HashSet<Fid>,
    /// Exact-match lookup indexes by range, dropped whenever any cell
    /// changes (so `VLOOKUP` down a column builds each index once).
    lookups: HashMap<RangeRef, Arc<super::func::lookup::LookupIndex>>,
    /// The sheet this engine computes (references naming it are local).
    own_sheet: String,
    /// Formula results computed so far (a change counter for results).
    evaluations: u64,
    /// Per column: `(r0, r1, fid)` cells covered by a formula's spill (its
    /// own cell included).
    spill_cols: HashMap<usize, Vec<(usize, usize, Fid)>>,
    /// `(rows, cols)` up to the furthest spilled cell.
    spill_extent: (usize, usize),
    /// Spill-range changes per formula in this recalculation (see
    /// [`MAX_RESPILLS`]).
    respills: HashMap<Fid, u32>,
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            rng: 0x9e37_79b9_7f4a_7c15,
            ..Default::default()
        }
    }

    pub fn formula_count(&self) -> usize {
        self.by_pos.len()
    }

    /// The result shown by the formula at `(row, col)`, if it is one.
    pub fn result(&self, row: usize, col: usize) -> Option<&Value> {
        let fid = *self.by_pos.get(&(row, col))?;
        self.slot(fid).map(|s| &s.result)
    }

    /// The result of the formula at `(row, col)` as the cell shows it.
    pub fn shown(&self, row: usize, col: usize) -> Option<&str> {
        let fid = *self.by_pos.get(&(row, col))?;
        self.slot(fid).map(|s| s.shown.as_str())
    }

    /// The parse error of the formula at `(row, col)`, if it didn't parse.
    pub fn parse_error(&self, row: usize, col: usize) -> Option<&ParseError> {
        let fid = *self.by_pos.get(&(row, col))?;
        self.slot(fid)?.expr.as_ref().err()
    }

    /// The parsed formula at `(row, col)`, if it parsed.
    pub fn expr(&self, row: usize, col: usize) -> Option<&Expr> {
        let fid = *self.by_pos.get(&(row, col))?;
        self.slot(fid)?.expr.as_deref().ok()
    }

    /// Positions of every formula (unordered).
    pub fn positions(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.by_pos.keys().copied()
    }

    // ---- spills -------------------------------------------------------------

    /// Whether any formula spills.
    pub fn has_spills(&self) -> bool {
        !self.spill_cols.is_empty()
    }

    /// `(rows, cols)` up to the furthest spilled cell (the used area
    /// spilled values extend the sheet's to).
    pub fn spill_extent(&self) -> (usize, usize) {
        self.spill_extent
    }

    /// The formula whose spill covers `(row, col)`, that cell included.
    fn spill_owner(&self, row: usize, col: usize) -> Option<Fid> {
        self.spill_cols
            .get(&col)?
            .iter()
            .find(|e| (e.0..=e.1).contains(&row))
            .map(|e| e.2)
    }

    /// The cells formula `fid` spills into, if it spills.
    fn spill_rect(&self, fid: Fid) -> Option<Rect> {
        let s = self.slot(fid)?;
        s.spill.as_ref().map(|sp| sp.rect(s.pos))
    }

    /// For a cell another formula spills into (not that formula's own
    /// cell): that formula's position.
    pub fn spilled_from(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let pos = self.slot(self.spill_owner(row, col)?)?.pos;
        (pos != (row, col)).then_some(pos)
    }

    /// The range `(r0, c0, r1, c1)` the formula at `(row, col)` spills
    /// into, if it spills.
    pub fn spill_range(&self, row: usize, col: usize) -> Option<(usize, usize, usize, usize)> {
        self.spill_rect(*self.by_pos.get(&(row, col))?)
    }

    /// The value spilled into `(row, col)` from another formula's cell.
    pub fn spilled_value(&self, row: usize, col: usize) -> Option<&Value> {
        let s = self.slot(self.spill_owner(row, col)?)?;
        let sp = s.spill.as_ref()?;
        (s.pos != (row, col)).then(|| &sp.values[sp.index(s.pos, row, col)])
    }

    /// [`Engine::spilled_value`] as the cell shows it.
    pub fn spilled_shown(&self, row: usize, col: usize) -> Option<&str> {
        let s = self.slot(self.spill_owner(row, col)?)?;
        let sp = s.spill.as_ref()?;
        (s.pos != (row, col)).then(|| sp.shown[sp.index(s.pos, row, col)].as_str())
    }

    /// Record that `fid`'s spill moved from `old` to `new`.
    fn move_spill(&mut self, fid: Fid, old: Option<Rect>, new: Option<Rect>) {
        if let Some((_, c0, _, c1)) = old {
            for c in c0..=c1 {
                if let Some(v) = self.spill_cols.get_mut(&c) {
                    v.retain(|e| e.2 != fid);
                    if v.is_empty() {
                        self.spill_cols.remove(&c);
                    }
                }
            }
        }
        if let Some((r0, c0, r1, c1)) = new {
            for c in c0..=c1 {
                self.spill_cols.entry(c).or_default().push((r0, r1, fid));
            }
        }
        let (rows, cols) = self.spill_extent;
        self.spill_extent = match old {
            // The furthest spill shrank or went: measure again.
            Some((_, _, r1, c1)) if r1 + 1 >= rows || c1 + 1 >= cols => self
                .spill_cols
                .iter()
                .fold((0, 0), |(rows, cols), (&c, v)| {
                    let bottom = v.iter().map(|e| e.1 + 1).max().unwrap_or(0);
                    (rows.max(bottom), cols.max(c + 1))
                }),
            _ => (rows, cols),
        };
        if let Some((_, _, r1, c1)) = new {
            self.spill_extent.0 = self.spill_extent.0.max(r1 + 1);
            self.spill_extent.1 = self.spill_extent.1.max(c1 + 1);
        }
    }

    /// Whether spilling from `fid`'s cell over `rect` is blocked: a
    /// non-empty cell in it (other than the formula's own), or another
    /// formula's spill.
    fn spill_blocked(&self, src: &dyn Source, fid: Fid, rect: Rect) -> bool {
        let Some(at) = self.slot(fid).map(|s| s.pos) else {
            return true;
        };
        let (r0, c0, r1, c1) = rect;
        for c in c0..=c1 {
            let mut hit = false;
            src.for_each_in_column(c, r0..r1 + 1, &mut |r, text| {
                hit |= !text.is_empty() && (r, c) != at;
            });
            let spilled = self
                .spill_cols
                .get(&c)
                .is_some_and(|v| v.iter().any(|&(a, b, f)| f != fid && a <= r1 && r0 <= b));
            if hit || spilled {
                return true;
            }
        }
        false
    }

    /// Whether formula `fid`'s references cover a cell of `rect` (where it
    /// would spill) other than its own: a cycle, as in Excel.
    fn reads_own_spill(&self, fid: Fid, rect: Rect) -> bool {
        let Some(s) = self.slot(fid) else {
            return false;
        };
        let (r0, c0, r1, c1) = rect;
        s.precedents.iter().any(|&(a0, b0, a1, b1)| {
            let (x0, y0, x1, y1) = (a0.max(r0), b0.max(c0), a1.min(r1), b1.min(c1));
            // The overlap, if any, is more than just the formula's own cell.
            x0 <= x1 && y0 <= y1 && (x0, y0, x1, y1) != (r0, c0, r0, c0)
        })
    }

    /// Settle formula `fid` (evaluated to `value`, or to `array` when its
    /// result is one): spill the array if its range is free, and if the
    /// spill changed shape, recalculate whatever reads the cells involved.
    fn finish(&mut self, src: &dyn Source, fid: Fid, value: Value, array: Option<Array>) {
        let Some(at) = self.slot(fid).map(|s| s.pos) else {
            return;
        };
        let old = self.spill_rect(fid);
        let mut value = value;
        let mut spill = None;
        let mut wants = None;
        if let Some(a) = array {
            let end = (
                at.0.checked_add(a.rows - 1)
                    .filter(|&r| r <= super::parse::MAX_ROW),
                at.1.checked_add(a.cols - 1)
                    .filter(|&c| c <= super::parse::MAX_COL),
            );
            value = Value::Error(ErrorKind::Spill);
            // Off the sheet's edge it can never spill; otherwise it waits
            // for its range to clear.
            if let (Some(r1), Some(c1)) = end {
                let rect = (at.0, at.1, r1, c1);
                wants = Some(rect);
                if self.reads_own_spill(fid, rect) {
                    // Its result would change what it reads.
                    (value, wants) = (Value::Error(ErrorKind::Circ), None);
                } else if !self.spill_blocked(src, fid, rect) {
                    value = a.data[0].clone();
                    spill = Some(Spill::new(a));
                }
            }
        }
        let mut new = spill.as_ref().map(|sp| sp.rect(at));
        if new != old {
            let n = self.respills.entry(fid).or_insert(0);
            *n += 1;
            if *n > MAX_RESPILLS {
                (value, spill, wants, new) = (Value::Error(ErrorKind::Circ), None, None, None);
            }
        }
        if self.slot(fid).is_some_and(|s| s.wants != wants) {
            self.unindex(fid);
            if let Some(s) = self.slot_mut(fid) {
                s.wants = wants;
                s.spill_deps = wants.map_or_else(Vec::new, |(r0, c0, r1, c1)| {
                    let mut deps = Vec::new();
                    if c1 > c0 {
                        deps.push((r0, c0 + 1, r0, c1));
                    }
                    if r1 > r0 {
                        deps.push((r0 + 1, c0, r1, c1));
                    }
                    deps
                });
            }
            self.index(fid);
        }
        if let Some(s) = self.slot_mut(fid) {
            s.settle(value);
            s.spill = spill;
        }
        if new != old {
            self.move_spill(fid, old, new);
            self.lookups.clear();
            // New work on a recalculation under way (see `settle_waits`).
            self.settle_waits();
            for rect in old.into_iter().chain(new) {
                self.mark_area(rect, Some(fid));
            }
        }
    }

    /// Whether a recalculation still has work queued.
    pub fn busy(&self) -> bool {
        !self.stack.is_empty()
    }

    fn slot(&self, fid: Fid) -> Option<&Slot> {
        self.slots.get(fid).and_then(Option::as_ref)
    }

    fn slot_mut(&mut self, fid: Fid) -> Option<&mut Slot> {
        self.slots.get_mut(fid).and_then(Option::as_mut)
    }

    /// Forget everything and register every formula cell of `src` (a load,
    /// or after a structural edit). Every formula is queued.
    pub fn rebuild(&mut self, src: &dyn Source) {
        let externals = std::mem::take(&mut self.externals);
        let calls = std::mem::take(&mut self.calls);
        *self = Engine::new();
        self.externals = externals;
        self.calls = calls;
        self.own_sheet = src.sheet_name().to_string();
        let (rows, cols) = src.extent();
        let mut found = Vec::new();
        for col in 0..cols {
            src.for_each_in_column(col, 0..rows, &mut |row, text| {
                if is_formula(text) {
                    found.push((row, col, text.to_string()));
                }
            });
        }
        for (row, col, text) in found {
            self.register(row, col, &text);
        }
    }

    /// Cells changed: re-read each one (registering, re-parsing, or
    /// unregistering its formula) and mark everything downstream dirty.
    pub fn cells_changed(&mut self, src: &dyn Source, cells: &[(usize, usize)]) {
        self.settle_waits();
        if !cells.is_empty() {
            self.lookups.clear();
        }
        if !self.own_sheet.eq_ignore_ascii_case(src.sheet_name()) {
            self.own_sheet = src.sheet_name().to_string();
        }
        for &(row, col) in cells {
            // A formula that goes takes its spill with it: what read the
            // spilled cells (or waited for them to clear) recalculates.
            let mut freed = None;
            if let Some(fid) = self.by_pos.get(&(row, col)).copied() {
                freed = self.spill_rect(fid);
                self.unregister(fid);
            }
            let text = src.text(row, col);
            if is_formula(text) {
                let text = text.to_string();
                self.register(row, col, &text);
            }
            self.mark_dependents(row, col);
            if let Some(rect) = freed {
                self.mark_area(rect, None);
            }
        }
        self.mark_volatile();
    }

    fn register(&mut self, row: usize, col: usize, stored: &str) {
        let expr = parse(display_text(stored)).map(Arc::new);
        let mut precedents = Vec::new();
        let mut sheets: Vec<String> = Vec::new();
        let mut volatile = false;
        if let Ok(e) = &expr {
            let mut refs = Vec::new();
            e.refs(&mut refs);
            for r in refs {
                match r.sheet.as_deref() {
                    Some(s) if !s.eq_ignore_ascii_case(&self.own_sheet) => {
                        let s = s.to_lowercase();
                        if !sheets.contains(&s) {
                            sheets.push(s);
                        }
                    }
                    _ => precedents.push(r.area.bounds()),
                }
            }
            volatile = eval::is_volatile(e) || self.calls_external(e);
        }
        let slot = Slot {
            pos: (row, col),
            expr,
            result: Value::Error(ErrorKind::Calc),
            shown: ErrorKind::Calc.as_str().to_string(),
            state: State::Dirty,
            volatile,
            precedents,
            sheets,
            spill: None,
            wants: None,
            spill_deps: Vec::new(),
        };
        let fid = match self.free.pop() {
            Some(f) => {
                self.slots[f] = Some(slot);
                f
            }
            None => {
                self.slots.push(Some(slot));
                self.slots.len() - 1
            }
        };
        self.by_pos.insert((row, col), fid);
        self.index(fid);
        self.stack.push(fid);
    }

    fn calls_external(&self, e: &Expr) -> bool {
        if self.externals.is_empty() {
            return false;
        }
        let mut names = Vec::new();
        e.calls(&mut names);
        names.iter().any(|n| self.externals.contains(&**n))
    }

    fn unregister(&mut self, fid: Fid) {
        self.unindex(fid);
        let spilled = self.spill_rect(fid);
        if spilled.is_some() {
            self.move_spill(fid, spilled, None);
        }
        if let Some(slot) = self.slots[fid].take() {
            self.by_pos.remove(&slot.pos);
        }
        self.waiting_on_calls.remove(&fid);
        self.free.push(fid);
    }

    /// What formula `fid` is indexed as reading: its references and its
    /// wanted spill range.
    fn read_rects(&self, fid: Fid) -> Vec<Rect> {
        self.slot(fid)
            .map(|s| s.precedents.iter().chain(&s.spill_deps).copied().collect())
            .unwrap_or_default()
    }

    fn index(&mut self, fid: Fid) {
        let precedents = self.read_rects(fid);
        for (r0, c0, r1, c1) in precedents {
            if r0 == r1 && c0 == c1 {
                self.cell_deps.entry((r0, c0)).or_default().push(fid);
            } else if c1 != usize::MAX && c1 - c0 < WIDE {
                for c in c0..=c1 {
                    self.col_deps.entry(c).or_default().push((r0, r1, fid));
                }
            } else {
                self.wide_deps.push((r0, c0, r1, c1, fid));
            }
        }
    }

    fn unindex(&mut self, fid: Fid) {
        let precedents = self.read_rects(fid);
        for (r0, c0, r1, c1) in precedents {
            if r0 == r1 && c0 == c1 {
                if let Some(v) = self.cell_deps.get_mut(&(r0, c0)) {
                    v.retain(|&f| f != fid);
                    if v.is_empty() {
                        self.cell_deps.remove(&(r0, c0));
                    }
                }
            } else if c1 != usize::MAX && c1 - c0 < WIDE {
                for c in c0..=c1 {
                    if let Some(v) = self.col_deps.get_mut(&c) {
                        v.retain(|e| e.2 != fid);
                    }
                }
            } else {
                self.wide_deps.retain(|e| e.4 != fid);
            }
        }
    }

    /// Positions of the formulas that read cell `(row, col)` directly.
    pub fn dependents(&self, row: usize, col: usize) -> Vec<(usize, usize)> {
        let mut fids = Vec::new();
        self.dependents_of(row, col, &mut fids);
        fids.sort_unstable();
        fids.dedup();
        fids.iter()
            .filter_map(|&f| self.slot(f).map(|s| s.pos))
            .collect()
    }

    /// Formulas reading cell `(row, col)`.
    fn dependents_of(&self, row: usize, col: usize, out: &mut Vec<Fid>) {
        if let Some(v) = self.cell_deps.get(&(row, col)) {
            out.extend(v);
        }
        if let Some(v) = self.col_deps.get(&col) {
            out.extend(v.iter().filter(|e| (e.0..=e.1).contains(&row)).map(|e| e.2));
        }
        out.extend(
            self.wide_deps
                .iter()
                .filter(|e| (e.0..=e.2).contains(&row) && (e.1..=e.3).contains(&col))
                .map(|e| e.4),
        );
    }

    /// Formulas reading any cell of `rect`.
    fn dependents_in(&self, rect: Rect, out: &mut Vec<Fid>) {
        let (r0, c0, r1, c1) = rect;
        if r0 == r1 && c0 == c1 {
            return self.dependents_of(r0, c0, out);
        }
        // Walk whichever is smaller: the rectangle or the index.
        let cells = (r1 - r0 + 1).saturating_mul(c1 - c0 + 1);
        if cells <= self.cell_deps.len() {
            for r in r0..=r1 {
                for c in c0..=c1 {
                    if let Some(v) = self.cell_deps.get(&(r, c)) {
                        out.extend(v);
                    }
                }
            }
        } else {
            for (&(r, c), v) in &self.cell_deps {
                if (r0..=r1).contains(&r) && (c0..=c1).contains(&c) {
                    out.extend(v);
                }
            }
        }
        let overlaps = |e: &&(usize, usize, Fid)| e.0 <= r1 && r0 <= e.1;
        if c1 - c0 < self.col_deps.len() {
            for c in c0..=c1 {
                if let Some(v) = self.col_deps.get(&c) {
                    out.extend(v.iter().filter(overlaps).map(|e| e.2));
                }
            }
        } else {
            for (c, v) in &self.col_deps {
                if (c0..=c1).contains(c) {
                    out.extend(v.iter().filter(overlaps).map(|e| e.2));
                }
            }
        }
        out.extend(
            self.wide_deps
                .iter()
                .filter(|e| e.0 <= r1 && r0 <= e.2 && e.1 <= c1 && c0 <= e.3)
                .map(|e| e.4),
        );
    }

    /// Mark everything downstream of cell `(row, col)` dirty.
    fn mark_dependents(&mut self, row: usize, col: usize) {
        self.mark_area((row, col, row, col), None);
    }

    /// Mark everything downstream of the cells of `rect` dirty (but not
    /// `skip`). Downstream of a formula is downstream of its cell and of
    /// every cell it spills into.
    fn mark_area(&mut self, rect: Rect, skip: Option<Fid>) {
        let mut frontier = vec![rect];
        let mut seen: HashSet<Fid> = skip.into_iter().collect();
        let mut deps = Vec::new();
        while let Some(area) = frontier.pop() {
            deps.clear();
            self.dependents_in(area, &mut deps);
            for &fid in &deps {
                if !seen.insert(fid) {
                    continue;
                }
                let spilled = self.spill_rect(fid);
                if let Some(slot) = self.slot_mut(fid) {
                    if slot.state == State::Clean {
                        slot.state = State::Dirty;
                    }
                    let (r, c) = slot.pos;
                    self.stack.push(fid);
                    frontier.push((r, c, r, c));
                    frontier.extend(spilled);
                }
            }
        }
    }

    /// Queue every volatile formula (and its dependents): they recalculate
    /// on every change.
    fn mark_volatile(&mut self) {
        for fid in self.fids_where(|s| s.volatile) {
            self.requeue(fid);
        }
    }

    /// The formulas passing `test`.
    fn fids_where(&self, test: impl Fn(&Slot) -> bool) -> Vec<Fid> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().filter(|s| test(s)).map(|_| i))
            .collect()
    }

    /// Queue formula `fid` and everything downstream of it.
    fn requeue(&mut self, fid: Fid) {
        let spilled = self.spill_rect(fid);
        if let Some(s) = self.slot_mut(fid) {
            s.state = State::Dirty;
            let pos = s.pos;
            self.stack.push(fid);
            self.mark_dependents(pos.0, pos.1);
            if let Some(rect) = spilled {
                self.mark_area(rect, Some(fid));
            }
        }
    }

    /// New work is about to go on a recalculation that may be part done.
    /// A `Waiting` formula is re-queued behind the inputs it found dirty, and
    /// a formula that reads one while it waits is taken for a cycle — which
    /// only holds while the stack is exactly as that wait left it. So turn
    /// every wait back into plain dirty work first; it re-queues itself
    /// behind its inputs again if they're still dirty.
    fn settle_waits(&mut self) {
        // A waiting formula is always on the stack.
        if self.stack.is_empty() {
            return;
        }
        for s in self.slots.iter_mut().flatten() {
            if s.state == State::Waiting {
                s.state = State::Dirty;
            }
        }
    }

    /// Another sheet's values changed: queue every formula that reads it, and
    /// what depends on those.
    pub fn sheet_changed(&mut self, name: &str) {
        let name = name.to_lowercase();
        let hit = self.fids_where(|s| s.sheets.contains(&name));
        if hit.is_empty() {
            return;
        }
        self.settle_waits();
        self.lookups.clear();
        for fid in hit {
            self.requeue(fid);
        }
    }

    /// How many formula results have been computed: when it hasn't moved,
    /// no result changed.
    pub fn evaluations(&self) -> u64 {
        self.evaluations
    }

    /// Queue every formula (`grid-recalculate`, F9).
    pub fn mark_all(&mut self) {
        self.settle_waits();
        self.lookups.clear();
        self.respills.clear();
        for (fid, slot) in self.slots.iter_mut().enumerate() {
            if let Some(s) = slot {
                s.state = State::Dirty;
                self.stack.push(fid);
            }
        }
    }

    /// Work through the queue until it's empty or `deadline` passes. Returns
    /// whether it finished; call again (next frame) to continue.
    pub fn recalc(&mut self, src: &dyn Source, deadline: Option<Instant>) -> bool {
        let mut n = 0u32;
        while let Some(fid) = self.stack.pop() {
            n += 1;
            if n.is_multiple_of(64)
                && let Some(d) = deadline
                && Instant::now() >= d
            {
                self.stack.push(fid);
                return false;
            }
            let Some(state) = self.slot(fid).map(|s| s.state) else {
                continue;
            };
            if state == State::Clean {
                continue;
            }
            let Evaluation {
                value,
                array,
                pending,
                circular,
                called,
            } = self.evaluate(src, fid);
            let pending: Vec<Fid> = pending
                .into_iter()
                .filter(|&p| p != fid && self.slot(p).is_some_and(|s| s.state != State::Clean))
                .collect();
            if pending.is_empty() || circular {
                self.evaluations += 1;
                self.finish(src, fid, value, array);
                if called {
                    self.waiting_on_calls.insert(fid);
                } else {
                    self.waiting_on_calls.remove(&fid);
                }
                continue;
            }
            // Inputs still dirty: wait behind them. One already waiting
            // (it's upstream of us and reads us) closes a cycle.
            let mut cycle = false;
            for &p in &pending {
                if self.slot(p).map(|s| s.state) == Some(State::Waiting) {
                    if let Some(s) = self.slot_mut(p) {
                        s.settle(Value::Error(ErrorKind::Circ));
                    }
                    cycle = true;
                }
            }
            if cycle {
                // Re-run now: the cycle members read as #CIRC!.
                self.stack.push(fid);
                if let Some(s) = self.slot_mut(fid) {
                    s.state = State::Dirty;
                }
                continue;
            }
            if let Some(s) = self.slot_mut(fid) {
                s.state = State::Waiting;
            }
            self.stack.push(fid);
            for p in pending {
                if self.slot(p).map(|s| s.state) == Some(State::Dirty) {
                    self.stack.push(p);
                }
            }
        }
        self.respills.clear();
        true
    }

    /// Evaluate one formula.
    fn evaluate(&mut self, src: &dyn Source, fid: Fid) -> Evaluation {
        let Some(slot) = self.slot_mut(fid) else {
            return Evaluation::of(Value::Empty);
        };
        slot.state = State::Evaluating;
        let pos = slot.pos;
        let expr = match &slot.expr {
            Ok(e) => Arc::clone(e),
            Err(_) => {
                slot.state = State::Dirty;
                return Evaluation::of(Value::Error(ErrorKind::Parse));
            }
        };
        let mut ctx = EngineCtx {
            engine: self,
            src,
            here: pos,
            pending: RefCell::new(Vec::new()),
            circular: Cell::new(false),
            called: false,
            now: src.now(),
        };
        let (value, array) = match eval::eval_formula(&expr, &mut ctx) {
            Outcome::Value(v) => (v, None),
            Outcome::Array(a) => (a.data[0].clone(), Some(a)),
        };
        let pending = ctx.pending.into_inner();
        let circular = ctx.circular.get();
        let called = ctx.called;
        if let Some(s) = self.slot_mut(fid) {
            s.state = State::Dirty;
        }
        let (value, array) = if circular {
            (Value::Error(ErrorKind::Circ), None)
        } else {
            (value, array)
        };
        Evaluation {
            value,
            array,
            pending,
            circular,
            called,
        }
    }

    fn next_random(&mut self) -> f64 {
        // xorshift64*: deterministic per engine, fine for RAND().
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }

    // ---- Scheme functions ---------------------------------------------------

    /// Declare the Scheme functions that exist (upper-case names). Formulas
    /// calling a newly (un)defined one are re-queued.
    pub fn set_externals(&mut self, names: HashSet<String>) {
        if names == self.externals {
            return;
        }
        self.functions_redefined(names);
    }

    /// A Scheme function was (re)defined: forget every memoized call and
    /// recompute everything that might call one, even when the set of names
    /// is unchanged (a redefinition changes what a call returns).
    pub fn functions_redefined(&mut self, names: HashSet<String>) {
        self.settle_waits();
        self.externals = names;
        self.calls.clear();
        // Re-evaluate everything that might call one (cheap: v1 re-queues all).
        let all: Vec<(Fid, bool)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (i, s.expr.is_ok())))
            .collect();
        for (fid, ok) in all {
            if ok {
                let vol = {
                    let s = self.slot(fid).unwrap();
                    let e = s.expr.as_ref().unwrap();
                    eval::is_volatile(e) || self.calls_external(e)
                };
                if let Some(s) = self.slot_mut(fid) {
                    s.volatile = vol;
                    s.state = State::Dirty;
                }
                self.stack.push(fid);
            }
        }
    }

    /// Calls queued since the last take: run each, then [`Self::deliver`].
    pub fn take_requests(&mut self) -> Vec<ExternalCall> {
        let reqs = std::mem::take(&mut self.requests);
        for r in &reqs {
            self.calls.insert(r.clone(), CallState::Running);
        }
        reqs
    }

    /// A Scheme call finished: remember its value and re-queue the formulas
    /// that were waiting for it.
    pub fn deliver(&mut self, call: ExternalCall, value: Value) {
        // A call forgotten since it started (its function was redefined)
        // returns a stale answer: drop it.
        if self.calls.get(&call) != Some(&CallState::Running) {
            return;
        }
        self.settle_waits();
        self.lookups.clear();
        self.calls.insert(call, CallState::Done(value));
        let waiting: Vec<Fid> = self.waiting_on_calls.drain().collect();
        for fid in waiting {
            self.requeue(fid);
        }
    }

    /// Forget memoized Scheme results (so the next recalculation calls the
    /// functions again — volatile behaviour, and after a redefinition).
    pub fn forget_calls(&mut self) {
        self.calls.retain(|_, s| *s == CallState::Running);
    }
}

/// What evaluating one formula found.
struct Evaluation {
    value: Value,
    /// The whole result when it's an array (`value` is its first element).
    array: Option<Array>,
    /// Inputs it read that were still dirty.
    pending: Vec<Fid>,
    /// It read itself (the value is then `#CIRC!`).
    circular: bool,
    /// It called a Scheme function that isn't done.
    called: bool,
}

impl Evaluation {
    /// A value found without reading anything.
    fn of(value: Value) -> Evaluation {
        Evaluation {
            value,
            array: None,
            pending: Vec::new(),
            circular: false,
            called: false,
        }
    }
}

/// The [`Ctx`] a formula evaluates in.
struct EngineCtx<'a> {
    engine: &'a mut Engine,
    src: &'a dyn Source,
    here: (usize, usize),
    pending: RefCell<Vec<Fid>>,
    circular: Cell<bool>,
    called: bool,
    now: f64,
}

impl EngineCtx<'_> {
    fn local(&self, sheet: Option<&str>) -> bool {
        sheet.is_none_or(|s| s.eq_ignore_ascii_case(self.src.sheet_name()))
    }

    /// A cell's value; a formula cell gives its result if computed, and an
    /// empty cell what's spilled into it.
    fn value_of(&self, row: usize, col: usize, text: &str) -> Value {
        if !is_formula(text) {
            if text.is_empty()
                && self.engine.has_spills()
                && let Some(fid) = self.engine.spill_owner(row, col)
            {
                return self.spilled(fid, row, col);
            }
            return Value::from_cell_text(text);
        }
        let Some(&fid) = self.engine.by_pos.get(&(row, col)) else {
            return Value::Error(ErrorKind::Calc);
        };
        let Some(slot) = self.engine.slot(fid) else {
            return Value::Error(ErrorKind::Calc);
        };
        match slot.state {
            State::Clean => slot.result.clone(),
            State::Evaluating => {
                self.circular.set(true);
                Value::Error(ErrorKind::Circ)
            }
            State::Dirty | State::Waiting => {
                self.pending.borrow_mut().push(fid);
                Value::Error(ErrorKind::Calc)
            }
        }
    }

    /// The value formula `fid` spills into `(row, col)`, once computed.
    fn spilled(&self, fid: Fid, row: usize, col: usize) -> Value {
        let Some(slot) = self.engine.slot(fid) else {
            return Value::Empty;
        };
        match slot.state {
            State::Clean => slot.spill.as_ref().map_or(Value::Empty, |sp| {
                sp.values[sp.index(slot.pos, row, col)].clone()
            }),
            State::Evaluating => {
                self.circular.set(true);
                Value::Error(ErrorKind::Circ)
            }
            State::Dirty | State::Waiting => {
                self.pending.borrow_mut().push(fid);
                Value::Error(ErrorKind::Calc)
            }
        }
    }

    /// The spills covering any of `range`'s cells: `(fid, rect)`.
    fn spills_in(&self, range: &RangeRef) -> Vec<(Fid, Rect)> {
        let mut out: Vec<(Fid, Rect)> = Vec::new();
        if !self.engine.has_spills() {
            return out;
        }
        let mut visit = |v: &Vec<(usize, usize, Fid)>| {
            for &(r0, r1, fid) in v {
                if r0 <= range.r1
                    && range.r0 <= r1
                    && !out.iter().any(|e| e.0 == fid)
                    && let Some(rect) = self.engine.spill_rect(fid)
                {
                    out.push((fid, rect));
                }
            }
        };
        if range.c1.saturating_sub(range.c0) < self.engine.spill_cols.len() {
            for c in range.c0..=range.c1 {
                if let Some(v) = self.engine.spill_cols.get(&c) {
                    visit(v);
                }
            }
        } else {
            for (c, v) in &self.engine.spill_cols {
                if (range.c0..=range.c1).contains(c) {
                    visit(v);
                }
            }
        }
        out
    }
}

impl Ctx for EngineCtx<'_> {
    fn cell(&self, sheet: Option<&str>, row: usize, col: usize) -> Value {
        if !self.local(sheet) {
            return sheet
                .and_then(|s| self.src.other_sheet(s))
                .map_or(Value::Error(ErrorKind::Ref), |o| o.get(row, col));
        }
        let text = self.src.text(row, col);
        self.value_of(row, col, text)
    }

    fn extent(&self, sheet: Option<&str>) -> Option<(usize, usize)> {
        if self.local(sheet) {
            // Spilled values extend the used area.
            let (rows, cols) = self.src.extent();
            let (sr, sc) = self.engine.spill_extent;
            return Some((rows.max(sr), cols.max(sc)));
        }
        sheet
            .and_then(|s| self.src.other_sheet(s))
            .map(SheetValues::extent)
    }

    fn for_each(&self, range: &RangeRef, f: &mut dyn FnMut(usize, usize, Value)) {
        if !self.local(range.sheet.as_deref()) {
            let Some(o) = range.sheet.as_deref().and_then(|s| self.src.other_sheet(s)) else {
                return;
            };
            // Like a local range: open ends stop at the data, a closed range
            // past it reads empty cells.
            let (rows, cols) = o.extent();
            let row_end = if range.r1 == usize::MAX {
                rows
            } else {
                range.r1 + 1
            };
            let col_end = if range.c1 == usize::MAX {
                cols
            } else {
                range.c1 + 1
            };
            for row in range.r0..row_end {
                for col in range.c0..col_end {
                    f(row, col, o.get(row, col));
                }
            }
            return;
        }
        let (rows, _) = self.src.extent();
        let spills = self.spills_in(range);
        if !spills.is_empty() {
            // Spilled cells have no text: read the stored ones, then lay
            // the spilled values over the empty cells they cover.
            let bottom = spills.iter().map(|s| s.1.2 + 1).max().unwrap_or(0);
            let row_end = range
                .r1
                .saturating_add(1)
                .min(rows.max(bottom))
                .max(range.r0);
            let width = range.cols();
            let height = row_end - range.r0;
            if crate::formula::func::dense_cells(width, height).is_ok() {
                let mut grid = vec![Value::Empty; width * height];
                for (i, col) in (range.c0..=range.c1).enumerate() {
                    self.src
                        .for_each_in_column(col, range.r0..row_end, &mut |row, text| {
                            grid[(row - range.r0) * width + i] = self.value_of(row, col, text);
                        });
                }
                for &(fid, (r0, c0, r1, c1)) in &spills {
                    let at = (r0, c0);
                    for r in r0.max(range.r0)..=r1.min(row_end - 1) {
                        for c in c0.max(range.c0)..=c1.min(range.c1) {
                            if (r, c) != at && self.src.text(r, c).is_empty() {
                                grid[(r - range.r0) * width + (c - range.c0)] =
                                    self.spilled(fid, r, c);
                            }
                        }
                    }
                }
                for (k, v) in grid.into_iter().enumerate() {
                    f(range.r0 + k / width, range.c0 + k % width, v);
                }
                if range.r1 != usize::MAX {
                    for row in row_end..range.r1 + 1 {
                        for col in range.c0..=range.c1 {
                            f(row, col, Value::Empty);
                        }
                    }
                }
                return;
            }
        }
        // Stored rows end at the data; anything past it in the range is empty.
        let row_end = range.r1.saturating_add(1).min(rows).max(range.r0);
        let tail = (range.r1 != usize::MAX).then(|| row_end..range.r1 + 1);
        if range.c0 == range.c1 {
            let col = range.c0;
            self.src
                .for_each_in_column(col, range.r0..row_end, &mut |row, text| {
                    f(row, col, self.value_of(row, col, text));
                });
            for row in tail.into_iter().flatten() {
                f(row, col, Value::Empty);
            }
            return;
        }
        // Several columns: gather each over the stored rows only, then emit
        // row-major (the tail past the data is all empty — never allocated,
        // so `A1:Z99999999` costs its data, not 2.6e9 cells).
        let width = range.cols();
        let height = row_end - range.r0;
        let mut grid = vec![Value::Empty; width.saturating_mul(height)];
        for (i, col) in (range.c0..=range.c1).enumerate() {
            self.src
                .for_each_in_column(col, range.r0..row_end, &mut |row, text| {
                    grid[(row - range.r0) * width + i] = self.value_of(row, col, text);
                });
        }
        for (k, v) in grid.into_iter().enumerate() {
            f(range.r0 + k / width, range.c0 + k % width, v);
        }
        for row in tail.into_iter().flatten() {
            for col in range.c0..=range.c1 {
                f(row, col, Value::Empty);
            }
        }
    }

    fn here(&self) -> (usize, usize) {
        self.here
    }

    fn now(&self) -> f64 {
        self.now
    }

    fn random(&mut self) -> f64 {
        self.engine.next_random()
    }

    fn lookup_index(&mut self, range: &RangeRef) -> Option<Arc<super::func::lookup::LookupIndex>> {
        if let Some(ix) = self.engine.lookups.get(range) {
            return Some(ix.clone());
        }
        let before = self.pending.borrow().len();
        let ix = Arc::new(super::func::lookup::LookupIndex::build(&*self, range));
        // Built over results still being computed: use it now, don't keep it.
        if self.pending.borrow().len() == before {
            self.engine.lookups.insert(range.clone(), ix.clone());
        }
        Some(ix)
    }

    fn spill_range(
        &mut self,
        sheet: Option<&str>,
        row: usize,
        col: usize,
    ) -> Result<RangeRef, ErrorKind> {
        // Other sheets keep values only, no formulas to spill.
        if !self.local(sheet) || !is_formula(self.src.text(row, col)) {
            return Err(ErrorKind::Ref);
        }
        let fid = *self.engine.by_pos.get(&(row, col)).ok_or(ErrorKind::Calc)?;
        let slot = self.engine.slot(fid).ok_or(ErrorKind::Calc)?;
        let (r1, c1) = match slot.state {
            State::Clean => match (&slot.spill, &slot.result) {
                (Some(sp), _) => (row + sp.rows - 1, col + sp.cols - 1),
                (None, Value::Error(e)) => return Err(*e),
                (None, _) => (row, col),
            },
            State::Evaluating => {
                self.circular.set(true);
                return Err(ErrorKind::Circ);
            }
            State::Dirty | State::Waiting => {
                self.pending.borrow_mut().push(fid);
                return Err(ErrorKind::Calc);
            }
        };
        Ok(RangeRef {
            sheet: None,
            r0: row,
            c0: col,
            r1,
            c1,
        })
    }

    fn call_external(&mut self, name: &str, args: &[Arg]) -> Option<Value> {
        if !self.engine.externals.contains(name) {
            return None;
        }
        let mut flat = Vec::with_capacity(args.len());
        for a in args {
            flat.push(match a {
                Arg::Value(v) => ExternalArg::Value(v.clone()),
                Arg::Omitted => ExternalArg::Value(Value::Empty),
                Arg::Array(arr) => ExternalArg::Rows(
                    arr.data
                        .chunks(arr.cols.max(1))
                        .map(<[Value]>::to_vec)
                        .collect(),
                ),
                // One cell arrives as its value; a real range as rows.
                Arg::Range(r) if r.is_cell() => {
                    ExternalArg::Value(self.cell(r.sheet.as_deref(), r.r0, r.c0))
                }
                Arg::Range(r) => {
                    let Some(r) = self.clamp(r) else {
                        return Some(Value::Error(ErrorKind::Ref));
                    };
                    if crate::formula::func::dense_cells(r.rows(), r.cols()).is_err() {
                        return Some(Value::Error(ErrorKind::Num));
                    }
                    let mut rows = vec![vec![Value::Empty; r.cols()]; r.rows()];
                    self.for_each(&r, &mut |row, col, v| rows[row - r.r0][col - r.c0] = v);
                    ExternalArg::Rows(rows)
                }
            });
        }
        // Inputs still being computed: wait for them before calling.
        if !self.pending.borrow().is_empty() {
            return Some(Value::Error(ErrorKind::Calc));
        }
        let call = ExternalCall {
            name: name.to_string(),
            args: flat,
        };
        match self.engine.calls.get(&call) {
            Some(CallState::Done(v)) => Some(v.clone()),
            Some(_) => {
                self.called = true;
                Some(Value::Error(ErrorKind::Calc))
            }
            None => {
                self.engine.calls.insert(call.clone(), CallState::Queued);
                self.engine.requests.push(call);
                self.called = true;
                Some(Value::Error(ErrorKind::Calc))
            }
        }
    }
}

impl std::hash::Hash for ExternalCall {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.name.hash(h);
        fn hv<H: std::hash::Hasher>(v: &Value, h: &mut H) {
            match v {
                Value::Number(n) => (0u8, n.to_bits()).hash(h),
                Value::Text(s) => (1u8, &**s).hash(h),
                Value::Bool(b) => (2u8, *b).hash(h),
                Value::Error(e) => (3u8, *e).hash(h),
                Value::Empty => 4u8.hash(h),
            }
        }
        for a in &self.args {
            match a {
                ExternalArg::Value(v) => hv(v, h),
                ExternalArg::Rows(rows) => rows.iter().flatten().for_each(|v| hv(v, h)),
            }
        }
    }
}

impl Eq for ExternalCall {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A sheet in a map, for tests.
    #[derive(Default)]
    struct MapSheet {
        cells: BTreeMap<(usize, usize), String>,
        extent: (usize, usize),
        /// Other sheets, by lower-case name.
        others: HashMap<String, SheetValues>,
    }

    impl MapSheet {
        fn set(&mut self, name: &str, text: &str) -> (usize, usize) {
            let pos = crate::grid::view::parse_cell_name(name).unwrap();
            let stored = if text.starts_with('=') {
                store_formula(text)
            } else {
                text.to_string()
            };
            if stored.is_empty() {
                self.cells.remove(&pos);
            } else {
                self.cells.insert(pos, stored);
            }
            self.extent = (self.extent.0.max(pos.0 + 1), self.extent.1.max(pos.1 + 1));
            pos
        }
    }

    impl Source for MapSheet {
        fn text(&self, row: usize, col: usize) -> &str {
            self.cells.get(&(row, col)).map_or("", String::as_str)
        }
        fn extent(&self) -> (usize, usize) {
            self.extent
        }
        fn for_each_in_column(
            &self,
            col: usize,
            rows: std::ops::Range<usize>,
            f: &mut dyn FnMut(usize, &str),
        ) {
            for r in rows {
                f(r, self.text(r, col));
            }
        }
        fn now(&self) -> f64 {
            45000.0
        }
        fn other_sheet(&self, name: &str) -> Option<&SheetValues> {
            self.others.get(&name.to_lowercase())
        }
    }

    fn val(e: &Engine, name: &str) -> Value {
        let (r, c) = crate::grid::view::parse_cell_name(name).unwrap();
        e.result(r, c).cloned().unwrap_or(Value::Empty)
    }

    fn edit(e: &mut Engine, s: &mut MapSheet, name: &str, text: &str) {
        let pos = s.set(name, text);
        e.cells_changed(s, &[pos]);
        assert!(e.recalc(s, None));
    }

    #[test]
    fn other_sheets_are_read_and_recalculate_when_they_change() {
        let costs = |a: f64| {
            SheetValues::from_cells([
                (0, 0, Value::text("rent")),
                (1, 0, Value::Number(a)),
                (2, 0, Value::Number(5.0)),
            ])
        };
        let mut s = MapSheet::default();
        s.others.insert("costs".into(), costs(10.0));
        s.set("A1", "=Costs!A2*2");
        s.set("A2", "=SUM(costs!A:A)");
        s.set("A3", "='Costs'!A1&\"!\"");
        s.set("A4", "=Nope!A1");
        s.set("A5", "=A1+1");
        s.set("B1", "7");
        s.set("A6", "=Sheet1!B1+1"); // its own name: a local reference
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A1"), Value::Number(20.0));
        assert_eq!(val(&e, "A2"), Value::Number(15.0));
        assert_eq!(val(&e, "A3"), Value::text("rent!"));
        assert_eq!(val(&e, "A4"), Value::Error(ErrorKind::Ref));
        // The other sheet changes: its readers and their dependents follow.
        s.others.insert("costs".into(), costs(100.0));
        e.sheet_changed("Costs");
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A1"), Value::Number(200.0));
        assert_eq!(val(&e, "A5"), Value::Number(201.0));
        assert_eq!(val(&e, "A2"), Value::Number(105.0));
        // A qualified reference to the sheet itself tracks like a local one.
        edit(&mut e, &mut s, "B1", "9");
        assert_eq!(val(&e, "A6"), Value::Number(10.0));
    }

    #[test]
    fn chains_update_incrementally() {
        let mut s = MapSheet::default();
        s.set("A1", "1");
        s.set("A2", "=A1+1");
        s.set("A3", "=A2*10");
        s.set("B1", "=SUM(A1:A3)");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A3"), Value::Number(20.0));
        assert_eq!(val(&e, "B1"), Value::Number(23.0));
        edit(&mut e, &mut s, "A1", "5");
        assert_eq!(val(&e, "A3"), Value::Number(60.0));
        assert_eq!(val(&e, "B1"), Value::Number(71.0));
        // Turning a formula into a value and back.
        edit(&mut e, &mut s, "A2", "100");
        assert_eq!(val(&e, "A3"), Value::Number(1000.0));
        assert_eq!(e.formula_count(), 2);
    }

    #[test]
    fn long_chains_need_no_recursion() {
        let mut s = MapSheet::default();
        s.set("A1", "1");
        for r in 2..=20_000 {
            s.set(&format!("A{r}"), &format!("=A{}+1", r - 1));
        }
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A20000"), Value::Number(20000.0));
        edit(&mut e, &mut s, "A1", "0");
        assert_eq!(val(&e, "A20000"), Value::Number(19999.0));
    }

    #[test]
    fn cycles_are_circ_and_recover() {
        let mut s = MapSheet::default();
        s.set("A1", "=B1+1");
        s.set("B1", "=A1+1");
        s.set("C1", "=A1*2");
        s.set("D1", "=D1");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A1"), Value::Error(ErrorKind::Circ));
        assert_eq!(val(&e, "B1"), Value::Error(ErrorKind::Circ));
        assert_eq!(val(&e, "C1"), Value::Error(ErrorKind::Circ));
        assert_eq!(val(&e, "D1"), Value::Error(ErrorKind::Circ));
        // Breaking the cycle recovers every member.
        edit(&mut e, &mut s, "B1", "4");
        assert_eq!(val(&e, "A1"), Value::Number(5.0));
        assert_eq!(val(&e, "C1"), Value::Number(10.0));
    }

    #[test]
    fn diamonds_and_ranges() {
        let mut s = MapSheet::default();
        s.set("A1", "2");
        s.set("B1", "=A1*2");
        s.set("C1", "=A1*3");
        s.set("D1", "=B1+C1");
        s.set("E5", "=SUM(A:D)");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "D1"), Value::Number(10.0));
        assert_eq!(val(&e, "E5"), Value::Number(22.0));
        edit(&mut e, &mut s, "A1", "1");
        assert_eq!(val(&e, "E5"), Value::Number(11.0));
        // A new cell inside a whole-column range is picked up.
        edit(&mut e, &mut s, "A9", "100");
        assert_eq!(val(&e, "E5"), Value::Number(111.0));
    }

    #[test]
    fn parse_errors_show_and_budget_resumes() {
        let mut s = MapSheet::default();
        s.set("A1", "=SUM(");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A1"), Value::Error(ErrorKind::Parse));
        assert!(e.parse_error(0, 0).is_some());
        // An expired deadline stops early; calling again finishes.
        for r in 1..=1000 {
            s.set(&format!("B{r}"), "=1+1");
        }
        e.rebuild(&s);
        assert!(!e.recalc(&s, Some(Instant::now())));
        assert!(e.busy());
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "B1000"), Value::Number(2.0));
    }

    #[test]
    fn scheme_calls_queue_memoize_and_deliver() {
        let mut s = MapSheet::default();
        s.set("A1", "3");
        s.set("B1", "=DOUBLE(A1)");
        s.set("B2", "=DOUBLE(A1)+1");
        s.set("C1", "=B1*10");
        let mut e = Engine::new();
        e.set_externals(["DOUBLE".to_string()].into_iter().collect());
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "B1"), Value::Error(ErrorKind::Calc));
        assert_eq!(val(&e, "C1"), Value::Error(ErrorKind::Calc));
        let reqs = e.take_requests();
        assert_eq!(reqs.len(), 1, "identical calls are merged");
        e.deliver(reqs[0].clone(), Value::Number(6.0));
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "B1"), Value::Number(6.0));
        assert_eq!(val(&e, "B2"), Value::Number(7.0));
        assert_eq!(val(&e, "C1"), Value::Number(60.0));
        // A changed argument makes a new call.
        edit(&mut e, &mut s, "A1", "4");
        assert_eq!(e.take_requests().len(), 1);
    }

    #[test]
    fn marker_helpers() {
        assert!(is_formula(&store_formula("=1")));
        assert!(!is_formula("=1"), "a loaded '=…' is text");
        assert_eq!(display_text(&store_formula("=A1")), "=A1");
        assert_eq!(display_text("plain"), "plain");
    }

    // ---- audit probes ----
    #[test]
    fn redefining_a_scheme_function_recomputes() {
        let mut s = MapSheet::default();
        s.set("A1", "3");
        s.set("B1", "=DOUBLE(A1)");
        let mut e = Engine::new();
        let names: HashSet<String> = ["DOUBLE".to_string()].into_iter().collect();
        e.set_externals(names.clone());
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        let reqs = e.take_requests();
        e.deliver(reqs[0].clone(), Value::Number(6.0));
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "B1"), Value::Number(6.0));
        // The editor's grid_register_function on a redefinition of DOUBLE:
        e.functions_redefined(names.clone());
        assert!(e.recalc(&s, None));
        assert_eq!(
            e.take_requests().len(),
            1,
            "redefinition must re-run the call"
        );
    }

    #[test]
    fn cross_sheet_ranges_intersect_in_arithmetic() {
        let mut s = MapSheet::default();
        s.others.insert(
            "costs".into(),
            SheetValues::from_cells([
                (0, 0, Value::Number(1.0)),
                (1, 0, Value::Number(2.0)),
                (2, 0, Value::Number(3.0)),
            ]),
        );
        // `@`: one value, as pre-dynamic-array Excel read every formula.
        s.set("B2", "=@Costs!A1:A3");
        s.set("C2", "=@Costs!A1:A3*10");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "B2"), Value::Number(2.0));
        assert_eq!(val(&e, "C2"), Value::Number(20.0));
    }

    #[test]
    fn whole_row_and_other_dependencies() {
        let mut s = MapSheet::default();
        s.set("A5", "=SUM(2:2)");
        s.set("B5", "=SUM(A1:C3)");
        s.set("C5", "=COUNTIF(A:A,\">1\")");
        s.set("D5", "=VLOOKUP(\"k\",F1:G3,2,FALSE)");
        s.set("F2", "k");
        s.set("G2", "=1+1");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "D5"), Value::Number(2.0));
        edit(&mut e, &mut s, "C2", "7");
        assert_eq!(val(&e, "A5"), Value::Number(9.0));
        assert_eq!(val(&e, "B5"), Value::Number(7.0));
        edit(&mut e, &mut s, "A9", "4");
        assert_eq!(val(&e, "C5"), Value::Number(2.0));
        edit(&mut e, &mut s, "G2", "=5+5");
        assert_eq!(val(&e, "D5"), Value::Number(10.0));
        // A cycle through a range.
        edit(&mut e, &mut s, "B1", "=SUM(A5:B5)");
        assert_eq!(val(&e, "B1"), Value::Error(ErrorKind::Circ));
        edit(&mut e, &mut s, "B1", "1");
        assert_eq!(val(&e, "B5"), Value::Number(8.0));
        assert_eq!(val(&e, "A5"), Value::Number(17.0));
    }

    #[test]
    fn volatile_and_lookup_chains() {
        let mut s = MapSheet::default();
        s.set("A1", "1");
        s.set("B1", "=A1*2");
        s.set("C1", "=MATCH(2,B1:B1,0)");
        s.set("D1", "=INDEX(B1:B1,1)+NOW()*0");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "C1"), Value::Number(1.0));
        edit(&mut e, &mut s, "A1", "3");
        assert_eq!(val(&e, "D1"), Value::Number(6.0));
        assert_eq!(val(&e, "C1"), Value::Error(ErrorKind::NA));
    }

    #[test]
    fn new_formula_during_sliced_recalc_is_no_cycle() {
        let mut s = MapSheet::default();
        s.set("A1", "1");
        for r in 2..=300 {
            s.set(&format!("A{r}"), &format!("=A{}+1", r - 1));
        }
        let mut e = Engine::new();
        e.rebuild(&s);
        // One slice runs out of time mid-chain.
        assert!(!e.recalc(&s, Some(Instant::now())));
        // The user enters a formula reading the end of the chain.
        let pos = s.set("B1", "=A300+1");
        e.cells_changed(&s, &[pos]);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "A300"), Value::Number(300.0));
        assert_eq!(val(&e, "B1"), Value::Number(301.0));
    }

    #[test]
    fn edit_during_sliced_recalc_is_no_cycle() {
        let mut s = MapSheet::default();
        s.set("A1", "1");
        for r in 2..=300 {
            s.set(&format!("A{r}"), &format!("=A{}+1", r - 1));
        }
        s.set("B1", "5");
        s.set("C1", "=B1+A300");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(!e.recalc(&s, Some(Instant::now())));
        edit(&mut e, &mut s, "B1", "6");
        assert_eq!(val(&e, "C1"), Value::Number(306.0));
    }

    /// What cell `name` shows: a formula's result, or the value spilled
    /// into it.
    fn at(e: &Engine, name: &str) -> Value {
        let (r, c) = crate::grid::view::parse_cell_name(name).unwrap();
        e.result(r, c)
            .or_else(|| e.spilled_value(r, c))
            .cloned()
            .unwrap_or(Value::Empty)
    }

    fn n(x: f64) -> Value {
        Value::Number(x)
    }

    fn sheet(cells: &[(&str, &str)]) -> (Engine, MapSheet) {
        let mut s = MapSheet::default();
        for (name, text) in cells {
            s.set(name, text);
        }
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        (e, s)
    }

    #[test]
    fn arrays_spill_and_are_read_like_cells() {
        let (mut e, mut s) = sheet(&[
            ("A1", "=SEQUENCE(3)"),
            ("B1", "=SUM(A1:A5)"),
            ("B2", "=A3*10"),
            ("C1", "=A1:A3*2"),
            ("D1", "=SORT({3,1,2},,,TRUE)"),
        ]);
        assert_eq!(
            [at(&e, "A1"), at(&e, "A2"), at(&e, "A3"), at(&e, "A4")],
            [n(1.0), n(2.0), n(3.0), Value::Empty]
        );
        assert_eq!(e.spill_range(0, 0), Some((0, 0, 2, 0)));
        assert_eq!(e.spilled_from(2, 0), Some((0, 0)));
        assert_eq!(e.spilled_from(0, 0), None, "the formula's own cell");
        assert_eq!(e.spilled_shown(1, 0), Some("2"));
        assert_eq!(at(&e, "B1"), n(6.0));
        assert_eq!(at(&e, "B2"), n(30.0));
        // A range in arithmetic spills too; so does a row.
        assert_eq!([at(&e, "C2"), at(&e, "C3")], [n(4.0), n(6.0)]);
        assert_eq!(
            [at(&e, "D1"), at(&e, "E1"), at(&e, "F1")],
            [n(1.0), n(2.0), n(3.0)]
        );
        assert_eq!(e.spill_extent(), (3, 6));
        // Editing an input reshapes nothing but the values.
        edit(&mut e, &mut s, "A1", "=SEQUENCE(3,1,10)");
        assert_eq!(at(&e, "B1"), n(33.0));
        assert_eq!(at(&e, "B2"), n(120.0));
        assert_eq!(at(&e, "C3"), n(24.0));
    }

    #[test]
    fn blocked_spills_show_spill_and_recover() {
        let (mut e, mut s) = sheet(&[("A1", "=SEQUENCE(3)"), ("B1", "=A3"), ("B2", "=COUNT(A:A)")]);
        assert_eq!(at(&e, "B1"), n(3.0));
        // Typing into the spill range blocks it.
        edit(&mut e, &mut s, "A2", "x");
        assert_eq!(at(&e, "A1"), Value::Error(ErrorKind::Spill));
        assert_eq!(e.spill_range(0, 0), None);
        assert_eq!(at(&e, "A3"), Value::Empty);
        assert_eq!(
            at(&e, "B1"),
            n(0.0),
            "the cell it spilled into is empty again"
        );
        assert_eq!(at(&e, "B2"), n(0.0));
        // Clearing the blocker re-spills, and the readers follow.
        edit(&mut e, &mut s, "A2", "");
        assert_eq!(at(&e, "A1"), n(1.0));
        assert_eq!(at(&e, "B1"), n(3.0));
        assert_eq!(at(&e, "B2"), n(3.0));
        // A formula blocks it too, and so does another formula's spill.
        edit(&mut e, &mut s, "A3", "=1");
        assert_eq!(at(&e, "A1"), Value::Error(ErrorKind::Spill));
        edit(&mut e, &mut s, "A3", "");
        edit(&mut e, &mut s, "C2", "=SEQUENCE(1,3,7)");
        edit(&mut e, &mut s, "C1", "=SEQUENCE(2)");
        assert_eq!(at(&e, "C1"), Value::Error(ErrorKind::Spill));
        assert_eq!(at(&e, "D2"), n(8.0));
        // The other spill going away frees the range.
        edit(&mut e, &mut s, "C2", "");
        assert_eq!([at(&e, "C1"), at(&e, "C2")], [n(1.0), n(2.0)]);
        // Off the sheet's edge it can never spill.
        edit(&mut e, &mut s, "XFD1", "=SEQUENCE(1,2)");
        assert_eq!(at(&e, "XFD1"), Value::Error(ErrorKind::Spill));
    }

    #[test]
    fn spill_range_references_follow_resizes() {
        let (mut e, mut s) = sheet(&[
            ("A1", "3"),
            ("B1", "=SEQUENCE(A1)"),
            ("C1", "=SUM(B1#)"),
            ("C2", "=ROWS(B1#)"),
            ("C3", "=B3"),
            ("C4", "=B5"),
            ("D1", "=B1#*10"),
            ("C5", "=A1#"),
        ]);
        assert_eq!([at(&e, "C1"), at(&e, "C2")], [n(6.0), n(3.0)]);
        assert_eq!([at(&e, "D1"), at(&e, "D3")], [n(10.0), n(30.0)]);
        assert_eq!(
            at(&e, "C5"),
            Value::Error(ErrorKind::Ref),
            "A1 holds no formula"
        );
        // Growing: the new cells' readers recalculate.
        edit(&mut e, &mut s, "A1", "5");
        assert_eq!([at(&e, "C1"), at(&e, "C2")], [n(15.0), n(5.0)]);
        assert_eq!(at(&e, "C4"), n(5.0));
        assert_eq!(at(&e, "D5"), n(50.0));
        // Shrinking: so do the cells it left.
        edit(&mut e, &mut s, "A1", "2");
        assert_eq!([at(&e, "C1"), at(&e, "C2")], [n(3.0), n(2.0)]);
        assert_eq!([at(&e, "C3"), at(&e, "C4")], [n(0.0), n(0.0)]);
        assert_eq!(at(&e, "D3"), Value::Empty);
        // A blocked spill's reference is its error.
        edit(&mut e, &mut s, "B2", "x");
        assert_eq!(at(&e, "C1"), Value::Error(ErrorKind::Spill));
        // One value: the reference is the one cell.
        edit(&mut e, &mut s, "A1", "1");
        assert_eq!([at(&e, "C1"), at(&e, "C2")], [n(1.0), n(1.0)]);
    }

    #[test]
    fn spills_feed_whole_column_reads_and_chains() {
        let (mut e, mut s) = sheet(&[
            ("A1", "=SEQUENCE(1000)"),
            ("B1", "=SUM(A:A)"),
            ("C1", "=SORT(A1#,,-1)"),
            ("D1", "=C1+C1000"),
            ("E1", "=MATCH(500,A:A,0)"),
        ]);
        assert_eq!(at(&e, "B1"), n(500_500.0));
        assert_eq!(at(&e, "C1"), n(1000.0));
        assert_eq!(at(&e, "D1"), n(1001.0));
        assert_eq!(at(&e, "E1"), n(500.0));
        edit(&mut e, &mut s, "A1", "=SEQUENCE(10)");
        assert_eq!(at(&e, "B1"), n(55.0));
        assert_eq!(at(&e, "D1"), n(10.0), "C1000 is empty now");
        assert_eq!(at(&e, "E1"), Value::Error(ErrorKind::NA));
        assert_eq!(e.spill_extent(), (10, 3));
    }

    #[test]
    fn a_spill_over_its_own_inputs_is_a_cycle() {
        // It reads the cells it spills into: never settles.
        let (e, _) = sheet(&[("A1", "=SEQUENCE(COUNT(A2:A5)+2)")]);
        assert_eq!(at(&e, "A1"), Value::Error(ErrorKind::Circ));
        let (e, _) = sheet(&[("B1", "=SORT(B2:B4)")]);
        assert_eq!(at(&e, "B1"), Value::Error(ErrorKind::Circ));
    }

    #[test]
    fn implicit_intersection_keeps_one_value() {
        let (e, _) = sheet(&[
            ("A1", "1"),
            ("A2", "2"),
            ("A3", "3"),
            ("B2", "=@A1:A3*10"),
            ("C2", "=A1:A3*10"),
        ]);
        assert_eq!(at(&e, "B2"), n(20.0));
        assert_eq!(e.spill_range(1, 1), None);
        assert_eq!([at(&e, "C2"), at(&e, "C4")], [n(10.0), n(30.0)]);
    }

    #[test]
    fn regression_if_range_condition_untaken_branch_is_not_a_cycle() {
        let mut s = MapSheet::default();
        s.set("A1", "1");
        s.set("A2", "2");
        s.set("A3", "3");
        s.set("C2", "=IF(A1:A3>0,1,D2)");
        s.set("D2", "=C2+1");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        assert_eq!(val(&e, "C2"), Value::Number(1.0));
        assert_eq!(val(&e, "D2"), Value::Number(2.0));
    }

    #[test]
    fn regression_ifs_untaken_condition_is_not_a_cycle() {
        let mut s = MapSheet::default();
        s.set("A1", "=IFS(TRUE,1,B1>0,2)");
        s.set("B1", "=A1+1");
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        // Same shape with IF, which stays lazy:
        s.set("A2", "=IF(TRUE,1,B2)");
        s.set("B2", "=A2+1");
        let mut e2 = Engine::new();
        e2.rebuild(&s);
        assert!(e2.recalc(&s, None));
        assert_eq!(val(&e2, "A2"), Value::Number(1.0));
        assert_eq!(val(&e, "A1"), Value::Number(1.0));
        assert_eq!(val(&e, "B1"), Value::Number(2.0));
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;

    struct Big;
    impl Source for Big {
        fn text(&self, row: usize, col: usize) -> &str {
            // Column A: keys "k0".."k49999"; column C: VLOOKUPs.
            match col {
                0 => Box::leak(format!("k{row}").into_boxed_str()),
                _ => "",
            }
        }
        fn extent(&self) -> (usize, usize) {
            (50_000, 3)
        }
        fn for_each_in_column(
            &self,
            col: usize,
            rows: std::ops::Range<usize>,
            f: &mut dyn FnMut(usize, &str),
        ) {
            for r in rows {
                f(r, self.text(r, col));
            }
        }
    }

    #[test]
    fn exact_lookups_share_one_index() {
        // 2,000 MATCHes over 50,000 rows: with the cached index this is
        // instant; a linear scan per formula would take ~10^8 steps.
        let src = Big;
        let mut e = Engine::new();
        for r in 0..2000 {
            e.register(
                r,
                2,
                &store_formula(&format!("=MATCH(\"k{}\",A:A,0)", r * 20)),
            );
        }
        let t = Instant::now();
        assert!(e.recalc(&src, None));
        assert_eq!(e.result(5, 2), Some(&Value::Number(101.0)));
        assert!(t.elapsed().as_secs_f64() < 5.0, "took {:?}", t.elapsed());
        assert_eq!(e.lookups.len(), 1);
    }
}

#[cfg(test)]
mod perf {
    use super::*;

    /// A procedural sheet: column A holds numbers 1..=9 cycling (no
    /// allocation per read); formula cells come from `formulas`.
    struct Sheet {
        rows: usize,
        cols: usize,
        formulas: HashMap<(usize, usize), String>,
    }

    const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];

    impl Source for Sheet {
        fn text(&self, row: usize, col: usize) -> &str {
            if let Some(f) = self.formulas.get(&(row, col)) {
                return f;
            }
            if col == 0 && row < self.rows {
                DIGITS[row % 9]
            } else {
                ""
            }
        }
        fn extent(&self) -> (usize, usize) {
            (self.rows, self.cols)
        }
        fn for_each_in_column(
            &self,
            col: usize,
            rows: std::ops::Range<usize>,
            f: &mut dyn FnMut(usize, &str),
        ) {
            for r in rows {
                f(r, self.text(r, col));
            }
        }
    }

    fn ms(t: Instant) -> f64 {
        t.elapsed().as_secs_f64() * 1000.0
    }

    /// The plan's targets; run with `cargo test perf -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn formula_perf_targets() {
        // 1. Full recalculation of 100k simple formulas (target < 250 ms).
        let mut s = Sheet {
            rows: 100_000,
            cols: 2,
            formulas: HashMap::new(),
        };
        for r in 0..100_000 {
            s.formulas
                .insert((r, 1), store_formula(&format!("=A{}*2", r + 1)));
        }
        let mut e = Engine::new();
        let t = Instant::now();
        e.rebuild(&s);
        let index_ms = ms(t);
        let t = Instant::now();
        assert!(e.recalc(&s, None));
        println!(
            "100k formulas: index {index_ms:.0} ms, full recalc {:.0} ms",
            ms(t)
        );

        // 2. =SUM(A:A) over 1M rows (target < 20 ms).
        let mut s = Sheet {
            rows: 1_000_000,
            cols: 2,
            formulas: HashMap::new(),
        };
        s.formulas.insert((0, 1), store_formula("=SUM(A:A)"));
        let mut e = Engine::new();
        e.rebuild(&s);
        let t = Instant::now();
        assert!(e.recalc(&s, None));
        println!("SUM(A:A) over 1M rows: {:.1} ms", ms(t));
        assert_eq!(e.result(0, 1), Some(&Value::Number(4_999_996.0)));

        // 3. One edit feeding 10k dependents (target < 16 ms).
        let mut s = Sheet {
            rows: 10_001,
            cols: 2,
            formulas: HashMap::new(),
        };
        for r in 1..=10_000 {
            s.formulas.insert((r, 1), store_formula("=$A$1*2+1"));
        }
        let mut e = Engine::new();
        e.rebuild(&s);
        assert!(e.recalc(&s, None));
        let t = Instant::now();
        e.cells_changed(&s, &[(0, 0)]);
        assert!(e.recalc(&s, None));
        println!("edit with 10k dependents: {:.1} ms", ms(t));
    }
}
