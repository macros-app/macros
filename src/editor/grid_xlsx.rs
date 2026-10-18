//! `.xlsx` files as grid buffers: loading a workbook's sheets (values and
//! formulas) into [`Workbook`]s, and saving a grid back as `.xlsx`. Opening
//! a file shows its first sheet; each other sheet opens as a buffer of its
//! own (`grid-open-sheet`, `report.xlsx<Costs>`). The sheet buffers of one
//! workbook share a [`BookStore`]: the sheet list and every sheet's
//! contents, so saving from any of them writes every sheet.
//!
//! The `.xlsx` support is deliberately minimal (see `src/xlsx`): values,
//! formulas and their cached results, and the hand-set formatting a grid
//! has (bold, palette colors, number formats). A file holding anything this
//! would drop on a rewrite — charts, other cell styles, hidden sheets, … —
//! opens *protected*: it can be viewed and edited, and saved as a new file
//! (`:w copy.xlsx`), but never written back over itself, so the original is
//! never silently damaged.

use crate::formula::engine::{MARK, display_text, is_formula, store_formula};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use crate::formula::engine::SheetValues;
use crate::formula::value::{ErrorKind, Value, general_format};
use crate::grid::cellfmt::{self, CellFmt, NumCode};
use crate::grid::model::Workbook;
use crate::xlsx::{self, WriteCell, XFormula, XStyle, XValue};

use gpui::Context;

use super::Editor;
use super::grid::{GridDoc, SHEET};

/// One sheet's contents as a grid holds them: what a sheet with no buffer
/// open is saved from and opens with.
#[derive(Clone, Debug, Default)]
pub(super) struct SheetData {
    /// Every non-empty cell's text (a formula with its leading MARK).
    pub cells: Vec<(usize, usize, String)>,
    /// Cells to save as text although they read as another type (see
    /// [`XlsxState::text`](super::grid::XlsxState)).
    pub text: HashMap<(usize, usize), String>,
    pub formats: Vec<((usize, usize), CellFmt)>,
    /// The values other sheets' formulas read (a formula's result), also
    /// cached in the file for a formula.
    pub values: SheetValues,
}

/// A workbook with several sheets, shared by the buffers showing them.
#[derive(Debug, Default)]
pub(super) struct BookStore {
    /// The sheet names, in workbook order.
    pub names: Vec<String>,
    /// The names as last loaded or saved: the workbook is modified while
    /// they differ (a sheet added, renamed, deleted or moved).
    pub saved_names: Vec<String>,
    /// Each sheet's contents by lower-case name, as of the last load or
    /// save plus sheet operations (renames rewrite references here too). An
    /// open sheet buffer holds the live contents of its sheet.
    pub sheets: HashMap<String, SheetData>,
    /// The `.xlsx` file the workbook saves to (`None`: a CSV that grew a
    /// sheet, saved with a Save As .xlsx first).
    pub path: Option<PathBuf>,
}

pub(super) type Book = Rc<RefCell<BookStore>>;

impl BookStore {
    /// A store for `names`, every sheet's data in `data` (same order).
    pub fn new(names: Vec<String>, data: Vec<SheetData>, path: Option<PathBuf>) -> Book {
        let sheets = names.iter().map(|n| n.to_lowercase()).zip(data).collect();
        Rc::new(RefCell::new(BookStore {
            saved_names: names.clone(),
            names,
            sheets,
            path,
        }))
    }

    /// A sheet added, renamed, deleted or moved since the last save.
    pub fn structure_modified(&self) -> bool {
        self.names != self.saved_names
    }

    /// Whether a save must go through the workbook (every sheet, and the
    /// sheet list) rather than one buffer's own: it has several sheets, its
    /// list changed, or the buffer saving it has no file of its own.
    pub fn saves_whole(&self, has_own_file: bool) -> bool {
        self.names.len() > 1 || self.structure_modified() || !has_own_file
    }

    /// The index of sheet `name` (exact, else ignoring case).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|s| s == name).or_else(|| {
            let lower = name.to_lowercase();
            self.names.iter().position(|s| s.to_lowercase() == lower)
        })
    }
}

