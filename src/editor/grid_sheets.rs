//! Workbook sheets: adding, renaming, deleting and moving the sheets of a
//! grid's workbook (`grid-add-sheet`, `grid-rename-sheet`,
//! `grid-delete-sheet`, `grid-move-sheet`), opening one, and saving every
//! sheet of a workbook at once.
//!
//! A workbook with several sheets keeps a [`BookStore`], shared by the
//! buffers showing its sheets: the sheet list, and each sheet's contents as
//! of the last load or save. An open sheet's buffer holds its live contents;
//! a save first copies every open sheet into the store
//! ([`Editor::grid_flush_book`]), then writes them all, so saving from any
//! sheet buffer saves the whole workbook and marks every one of them saved.
//! A CSV grid that grows a second sheet gets a store with no file: it saves
//! through the Save As .xlsx prompt.
//!
//! Each sheet operation is one undo step, in the buffer shown after it (the
//! buffer it ran in; for deleting the sheet shown, the sheet shown next).
//! Undoing it there reverses it across the workbook: its own reference
//! rewrites are undone with it, and those it made in other open sheet
//! buffers are reversed as edits of their own in those buffers.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::Context;

use super::grid::{GridDoc, SHEET, bytes_hash};
use super::grid_xlsx::{Book, BookStore, SheetData};
use super::{Editor, MinibufferPurpose};
use crate::formula::ast::Expr;
use crate::formula::engine::{display_text, is_formula, store_formula};
use crate::formula::{parse, print, refs};
use crate::grid::model::Workbook;

/// A cell a sheet operation rewrote outside the buffer its undo step lives
/// in: in an open sheet buffer, or in the store's copy of a sheet.
#[derive(Clone, Debug)]
pub(super) struct Fixup {
    /// In the store (`true`) or in the sheet's open buffer.
    store: bool,
    /// The sheet's name, lower case.
    sheet: String,
    row: usize,
    col: usize,
    before: String,
    after: String,
}

/// One sheet operation, as its undo step remembers it.
#[derive(Clone, Debug)]
pub(super) enum SheetStep {
    /// Sheet `name` was added at `index`; `data` is what it held when the
    /// add was undone (put back by a redo).
    Add {
        index: usize,
        name: String,
        data: Rc<RefCell<SheetData>>,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Sheet `name` at `index` was deleted: what it held, the references to
    /// it other sheets lost, and whether it was the sheet shown.
    Delete {
        index: usize,
        name: String,
        data: Rc<RefCell<SheetData>>,
        fixups: Rc<RefCell<Vec<Fixup>>>,
        shown: bool,
    },
    Move {
        from: usize,
        to: usize,
    },
}

/// Why `name` can't name a sheet of `names` (ignoring sheet `except`), or
/// the name, trimmed: Excel's rules — not blank, none of `[]:*?/\`, no
/// apostrophe at either end, at most 31 characters, and unique ignoring case.
fn valid_name(name: &str, names: &[String], except: Option<usize>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a sheet name can't be blank".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| "[]:*?/\\".contains(*c) || c.is_control())
    {
        return Err(format!("a sheet name can't contain {c:?}"));
    }
    if name.starts_with('\'') || name.ends_with('\'') {
        return Err("a sheet name can't start or end with an apostrophe".into());
    }
    if name.encode_utf16().count() > 31 {
        return Err("a sheet name is at most 31 characters".into());
    }
    let lower = name.to_lowercase();
    if names
        .iter()
        .enumerate()
        .any(|(i, n)| Some(i) != except && n.to_lowercase() == lower)
    {
        return Err(format!("there's already a sheet named {name}"));
    }
    Ok(name.to_string())
}

/// The first of Sheet2, Sheet3, … that `names` doesn't use.
pub(super) fn next_sheet_name(names: &[String]) -> String {
    (names.len().max(1) + 1..)
        .map(|n| format!("Sheet{n}"))
        .find(|n| valid_name(n, names, None).is_ok())
        .expect("a free sheet name")
}

/// Formula text `raw` with `f` applied to its expression, if `f` changed it.
fn rewritten(raw: &str, f: &dyn Fn(&mut Expr) -> bool) -> Option<String> {
    if !is_formula(raw) {
        return None;
    }
    let mut e = parse::parse(display_text(raw)).ok()?;
    f(&mut e).then(|| store_formula(&print::print(&e)))
}

/// Every formula cell of a grid that `f` rewrites: `(row, col, before, after)`.
fn grid_rewrites(
    g: &GridDoc,
    f: &dyn Fn(&mut Expr) -> bool,
) -> Vec<(usize, usize, String, String)> {
    let (rows, cols) = g.used();
    let mut out = Vec::new();
    for c in 0..cols {
        g.wb.for_each_in_column(SHEET, c, 0..rows, |r, raw| {
            if let Some(new) = rewritten(raw, f) {
                out.push((r, c, raw.to_string(), new));
            }
        });
    }
    out
}

/// Rewrite the formulas of a stored sheet with `f`; returns what changed.
fn data_rewrites(
    d: &mut SheetData,
    f: &dyn Fn(&mut Expr) -> bool,
) -> Vec<(usize, usize, String, String)> {
    let mut out = Vec::new();
    for (r, c, raw) in &mut d.cells {
        if let Some(new) = rewritten(raw, f) {
            out.push((*r, *c, std::mem::replace(raw, new.clone()), new));
        }
    }
    out
}

impl GridDoc {
    /// The name of the sheet this buffer shows (`""` for a lone CSV).
    pub(super) fn sheet_name_shown(&self) -> &str {
        self.xlsx
            .sheets
            .get(self.xlsx.sheet)
            .map_or(&self.wb.sheet(SHEET).name, |s| s)
    }

