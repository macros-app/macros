//! Rewriting references when the grid changes shape or a formula moves.
//!
//! - Inserting/deleting rows or columns shifts every coordinate past the
//!   edit, anchored or not (as Excel does). A reference wholly inside deleted
//!   rows becomes `#REF!`; a range losing one end shrinks.
//! - Copying, filling, pasting, and sorting move a formula: its *unanchored*
//!   coordinates move with it ([`translate`]), `$` ones stay.
//! - Moving rows or columns (org-table `M-arrows`) moves cells, so every
//!   reference *to* a moved cell follows it ([`remap_rows`], [`remap_cols`]).
//!
//! Each function edits the expression in place and returns whether anything
//! changed (so callers only rewrite cells whose text actually changes).

use super::ast::{Area, Coord, Expr};
use super::value::ErrorKind;

/// Which axis an edit runs along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Rows,
    Cols,
}

/// A reference's extent along one axis: one coordinate, or a span's two
/// ends (in either order).
enum Span<'a> {
    Point(&'a mut Coord),
    Range(&'a mut Coord, &'a mut Coord),
}

/// Call `f` with the extent along `axis` of each reference to `sheet` (the
/// sheet a structural edit happened on: unqualified, or naming it). `f`
/// returns whether the reference died (`#REF!`).
fn for_each_span(expr: &mut Expr, sheet: &str, axis: Axis, mut f: impl FnMut(Span) -> bool) {
    expr.for_each_ref_mut(&mut |e| {
        let Expr::Ref(r) = e else { return };
        if r.sheet
            .as_deref()
            .is_some_and(|s| !s.eq_ignore_ascii_case(sheet))
        {
            return; // another sheet's cells didn't move
        }
        let span = match (&mut r.area, axis) {
            (Area::Cell { row, .. }, Axis::Rows) => Span::Point(row),
            (Area::Cell { col, .. }, Axis::Cols) => Span::Point(col),
            (Area::Range { r0, r1, .. }, Axis::Rows) | (Area::Rows { r0, r1 }, Axis::Rows) => {
                Span::Range(r0, r1)
            }
            (Area::Range { c0, c1, .. }, Axis::Cols) | (Area::Cols { c0, c1 }, Axis::Cols) => {
                Span::Range(c0, c1)
            }
            _ => return,
        };
        if f(span) {
            *e = Expr::Error(ErrorKind::Ref);
        }
    });
}

/// Rows/columns `at..at+n` were inserted (`n > 0`) on `sheet`.
pub fn insert(expr: &mut Expr, sheet: &str, axis: Axis, at: usize, n: usize) -> bool {
    use super::parse::{MAX_COL, MAX_ROW};
    let max = match axis {
        Axis::Rows => MAX_ROW,
        Axis::Cols => MAX_COL,
    };
    let mut changed = false;
    // Shift one coordinate; true when it would be pushed off the sheet (past
    // XFD / the last row), which makes the reference `#REF!` — as `translate`
    // does — rather than an `XFE1` that re-parses as a name.
    let mut shift = |c: &mut Coord| {
        // An open end (`A:A`'s rows) sits past the sheet and never moves.
        if c.index >= at && c.index <= max {
            changed = true;
            match c.index.checked_add(n).filter(|&i| i <= max) {
                Some(i) => c.index = i,
                None => return true,
            }
        }
        false
    };
    for_each_span(expr, sheet, axis, |span| match span {
        Span::Point(c) => shift(c),
        // A range grows when rows go in strictly inside it (or right at
        // its end-row's position, as Excel does at the last row).
        Span::Range(a, b) => {
            let dead_a = shift(a);
            shift(b) || dead_a
        }
    });
    changed
}

fn order<'a>(a: &'a mut Coord, b: &'a mut Coord) -> (&'a mut Coord, &'a mut Coord) {
    if a.index <= b.index { (a, b) } else { (b, a) }
}

/// Rows/columns `at..at+n` were deleted on `sheet`.
pub fn delete(expr: &mut Expr, sheet: &str, axis: Axis, at: usize, n: usize) -> bool {
    let end = at + n;
    let mut changed = false;
    for_each_span(expr, sheet, axis, |span| match span {
        Span::Point(c) => {
            if c.index >= end {
                c.index -= n;
                changed = true;
            } else if c.index >= at {
                changed = true;
                return true;
            }
            false
        }
        Span::Range(a, b) => {
            let (lo, hi) = order(a, b);
            let (l, h) = (lo.index, hi.index);
            if l >= at && h < end {
                changed = true;
                return true; // wholly deleted
            }
            // A range losing one end shrinks.
            let new_l = if l >= end {
                l - n
            } else if l >= at {
                at
            } else {
                l
            };
            let new_h = if h >= end {
                h - n
            } else if h >= at {
                at.saturating_sub(1)
            } else {
                h
            };
            if new_l != l || new_h != h {
                lo.index = new_l;
                hi.index = new_h;
                changed = true;
            }
            false
        }
    });
    changed
}