/// A loaded `.xlsx`: the grid workbook, and what a rewrite would lose.
pub(super) struct Loaded {
    pub wb: Workbook,
    /// Why the file can't be saved over itself (empty: it can).
    pub lossy: Vec<String>,
    /// Every sheet's name, in workbook order.
    pub sheets: Vec<String>,
    /// The other sheets' values as the file last saved them (a formula
    /// cell's cached result), by lower-case name.
    pub others: HashMap<String, SheetValues>,
    /// Cells the file stores as text whose text would otherwise be written
    /// back as another type (`"12"`, `"TRUE"`, `"#N/A"`).
    pub text_cells: HashMap<(usize, usize), String>,
    /// The sheet's cell formatting, as the grid's hand-set formats.
    pub formats: Vec<((usize, usize), CellFmt)>,
    /// Every sheet's contents, in workbook order.
    pub data: Vec<SheetData>,
}

/// A file's cell format as a grid format; `None` when a color isn't one of
/// the palette's.
fn cell_fmt(st: &XStyle) -> Option<CellFmt> {
    let color = |c: Option<u32>| match c {
        None => Some(None),
        Some(argb) => cellfmt::argb_index(argb).map(Some),
    };
    Some(CellFmt {
        bold: st.bold,
        text: color(st.color)?,
        fill: color(st.fill)?,
        num: st.num_fmt.as_deref().and_then(NumCode::intern),
    })
}

/// A grid format as a file's cell format.
fn x_style(f: CellFmt) -> XStyle {
    let argb = |i: Option<u8>| i.and_then(|i| cellfmt::ARGB.get(i as usize).copied());
    XStyle {
        bold: f.bold,
        color: argb(f.text),
        fill: argb(f.fill),
        num_fmt: f.num.map(|n| n.code().to_string()),
    }
}

/// The text a cached/plain `.xlsx` value is stored as in a grid cell.
fn value_text(v: &XValue) -> String {
    match v {
        XValue::Number(n) => number_text(*n),
        // The grid marks a formula cell with a leading MARK: text from the
        // file never starts with one (it would turn into a live formula).
        XValue::Text(s) => s.trim_start_matches(MARK).to_string(),
        XValue::Bool(b) => (if *b { "TRUE" } else { "FALSE" }).to_string(),
        XValue::Error(e) => e.clone(),
        XValue::Empty => String::new(),
    }
}

/// The functions and defined names in formula `text` that would evaluate
/// to `#NAME?` here: not built in, not an evaluator form, and not one of the
/// Scheme `define-grid-function`s in `externals`. Empty if it doesn't parse
/// (the cell then loads as it always did).
fn unsupported_names(text: &str, externals: &HashSet<String>) -> Vec<String> {
    use crate::formula::ast::Expr;
    use crate::formula::func;
    fn walk(e: &Expr, externals: &HashSet<String>, out: &mut Vec<String>) {
        match e {
            Expr::Call(name, _)
                if func::lookup(name).is_none()
                    && func::special(name).is_none()
                    && !externals.contains(&**name) =>
            {
                out.push(name.to_string());
            }
            Expr::Name(name) => out.push(name.to_string()),
            _ => {}
        }
        e.for_each_child(&mut |c| walk(c, externals, out));
    }
    let mut out = Vec::new();
    if let Ok(expr) = crate::formula::parse::parse(text) {
        walk(&expr, externals, &mut out);
    }
    out
}