    /// Set cells as one model edit; `while` absorbing it into the formulas.
    fn set_cells_now(&mut self, edits: Vec<(usize, usize, String)>) {
        if !edits.is_empty() {
            self.model_op(|wb| wb.set_cells(SHEET, &edits));
        }
    }

    /// Bring formulas up to date with edits and with sheets `changed`
    /// (references to them resolve anew), to completion.
    fn settle_sheets(&mut self, changed: &[&str]) {
        self.absorb_changes();
        for name in changed {
            self.formulas.sheet_changed(name);
        }
        while self.recalc_slice() {}
    }
}

impl Editor {
    /// The workbook the active grid belongs to, if it has a store.
    pub(super) fn active_book(&self) -> Option<Book> {
        self.active.grid.as_deref()?.xlsx.book.clone()
    }

    /// The open buffers showing sheets of `book` (`None`: the active one).
    pub(super) fn book_slots(&self, book: &Book) -> Vec<Option<usize>> {
        std::iter::once(None)
            .chain((0..self.inactive.len()).map(Some))
            .filter(|&s| {
                self.doc_at(s)
                    .grid
                    .as_deref()
                    .and_then(|g| g.xlsx.book.as_ref())
                    .is_some_and(|b| Rc::ptr_eq(b, book))
            })
            .collect()
    }

    /// The live workbook (one with a store) saved to `path`, if a buffer
    /// shows a sheet of one.
    fn live_book_for(&self, path: &Path) -> Option<Book> {
        let id = super::buffers::file_identity(path);
        self.docs()
            .filter_map(|d| d.grid.as_deref()?.xlsx.book.clone())
            .find(|b| {
                b.borrow()
                    .path
                    .as_deref()
                    .is_some_and(|p| super::buffers::file_identity(p) == id)
            })
    }

    /// The name sheet buffers of `book` are called after (`report.xlsx`).
    fn book_label(&self, book: &Book) -> String {
        let file_name = |p: &Path| p.file_name().map(|f| f.to_string_lossy().into_owned());
        if let Some(name) = book.borrow().path.as_deref().and_then(file_name) {
            return name;
        }
        self.book_slots(book)
            .into_iter()
            .find_map(|s| self.doc_at(s).path.as_deref().and_then(file_name))
            .unwrap_or_else(|| "workbook".into())
    }

    /// The active grid's workbook, made for it if it has none yet (one sheet,
    /// the grid itself). `None` (with a message) when it isn't a grid.
    fn ensure_book(&mut self, what: &str, cx: &mut Context<Self>) -> Option<Book> {
        let path = self.active.path.clone();
        let Some(g) = self.active.grid.as_deref_mut() else {
            self.echo(format!("{what}: the current buffer is not a grid"), cx);
            return None;
        };
        if g.loading.is_some() {
            self.echo(format!("{what}: still loading — try once it finishes"), cx);
            return None;
        }
        if let Some(book) = &g.xlsx.book {
            return Some(book.clone());
        }
        let name = g.sheet_name_shown().to_string();
        let path = path
            .or_else(|| g.xlsx.source.clone())
            .filter(|_| g.is_xlsx());
        let book = BookStore::new(vec![name.clone()], vec![g.sheet_data()], path);
        g.xlsx.sheets = vec![name];
        g.xlsx.sheet = 0;
        g.xlsx.book = Some(book.clone());
        Some(book)
    }

    /// Copy every open sheet of `book` into its store, so it can be written
    /// (or a sheet reopened) as the buffers hold it now.
    pub(super) fn grid_flush_book(&mut self, book: &Book) {
        for s in self.book_slots(book) {
            if let Some(g) = self.doc_at(s).grid.as_deref() {
                let name = g.sheet_name_shown().to_lowercase();
                let data = g.sheet_data();
                book.borrow_mut().sheets.insert(name, data);
            }
        }
    }