/// Rows/columns `gone` (sorted, distinct) were deleted on `sheet`: what
/// [`delete`] does for each of them, last first, in one pass.
pub fn delete_set(expr: &mut Expr, sheet: &str, axis: Axis, gone: &[usize]) -> bool {
    use super::parse::{MAX_COL, MAX_ROW};
    let max = match axis {
        Axis::Rows => MAX_ROW,
        Axis::Cols => MAX_COL,
    };
    // How many deleted lines lie before `i`.
    let before = |i: usize| gone.partition_point(|&g| g < i);
    let mut changed = false;
    let mut set = |c: &mut Coord, to: usize| {
        changed |= to != c.index;
        c.index = to;
    };
    let mut died = false;
    for_each_span(expr, sheet, axis, |span| match span {
        Span::Point(c) => {
            if c.index > max {
                false
            } else if gone.binary_search(&c.index).is_ok() {
                died = true;
                true
            } else {
                let to = c.index - before(c.index);
                set(c, to);
                false
            }
        }
        Span::Range(a, b) => {
            let (lo, hi) = order(a, b);
            let (l, h) = (lo.index, hi.index);
            if h > max {
                // An open end covers wherever the lines go.
                set(lo, l - before(l));
                return false;
            }
            if before(h + 1) - before(l) == h - l + 1 {
                died = true;
                return true; // wholly deleted
            }
            // The first and last survivors inside become the ends.
            set(lo, l - before(l));
            set(hi, h - before(h + 1));
            false
        }
    });
    changed || died
}

/// Move the formula by `(dr, dc)`: unanchored coordinates follow, `$` ones
/// stay (copy, fill, paste, sort). A coordinate pushed off the grid's top or
/// left edge makes that reference `#REF!`, as in Excel.
pub fn translate(expr: &mut Expr, dr: isize, dc: isize) -> bool {
    if dr == 0 && dc == 0 {
        return false;
    }
    let mut changed = false;
    expr.for_each_ref_mut(&mut |e| {
        let Expr::Ref(r) = e else { return };
        let mut dead = false;
        let mut mv = |c: &mut Coord, d: isize, max: usize| {
            if c.abs || d == 0 {
                return;
            }
            match c.index.checked_add_signed(d) {
                Some(i) if i <= max => {
                    c.index = i;
                    changed = true;
                }
                _ => dead = true,
            }
        };
        use super::parse::{MAX_COL, MAX_ROW};
        match &mut r.area {
            Area::Cell { row, col } => {
                mv(row, dr, MAX_ROW);
                mv(col, dc, MAX_COL);
            }
            Area::Range { r0, c0, r1, c1 } => {
                mv(r0, dr, MAX_ROW);
                mv(r1, dr, MAX_ROW);
                mv(c0, dc, MAX_COL);
                mv(c1, dc, MAX_COL);
            }
            Area::Cols { c0, c1 } => {
                mv(c0, dc, MAX_COL);
                mv(c1, dc, MAX_COL);
            }
            Area::Rows { r0, r1 } => {
                mv(r0, dr, MAX_ROW);
                mv(r1, dr, MAX_ROW);
            }
        }
        if dead {
            *e = Expr::Error(ErrorKind::Ref);
            changed = true;
        }
    });
    changed
}