/// Load an `.xlsx` package, showing sheet `index` (0 = the first).
/// `externals` are the Scheme grid functions defined now (upper case):
/// formulas calling only those and built-ins load live; any other formula
/// keeps the result Excel saved for it (see [`unsupported_names`]). Every
/// sheet is read, since a save writes every sheet.
pub(super) fn load(
    bytes: &[u8],
    index: usize,
    externals: &HashSet<String>,
) -> Result<Loaded, String> {
    if bytes.is_empty() {
        // A new file: an empty sheet, created on first save.
        return Ok(Loaded {
            wb: Workbook::from_cells("Sheet1", Vec::new()),
            lossy: Vec::new(),
            sheets: vec!["Sheet1".into()],
            others: HashMap::new(),
            text_cells: HashMap::new(),
            formats: Vec::new(),
            data: vec![SheetData::default()],
        });
    }
    let book = xlsx::read(bytes).map_err(|e| e.to_string())?;
    if book.sheets.is_empty() {
        return Err("the workbook has no worksheets".into());
    }
    let Some(sheet) = book.sheets.get(index) else {
        return Err(format!("the workbook has no sheet {}", index + 1));
    };
    let sheets: Vec<String> = book.sheets.iter().map(|s| s.name.clone()).collect();
    let mut lossy = book.not_preserved.clone();
    if book.date1904 {
        lossy.push("the 1904 date system".into());
    }
    // Names this grid can't evaluate, across the workbook (sorted for the
    // message).
    let mut unsupported = std::collections::BTreeSet::new();
    let data: Vec<SheetData> = book
        .sheets
        .iter()
        .map(|s| sheet_data(&book, s, externals, &mut lossy, &mut unsupported))
        .collect();
    let shown = &data[index];
    // Stored densely: a single far-away cell (XFD1048576) would ask for a
    // vast grid.
    let (rows, cols) = shown.cells.iter().fold((0, 0), |(r, c), (row, col, _)| {
        (r.max(row + 1), c.max(col + 1))
    });
    if !crate::grid::model::fits(rows, cols) {
        return Err(format!(
            "sheet \"{}\" spans {rows} rows × {cols} columns, too large to open",
            sheet.name
        ));
    }
    if !unsupported.is_empty() {
        const SHOWN: usize = 5;
        let mut names: Vec<_> = unsupported.iter().take(SHOWN).cloned().collect();
        if unsupported.len() > SHOWN {
            names.push(format!("{} more", unsupported.len() - SHOWN));
        }
        lossy.push(format!(
            "formulas using {} (unsupported here; showing their saved results)",
            names.join(", ")
        ));
    }
    let wb = Workbook::from_cells(&sheet.name, shown.cells.clone());
    let others = sheets
        .iter()
        .zip(&data)
        .enumerate()
        .filter(|&(i, _)| i != index)
        .map(|(_, (name, d))| (name.to_lowercase(), d.values.clone()))
        .collect();
    Ok(Loaded {
        wb,
        lossy,
        sheets,
        others,
        text_cells: shown.text.clone(),
        formats: shown.formats.clone(),
        data,
    })
}

/// One sheet of a read workbook as grid contents, noting in `lossy` what a
/// rewrite would lose and in `unsupported` the names its formulas use that
/// this grid can't evaluate.
fn sheet_data(
    book: &xlsx::Book,
    sheet: &xlsx::XSheet,
    externals: &HashSet<String>,
    lossy: &mut Vec<String>,
    unsupported: &mut std::collections::BTreeSet<String>,
) -> SheetData {
    // Shared formulas: the master carries the text; each child is the
    // master's formula moved to the child's cell.
    let mut masters: HashMap<u32, (usize, usize, String)> = HashMap::new();
    for c in &sheet.cells {
        if let Some(XFormula::SharedMaster { si, text, .. }) = &c.formula {
            masters.insert(*si, (c.row, c.col, text.clone()));
        }
    }
    let mut cells = Vec::with_capacity(sheet.cells.len());
    let mut text_cells = HashMap::new();
    let mut formats = Vec::new();
    for c in &sheet.cells {
        if let Some(st) = book.styles.get(c.style).filter(|s| !s.is_default()) {
            match cell_fmt(st) {
                Some(f) => formats.push(((c.row, c.col), f)),
                None if !lossy.iter().any(|l| l == "cell colors") => {
                    lossy.push("cell colors".into());
                }
                None => {}
            }
        }
        let formula = match &c.formula {
            Some(XFormula::Normal(f)) | Some(XFormula::SharedMaster { text: f, .. }) => {
                Some(format!("={f}"))
            }
            Some(XFormula::Array { text, .. }) => Some(format!("={text}")),
            Some(XFormula::SharedChild { si }) => masters.get(si).map(|(mr, mc, f)| {
                super::grid_formula::translated(
                    &format!("={f}"),
                    c.row as isize - *mr as isize,
                    c.col as isize - *mc as isize,
                )
            }),
            None => None,
        };
        // A formula we can't evaluate would recalculate to `#NAME?` and
        // replace the result Excel computed: keep that result instead.
        let formula = formula.filter(|f| {
            let missing = unsupported_names(f, externals);
            let ok = missing.is_empty();
            unsupported.extend(missing);
            ok
        });
        let live = formula.is_some();
        let text = match formula {
            Some(f) => store_formula(&f),
            None => value_text(&c.value),
        };
        if !text.is_empty() {
            if !live
                && matches!(c.value, XValue::Text(_))
                && !matches!(plain_value(&text), XValue::Text(_))
            {
                text_cells.insert((c.row, c.col), text.clone());
            }
            cells.push((c.row, c.col, text));
        }
    }
    let values = SheetValues::from_cells(
        sheet
            .cells
            .iter()
            .map(|c| (c.row, c.col, cached_value(&c.value))),
    );
    SheetData {
        cells,
        text: text_cells,
        formats,
        values,
    }
}