    /// After the sheet list changed: every open sheet buffer of `book` takes
    /// the new list and finds its sheet in it (`renamed`: its old and new
    /// name), and its modified flag follows the workbook's.
    fn sheet_resync(&mut self, book: &Book, renamed: Option<(&str, &str)>, cx: &mut Context<Self>) {
        let names = book.borrow().names.clone();
        for s in self.book_slots(book) {
            let doc = self.doc_at_mut(s);
            let Some(g) = doc.grid.as_deref_mut() else {
                continue;
            };
            let mut mine = g.sheet_name_shown().to_string();
            if let Some((from, to)) = renamed
                && mine == from
            {
                mine = to.to_string();
            }
            g.xlsx.sheet = names.iter().position(|n| *n == mine).unwrap_or(0);
            g.xlsx.sheets = names.clone();
            g.xlsx.sheet_published = None;
            doc.modified = g.modified();
        }
        let open: Vec<Option<usize>> = self.book_slots(book);
        self.grid_sync_sheets_from(open, cx);
    }

    // --- Opening a sheet ----------------------------------------------------

    /// Open sheet `index` of `book` in a buffer of its own: the file's own
    /// buffer when it's the first sheet and no buffer visits the file,
    /// otherwise `report.xlsx<Costs>`, which visits no file (saving it saves
    /// the workbook).
    pub(super) fn grid_install_sheet(&mut self, book: &Book, index: usize, cx: &mut Context<Self>) {
        let slots = self.book_slots(book);
        let (lossy, hash) = slots
            .iter()
            .find_map(|&s| self.doc_at(s).grid.as_deref())
            .map(|g| (g.xlsx.lossy.clone(), g.disk_hash()))
            .unwrap_or_default();
        let file_open = slots.iter().any(|&s| self.doc_at(s).path.is_some());
        let (sheet, names, data, others, path) = {
            let b = book.borrow();
            let Some(sheet) = b.names.get(index).cloned() else {
                return;
            };
            let lower = sheet.to_lowercase();
            let others = b
                .names
                .iter()
                .map(|n| n.to_lowercase())
                .filter(|n| *n != lower)
                .map(|n| {
                    let v = b.sheets.get(&n).map(|d| d.values.clone());
                    (n, v.unwrap_or_default())
                })
                .collect();
            let data = b.sheets.get(&lower).cloned().unwrap_or_default();
            (sheet, b.names.clone(), data, others, b.path.clone())
        };
        let (rows, cols) = data.cells.iter().fold((0, 0), |(r, c), (row, col, _)| {
            (r.max(row + 1), c.max(col + 1))
        });
        if !crate::grid::model::fits(rows, cols) {
            self.echo(
                format!("Sheet {sheet} spans {rows} rows × {cols} columns, too large to open"),
                cx,
            );
            return;
        }
        let visits_file = index == 0 && !file_open && path.is_some();
        let mut grid = GridDoc::new(
            Workbook::from_cells(&sheet, data.cells),
            Some("xlsx".into()),
            hash,
            &self.grid_ui.opts,
        );
        grid.xlsx.lossy = lossy;
        grid.xlsx.sheets = names.clone();
        grid.xlsx.sheet = index;
        grid.xlsx.source = if visits_file { None } else { path.clone() };
        grid.xlsx.other_sheets = others;
        grid.xlsx.text = data.text;
        grid.xlsx.book = Some(book.clone());
        for ((r, c), f) in data.formats {
            grid.formats.set(r, c, f);
        }
        grid.mark_saved();
        grid.rebuild_formulas();
        let label = self.book_label(book);
        let key = path.clone().unwrap_or_else(|| PathBuf::from(&label));
        let (buffer, doc_path) = if visits_file {
            (self.unique_buffer_name(&label, &key), path)
        } else {
            (
                self.unique_buffer_name(&format!("{label}<{sheet}>"), &key),
                None,
            )
        };
        self.install_grid_doc(buffer, doc_path, grid, true, cx);
        self.echo(
            format!("Sheet {sheet} ({} of {})", index + 1, names.len()),
            cx,
        );
        self.grid_after(cx);
        // Sheets already open may hold unsaved edits: read theirs, not the
        // store's. They hand their values over again even if unchanged since
        // they last did (there was no one to give them to then).
        for s in self.book_slots(book) {
            if let Some(g) = self.doc_at_mut(s).grid.as_deref_mut() {
                g.xlsx.sheet_published = None;
            }
        }
        let open = self.book_slots(book);
        self.grid_sync_sheets_from(open, cx);
        self.grid_fire_load();
    }

    /// A find-file of `path`, an `.xlsx` whose workbook is open in sheet
    /// buffers but not in a buffer of its own: show its first sheet from
    /// what those buffers hold (not the file), so they stay one workbook.
    /// Returns whether it did.
    pub(super) fn grid_open_live_book(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let Some(book) = self.live_book_for(path) else {
            return false;
        };
        let shown = self.book_slots(&book).into_iter().find(|&s| {
            self.doc_at(s)
                .grid
                .as_deref()
                .is_some_and(|g| g.xlsx.sheet == 0)
        });
        match shown {
            Some(s) => {
                let name = self.doc_at(s).name.clone();
                self.switch_to_buffer(&name, cx);
            }
            None => {
                self.grid_flush_book(&book);
                self.grid_install_sheet(&book, 0, cx);
            }
        }
        true
    }

    // --- Saving -------------------------------------------------------------

