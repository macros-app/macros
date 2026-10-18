//! Helix-style multi-cursor editing in grids: `C` / `M-C` add a cursor on
//! the cell below / above each cursor, `,` keeps only the primary, `(` / `)`
//! rotate which one is primary, and `s` puts a cursor on every selected cell
//! matching a regex. The extra cursors live in [`GridView::cursors`]; plain
//! motions move them all, and a cell edit (`i` / `a` / `c`) shows in every
//! cursor's cell while it's typed and lands in all of them, as one undo
//! step, on commit (see [`Spread`]). A formula follows each cell the way a
//! copy-down moves it: unanchored references shift.
//!
//! [`GridView::cursors`]: crate::grid::view::GridView::cursors

use std::borrow::Cow;

use gpui::Context;

use super::grid::GridDoc;
use super::*;
use crate::formula::refs::Axis;
use crate::grid::cmd::{Dir, GridCmd};
use crate::grid::view::{CellEdit, CellRange, Pos, Spread};

/// Most cursors `s` makes (each is drawn and edited).
const MAX_CURSORS: usize = 10_000;

impl GridDoc {
    /// Every cursor, the primary included, in reading order.
    pub(super) fn all_cursors(&self) -> Vec<Pos> {
        let mut all = self.view.cursors.clone();
        all.push(self.view.cursor);
        all.sort_unstable();
        all.dedup();
        all
    }

    /// Make `primary` the primary cursor and `rest` (which may include it)
    /// the others.
    fn set_cursors(&mut self, primary: Pos, rest: Vec<Pos>) {
        let mut rest = rest;
        rest.retain(|&p| p != primary);
        rest.sort_unstable();
        rest.dedup();
        self.view.cursors = rest;
        self.move_to(primary.0, primary.1, false);
    }

    /// The multi-cursor commands, and a plain motion while there are extra
    /// cursors. `None` when `cmd` isn't one of those.
    pub(super) fn apply_cursors(&mut self, cmd: GridCmd) -> Option<Result<(), Cow<'static, str>>> {
        use GridCmd::*;
        let several = !self.view.cursors.is_empty();
        Some(match cmd {
            AddCursorBelow | AddCursorAbove => self.add_cursors(cmd == AddCursorBelow),
            Move(d) if several => {
                self.move_cursors(d);
                Ok(())
            }
            KeepPrimaryCursor => {
                self.view.cursors.clear();
                Ok(())
            }
            RemovePrimaryCursor | RotateCursorsForward | RotateCursorsBackward if !several => {
                Err("Only one cursor".into())
            }
            RemovePrimaryCursor => {
                let all = self.all_cursors();
                let i = all.binary_search(&self.view.cursor).unwrap_or(0);
                let mut rest = all;
                rest.remove(i);
                let next = rest[i.min(rest.len() - 1)];
                self.set_cursors(next, rest);
                Ok(())
            }
            RotateCursorsForward | RotateCursorsBackward => {
                let all = self.all_cursors();
                let n = all.len();
                let i = all.binary_search(&self.view.cursor).unwrap_or(0);
                let j = if cmd == RotateCursorsForward {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.set_cursors(all[j], all);
                Ok(())
            }
            _ => return None,
        })
    }