/// Rows or columns moved on `sheet`: a reference to row `r` now points at
/// `map(r)` (moving cells takes references along). A range keeps its ends
/// when what moved stayed inside it (reordering summed rows keeps the
/// total), and otherwise stretches over where its ends went.
pub fn remap(expr: &mut Expr, sheet: &str, axis: Axis, map: &dyn Fn(usize) -> usize) -> bool {
    let mut changed = false;
    let mut m = |c: &mut Coord, to: usize| {
        if c.index != usize::MAX && to != c.index {
            c.index = to;
            changed = true;
        }
    };
    for_each_span(expr, sheet, axis, |span| {
        match span {
            Span::Point(c) => {
                let to = map(c.index);
                m(c, to);
            }
            Span::Range(a, b) => {
                let (lo, hi) = order(a, b);
                if hi.index == usize::MAX {
                    return false; // an open-ended span covers wherever rows go
                }
                let (l, h) = (map(lo.index), map(hi.index));
                let inside = |i: usize| (lo.index..=hi.index).contains(&i);
                if !(inside(l) && inside(h)) {
                    m(lo, l.min(h));
                    m(hi, l.max(h));
                }
            }
        }
        false
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::parse::parse;
    use crate::formula::print::print;

    fn run(src: &str, f: impl FnOnce(&mut Expr) -> bool) -> String {
        let mut e = parse(src).unwrap();
        f(&mut e);
        print(&e)
    }

    #[test]
    fn inserting_rows_shifts_and_grows() {
        let ins = |s: &str| run(s, |e| insert(e, "Sheet1", Axis::Rows, 4, 2)); // before row 5
        assert_eq!(ins("=A3+A5+$A$9"), "=A3+A7+$A$11");
        assert_eq!(ins("=SUM(A1:A10)"), "=SUM(A1:A12)");
        assert_eq!(ins("=SUM(A5:A10)"), "=SUM(A7:A12)");
        assert_eq!(ins("=SUM(A1:A4)"), "=SUM(A1:A4)");
        assert_eq!(ins("=SUM(B:B)"), "=SUM(B:B)");
        let cols = run("=B1+D1", |e| insert(e, "Sheet1", Axis::Cols, 2, 1));
        assert_eq!(cols, "=B1+E1");
        // Pushed past the last column: #REF!, not `XFE1` (a name).
        let off = run("=XFD1+A1", |e| insert(e, "Sheet1", Axis::Cols, 0, 1));
        assert_eq!(off, "=#REF!+B1");
    }

    #[test]
    fn deleting_rows_shrinks_or_breaks() {
        let del = |s: &str| run(s, |e| delete(e, "Sheet1", Axis::Rows, 4, 2)); // rows 5-6
        assert_eq!(del("=A3+A8"), "=A3+A6");
        assert_eq!(del("=A5+1"), "=#REF!+1");
        assert_eq!(del("=SUM(A1:A10)"), "=SUM(A1:A8)");
        assert_eq!(del("=SUM(A5:A6)"), "=SUM(#REF!)");
        assert_eq!(del("=SUM(A5:A10)"), "=SUM(A5:A8)");
        assert_eq!(del("=SUM(A1:A6)"), "=SUM(A1:A4)");
    }

    #[test]
    fn deleting_a_row_set_matches_deleting_each_row() {
        let gone = [2, 4, 5, 9]; // rows 3, 5, 6, 10
        let at_once = |s: &str| run(s, |e| delete_set(e, "Sheet1", Axis::Rows, &gone));
        let one_by_one = |s: &str| {
            run(s, |e| {
                let mut changed = false;
                for &g in gone.iter().rev() {
                    changed |= delete(e, "Sheet1", Axis::Rows, g, 1);
                }
                changed
            })
        };
        assert_eq!(at_once("=A3+A4+A11"), "=#REF!+A3+A7");
        assert_eq!(at_once("=SUM(A1:A12)"), "=SUM(A1:A8)");
        assert_eq!(at_once("=SUM(A5:A6)"), "=SUM(#REF!)");
        assert_eq!(at_once("=SUM(A3:A5)"), "=SUM(A3:A3)");
        assert_eq!(at_once("=SUM($A$4:A10)"), "=SUM($A$3:A6)");
        assert_eq!(at_once("=Costs!A11"), "=Costs!A11");
        for r0 in 1..=12 {
            for r1 in r0..=12 {
                for src in [format!("=SUM(B{r0}:C{r1})"), format!("=A{r0}+A{r1}")] {
                    assert_eq!(at_once(&src), one_by_one(&src), "{src}");
                }
            }
        }
    }

    #[test]
    fn translate_moves_unanchored_axes() {
        let t = |s: &str, dr, dc| run(s, |e| translate(e, dr, dc));
        assert_eq!(t("=A1+$B1+C$1+$D$1", 2, 1), "=B3+$B3+D$1+$D$1");
        assert_eq!(t("=SUM(A1:A3)", 1, 0), "=SUM(A2:A4)");
        assert_eq!(t("=A1", -1, 0), "=#REF!");
    }

    #[test]
    fn remap_follows_moved_cells() {
        // Rows 2 and 3 swapped (0-based 1 and 2).
        let swap = |r: usize| match r {
            1 => 2,
            2 => 1,
            r => r,
        };
        assert_eq!(
            run("=A2*2+A3", |e| remap(e, "Sheet1", Axis::Rows, &swap)),
            "=A3*2+A2"
        );
    }

    #[test]
    fn other_sheet_refs_are_not_shifted() {
        // Rows inserted on THIS sheet must not move references into another sheet.
        assert_eq!(
            run("=Costs!A5", |e| insert(e, "Sheet1", Axis::Rows, 1, 1)),
            "=Costs!A5"
        );
        assert_eq!(
            run("=Costs!A5", |e| delete(e, "Sheet1", Axis::Rows, 4, 1)),
            "=Costs!A5"
        );
    }

    #[test]
    fn translating_past_xfd_is_ref_error() {
        let out = run("=XFD1", |e| translate(e, 0, 1));
        assert!(
            parse(&out).is_ok_and(|e| !matches!(e, Expr::Name(_))),
            "{out}"
        );
        assert_eq!(out, "=#REF!");
    }

    #[test]
    fn deleting_a_whole_range_in_a_function() {
        assert_eq!(
            run("=SUM(A5:A6)", |e| delete(e, "Sheet1", Axis::Rows, 4, 2)),
            "=SUM(#REF!)"
        );
    }
}