    /// Write every sheet of `book` to `path` and mark all its buffers saved.
    /// Returns the number of sheets written.
    pub(super) fn grid_write_book(&mut self, book: &Book, path: &Path) -> std::io::Result<usize> {
        self.grid_flush_book(book);
        let slots = self.book_slots(book);
        if slots
            .iter()
            .filter_map(|&s| self.doc_at(s).grid.as_deref())
            .any(|g| g.loading.is_some())
        {
            return Err(std::io::Error::other(
                "still loading — save once it finishes",
            ));
        }
        let bytes = match slots.first() {
            Some(&s) => self
                .doc_at(s)
                .grid
                .as_deref()
                .map(GridDoc::xlsx_bytes)
                .unwrap_or_default(),
            None => return Err(std::io::Error::other("no sheet of it is open")),
        };
        crate::save::write_now(path, &bytes)?;
        let hash = bytes_hash(&bytes);
        for s in slots {
            let doc = self.doc_at_mut(s);
            if let Some(g) = doc.grid.as_deref_mut() {
                g.saved_as(hash);
            }
            doc.modified = false;
        }
        let mut b = book.borrow_mut();
        b.saved_names = b.names.clone();
        Ok(b.names.len())
    }

    /// `C-x C-s` in a sheet of a workbook with several sheets (or in a sheet
    /// buffer): save the whole workbook. One that came from a CSV asks where
    /// to save it as `.xlsx` first.
    pub(super) fn grid_save_book(&mut self, book: &Book, cx: &mut Context<Self>) {
        let Some(path) = book.borrow().path.clone() else {
            let label = self.book_label(book);
            let csv = self
                .book_slots(book)
                .into_iter()
                .find_map(|s| self.doc_at(s).path.clone())
                .unwrap_or_else(|| PathBuf::from(&label));
            self.open_minibuffer(
                "A workbook with several sheets saves as .xlsx — save as: ",
                MinibufferPurpose::GridSaveAsXlsx,
                &csv.with_extension("xlsx").to_string_lossy(),
                cx,
            );
            return;
        };
        if let Some(lossy) = self
            .active
            .grid
            .as_deref()
            .map(|g| g.xlsx.lossy.join(", "))
            .filter(|l| !l.is_empty())
        {
            self.echo(
                format!("Not saving over this file: it has {lossy}, which would be lost — :w copy.xlsx saves a copy"),
                cx,
            );
            return;
        }
        let saving = self.active.name.clone();
        self.run_hook_blocking("before-save-hook", cx);
        if self.active.name != saving {
            self.echo(
                format!("Not saved: {saving} changed during before-save-hook"),
                cx,
            );
            return;
        }
        let name = path
            .file_name()
            .map_or_else(|| saving.clone(), |f| f.to_string_lossy().into_owned());
        match self.grid_write_book(book, &path) {
            Ok(n) => {
                let sheets = if n == 1 { "sheet" } else { "sheets" };
                self.echo(format!("Wrote {name} ({n} {sheets})"), cx);
                self.run_hook("after-save-hook");
                self.rewatch_file(&path, cx);
            }
            Err(err) => self.echo(format!("Save of {name} failed: {err}"), cx),
        }
    }

    /// Save As `.xlsx` for a workbook with several sheets (one that came from
    /// a CSV, or a copy): write every sheet to `target`, which the workbook
    /// then saves to; the buffer that visited the old file visits it, and
    /// the sheet buffers are named after it.
    pub(super) fn grid_save_book_as(&mut self, book: &Book, target: &Path, cx: &mut Context<Self>) {
        if let Err(err) = self.grid_write_book(book, target) {
            self.echo(format!("Save failed: {err}"), cx);
            return;
        }
        book.borrow_mut().path = Some(target.to_path_buf());
        let label = self.book_label(book);
        let slots = self.book_slots(book);
        // The buffer visiting the old file (else the active one) visits the
        // new one.
        let file_buffer = slots
            .iter()
            .copied()
            .find(|&s| self.doc_at(s).path.is_some())
            .unwrap_or(None);
        let mut renames = Vec::new();
        for s in slots {
            let doc = self.doc_at_mut(s);
            let Some(g) = doc.grid.as_deref_mut() else {
                continue;
            };
            g.ext = Some("xlsx".into());
            if s == file_buffer {
                g.xlsx.source = None;
                doc.path = Some(target.to_path_buf());
                renames.push((doc.name.clone(), label.clone()));
            } else {
                g.xlsx.source = Some(target.to_path_buf());
                let sheet = g.sheet_name_shown().to_string();
                doc.path = None;
                renames.push((doc.name.clone(), format!("{label}<{sheet}>")));
            }
        }
        for (old, base) in renames {
            if old != base {
                let new = self.unique_buffer_name(&base, target);
                self.rename_doc(&old, &new);
            }
        }
        self.reconcile_watches(cx);
        self.echo(format!("Wrote {label} — the workbook now saves there"), cx);
        self.grid_after(cx);
    }