/// A number from the file as cell text: as Excel shows it (15 significant
/// digits) when that is exactly the stored value, else the shortest text
/// that reads back as it — so a save never changes a number.
fn number_text(n: f64) -> String {
    let shown = general_format(n);
    if shown.parse::<f64>() == Ok(n) {
        shown
    } else {
        format!("{n}")
    }
}

/// How a plain (non-formula) cell's text is written: as a number only when
/// it is exactly a number's text (`12`, `3.5`), so `007`, `1e5` and
/// `12345678901234567890` stay the text that was typed; `TRUE`/`FALSE` as
/// booleans; an error's text (`#N/A`) as that error; everything else as
/// text. (A cell the file stored as text keeps that type; see
/// `XlsxState::text`.)
fn plain_value(text: &str) -> XValue {
    match Value::from_cell_text(text) {
        Value::Number(n) if number_text(n) == text => XValue::Number(n),
        Value::Bool(b) if text == "TRUE" || text == "FALSE" => XValue::Bool(b),
        _ if ErrorKind::parse(text).is_some() => XValue::Error(text.to_string()),
        _ => XValue::Text(text.to_string()),
    }
}

/// A cell value as stored in the file (for a formula, its cached result).
fn cached_value(v: &XValue) -> Value {
    match v {
        XValue::Number(n) => Value::Number(*n),
        XValue::Text(s) => Value::text(s),
        XValue::Bool(b) => Value::Bool(*b),
        XValue::Error(e) => ErrorKind::parse(e).map_or_else(|| Value::text(e), Value::Error),
        XValue::Empty => Value::Empty,
    }
}

fn result_value(v: &Value) -> XValue {
    match v {
        Value::Number(n) => XValue::Number(*n),
        Value::Text(s) => XValue::Text(s.to_string()),
        Value::Bool(b) => XValue::Bool(*b),
        Value::Error(e) => XValue::Error(e.as_str().to_string()),
        Value::Empty => XValue::Empty,
    }
}

