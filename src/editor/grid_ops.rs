//! The Emacs-flavored grid commands for [`Editor`]: kill-ring yanking and
//! `M-y`, org-table structure moves, copy-down series, case changes,
//! narrowing and filtering, column hiding, completion, column jumps, and
//! export to / import from org and Markdown tables. Split out of `grid.rs`
//! for size; dispatched from [`Editor::grid_command`].

use gpui::Context;

use super::grid::{GridDoc, RowFilter, SHEET, YankState};
use super::*;
use crate::formula::refs::Axis;
use crate::grid::cellfmt::{self, CellFmt, NumCode};
use crate::grid::cmd::GridCmd;
use crate::grid::export;
use crate::grid::series::next_in_series;
use crate::grid::style::RuleTest;
use crate::grid::view::{self, CellRange, Pos, SelKind, cell_name, col_name};

/// Distinct values offered by `grid-complete-cell` when a column has no
/// declared choices.
const MAX_COMPLETIONS: usize = 2000;

/// Most cells one formatting command changes (each is a map entry).
const MAX_FORMAT_CELLS: usize = 1_000_000;

/// Which way a case command changes text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CaseOp {
    Upper,
    Lower,
    Capitalize,
}

/// `hello WORLD` → `Hello World` (Emacs `capitalize-word`: the first letter
/// of each alphanumeric run up, the rest down).
fn capitalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_word = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            if in_word {
                out.extend(ch.to_lowercase());
            } else {
                out.extend(ch.to_uppercase());
            }
            in_word = true;
        } else {
            out.push(ch);
            in_word = false;
        }
    }
    out
}

/// The regex flag for smart case: case-insensitive (`(?i)`) unless the
/// pattern has a capital.
pub(super) fn smart_case(pattern: &str) -> &'static str {
    if pattern.chars().any(char::is_uppercase) {
        ""
    } else {
        "(?i)"
    }
}

/// A typed filter: `>10`, `<=5`, `=open`, `!=done`, `10..20`, `empty`,
/// `!empty`, or else a regex (smart case: case-insensitive unless the
/// pattern has a capital). Returns the test and a short label.
pub(super) fn parse_filter(input: &str) -> Result<(RuleTest, String), String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("nothing to filter by".into());
    }
    for op in [">=", "<=", "!=", ">", "<", "="] {
        if let Some(rest) = t.strip_prefix(op) {
            let test = RuleTest::parse(op, rest.trim())?;
            return Ok((test, format!("{op}{}", rest.trim())));
        }
    }
    match t {
        "empty" => return Ok((RuleTest::Empty, " empty".into())),
        "!empty" | "not-empty" => return Ok((RuleTest::NotEmpty, " not empty".into())),
        _ => {}
    }
    if let Some((a, b)) = t.split_once("..")
        && a.trim().parse::<f64>().is_ok()
        && b.trim().parse::<f64>().is_ok()
    {
        return Ok((RuleTest::parse("between", t)?, format!(" {t}")));
    }
    let test = RuleTest::parse("matches", &format!("{}{t}", smart_case(t)))?;
    Ok((test, format!("~{t}")))
}

impl GridDoc {
    /// The rows (columns) a structure command acts on: the selection's, or
    /// the cursor's.
    fn target(&self, axis: Axis) -> (usize, usize) {
        if self.view.anchor.is_some() {
            self.selection().span(axis)
        } else {
            let i = view::coord(self.view.cursor, axis);
            (i, i)
        }
    }

    /// Shift the cursor and anchor rows (columns) by `d` (after moving a
    /// block).
    fn shift(&mut self, axis: Axis, d: isize) {
        let i = view::coord_mut(&mut self.view.cursor, axis);
        *i = i.saturating_add_signed(d);
        if let Some(a) = self.view.anchor.as_mut() {
            let i = view::coord_mut(a, axis);
            *i = i.saturating_add_signed(d);
        }
        self.view.reveal_cursor();
    }

    /// Rows `start..` were reordered so new row `start + k` is old row
    /// `perm[k]`: a positional narrowing (`C-x n n`) follows its rows, so
    /// the rows it showed stay the ones shown. (Rows hidden by an earlier
    /// filter move through the window too, so positions alone would show
    /// the wrong ones.)
    pub(super) fn follow_narrowing(&mut self, start: usize, perm: &[usize]) {
        let mut new_of = vec![0; perm.len()];
        for (k, &old) in perm.iter().enumerate() {
            new_of[old - start] = start + k;
        }
        let window = start..start + perm.len();
        for f in &mut self.filters {
            if let RowFilter::Rows { rows, .. } = f {
                for r in rows.iter_mut() {
                    if window.contains(&*r) {
                        *r = new_of[*r - start];
                    }
                }
                rows.sort_unstable();
            }
        }
    }