    /// Save every workbook with a modified sheet buffer (`save-some-buffers`,
    /// saving on quit): each once, whole. Returns what failed.
    pub(super) fn grid_save_modified_books(&mut self) -> Vec<String> {
        let mut books: Vec<Book> = Vec::new();
        for d in self.docs() {
            if let Some(book) = d.grid.as_deref().and_then(|g| g.xlsx.book.as_ref())
                && d.modified
                && book.borrow().saves_whole(d.path.is_some())
                && !books.iter().any(|b| Rc::ptr_eq(b, book))
            {
                books.push(book.clone());
            }
        }
        let mut failed = Vec::new();
        for book in books {
            let label = self.book_label(&book);
            let lossy = self
                .book_slots(&book)
                .into_iter()
                .find_map(|s| self.doc_at(s).grid.as_deref())
                .map(|g| g.xlsx.lossy.join(", "))
                .unwrap_or_default();
            let path = book.borrow().path.clone();
            let result = match path {
                None => Err(std::io::Error::other(
                    "it has several sheets — save it as .xlsx",
                )),
                Some(_) if !lossy.is_empty() => Err(std::io::Error::other(format!(
                    "it has {lossy}, which saving over it would lose — save a copy with :w"
                ))),
                Some(path) => self.grid_write_book(&book, &path).map(|_| ()),
            };
            if let Err(err) = result {
                failed.push(format!("{label}: {err}"));
            }
        }
        failed
    }

    // --- Commands -----------------------------------------------------------

    /// `grid-add-sheet`: add an empty sheet after the last, named `name` (or
    /// the next free SheetN), and show it.
    pub(super) fn grid_add_sheet(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(book) = self.ensure_book("grid-add-sheet", cx) else {
            return;
        };
        let names = book.borrow().names.clone();
        let name = if name.trim().is_empty() {
            next_sheet_name(&names)
        } else {
            match valid_name(name, &names, None) {
                Ok(n) => n,
                Err(e) => {
                    self.echo(format!("Not added: {e}"), cx);
                    return;
                }
            }
        };
        let index = names.len();
        self.sheet_insert(&book, index, &name, SheetData::default());
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.record_sheet_step(SheetStep::Add {
                index,
                name: name.clone(),
                data: Rc::default(),
            });
        }
        self.sheet_resync(&book, None, cx);
        self.grid_install_sheet(&book, index, cx);
        self.echo(format!("Added sheet {name}"), cx);
    }

    /// `grid-rename-sheet`: rename sheet `from` (`""`: the one shown) to `to`,
    /// rewriting every reference to it.
    pub(super) fn grid_rename_sheet(&mut self, from: &str, to: &str, cx: &mut Context<Self>) {
        let csv = self
            .active
            .grid
            .as_deref()
            .is_some_and(|g| !g.is_xlsx() && g.xlsx.book.is_none());
        if csv {
            self.echo(
                "grid-rename-sheet: a CSV file has no sheet names — add a sheet first (grid-add-sheet)",
                cx,
            );
            return;
        }
        let Some(book) = self.ensure_book("grid-rename-sheet", cx) else {
            return;
        };
        let (index, names) = {
            let b = book.borrow();
            let shown = self.active.grid.as_deref().map_or(0, |g| g.xlsx.sheet);
            let index = if from.trim().is_empty() {
                Some(shown)
            } else {
                b.index_of(from.trim())
            };
            (index, b.names.clone())
        };
        let Some(index) = index else {
            self.echo(
                format!(
                    "No sheet named {:?} (sheets: {})",
                    from.trim(),
                    names.join(", ")
                ),
                cx,
            );
            return;
        };
        let to = match valid_name(to, &names, Some(index)) {
            Ok(n) => n,
            Err(e) => {
                self.echo(format!("Not renamed: {e}"), cx);
                return;
            }
        };
        let from = names[index].clone();
        if from == to {
            self.echo(format!("Sheet {from} already has that name"), cx);
            return;
        }
        self.sheet_rename(&book, &from, &to, true, cx);
        self.echo(format!("Renamed sheet {from} to {to}"), cx);
        self.grid_after(cx);
    }