impl Editor {
    /// `grid-open-sheet`: show sheet `name` of the active grid's workbook.
    /// The first sheet is the file's own buffer; every other sheet opens as
    /// a buffer of its own, `report.xlsx<Costs>`, which visits no file:
    /// saving it saves the whole workbook. An open sheet buffer is reused.
    pub(super) fn grid_open_sheet(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            self.echo("grid-open-sheet: the current buffer is not a grid", cx);
            return;
        };
        let name = name.trim();
        let Some(book) = g.xlsx.book.clone() else {
            match g.xlsx.sheets.first() {
                Some(only) if only.eq_ignore_ascii_case(name) => {
                    self.echo(format!("Already showing sheet {only}"), cx);
                }
                Some(only) => self.echo(format!("No sheet named {name:?} (sheets: {only})"), cx),
                None => self.echo(
                    "grid-open-sheet: this grid has one sheet (grid-add-sheet adds another)",
                    cx,
                ),
            }
            return;
        };
        let (index, all) = {
            let b = book.borrow();
            (b.index_of(name), b.names.join(", "))
        };
        let Some(index) = index else {
            self.echo(format!("No sheet named {name:?} (sheets: {all})"), cx);
            return;
        };
        if index == g.xlsx.sheet {
            let sheet = g.sheet_name_shown().to_string();
            self.echo(format!("Already showing sheet {sheet}"), cx);
            return;
        }
        // Reuse this workbook's buffer for the sheet — found by workbook,
        // not by name: two workbooks can share a file name.
        let open = self.book_slots(&book).into_iter().find(|&s| {
            self.doc_at(s)
                .grid
                .as_deref()
                .is_some_and(|g| g.xlsx.sheet == index)
        });
        if let Some(s) = open {
            let name = self.doc_at(s).name.clone();
            self.switch_to_buffer(&name, cx);
            return;
        }
        self.grid_install_sheet(&book, index, cx);
    }

    /// Hand each listed sheet buffer's values (`None` = the active buffer)
    /// to the other open sheets of its workbook; a sheet whose results change
    /// in turn passes its own on, so chains across sheets settle.
    pub(super) fn grid_sync_sheets_from(
        &mut self,
        mut work: Vec<Option<usize>>,
        cx: &mut Context<Self>,
    ) {
        let mut rounds = 0;
        let mut changed_any = false;
        while let Some(from) = work.pop() {
            // A cycle between sheets settles like a cycle of cells would
            // not: stop after a bounded number of hand-offs.
            rounds += 1;
            if rounds > 64 {
                break;
            }
            let book = workbook_of(self.doc_at(from));
            if book.is_none() {
                continue;
            }
            let (index, name, values) = {
                let Some(g) = self.doc_at_mut(from).grid.as_deref_mut() else {
                    continue;
                };
                let key = (g.wb.generation(), g.formulas.evaluations());
                if g.xlsx.sheet_published == Some(key) || g.formulas.busy() {
                    continue;
                }
                g.xlsx.sheet_published = Some(key);
                (
                    g.xlsx.sheet,
                    g.sheet_name_shown().to_string(),
                    g.sheet_values(),
                )
            };
            let lower = name.to_lowercase();
            let slots: Vec<Option<usize>> = std::iter::once(None)
                .chain((0..self.inactive.len()).map(Some))
                .filter(|&s| s != from)
                .collect();
            for to in slots {
                if !same_book(&workbook_of(self.doc_at(to)), &book) {
                    continue;
                }
                let Some(g) = self.doc_at_mut(to).grid.as_deref_mut() else {
                    continue;
                };
                if g.xlsx.sheet == index || g.xlsx.other_sheets.get(&lower) == Some(&values) {
                    continue;
                }
                g.xlsx.other_sheets.insert(lower.clone(), values.clone());
                g.formulas.sheet_changed(&name);
                while g.recalc_slice() {}
                changed_any = true;
                work.push(to);
            }
        }
        if changed_any {
            cx.notify();
        }
    }

    pub(super) fn doc_at(&self, slot: Option<usize>) -> &super::Document {
        match slot {
            None => &self.active,
            Some(i) => &self.inactive[i],
        }
    }

    pub(super) fn doc_at_mut(&mut self, slot: Option<usize>) -> &mut super::Document {
        match slot {
            None => &mut self.active,
            Some(i) => &mut self.inactive[i],
        }
    }
}

/// The workbook a grid buffer shows one sheet of, when it has a store.
fn workbook_of(doc: &super::Document) -> Option<Book> {
    doc.grid.as_deref()?.xlsx.book.clone()
}

/// Whether two buffers' workbooks are the same one.
fn same_book(a: &Option<Book>, b: &Option<Book>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if Rc::ptr_eq(a, b))
}