    /// Column `c`'s header name (row 1) when the sheet has a header.
    pub(super) fn header_name(&self, c: usize) -> Option<&str> {
        (self.wb.header(SHEET) && c < self.used().1)
            .then(|| self.cell(0, c))
            .filter(|n| !n.trim().is_empty())
    }

    /// `C (price)` — a column as prompts name it.
    pub(super) fn col_label(&self, c: usize) -> String {
        match self.header_name(c) {
            Some(n) => format!("{} ({n})", col_name(c)),
            None => col_name(c),
        }
    }
}

impl Editor {
    // --- Kill ring -----------------------------------------------------------

    /// `C-y` / `p`: paste the newest kill. Text another app put on the
    /// clipboard since our last kill joins the kill ring first (Emacs's
    /// interprogram paste), so `M-y` can cycle back past it.
    pub(super) fn grid_paste(&mut self, cx: &mut Context<Self>) {
        if let Some(clip) = cx.read_from_clipboard().and_then(|i| i.text())
            && !clip.is_empty()
            && self.kill_ring.last() != Some(&clip)
        {
            self.kill_append = false;
            self.push_kill(clip);
        }
        let Some(text) = self.kill_ring.last().cloned() else {
            self.echo("Kill ring is empty", cx);
            return;
        };
        let index = self.kill_ring.len() - 1;
        self.grid_paste_recorded(&text, index, cx);
    }