    /// `C` / `M-C`: copy every cursor one shown row down (up). Cursors at the
    /// top edge have nowhere to go; the primary's copy becomes the primary.
    fn add_cursors(&mut self, down: bool) -> Result<(), Cow<'static, str>> {
        let all = self.all_cursors();
        let (pr, pc) = self.view.cursor;
        let copy = |g: &Self, (r, c): Pos| {
            let nr = g.step(Axis::Rows, r, down);
            (nr != r).then_some((nr, c))
        };
        let Some(primary) = copy(self, (pr, pc)) else {
            return Err("No row there for another cursor".into());
        };
        let mut rest = all.clone();
        rest.extend(all.iter().filter_map(|&p| copy(self, p)));
        self.deselect();
        self.set_cursors(primary, rest);
        Ok(())
    }

    /// A plain motion with several cursors: every cursor steps `d`-ward.
    /// One already at the edge stays put (and merges into a neighbour that
    /// lands on it).
    fn move_cursors(&mut self, d: Dir) {
        let step = |g: &Self, (r, c): Pos| match d.axis() {
            Axis::Rows => (g.step(Axis::Rows, r, d.forward()), c),
            Axis::Cols => (r, g.step(Axis::Cols, c, d.forward())),
        };
        let primary = step(self, self.view.cursor);
        let rest: Vec<Pos> = self.view.cursors.iter().map(|&p| step(self, p)).collect();
        self.deselect();
        self.set_cursors(primary, rest);
    }

    /// Commit moves `(dr, dc)` (Enter, Tab, …): with several cursors they all
    /// move, as a motion would.
    pub(super) fn move_after_commit(&mut self, edit: &CellEdit, dr: isize, dc: isize) {
        let dir = match (dr.signum(), dc.signum()) {
            (1, _) => Some(Dir::Down),
            (-1, _) => Some(Dir::Up),
            (_, 1) => Some(Dir::Right),
            (_, -1) => Some(Dir::Left),
            _ => None,
        };
        self.deselect();
        match dir {
            Some(d) if !self.view.cursors.is_empty() => {
                self.view.cursor = (edit.row, edit.col);
                self.move_cursors(d);
            }
            _ => {
                let r = edit.row.saturating_add_signed(dr);
                let c = edit.col.saturating_add_signed(dc);
                self.move_to(r, c, false);
            }
        }
    }

    /// What extra cursor `(r, c)`'s cell becomes under the open edit `e` (see
    /// [`Spread`]): `i` puts what was typed before the edited cell's text in
    /// front of this cell's, `a` what was typed after it at its end; `c`, or
    /// an edit that changed the original text itself, sets the typed text.
    /// A formula moves with the cell, as copy-down moves it.
    pub(super) fn spread_text(&self, e: &CellEdit, (r, c): Pos) -> String {
        let shifted = || {
            let dr = r as isize - e.row as isize;
            let dc = c as isize - e.col as isize;
            super::grid_formula::translated(&e.text, dr, dc)
        };
        match self.view.spread {
            Spread::Insert => match e.text.strip_suffix(e.original.as_str()) {
                Some(typed) => format!("{typed}{}", self.source(r, c)),
                None => shifted(),
            },
            Spread::Append => match e.text.strip_prefix(e.original.as_str()) {
                Some(typed) => format!("{}{typed}", self.source(r, c)),
                None => shifted(),
            },
            Spread::Replace => shifted(),
        }
    }

    /// The cell writes committing edit `e` makes: its own cell, then each
    /// extra cursor's. Empty when a single-cursor edit changed nothing.
    pub(super) fn commit_writes(&self, e: &CellEdit) -> Vec<(usize, usize, String)> {
        if self.view.cursors.is_empty() {
            if e.text == e.original {
                return Vec::new();
            }
            return vec![(e.row, e.col, e.text.clone())];
        }
        let mut writes = vec![(e.row, e.col, e.text.clone())];
        writes.extend(
            self.view
                .cursors
                .iter()
                .filter(|&&p| p != (e.row, e.col))
                .map(|&(r, c)| (r, c, self.spread_text(e, (r, c)))),
        );
        writes
    }

    /// `s`: a cursor on every shown cell of the selection (the whole data
    /// when only one cell is selected) whose value matches `re`. The first
    /// match becomes the primary. Returns how many matched.
    fn select_matching(&mut self, re: &regex::Regex) -> usize {
        let (rows, cols) = self.used();
        let mut area = self.selection();
        if area.is_single() {
            area = CellRange {
                r0: 0,
                c0: 0,
                r1: rows.saturating_sub(1),
                c1: cols.saturating_sub(1),
            };
        }
        let hits: Vec<Pos> = self
            .visible_cells(&area)
            .into_iter()
            .filter(|&(r, c)| self.view.cols.shown(c) && re.is_match(self.cell(r, c)))
            .take(MAX_CURSORS)
            .collect();
        if let Some(&first) = hits.first() {
            self.deselect();
            self.set_cursors(first, hits.clone());
        }
        hits.len()
    }
}

impl Editor {
    /// `s` in a grid: ask for the regex to select cells by.
    pub(super) fn grid_select_matching_prompt(&mut self, cx: &mut Context<Self>) {
        self.open_minibuffer(
            "Select cells matching: ",
            MinibufferPurpose::GridSelectMatching,
            "",
            cx,
        );
    }

    /// The `s` prompt's answer: a regex, smart case (as grid search).
    pub(super) fn grid_select_matching(&mut self, pat: &str, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let smart = super::grid_ops::smart_case(pat);
        let re = match regex::Regex::new(&format!("{smart}{pat}")) {
            Ok(re) => re,
            Err(e) => return self.echo(format!("Bad regex: {e}"), cx),
        };
        match g.select_matching(&re) {
            0 => self.echo(format!("No cell matches {pat}"), cx),
            1 => self.echo("1 cursor", cx),
            n => self.echo(format!("{n} cursors"), cx),
        }
        self.grid_after(cx);
    }
}