impl GridDoc {
    /// This sheet's values as other sheets' formulas read them.
    pub(super) fn sheet_values(&self) -> SheetValues {
        let (rows, cols) = self.used();
        let mut cells = Vec::new();
        for c in 0..cols {
            self.wb.for_each_in_column(SHEET, c, 0..rows, |r, raw| {
                if raw.is_empty() {
                    return;
                }
                let v = if is_formula(raw) {
                    self.formulas.result(r, c).cloned().unwrap_or(Value::Empty)
                } else {
                    Value::from_cell_text(raw)
                };
                cells.push((r, c, v));
            });
        }
        SheetValues::from_cells(cells)
    }

    /// This sheet's contents as saved: every non-empty cell, the cells kept
    /// as text, the formatting, and the values (formula results).
    pub(super) fn sheet_data(&self) -> SheetData {
        let (rows, cols) = self.used();
        let mut cells = Vec::new();
        for c in 0..cols {
            self.wb.for_each_in_column(SHEET, c, 0..rows, |r, raw| {
                if !raw.is_empty() {
                    cells.push((r, c, raw.to_string()));
                }
            });
        }
        SheetData {
            cells,
            text: self.xlsx.text.clone(),
            formats: self
                .formats
                .positions()
                .map(|(r, c)| ((r, c), self.formats.get(r, c)))
                .collect(),
            values: self.sheet_values(),
        }
    }

    /// The grid as an `.xlsx` package: every non-empty cell, formulas with
    /// their current results cached (so other apps show values at once),
    /// and every formatted cell's formatting. A sheet of a workbook with
    /// several sheets writes them all: this one as it is now, the others
    /// from the shared [`BookStore`] (which the editor brings up to date
    /// with their open buffers before a save; see
    /// [`Editor::grid_flush_book`]).
    pub(super) fn xlsx_bytes(&self) -> Vec<u8> {
        let own = self.sheet_data();
        let Some(book) = self.xlsx.book.as_ref() else {
            return write_sheets(&[(&self.wb.sheet(SHEET).name, &own)]);
        };
        let b = book.borrow();
        let me = self.xlsx.sheets.get(self.xlsx.sheet);
        let empty = SheetData::default();
        let sheets: Vec<(&str, &SheetData)> = b
            .names
            .iter()
            .map(|n| {
                let data = if Some(n) == me {
                    &own
                } else {
                    b.sheets.get(&n.to_lowercase()).unwrap_or(&empty)
                };
                (n.as_str(), data)
            })
            .collect();
        write_sheets(&sheets)
    }
}

/// A cell to write, owned: position, value, formula text, formatting.
type OwnedCell = (usize, usize, XValue, Option<String>, Option<XStyle>);

/// A sheet's cells as written: formulas with their results cached, cells the
/// file stored as text kept as text, and formatted cells without a value as
/// formatting only.
fn owned_cells(d: &SheetData) -> Vec<OwnedCell> {
    let styles: HashMap<(usize, usize), XStyle> =
        d.formats.iter().map(|&(at, f)| (at, x_style(f))).collect();
    let mut filled = HashSet::new();
    let mut out = Vec::with_capacity(d.cells.len());
    for (r, c, raw) in &d.cells {
        if raw.is_empty() {
            continue;
        }
        let (r, c) = (*r, *c);
        filled.insert((r, c));
        let (value, formula) = if is_formula(raw) {
            let f = display_text(raw).trim_start_matches('=').to_string();
            (result_value(&d.values.get(r, c)), Some(f))
        } else if d.text.get(&(r, c)).is_some_and(|t| t == raw) {
            (XValue::Text(raw.to_string()), None)
        } else {
            (plain_value(raw), None)
        };
        out.push((r, c, value, formula, styles.get(&(r, c)).cloned()));
    }
    for (&(r, c), st) in &styles {
        if !filled.contains(&(r, c)) {
            out.push((r, c, XValue::Empty, None, Some(st.clone())));
        }
    }
    out
}