    /// Paste `text` (kill-ring entry `index`) and remember it for `M-y`.
    fn grid_paste_recorded(&mut self, text: &str, index: usize, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let (cursor, anchor, depth) = (g.view.cursor, g.view.anchor, g.hist_len());
        self.kill_index = index;
        self.grid_paste_text(text, cx);
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.yank = (g.hist_len() > depth).then(|| YankState {
                index,
                depth: g.hist_len(),
                cursor,
                anchor,
            });
        }
    }

    /// `M-y` right after a paste: replace it with the next older kill.
    pub(super) fn grid_yank_pop(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let Some(y) = g.yank.take().filter(|y| y.depth == g.hist_len()) else {
            self.echo("Previous command was not a yank", cx);
            return;
        };
        let len = self.kill_ring.len();
        if len < 2 {
            self.echo("No older kill to yank", cx);
            return;
        }
        self.grid_command(GridCmd::Undo, cx);
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.view.cursor = y.cursor;
            g.view.anchor = y.anchor;
            g.view.sel_kind = SelKind::Cells;
        }
        let index = (y.index + len - 1) % len;
        let text = self.kill_ring[index].clone();
        self.grid_paste_recorded(&text, index, cx);
    }

    /// `C-k`: kill the cells from the cursor to the row's last filled cell.
    pub(super) fn grid_kill_to_row_end(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let (r, c) = g.view.cursor;
        let (rows, cols) = g.used();
        let last = (c..cols)
            .rev()
            .find(|&k| r < rows && !g.cell(r, k).is_empty());
        let Some(last) = last else {
            self.echo("Nothing to kill", cx);
            return;
        };
        let row: Vec<String> = (c..=last).map(|k| g.cell(r, k).to_string()).collect();
        let text = if row.len() == 1 {
            row[0].clone()
        } else {
            view::to_tsv_block(std::slice::from_ref(&row))
        };
        let edits: Vec<(usize, usize, String)> =
            (c..=last).map(|k| (r, k, String::new())).collect();
        g.set_cells_tracked(&edits);
        self.set_clipboard(&text, cx);
        self.push_kill(text);
        self.echo(format!("Killed {} cells", row.len()), cx);
        self.grid_after(cx);
    }

    // --- Structure -----------------------------------------------------------

    /// `M-↑` / `M-↓`: move the cursor's row (or the selected rows) past the
    /// shown row above / below. Rows move as a permutation, so they stay
    /// byte-exact on save.
    pub(super) fn grid_move_rows(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let (r0, r1) = g.target(Axis::Rows);
        let (rows, _) = g.used();
        let first = usize::from(g.wb.header(SHEET));
        if r1 >= rows || r0 < first {
            self.echo("Can't move the header row or past the data", cx);
            return;
        }
        let col = g.view.cursor.1;
        let moved = if down {
            let next = g.view.rows.next_visible(r1 + 1);
            if next >= rows {
                self.echo("Already at the bottom", cx);
                return;
            }
            // [block][hidden between][next] → [hidden between][next][block]
            let mut perm: Vec<usize> = (r1 + 1..=next).collect();
            perm.extend(r0..=r1);
            let ok = g.model_op(|wb| wb.permute_rows(SHEET, r0, perm.clone(), col));
            if ok {
                g.follow_narrowing(r0, &perm);
                g.shift(Axis::Rows, (next - r1) as isize);
            }
            ok
        } else {
            let Some(prev) = r0
                .checked_sub(1)
                .filter(|&p| p >= first)
                .and_then(|p| g.view.rows.prev_visible(p))
                .filter(|&p| p >= first)
            else {
                self.echo("Already at the top", cx);
                return;
            };
            // [prev][hidden between][block] → [block][prev][hidden between]
            let mut perm: Vec<usize> = (r0..=r1).collect();
            perm.extend(prev..r0);
            let ok = g.model_op(|wb| wb.permute_rows(SHEET, prev, perm.clone(), col));
            if ok {
                g.follow_narrowing(prev, &perm);
                g.shift(Axis::Rows, -((r0 - prev) as isize));
            }
            ok
        };
        if moved {
            // The filter's row set follows the moved rows.
            g.apply_filters();
        }
        self.grid_after(cx);
    }

    /// `M-←` / `M-→`: move the cursor's column (or the selected columns) one
    /// place left / right, carrying their widths along.
    pub(super) fn grid_move_columns(&mut self, right: bool, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let (c0, c1) = g.target(Axis::Cols);
        let (_, cols) = g.used();
        if c1 >= cols {
            self.echo("Past the data", cx);
            return;
        }
        // Moving the block right = moving its right neighbour to its left.
        let (from, to, d) = if right {
            if c1 + 1 >= cols {
                self.echo("Already the last column", cx);
                return;
            }
            (c1 + 1, c0, 1isize)
        } else {
            let Some(from) = c0.checked_sub(1) else {
                self.echo("Already the first column", cx);
                return;
            };
            (from, c1, -1isize)
        };
        // Widths and hidden flags move with the columns (`model_op`).
        if g.model_op(|wb| wb.move_col(SHEET, from, to)) {
            g.shift(Axis::Cols, d);
            self.grid_persist_view();
        }
        self.grid_after(cx);
    }

    /// `M-u` / `M-l` / `M-c` over the selected (shown) cells.
    pub(super) fn grid_change_case(&mut self, op: CaseOp, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let sel = g.selection();
        let edits: Vec<(usize, usize, String)> = g
            .visible_cells(&sel)
            .into_iter()
            // Text only: a formula keeps its formula (not its result
            // cased), and text starting with `=` stays text.
            .filter(|&(r, c)| !g.is_formula_at(r, c))
            .map(|(r, c)| {
                let v = g.source(r, c);
                let new = match op {
                    CaseOp::Upper => v.to_uppercase(),
                    CaseOp::Lower => v.to_lowercase(),
                    CaseOp::Capitalize => capitalize(&v),
                };
                (r, c, new)
            })
            .collect();
        g.set_cells_tracked(&edits);
        self.grid_after(cx);
    }

    /// `S-RET` (org-table's copy-down): copy the cell into the row below,
    /// continuing its series, and follow it; on an empty cell, fill it from
    /// the nearest value above. Over a multi-row selection, fill every
    /// column of it down from its first values. `C-u` copies without
    /// incrementing.
    pub(super) fn grid_copy_down(&mut self, plain: bool, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let sel = g.selection();
        let first = usize::from(g.wb.header(SHEET));
        let step = |prev: Option<&str>, cur: &str, n: i64| {
            if plain {
                cur.to_string()
            } else {
                next_in_series(prev, cur, n)
            }
        };
        let mut edits = Vec::new();
        if sel.rows() > 1 {
            let rows: Vec<usize> = (sel.r0.max(first)..=sel.r1)
                .filter(|&r| g.row_shown(r))
                .collect();
            for c in sel.c0..=sel.c1 {
                // Seeds: the leading filled cells (two set the step).
                let seeds: Vec<usize> = rows
                    .iter()
                    .copied()
                    .take_while(|&r| !g.source(r, c).is_empty())
                    .take(2)
                    .collect();
                let Some(&last) = seeds.last() else { continue };
                if g.is_formula_at(last, c) {
                    // A formula fills down as itself, references moving.
                    let f = g.source(last, c).to_string();
                    for &r in rows.iter().skip_while(|&&r| r <= last) {
                        let dr = r as isize - last as isize;
                        edits.push((r, c, super::grid_formula::translated(&f, dr, 0)));
                    }
                    continue;
                }
                let prev = (seeds.len() == 2).then(|| g.cell(seeds[0], c).to_string());
                let cur = g.cell(last, c).to_string();
                for (k, &r) in rows.iter().skip(seeds.len()).enumerate() {
                    edits.push((r, c, step(prev.as_deref(), &cur, k as i64 + 1)));
                }
            }
        } else {
            let (r, c) = g.view.cursor;
            let above = |g: &GridDoc, r: usize| {
                r.checked_sub(1)
                    .filter(|&p| p >= first)
                    .and_then(|p| g.view.rows.prev_visible(p))
                    .filter(|&p| p >= first)
            };
            let cur = g.source(r, c).to_string();
            if cur.is_empty() {
                let mut src = above(g, r);
                while let Some(s) = src
                    && g.source(s, c).is_empty()
                {
                    src = above(g, s);
                }
                let Some(s) = src else {
                    self.echo("Nothing above to copy", cx);
                    return;
                };
                let v = if g.is_formula_at(s, c) {
                    super::grid_formula::translated(&g.source(s, c), r as isize - s as isize, 0)
                } else {
                    let prev = above(g, s).map(|p| g.cell(p, c).to_string());
                    step(prev.as_deref(), g.cell(s, c), 1)
                };
                edits.push((r, c, v));
            } else {
                let below = g.view.rows.next_visible(r + 1);
                let v = if g.is_formula_at(r, c) {
                    super::grid_formula::translated(&cur, below as isize - r as isize, 0)
                } else {
                    let prev = above(g, r).map(|p| g.cell(p, c).to_string());
                    step(prev.as_deref(), &cur, 1)
                };
                edits.push((below, c, v));
                g.view.anchor = None;
                g.move_to(below, c, false);
            }
        }
        g.set_cells_tracked(&edits);
        self.grid_after(cx);
    }

    // --- Selection & view ------------------------------------------------------

    /// `C-x C-x`: swap the cursor with the selection's other corner.
    pub(super) fn grid_exchange_anchor(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        match g.view.anchor {
            Some(a) => {
                g.view.anchor = Some(g.view.cursor);
                g.view.cursor = a;
                g.view.reveal_cursor();
                self.grid_after(cx);
            }
            None => self.echo("No selection", cx),
        }
    }

    /// `C-l`: the cursor row to the middle of the window, then the top, then
    /// the bottom (Emacs `recenter-top-bottom`).
    pub(super) fn grid_recenter(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let r = g.view.cursor.0;
        if r < g.view.frozen_rows {
            return;
        }
        let z = f64::from(g.view.zoom).max(0.1);
        let frozen = g.view.frozen_height();
        let avail = (g.view.viewport.1 / z - frozen).max(0.0);
        let y0 = g.view.rows.start(r) - frozen;
        let h = g.view.rows.size(r);
        g.view.scroll_y = match g.recenter_phase % 3 {
            0 => y0 - (avail - h) / 2.0,
            1 => y0,
            _ => y0 + h - avail,
        };
        g.recenter_phase = g.recenter_phase.wrapping_add(1);
        g.view.clamp_scroll();
        self.grid_after(cx);
    }

    /// Hide the cursor's column (or the selected columns).
    pub(super) fn grid_hide_columns(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let (c0, c1) = g.target(Axis::Cols);
        g.set_cols_hidden(c0..=c1, true);
        g.view.anchor = None;
        let (r, c) = g.view.cursor;
        g.move_to(r, c, false);
        let n = c1 - c0 + 1;
        self.grid_persist_view();
        self.echo(
            format!(
                "Hid {n} column{} (grid-show-columns brings them back)",
                if n == 1 { "" } else { "s" }
            ),
            cx,
        );
        self.grid_after(cx);
    }

    /// Show hidden columns: those inside the selection, or all of them.
    pub(super) fn grid_show_columns(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let range = g.view.anchor.is_some().then(|| g.target(Axis::Cols));
        let hidden: Vec<usize> = g
            .view
            .cols
            .hidden()
            .filter(|c| range.is_none_or(|(a, b)| (a..=b).contains(c)))
            .collect();
        g.set_cols_hidden(hidden.iter().copied(), false);
        self.grid_persist_view();
        self.echo(format!("Showing {} hidden column(s)", hidden.len()), cx);
        self.grid_after(cx);
    }

    // --- Narrowing & filtering -------------------------------------------------

    /// Add a row filter and show the result.
    fn grid_push_filter(&mut self, filter: RowFilter, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        g.filters.push(filter);
        let shown = g.apply_filters();
        let (r, c) = g.view.cursor;
        g.view.anchor = None;
        g.move_to(r, c, false);
        let status = g.filter_status();
        if shown == 0 {
            self.echo(format!("{status} — no rows match (C-x n w widens)"), cx);
        } else {
            self.echo(format!("{status} (C-x n w widens)"), cx);
        }
        self.grid_after(cx);
    }

    /// `C-x n n`: show only the selected rows (plus the header).
    pub(super) fn grid_narrow_to_selection(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let (r0, r1) = g.target(Axis::Rows);
        let rows: Vec<usize> = (r0..=r1.min(g.used().0.saturating_sub(1)))
            .filter(|&r| g.row_shown(r))
            .collect();
        let label = if r0 == r1 {
            format!("row {}", r0 + 1)
        } else {
            format!("rows {}–{}", r0 + 1, r1 + 1)
        };
        self.grid_push_filter(RowFilter::Rows { rows, label }, cx);
    }

    /// Prompt for a filter on the cursor's column.
    pub(super) fn grid_filter_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let c = g.view.cursor.1;
        let prompt = format!(
            "Filter {} by (>10  <=5  =x  !=x  1..9  empty  or a regex): ",
            g.col_label(c)
        );
        self.open_minibuffer(&prompt, MinibufferPurpose::GridFilter(c), "", cx);
    }

    pub(super) fn grid_filter_submit(&mut self, col: usize, input: &str, cx: &mut Context<Self>) {
        match parse_filter(input) {
            Ok((test, label)) => {
                let label = format!("{}{label}", col_name(col));
                self.grid_push_filter(RowFilter::Test { col, test, label }, cx);
            }
            Err(e) => self.echo(format!("Filter: {e}"), cx),
        }
    }

    /// Show only rows whose cursor-column value equals the cursor cell's.
    pub(super) fn grid_filter_by_value(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let (r, c) = g.view.cursor;
        let value = g.cell(r, c).to_string();
        let test = if value.is_empty() {
            RuleTest::Empty
        } else {
            RuleTest::Eq(value.clone())
        };
        let label = format!("{}={value}", col_name(c));
        self.grid_push_filter(
            RowFilter::Test {
                col: c,
                test,
                label,
            },
            cx,
        );
    }

    /// `C-x n w`: drop every narrowing and filter.
    pub(super) fn grid_widen(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        if g.filters.is_empty() {
            self.echo("Not narrowed", cx);
            return;
        }
        g.filters.clear();
        g.apply_filters();
        g.view.reveal_cursor();
        self.echo("Widened", cx);
        self.grid_after(cx);
    }

    // --- Completion & column jumps ----------------------------------------------

    /// Complete the cursor cell (or the cell being edited) from the column's
    /// `grid-column-choices`, else from the values already in the column,
    /// most common first.
    pub(super) fn grid_complete_cell(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let (r, c, initial) = match g.view.edit.take() {
            Some(e) => (e.row, e.col, e.text),
            None => {
                let (r, c) = g.view.cursor;
                (r, c, g.cell(r, c).to_string())
            }
        };
        let candidates = match g.choices.get(&c) {
            Some(choices) => choices.clone(),
            None => {
                let rows = g.used().0;
                let start = usize::from(g.wb.header(SHEET)).min(rows);
                let mut counts: std::collections::HashMap<String, usize> = Default::default();
                if c < g.used().1 {
                    g.wb.for_each_in_column(SHEET, c, start..rows, |_, v| {
                        if !v.is_empty() {
                            *counts.entry(v.to_string()).or_default() += 1;
                        }
                    });
                }
                let mut values: Vec<(String, usize)> = counts.into_iter().collect();
                values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                values.truncate(MAX_COMPLETIONS);
                values.into_iter().map(|(v, _)| v).collect()
            }
        };
        let prompt = format!("{} {}: ", cell_name(r, c), g.header_name(c).unwrap_or(""));
        self.completing_candidates = candidates;
        self.open_minibuffer(&prompt, MinibufferPurpose::GridComplete(r, c), &initial, cx);
        self.grid_after(cx);
    }

    pub(super) fn grid_complete_submit(
        &mut self,
        r: usize,
        c: usize,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(g) = self.active.grid.as_deref_mut() {
            g.set_cells_tracked(&[(r, c, text.to_string())]);
        }
        self.grid_after(cx);
    }

    /// Jump to a column by header name (or letter), with completion.
    pub(super) fn grid_jump_to_column(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let cols = g.used().1;
        let candidates: Vec<String> = (0..cols)
            .filter(|&c| g.view.cols.shown(c))
            .map(|c| match g.header_name(c) {
                Some(n) => format!("{n} ({})", col_name(c)),
                None => col_name(c),
            })
            .collect();
        self.completing_candidates = candidates;
        self.open_minibuffer("Column: ", MinibufferPurpose::GridColumn, "", cx);
    }

    pub(super) fn grid_jump_submit(&mut self, input: &str, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let t = input.trim();
        let cols = g.used().1;
        // "price (C)" from the candidates, a header name, or a letter.
        let from_label = t
            .rsplit_once('(')
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .and_then(view::parse_col_name);
        let by_name = || {
            (0..cols).find(|&c| {
                g.header_name(c)
                    .is_some_and(|n| n.trim().eq_ignore_ascii_case(t))
            })
        };
        let by_part = || {
            let lower = t.to_lowercase();
            (0..cols).find(|&c| {
                g.header_name(c)
                    .is_some_and(|n| n.to_lowercase().contains(&lower))
            })
        };
        // A header name beats reading the input as letters ("stat" is also
        // a column name, far to the right).
        let target = from_label
            .or_else(by_name)
            .or_else(by_part)
            .or_else(|| view::parse_col_name(t));
        match target {
            Some(c) => {
                let r = g.view.cursor.0;
                g.view.anchor = None;
                g.move_to(r, c, false);
                self.grid_after(cx);
            }
            None => self.echo(format!("No column {t}"), cx),
        }
    }

    // --- Export / import ------------------------------------------------------

    /// Copy the selection (the whole sheet when one cell is selected) as an
    /// org table, a Markdown table, or JSON — onto the kill ring and the
    /// clipboard. Filtered-out rows are left out.
    pub(super) fn grid_copy_as(&mut self, cmd: GridCmd, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref() else {
            return;
        };
        let mut sel = g.selection();
        let (rows, cols) = g.used();
        if sel.is_single() {
            sel = CellRange {
                r0: 0,
                c0: 0,
                r1: rows.saturating_sub(1),
                c1: cols.saturating_sub(1),
            };
        }
        if rows == 0 || cols == 0 {
            self.echo("No data", cx);
            return;
        }
        let block = g.visible_block(&sel);
        let header = g.wb.header(SHEET) && sel.r0 == 0;
        let (text, what) = match cmd {
            GridCmd::CopyAsMarkdown => (
                export::to_markdown(&block, header, sel.c0),
                "Markdown table",
            ),
            GridCmd::CopyAsJson => (export::to_json(&block, header, sel.c0), "JSON"),
            _ => (export::to_org(&block, header), "org table"),
        };
        let n = block.len();
        self.set_clipboard(&text, cx);
        self.kill_append = false;
        self.push_kill(text);
        self.echo(format!("Copied {n} rows as {what}"), cx);
    }

    /// `M-x org-table-to-grid` in a text buffer: open the org or Markdown
    /// table around point as a new grid buffer (`C-x C-w` saves it as CSV).
    pub(super) fn org_table_to_grid(&mut self, cx: &mut Context<Self>) {
        if self.active.grid.is_some() {
            self.echo("Already a grid", cx);
            return;
        }
        let buf = &self.active.buffer;
        let here = buf.line_index_at_byte(self.point());
        let is_row = |i: usize| buf.line(i).1.trim_start().starts_with('|');
        if !is_row(here) {
            self.echo("No org or Markdown table at point", cx);
            return;
        }
        let mut top = here;
        while top > 0 && is_row(top - 1) {
            top -= 1;
        }
        let mut bottom = here;
        while bottom + 1 < buf.line_count() && is_row(bottom + 1) {
            bottom += 1;
        }
        let lines: Vec<String> = (top..=bottom).map(|i| buf.line(i).1).collect();
        let refs: Vec<&str> = lines
            .iter()
            .map(|l| l.trim_end_matches(['\n', '\r']))
            .collect();
        let Some((rows, header)) = export::parse_table(&refs) else {
            self.echo("No org or Markdown table at point", cx);
            return;
        };
        let csv = export::to_csv(&rows);
        let wb = match crate::grid::parse::parse_now(csv.into_bytes(), Some("csv")) {
            Ok(mut wb) => {
                wb.set_header(SHEET, header);
                wb
            }
            Err(e) => {
                self.echo(format!("Could not read the table: {e}"), cx);
                return;
            }
        };
        let base = format!("*table: {}*", self.active.name);
        let mut name = base.clone();
        let mut n = 2;
        while self.active.name == name || self.inactive.iter().any(|d| d.name == name) {
            name = format!("{base}<{n}>");
            n += 1;
        }
        self.open_grid_workbook(name.clone(), wb, cx);
        self.echo(
            format!(
                "{} rows from the table — C-x C-w writes it as CSV",
                rows.len()
            ),
            cx,
        );
    }

    /// Bold, text color, fill, a number format, or clear, on the selection
    /// (or the cursor cell). The new value comes from the cell the selection
    /// started on (Excel's active cell) — the next color after its color,
    /// bold unless it is bold, its format with a decimal place more — so the
    /// whole selection ends up alike. `code` (a format code or preset name)
    /// overrides `cmd` with that number format.
    pub(super) fn grid_format(&mut self, cmd: GridCmd, code: Option<&str>, cx: &mut Context<Self>) {
        let Some(g) = self.active.grid.as_deref_mut() else {
            return;
        };
        let (r, c) = g.view.cursor;
        let sel = if g.view.anchor.is_some() {
            g.selection()
        } else {
            CellRange::cell((r, c))
        };
        if sel.rows().saturating_mul(sel.cols()) > MAX_FORMAT_CELLS {
            self.echo(
                format!("Too many cells to format at once (over {MAX_FORMAT_CELLS})"),
                cx,
            );
            return;
        }
        let (ar, ac) = g.view.anchor.unwrap_or((r, c));
        let cur = g.formats.get(ar, ac);
        let preset = |name: &str| NumCode::intern(cellfmt::preset_code(name));
        let num = match (code, cmd) {
            (Some(code), _) => Some(preset(code)),
            (None, GridCmd::FormatGeneral) => Some(None),
            (None, GridCmd::FormatNumber) => Some(preset("number")),
            (None, GridCmd::FormatCurrency) => Some(preset("currency")),
            (None, GridCmd::FormatPercent) => Some(preset("percent")),
            (None, GridCmd::FormatDate) => Some(preset("date")),
            (None, GridCmd::FormatText) => Some(preset("text")),
            (None, GridCmd::IncreaseDecimals | GridCmd::DecreaseDecimals) => {
                let up = cmd == GridCmd::IncreaseDecimals;
                let base = match cur.num {
                    Some(code) => code.code().to_string(),
                    // General: start from the decimals the value shows.
                    None => general_code(g.cell(ar, ac)),
                };
                match cellfmt::step_decimals(&base, up) {
                    Some(code) => Some(NumCode::intern(&code)),
                    None => {
                        let why = if up { "more" } else { "fewer" };
                        self.echo(format!("No {why} decimal places in this format"), cx);
                        return;
                    }
                }
            }
            _ => None,
        };
        let cells: Vec<Pos> = sel
            .cells()
            .filter(|&(r, c)| g.view.rows.shown(r) && g.view.cols.shown(c))
            .collect();
        let n = cells.len();
        let color = |i: Option<u8>| i.map_or("none", cellfmt::color_name);
        let what = match (num, cmd) {
            (Some(num), _) => {
                g.format_cells(cells, |f| CellFmt { num, ..f });
                format!("Number format: {}", cellfmt::describe(num))
            }
            (None, GridCmd::ToggleBold) => {
                let on = !cur.bold;
                g.format_cells(cells, |f| CellFmt { bold: on, ..f });
                if on { "Bold" } else { "Not bold" }.to_string()
            }
            (None, GridCmd::CycleTextColor) => {
                let next = cellfmt::next_color(cur.text);
                g.format_cells(cells, |f| CellFmt { text: next, ..f });
                format!("Text color: {}", color(next))
            }
            (None, GridCmd::CycleFillColor) => {
                let next = cellfmt::next_color(cur.fill);
                g.format_cells(cells, |f| CellFmt { fill: next, ..f });
                format!("Fill: {}", color(next))
            }
            _ => {
                g.format_cells(cells, |_| CellFmt::default());
                "Cleared formatting".to_string()
            }
        };
        let cells = if n == 1 {
            String::new()
        } else {
            format!(" ({n} cells)")
        };
        self.echo(format!("{what}{cells}"), cx);
        self.grid_after(cx);
    }
}

