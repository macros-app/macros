//! Between pre-dynamic-array formulas and dynamic-array ones.
//!
//! Excel before 365 showed every formula as one value: a multi-cell range
//! or array where one value was wanted was *implicitly intersected* (the
//! cell in the formula's own row or column). Dynamic-array Excel spills it
//! instead, so a formula from an old file gets an explicit `@` wherever the
//! old reading intersected — `=A1:A3*2` becomes `=@A1:A3*2` — and keeps
//! computing what it did. Saving reverses that: a formula whose only `@`s
//! are those (and that calls no dynamic-array function) is written the old
//! way, so every Excel reads it alike; any other is written as a
//! dynamic-array formula.
//!
//! The positions that intersect are the formula's *value positions*: the
//! formula itself, operands of operators and parentheses, and the
//! arguments of the evaluator's own forms (`IF`, `CHOOSE`, …), which pass
//! a value through. A function's arguments aren't (`SUM(A1:A3)` takes the
//! range), nor is anything under an `@` or a `#`.

use super::ast::{Expr, UnOp};
use super::func;

/// Whether `e` is a multi-cell reference or array, which an old formula
/// intersects in a value position.
fn many(e: &Expr) -> bool {
    match e {
        Expr::Ref(r) => {
            let (r0, c0, r1, c1) = r.area.bounds();
            r0 != r1 || c0 != c1
        }
        Expr::Array(rows) => rows.len() * rows.first().map_or(0, Vec::len) > 1,
        _ => false,
    }
}

/// Apply `f` to each value position under `e` (not `e` itself), mutably.
fn for_each_position(e: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    match e {
        Expr::Unary(UnOp::Neg | UnOp::Plus, x) | Expr::Percent(x) | Expr::Paren(x) => f(x),
        Expr::Binary(_, a, b) => {
            f(a);
            f(b);
        }
        Expr::Call(name, args) if func::special(name).is_some() => args.iter_mut().for_each(f),
        _ => {}
    }
}

/// An old formula as dynamic-array Excel reads it: `@` on each multi-cell
/// reference or array in a value position.
pub fn mark(e: &mut Expr) {
    if many(e) {
        let inner = std::mem::replace(e, Expr::Missing);
        *e = Expr::Unary(UnOp::At, Box::new(inner));
        return;
    }
    for_each_position(e, &mut mark);
}

/// [`mark`] undone: the `@`s it would add removed.
pub fn strip(e: &mut Expr) {
    if let Expr::Unary(UnOp::At, x) = e
        && many(x)
    {
        let inner = std::mem::replace(&mut **x, Expr::Missing);
        *e = inner;
        return;
    }
    for_each_position(e, &mut strip);
}

/// Whether formula `e` reads the same in pre-dynamic-array Excel once
/// [`strip`]ped: it calls no dynamic-array function, has no `A1#`, and
/// marks every value position that would spill with `@`.
pub fn is_legacy(e: &Expr) -> bool {
    fn plain(e: &Expr) -> bool {
        let here = match e {
            Expr::Spill(_) => false,
            Expr::Call(name, _) => func::lookup(name).is_none_or(|f| f.array.is_none()),
            _ => true,
        };
        let mut ok = here;
        e.for_each_child(&mut |c| ok &= plain(c));
        ok
    }
    if !plain(e) {
        return false;
    }
    let mut round = e.clone();
    strip(&mut round);
    mark(&mut round);
    round == *e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::parse::parse;
    use crate::formula::print::print;

    fn marked(src: &str) -> String {
        let mut e = parse(src).unwrap();
        mark(&mut e);
        print(&e)
    }

    #[test]
    fn old_formulas_gain_at_where_they_intersected() {
        for (old, new) in [
            ("=A1:A3", "=@A1:A3"),
            ("=A1:A3*2+B:B", "=@A1:A3*2+@B:B"),
            ("=-(A1:B2)", "=-(@A1:B2)"),
            ("=IF(A1:A3>1,B1:B3,0)", "=IF(@A1:A3>1,@B1:B3,0)"),
            ("={1,2}", "=@{1,2}"),
            // Unchanged: a function takes the range, one cell is one value.
            ("=SUM(A1:A3)", "=SUM(A1:A3)"),
            ("=A1+{1}", "=A1+{1}"),
            ("=INDEX(A1:B3,2,2)*C1", "=INDEX(A1:B3,2,2)*C1"),
        ] {
            assert_eq!(marked(old), new, "{old}");
            // And that is the old form again.
            let e = parse(new).unwrap();
            assert!(is_legacy(&e), "{new}");
            let mut s = e.clone();
            strip(&mut s);
            assert_eq!(print(&s), old);
        }
    }

    #[test]
    fn dynamic_formulas_are_not_legacy() {
        for src in [
            "=A1:A3",
            "=A1:A3*2",
            "=SORT(A1:A3)",
            "=SUM(FILTER(A1:A3,B1:B3))",
            "=A1#",
        ] {
            let e = parse(src).unwrap();
            assert!(!is_legacy(&e), "{src}");
        }
    }
}