/// An `.xlsx` package of `sheets`, in order.
fn write_sheets(sheets: &[(&str, &SheetData)]) -> Vec<u8> {
    let owned: Vec<Vec<OwnedCell>> = sheets.iter().map(|(_, d)| owned_cells(d)).collect();
    xlsx::write_book(
        sheets
            .iter()
            .zip(&owned)
            .map(|((name, _), cells)| {
                let cells = cells.iter().map(|(r, c, v, f, st)| WriteCell {
                    row: *r,
                    col: *c,
                    value: v,
                    formula: f.as_deref(),
                    style: st.as_ref(),
                });
                (*name, cells)
            })
            .collect(),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_values_keep_their_text() {
        assert_eq!(plain_value("12"), XValue::Number(12.0));
        assert_eq!(plain_value("3.5"), XValue::Number(3.5));
        assert_eq!(plain_value("007"), XValue::Text("007".into()));
        assert_eq!(plain_value("1e5"), XValue::Text("1e5".into()));
        assert_eq!(plain_value("TRUE"), XValue::Bool(true));
        assert_eq!(plain_value("true"), XValue::Text("true".into()));
        assert_eq!(plain_value("hello"), XValue::Text("hello".into()));
    }

    #[test]
    fn written_workbook_loads_back() {
        let wb = Workbook::from_cells(
            "Data",
            vec![
                (0, 0, "n".into()),
                (1, 0, "2".into()),
                (2, 0, "007".into()),
                (3, 0, store_formula("=A2*10")),
            ],
        );
        let mut cells = Vec::new();
        for (r, c, raw) in [(0, 0, "n"), (1, 0, "2"), (2, 0, "007")] {
            cells.push((r, c, plain_value(raw), None::<String>));
        }
        cells.push((3, 0, XValue::Number(20.0), Some("A2*10".into())));
        let bytes = xlsx::write(
            "Data",
            cells.iter().map(|(r, c, v, f)| WriteCell {
                row: *r,
                col: *c,
                value: v,
                formula: f.as_deref(),
                style: None,
            }),
            false,
        );
        let loaded = load(&bytes, 0, &HashSet::new()).unwrap();
        assert!(loaded.lossy.is_empty(), "{:?}", loaded.lossy);
        assert_eq!(loaded.wb.cell(SHEET, 2, 0), "007");
        assert_eq!(loaded.wb.cell(SHEET, 3, 0), store_formula("=A2*10"));
        assert_eq!(loaded.wb.sheet(SHEET).name, "Data");
        drop(wb);
    }

    fn book_of(cells: &[(usize, usize, XValue, Option<&str>)]) -> Vec<u8> {
        xlsx::write(
            "S",
            cells.iter().map(|(r, c, v, f)| WriteCell {
                row: *r,
                col: *c,
                value: v,
                formula: *f,
                style: None,
            }),
            false,
        )
    }

    /// Load `bytes` as a grid and save it again, through the grid's own
    /// save path.
    fn resave_grid(bytes: &[u8]) -> crate::xlsx::Book {
        let l = load(bytes, 0, &HashSet::new()).unwrap();
        assert!(l.lossy.is_empty(), "{:?}", l.lossy);
        let opts = super::super::grid::GridOptions::default();
        let mut g = GridDoc::new(l.wb, Some("xlsx".into()), 0, &opts);
        g.xlsx.text = l.text_cells;
        xlsx::read(&g.xlsx_bytes()).unwrap()
    }

    /// A formula calling a function the grid lacks keeps Excel's saved
    /// result (not `#NAME?`), cells built on it compute from that result, and
    /// the load is flagged lossy so a save can't overwrite the original. A
    /// Scheme grid function of that name makes the formula live.
    #[test]
    fn unsupported_functions_keep_their_cached_results() {
        let bytes = book_of(&[
            (0, 0, XValue::Number(-150.0), Some("PMT(0.05,10,1000)")),
            (1, 0, XValue::Number(-300.0), Some("A1*2")),
            (2, 0, XValue::Number(3.0), Some("SUM(1,2)")),
        ]);
        let l = load(&bytes, 0, &HashSet::new()).unwrap();
        assert_eq!(l.wb.cell(SHEET, 0, 0), "-150");
        assert_eq!(l.wb.cell(SHEET, 1, 0), store_formula("=A1*2"));
        assert_eq!(l.wb.cell(SHEET, 2, 0), store_formula("=SUM(1,2)"));
        assert!(l.lossy.iter().any(|m| m.contains("PMT")), "{:?}", l.lossy);

        let externals: HashSet<String> = ["PMT".to_string()].into_iter().collect();
        let l = load(&bytes, 0, &externals).unwrap();
        assert_eq!(l.wb.cell(SHEET, 0, 0), store_formula("=PMT(0.05,10,1000)"));
        assert!(l.lossy.is_empty(), "{:?}", l.lossy);
    }

    #[test]
    fn numbers_survive_a_round_trip_exactly() {
        let bytes = book_of(&[
            (0, 0, XValue::Number(12345678901234567.0), None),
            (1, 0, XValue::Number(0.1 + 0.2), None),
            (2, 0, XValue::Number(2.5), None),
        ]);
        let l = load(&bytes, 0, &HashSet::new()).unwrap();
        // Shown as Excel would where that's exact.
        assert_eq!(l.wb.cell(SHEET, 2, 0), "2.5");
        let book = resave_grid(&bytes);
        let v: Vec<XValue> = book.sheets[0]
            .cells
            .iter()
            .map(|c| c.value.clone())
            .collect();
        assert_eq!(
            v,
            [
                XValue::Number(12345678901234567.0),
                XValue::Number(0.1 + 0.2),
                XValue::Number(2.5)
            ]
        );
    }

    #[test]
    fn cell_types_survive_a_round_trip() {
        let bytes = book_of(&[
            (0, 0, XValue::Text("12".into()), None),
            (1, 0, XValue::Error("#N/A".into()), None),
            (2, 0, XValue::Text("TRUE".into()), None),
            (3, 0, XValue::Number(12.0), None),
        ]);
        let book = resave_grid(&bytes);
        let v: Vec<XValue> = book.sheets[0]
            .cells
            .iter()
            .map(|c| c.value.clone())
            .collect();
        assert_eq!(
            v,
            [
                XValue::Text("12".into()),
                XValue::Error("#N/A".into()),
                XValue::Text("TRUE".into()),
                XValue::Number(12.0)
            ]
        );
    }

    #[test]
    fn a_far_away_cell_is_refused() {
        let bytes = book_of(&[(1_048_575, 16_383, XValue::Number(1.0), None)]);
        assert!(load(&bytes, 0, &HashSet::new()).is_err_and(|e| e.contains("too large")));
    }

    #[test]
    fn text_from_the_file_never_becomes_a_formula() {
        let bytes = book_of(&[(0, 0, XValue::Text("\u{1}=1+1".into()), None)]);
        let l = load(&bytes, 0, &HashSet::new()).unwrap();
        assert!(!is_formula(l.wb.cell(SHEET, 0, 0)));
        assert_eq!(l.wb.cell(SHEET, 0, 0), "=1+1");
    }

    #[test]
    fn regression_text_number_keeps_type_after_edit_and_undo() {
        let bytes = book_of(&[(0, 0, XValue::Text("12".into()), None)]);
        let l = load(&bytes, 0, &HashSet::new()).unwrap();
        let opts = super::super::grid::GridOptions::default();
        let mut g = GridDoc::new(l.wb, Some("xlsx".into()), 0, &opts);
        g.xlsx.text = l.text_cells;
        g.model_op(|wb| wb.set_cells(SHEET, &[(0usize, 0usize, "x".to_string())]));
        while g.absorb_changes() {}
        // `u`: a Model { steps: 1, layout: None } entry replays wb.undo().
        g.wb.undo();
        while g.absorb_changes() {}
        assert_eq!(g.wb.cell(SHEET, 0, 0), "12");
        let book = xlsx::read(&g.xlsx_bytes()).unwrap();
        assert_eq!(book.sheets[0].cells[0].value, XValue::Text("12".into()));
    }
}