/// The format code that shows `value` as General does, as a starting point
/// for stepping decimals: `0` for `12`, `0.00` for `3.25`.
fn general_code(value: &str) -> String {
    let decimals = value
        .trim()
        .split_once('.')
        .map_or(0, |(_, f)| f.bytes().take_while(u8::is_ascii_digit).count());
    if decimals == 0 {
        "0".into()
    } else {
        format!("0.{}", "0".repeat(decimals))
    }
}

impl Editor {
    /// The grid's `:s/pat/rep/[gciI]`: over the selection (the cursor cell
    /// when nothing is selected; `:'<,'>s` too), or the whole sheet with
    /// `:%s`. Vim's pattern and replacement dialects are translated here and
    /// the replacing is `grid-substitute` (scheme/grid.scm), so `c` runs the
    /// same y/n session as `M-%`. Without `g` only a cell's first match is
    /// replaced, as on a line.
    pub(super) fn grid_ex_substitute(
        &mut self,
        parsed: &super::ex::ExLine,
        cmd: &str,
        cx: &mut Context<Self>,
    ) {
        use super::ex::{AddrBase, ExRange, parse_sub, vim_pattern, vim_replacement};
        let mark = |a: &super::ex::Addr, m: char| a.base == AddrBase::Mark(m) && a.offset == 0;
        let scope = match &parsed.range {
            None => "selection",
            Some(ExRange::Whole) => "sheet",
            Some(ExRange::Two(a, _, b)) if mark(a, '<') && mark(b, '>') => "selection",
            Some(_) => {
                return self.echo(
                    format!(":{cmd}: a grid's :s takes no line range — :s for the selection, :%s for the sheet"),
                    cx,
                );
            }
        };
        let spec = parse_sub(&parsed.arg);
        if spec.bare || spec.pattern.is_empty() {
            return self.echo(
                "Usage: :s/pattern/replacement/[gci] (:%s for the whole sheet)",
                cx,
            );
        }
        if spec.flags.count_only {
            return self.echo(
                ":s with the n flag (count only) isn't supported in a grid",
                cx,
            );
        }
        let source = format!(
            "(grid-substitute {} {} '{} '{scope} {})",
            crate::script::steel_quote(&vim_pattern(&spec.pattern, spec.flags.ignore_case())),
            crate::script::steel_quote(&vim_replacement(&spec.replacement)),
            if spec.flags.global {
                "raw"
            } else {
                "raw-first"
            },
            if spec.flags.confirm { "#t" } else { "#f" },
        );
        self.publish_point();
        self.script.dispatch(&source, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_capitalize_and_filter_syntax() {
        assert_eq!(capitalize("hello WORLD-wide 3rd"), "Hello World-Wide 3rd");
        let (t, label) = parse_filter(">= 10").unwrap();
        assert!(t.test_value("10") && !t.test_value("9"));
        assert_eq!(label, ">=10");
        let (t, _) = parse_filter("2..5").unwrap();
        assert!(t.test_value("3") && !t.test_value("6"));
        let (t, label) = parse_filter("err").unwrap();
        assert!(t.test_value("ERROR: x"), "smart case");
        assert_eq!(label, "~err");
        let (t, _) = parse_filter("Err").unwrap();
        assert!(!t.test_value("error"));
        let (t, _) = parse_filter("!=done").unwrap();
        assert!(t.test_value("open") && !t.test_value("done"));
        assert!(parse_filter("empty").unwrap().0.test_value(""));
        assert!(parse_filter("  ").is_err());
        assert!(parse_filter(">x").is_err());
    }

    /// `M-↓` under a narrowing to rows 3, 4 and 6 (row 5 hidden by an
    /// earlier filter): moving row 4 past row 6 keeps showing the same three
    /// rows, now at 3, 5 and 6.
    #[test]
    fn narrowing_follows_moved_rows() {
        let wb =
            crate::grid::parse::parse_now(b"k\n0\n1\n2\n3\n4\n5\n".to_vec(), Some("csv")).unwrap();
        let mut g = GridDoc::new(wb, Some("csv".into()), 0, &Default::default());
        g.filters.push(RowFilter::Rows {
            rows: vec![2, 3, 5],
            label: "rows".into(),
        });
        // Old rows 4 (hidden) and 5 (next shown) move up past row 3.
        g.follow_narrowing(3, &[4, 5, 3]);
        let RowFilter::Rows { rows, .. } = &g.filters[0] else {
            unreachable!()
        };
        assert_eq!(rows, &[2, 4, 5]);
    }
}