    /// `grid-delete-sheet`: delete sheet `name` (`""`: the one shown); every
    /// reference to it becomes `#REF!`. The last sheet can't be deleted.
    pub(super) fn grid_delete_sheet(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(book) = self.active_book() else {
            self.echo("grid-delete-sheet: this grid has only one sheet", cx);
            return;
        };
        let (index, names) = {
            let b = book.borrow();
            let shown = self.active.grid.as_deref().map_or(0, |g| g.xlsx.sheet);
            let index = if name.trim().is_empty() {
                Some(shown)
            } else {
                b.index_of(name.trim())
            };
            (index, b.names.clone())
        };
        let Some(index) = index else {
            self.echo(
                format!(
                    "No sheet named {:?} (sheets: {})",
                    name.trim(),
                    names.join(", ")
                ),
                cx,
            );
            return;
        };
        if names.len() < 2 {
            self.echo("Not deleted: a workbook keeps at least one sheet", cx);
            return;
        }
        let name = names[index].clone();
        let shown = self
            .active
            .grid
            .as_deref()
            .is_some_and(|g| g.xlsx.sheet == index);
        if shown {
            // The undo step lives in the sheet shown next.
            let next = if index + 1 < names.len() {
                index + 1
            } else {
                index - 1
            };
            self.grid_open_sheet(&names[next], cx);
            if self.active_book().is_none_or(|b| !Rc::ptr_eq(&b, &book))
                || self
                    .active
                    .grid
                    .as_deref()
                    .is_none_or(|g| g.xlsx.sheet != next)
            {
                return;
            }
        }
        let step = SheetStep::Delete {
            index,
            name: name.clone(),
            data: Rc::default(),
            fixups: Rc::default(),
            shown,
        };
        self.sheet_remove(&book, &step, true, cx);
        self.echo(format!("Deleted sheet {name}"), cx);
        self.grid_after(cx);
    }

    /// `grid-move-sheet`: move the sheet shown `by` places (negative: left).
    pub(super) fn grid_move_sheet(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(book) = self.active_book() else {
            self.echo("grid-move-sheet: this grid has only one sheet", cx);
            return;
        };
        let len = book.borrow().names.len();
        let from = self.active.grid.as_deref().map_or(0, |g| g.xlsx.sheet);
        let to = (from as isize + by).clamp(0, len as isize - 1) as usize;
        if to == from {
            self.echo("The sheet is already at that end", cx);
            return;
        }
        self.sheet_move(&book, from, to);
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.record_sheet_step(SheetStep::Move { from, to });
        }
        self.sheet_resync(&book, None, cx);
        let names = book.borrow().names.join(" · ");
        self.echo(format!("Sheets: {names}"), cx);
        self.grid_after(cx);
    }

    // --- The operations, shared by commands, undo and redo ------------------

    /// Put sheet `name` holding `data` at `index`: in the store, and in the
    /// other sheets' view of the workbook (references to it resolve now).
    fn sheet_insert(&mut self, book: &Book, index: usize, name: &str, data: SheetData) {
        let lower = name.to_lowercase();
        let values = data.values.clone();
        {
            let mut b = book.borrow_mut();
            let index = index.min(b.names.len());
            b.names.insert(index, name.to_string());
            b.sheets.insert(lower.clone(), data);
        }
        for s in self.book_slots(book) {
            if let Some(g) = self.doc_at_mut(s).grid.as_deref_mut() {
                g.xlsx.other_sheets.insert(lower.clone(), values.clone());
                g.settle_sheets(&[name]);
            }
        }
    }

    /// Take sheet `name` out of `book` without touching references to it
    /// (undoing an add); its buffer, if open, is killed. Returns what it held.
    fn sheet_take(&mut self, book: &Book, name: &str, cx: &mut Context<Self>) -> SheetData {
        self.grid_flush_book(book);
        let lower = name.to_lowercase();
        let data = {
            let mut b = book.borrow_mut();
            b.names.retain(|n| n.to_lowercase() != lower);
            b.sheets.remove(&lower).unwrap_or_default()
        };
        let mut doomed = None;
        for s in self.book_slots(book) {
            if let Some(g) = self.doc_at_mut(s).grid.as_deref_mut() {
                if g.sheet_name_shown().to_lowercase() == lower {
                    doomed = s;
                    continue;
                }
                g.xlsx.other_sheets.remove(&lower);
                g.settle_sheets(&[name]);
            }
        }
        if let Some(i) = doomed {
            let buffer = self.inactive[i].name.clone();
            self.kill_buffer_named(&buffer, cx);
        }
        data
    }

    /// Rename sheet `from` to `to` across `book`: the sheet list, references
    /// in every sheet (stored and open), and the sheet's buffer. With
    /// `record`, the active buffer's rewrites and the undo step are one step
    /// there; without (an undo or redo, which replayed them), the active
    /// buffer's cells are left alone.
    fn sheet_rename(
        &mut self,
        book: &Book,
        from: &str,
        to: &str,
        record: bool,
        cx: &mut Context<Self>,
    ) {
        let rename = |e: &mut Expr| refs::rename_sheet(e, from, to);
        let (lower_from, lower_to) = (from.to_lowercase(), to.to_lowercase());
        {
            let mut b = book.borrow_mut();
            if let Some(i) = b.names.iter().position(|n| n == from) {
                b.names[i] = to.to_string();
            }
            if let Some(d) = b.sheets.remove(&lower_from) {
                b.sheets.insert(lower_to.clone(), d);
            }
            for d in b.sheets.values_mut() {
                data_rewrites(d, &rename);
            }
        }
        let mut renamed_buffer = None;
        for s in self.book_slots(book) {
            let doc = self.doc_at_mut(s);
            let Some(g) = doc.grid.as_deref_mut() else {
                continue;
            };
            if g.sheet_name_shown() == from {
                g.wb.rename_sheet(SHEET, to);
                if doc.path.is_none() {
                    renamed_buffer = Some(doc.name.clone());
                }
            }
            if let Some(v) = g.xlsx.other_sheets.remove(&lower_from) {
                g.xlsx.other_sheets.insert(lower_to.clone(), v);
            }
            let active = s.is_none();
            if !active || record {
                let edits: Vec<_> = grid_rewrites(g, &rename)
                    .into_iter()
                    .map(|(r, c, _, new)| (r, c, new))
                    .collect();
                let step = SheetStep::Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                };
                if active {
                    g.grouped(|g| {
                        g.set_cells_now(edits);
                        g.record_sheet_step(step);
                    });
                } else {
                    g.set_cells_now(edits);
                }
            }
            g.settle_sheets(&[from, to]);
        }
        if let Some(old) = renamed_buffer {
            let label = self.book_label(book);
            let key = book
                .borrow()
                .path
                .clone()
                .unwrap_or_else(|| PathBuf::from(&label));
            let new = self.unique_buffer_name(&format!("{label}<{to}>"), &key);
            self.rename_doc(&old, &new);
        }
        self.sheet_resync(book, Some((from, to)), cx);
    }

    /// Delete the sheet `step` (a [`SheetStep::Delete`]) names: references to
    /// it become `#REF!` everywhere, its buffer (if open) is killed, and the
    /// step learns what it held and what the rewrite changed. With `record`,
    /// the active buffer's rewrites and the step are one undo step there;
    /// without (a redo), the active buffer's cells are left alone.
    fn sheet_remove(
        &mut self,
        book: &Book,
        step: &SheetStep,
        record: bool,
        cx: &mut Context<Self>,
    ) {
        let SheetStep::Delete {
            name, data, fixups, ..
        } = step
        else {
            return;
        };
        let lower = name.to_lowercase();
        let delete = |e: &mut Expr| refs::delete_sheet(e, name);
        self.grid_flush_book(book);
        let mut fix = Vec::new();
        {
            let mut b = book.borrow_mut();
            b.names.retain(|n| n.to_lowercase() != lower);
            *data.borrow_mut() = b.sheets.remove(&lower).unwrap_or_default();
            for (sheet, d) in b.sheets.iter_mut() {
                for (row, col, before, after) in data_rewrites(d, &delete) {
                    fix.push(Fixup {
                        store: true,
                        sheet: sheet.clone(),
                        row,
                        col,
                        before,
                        after,
                    });
                }
            }
        }
        let mut doomed = None;
        for s in self.book_slots(book) {
            let Some(g) = self.doc_at_mut(s).grid.as_deref_mut() else {
                continue;
            };
            let sheet = g.sheet_name_shown().to_lowercase();
            if sheet == lower {
                doomed = s;
                continue;
            }
            g.xlsx.other_sheets.remove(&lower);
            let active = s.is_none();
            if !active || record {
                let rewrites = grid_rewrites(g, &delete);
                let edits: Vec<_> = rewrites
                    .iter()
                    .map(|(r, c, _, new)| (*r, *c, new.clone()))
                    .collect();
                if active {
                    g.grouped(|g| {
                        g.set_cells_now(edits);
                        g.record_sheet_step(step.clone());
                    });
                } else {
                    g.set_cells_now(edits);
                    fix.extend(rewrites.into_iter().map(|(row, col, before, after)| Fixup {
                        store: false,
                        sheet: sheet.clone(),
                        row,
                        col,
                        before,
                        after,
                    }));
                }
            }
            g.settle_sheets(&[name]);
        }
        *fixups.borrow_mut() = fix;
        if let Some(i) = doomed {
            let buffer = self.inactive[i].name.clone();
            self.kill_buffer_named(&buffer, cx);
        }
        self.sheet_resync(book, None, cx);
    }

    /// Undo [`Self::sheet_remove`]: put the sheet back where it was with what
    /// it held, and the references to it the delete broke (the active
    /// buffer's own came back with its undo).
    fn sheet_restore(&mut self, book: &Book, step: &SheetStep, cx: &mut Context<Self>) {
        let SheetStep::Delete {
            index,
            name,
            data,
            fixups,
            shown,
        } = step
        else {
            return;
        };
        {
            let mut b = book.borrow_mut();
            for f in fixups.borrow().iter().filter(|f| f.store) {
                if let Some(d) = b.sheets.get_mut(&f.sheet)
                    && let Some(cell) = d
                        .cells
                        .iter_mut()
                        .find(|(r, c, t)| (*r, *c) == (f.row, f.col) && *t == f.after)
                {
                    cell.2 = f.before.clone();
                }
            }
        }
        for s in self.book_slots(book) {
            if s.is_none() {
                continue;
            }
            let Some(g) = self.doc_at_mut(s).grid.as_deref_mut() else {
                continue;
            };
            let sheet = g.sheet_name_shown().to_lowercase();
            let edits: Vec<_> = fixups
                .borrow()
                .iter()
                .filter(|f| !f.store && f.sheet == sheet)
                .filter(|f| g.wb.cell(SHEET, f.row, f.col) == f.after)
                .map(|f| (f.row, f.col, f.before.clone()))
                .collect();
            g.set_cells_now(edits);
        }
        let data = data.borrow().clone();
        self.sheet_insert(book, *index, name, data);
        self.sheet_resync(book, None, cx);
        if *shown {
            self.grid_install_sheet(book, *index, cx);
        }
    }

    /// Move sheet `from` to position `to` in `book`'s list.
    fn sheet_move(&mut self, book: &Book, from: usize, to: usize) {
        let mut b = book.borrow_mut();
        if from < b.names.len() && to < b.names.len() {
            let name = b.names.remove(from);
            b.names.insert(to, name);
        }
    }

    /// Carry out the sheet operations an undo or redo in the active buffer
    /// just replayed (its own cells already are).
    pub(super) fn grid_sheet_replays(&mut self, cx: &mut Context<Self>) {
        let Some(replays) = self
            .active
            .grid
            .as_deref_mut()
            .map(|g| std::mem::take(&mut g.xlsx.sheet_replays))
            .filter(|r| !r.is_empty())
        else {
            return;
        };
        let Some(book) = self.active_book() else {
            return;
        };
        for (step, undo) in replays {
            match (&step, undo) {
                (SheetStep::Add { name, data, .. }, true) => {
                    *data.borrow_mut() = self.sheet_take(&book, name, cx);
                    self.sheet_resync(&book, None, cx);
                    self.echo(format!("Undid adding sheet {name}"), cx);
                }
                (SheetStep::Add { index, name, data }, false) => {
                    let d = data.borrow().clone();
                    self.sheet_insert(&book, *index, name, d);
                    self.sheet_resync(&book, None, cx);
                    self.echo(format!("Added sheet {name} again"), cx);
                }
                (SheetStep::Rename { from, to }, true) => {
                    self.sheet_rename(&book, to, from, false, cx);
                    self.echo(format!("Sheet {to} is {from} again"), cx);
                }
                (SheetStep::Rename { from, to }, false) => {
                    self.sheet_rename(&book, from, to, false, cx);
                    self.echo(format!("Renamed sheet {from} to {to} again"), cx);
                }
                (SheetStep::Delete { name, .. }, true) => {
                    self.sheet_restore(&book, &step, cx);
                    self.echo(format!("Restored sheet {name}"), cx);
                }
                (SheetStep::Delete { name, .. }, false) => {
                    self.sheet_remove(&book, &step, false, cx);
                    self.echo(format!("Deleted sheet {name} again"), cx);
                }
                (SheetStep::Move { from, to }, true) => {
                    self.sheet_move(&book, *to, *from);
                    self.sheet_resync(&book, None, cx);
                }
                (SheetStep::Move { from, to }, false) => {
                    self.sheet_move(&book, *from, *to);
                    self.sheet_resync(&book, None, cx);
                }
            }
        }
    }

    /// Rename buffer `old` to `new` (and every window showing it).
    fn rename_doc(&mut self, old: &str, new: &str) {
        if self.active.name == old {
            self.active.name = new.to_string();
        } else if let Some(d) = self.inactive.iter_mut().find(|d| d.name == old) {
            d.name = new.to_string();
        } else {
            return;
        }
        for pane in &mut self.panes {
            if pane.buffer == old {
                pane.buffer = new.to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_names_follow_excel_rules() {
        let names = vec!["Sheet1".to_string(), "Costs".to_string()];
        assert_eq!(valid_name(" Q1 Sales ", &names, None).unwrap(), "Q1 Sales");
        assert!(valid_name("costs", &names, None).is_err());
        assert_eq!(valid_name("costs", &names, Some(1)).unwrap(), "costs");
        assert!(valid_name("a/b", &names, None).is_err());
        assert!(valid_name("'x", &names, None).is_err());
        assert!(valid_name("", &names, None).is_err());
        assert!(valid_name(&"x".repeat(32), &names, None).is_err());
        assert_eq!(next_sheet_name(&names), "Sheet3");
        assert_eq!(
            next_sheet_name(&["Sheet1".into(), "sheet3".into()]),
            "Sheet4"
        );
        assert_eq!(next_sheet_name(&[]), "Sheet2");
    }

    #[test]
    fn rewriting_formula_text() {
        let raw = store_formula("=SUM(Costs!A1:A3)+'Q1 Sales'!B2");
        let to = rewritten(&raw, &|e| refs::rename_sheet(e, "q1 sales", "Q2"));
        assert_eq!(
            to.as_deref().map(display_text),
            Some("=SUM(Costs!A1:A3)+'Q2'!B2")
        );
        assert_eq!(rewritten("plain", &|_| true), None);
        let gone = rewritten(&raw, &|e| refs::delete_sheet(e, "Costs"));
        assert_eq!(
            gone.as_deref().map(display_text),
            Some("=SUM(#REF!)+'Q1 Sales'!B2")
        );
    }
}
