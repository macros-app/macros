//! Evaluating an [`Expr`] against a [`Ctx`].
//!
//! Intermediate results can be a single value, a reference (kept lazy, so a
//! function like `SUM` scans the grid's storage directly), or an array (array
//! constants and element-wise arithmetic such as `(A1:A3>1)*B1:B3`, which
//! `SUMPRODUCT` consumes, and the dynamic-array functions return). A
//! formula whose result is an array or a multi-cell reference *spills* it
//! ([`eval_formula`]): the engine fills the cells below and to the right.
//! The `@` operator, and [`eval_cell`] for a whole formula, take one value
//! instead: an array's top-left element, or for a multi-cell reference
//! Excel's *implicit intersection* — the cell of the range in the formula's
//! own row or column — or `#VALUE!` when there's none.

use std::sync::Arc;

use super::ast::{BinOp, Expr, UnOp};
use super::func::{self, Arg, Array, Ctx, IntoValue, RangeRef};
use super::value::{ErrorKind, Value, compare};

/// An intermediate result.
#[derive(Clone, Debug, PartialEq)]
pub enum Ev {
    V(Value),
    R(RangeRef),
    /// An array, and — when it came from arithmetic on a reference — that
    /// reference's top-left cell, so a cell showing it can still use
    /// implicit intersection (`=A1:A3*10` in row 2 shows `A2*10`).
    A(Array, Option<(usize, usize)>),
}

/// Evaluate a formula to one value (implicit intersection at the top, as
/// pre-dynamic-array Excel shows every formula).
#[cfg(test)]
pub fn eval_cell(expr: &Expr, ctx: &mut dyn Ctx) -> Value {
    let ev = eval(expr, ctx);
    shown(to_scalar(ev, ctx))
}

/// A value as a cell shows it: a reference to an empty cell shows 0 (`=A1`
/// on an empty A1), and a number that isn't finite is `#NUM!`.
fn shown(v: Value) -> Value {
    match v {
        Value::Empty => Value::Number(0.0),
        Value::Number(n) if !n.is_finite() => Value::Error(ErrorKind::Num),
        v => v,
    }
}

/// What a formula's cell shows: one value, or an array that spills.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Value(Value),
    /// More than one value (each as [`eval_cell`] would show it).
    Array(Array),
}

/// Evaluate a formula as dynamic-array Excel does: an array or multi-cell
/// reference result is the whole array, to spill.
pub fn eval_formula(expr: &Expr, ctx: &mut dyn Ctx) -> Outcome {
    let ev = eval(expr, ctx);
    if !is_multi(&ev) {
        return Outcome::Value(shown(to_scalar(ev, ctx)));
    }
    let (rows, cols, data) = materialize(ev, ctx);
    if rows * cols == 1 {
        let v = data.into_iter().next().unwrap_or(Value::Empty);
        return Outcome::Value(shown(v));
    }
    Outcome::Array(Array {
        rows,
        cols,
        data: data.into_iter().map(shown).collect(),
    })
}

pub fn eval(expr: &Expr, ctx: &mut dyn Ctx) -> Ev {
    match expr {
        Expr::Number(n) => Ev::V(Value::Number(*n)),
        Expr::Text(s) => Ev::V(Value::Text(s.clone())),
        Expr::Bool(b) => Ev::V(Value::Bool(*b)),
        Expr::Error(e) => Ev::V(Value::Error(*e)),
        Expr::Missing => Ev::V(Value::Empty),
        Expr::Name(_) => Ev::V(Value::Error(ErrorKind::Name)),
        Expr::Paren(e) => eval(e, ctx),
        Expr::Array(rows) => {
            let cols = rows.first().map_or(0, Vec::len);
            let data = rows
                .iter()
                .flatten()
                .map(|e| to_scalar(eval(e, ctx), ctx))
                .collect();
            Ev::A(
                Array {
                    rows: rows.len(),
                    cols,
                    data,
                },
                None,
            )
        }
        Expr::Ref(r) => {
            if r.sheet.is_some() && ctx.extent(r.sheet.as_deref()).is_none() {
                return Ev::V(Value::Error(ErrorKind::Ref));
            }
            let (r0, c0, r1, c1) = r.area.bounds();
            Ev::R(RangeRef {
                sheet: r.sheet.clone(),
                r0,
                c0,
                r1,
                c1,
            })
        }
        Expr::Unary(UnOp::At, e) => {
            let ev = eval(e, ctx);
            Ev::V(to_scalar(ev, ctx))
        }
        Expr::Spill(e) => match eval(e, ctx) {
            Ev::R(r) if r.is_cell() => match ctx.spill_range(r.sheet.as_deref(), r.r0, r.c0) {
                Ok(range) => Ev::R(range),
                Err(e) => Ev::V(Value::Error(e)),
            },
            Ev::V(Value::Error(e)) => Ev::V(Value::Error(e)),
            _ => Ev::V(Value::Error(ErrorKind::Ref)),
        },
        Expr::Unary(op, e) => {
            let ev = eval(e, ctx);
            let d1904 = ctx.date1904();
            map1(ev, ctx, |v| match op {
                UnOp::Neg => match v.to_number_in(d1904) {
                    Ok(n) => Value::Number(-n),
                    Err(e) => Value::Error(e),
                },
                // Unary plus changes nothing, not even text into a number.
                UnOp::Plus | UnOp::At => v,
            })
        }
        Expr::Percent(e) => {
            let ev = eval(e, ctx);
            let d1904 = ctx.date1904();
            map1(ev, ctx, |v| match v.to_number_in(d1904) {
                Ok(n) => Value::Number(n / 100.0),
                Err(e) => Value::Error(e),
            })
        }
        Expr::Binary(op, a, b) => {
            let a = eval(a, ctx);
            let b = eval(b, ctx);
            let d1904 = ctx.date1904();
            map2(a, b, ctx, |x, y| binary(*op, x, y, d1904))
        }
        Expr::Call(name, args) => call(name, args, ctx),
    }
}

/// One binary operation on two values (errors propagate, left first);
/// `date1904` is the workbook's date system, for date text.
fn binary(op: BinOp, a: Value, b: Value, date1904: bool) -> Value {
    if let Value::Error(e) = a {
        return Value::Error(e);
    }
    if let Value::Error(e) = b {
        return Value::Error(e);
    }
    let arith =
        |f: fn(f64, f64) -> Value| match (a.to_number_in(date1904), b.to_number_in(date1904)) {
            (Ok(x), Ok(y)) => f(x, y),
            (Err(e), _) | (_, Err(e)) => Value::Error(e),
        };
    match op {
        BinOp::Add => arith(|x, y| num(x + y)),
        BinOp::Sub => arith(|x, y| num(x - y)),
        BinOp::Mul => arith(|x, y| num(x * y)),
        BinOp::Div => arith(|x, y| {
            if y == 0.0 {
                Value::Error(ErrorKind::Div0)
            } else {
                num(x / y)
            }
        }),
        BinOp::Pow => arith(|x, y| func::math::excel_power(x, y).into_value()),
        BinOp::Concat => match (a.to_text(), b.to_text()) {
            // Excel's 32,767-character cell limit: `#VALUE!` past it (and no
            // unbounded growth from a chain of doubling `&`s).
            (Ok(x), Ok(y)) if x.chars().count() + y.chars().count() > func::MAX_TEXT_LEN => {
                Value::Error(ErrorKind::Value)
            }
            (Ok(x), Ok(y)) => Value::Text(Arc::from(format!("{x}{y}"))),
            (Err(e), _) | (_, Err(e)) => Value::Error(e),
        },
        BinOp::Eq => Value::Bool(compare(&a, &b).is_eq()),
        BinOp::Ne => Value::Bool(!compare(&a, &b).is_eq()),
        BinOp::Lt => Value::Bool(compare(&a, &b).is_lt()),
        BinOp::Gt => Value::Bool(compare(&a, &b).is_gt()),
        BinOp::Le => Value::Bool(compare(&a, &b).is_le()),
        BinOp::Ge => Value::Bool(compare(&a, &b).is_ge()),
    }
}

/// A computed number as a cell value (`#NUM!` when not finite).
fn num(n: f64) -> Value {
    n.into_value()
}

/// An intermediate result as one value (see the module docs).
pub fn to_scalar(ev: Ev, ctx: &mut dyn Ctx) -> Value {
    match ev {
        Ev::V(v) => v,
        Ev::A(a, Some((r0, c0))) => {
            // Intersect like the reference it came from.
            let (row, col) = ctx.here();
            if a.rows == 1 && (c0..c0 + a.cols).contains(&col) {
                a.data[col - c0].clone()
            } else if a.cols == 1 && (r0..r0 + a.rows).contains(&row) {
                a.data[row - r0].clone()
            } else if a.rows * a.cols == 1 {
                a.data[0].clone()
            } else {
                Value::Error(ErrorKind::Value)
            }
        }
        Ev::A(a, None) => a.data.into_iter().next().unwrap_or(Value::Empty),
        Ev::R(r) => {
            if ctx.extent(r.sheet.as_deref()).is_none() {
                return Value::Error(ErrorKind::Ref);
            }
            if r.is_cell() {
                return ctx.cell(r.sheet.as_deref(), r.r0, r.c0);
            }
            let (row, col) = ctx.here();
            if r.r0 == r.r1 && (r.c0..=r.c1).contains(&col) {
                ctx.cell(r.sheet.as_deref(), r.r0, col)
            } else if r.c0 == r.c1 && (r.r0..=r.r1).contains(&row) {
                ctx.cell(r.sheet.as_deref(), row, r.c0)
            } else {
                Value::Error(ErrorKind::Value)
            }
        }
    }
}

/// The reference cell an intermediate is anchored at, if any.
fn origin(ev: &Ev) -> Option<(usize, usize)> {
    match ev {
        Ev::R(r) => Some((r.r0, r.c0)),
        Ev::A(_, o) => *o,
        _ => None,
    }
}

/// An intermediate as an array (keeping its anchor), read once.
fn as_array(ev: Ev, ctx: &mut dyn Ctx) -> Ev {
    let o = origin(&ev);
    let (rows, cols, data) = materialize(ev, ctx);
    Ev::A(Array { rows, cols, data }, o)
}

/// Whether some element of array `ev` is the boolean `want`.
fn picks(ev: &Ev, want: bool) -> bool {
    match ev {
        Ev::A(a, _) => a.data.iter().any(|v| v.to_bool() == Ok(want)),
        _ => true,
    }
}

/// Element-wise view of an intermediate: `(rows, cols, values)`.
fn materialize(ev: Ev, ctx: &mut dyn Ctx) -> (usize, usize, Vec<Value>) {
    match ev {
        Ev::V(v) => (1, 1, vec![v]),
        Ev::A(a, _) => (a.rows, a.cols, a.data),
        Ev::R(r) => {
            let Some(r) = ctx.clamp(&r) else {
                return (1, 1, vec![Value::Error(ErrorKind::Ref)]);
            };
            let (rows, cols) = (r.rows(), r.cols());
            if crate::formula::func::dense_cells(rows, cols).is_err() {
                return (1, 1, vec![Value::Error(ErrorKind::Num)]);
            }
            let mut data = vec![Value::Empty; rows.saturating_mul(cols)];
            ctx.for_each(&r, &mut |row, col, v| {
                data[(row - r.r0) * cols + (col - r.c0)] = v;
            });
            (rows, cols, data)
        }
    }
}

fn is_multi(ev: &Ev) -> bool {
    match ev {
        Ev::V(_) => false,
        Ev::A(a, _) => a.rows * a.cols != 1,
        Ev::R(r) => !r.is_cell(),
    }
}

fn map1(ev: Ev, ctx: &mut dyn Ctx, f: impl Fn(Value) -> Value) -> Ev {
    if !is_multi(&ev) {
        return Ev::V(f(to_scalar(ev, ctx)));
    }
    let o = origin(&ev);
    let (rows, cols, data) = materialize(ev, ctx);
    Ev::A(
        Array {
            rows,
            cols,
            data: data.into_iter().map(f).collect(),
        },
        o,
    )
}

/// Element-wise binary op with Excel's broadcasting: a 1-wide/1-tall side
/// repeats along that axis; past the end of a mismatched axis is `#N/A`.
fn map2(a: Ev, b: Ev, ctx: &mut dyn Ctx, f: impl Fn(Value, Value) -> Value) -> Ev {
    map_n(vec![a, b], ctx, |v| {
        let [x, y] = <[Value; 2]>::try_from(v).unwrap_or_else(|_| unreachable!());
        f(x, y)
    })
}

/// Element-wise op over any number of intermediates, broadcast like
/// [`map2`]; `f` gets one value from each, in order.
fn map_n(evs: Vec<Ev>, ctx: &mut dyn Ctx, f: impl Fn(Vec<Value>) -> Value) -> Ev {
    if !evs.iter().any(is_multi) {
        let vals = evs.into_iter().map(|e| to_scalar(e, ctx)).collect();
        return Ev::V(f(vals));
    }
    let o = evs.iter().find_map(origin);
    let mats: Vec<(usize, usize, Vec<Value>)> =
        evs.into_iter().map(|e| materialize(e, ctx)).collect();
    let rows = mats.iter().map(|m| m.0).max().unwrap_or(1);
    let cols = mats.iter().map(|m| m.1).max().unwrap_or(1);
    // Broadcasting a tall column against a wide row multiplies their sizes
    // (`A1:A1000000*A1:XFD1`): cap the result like any dense array.
    if func::dense_cells(rows, cols).is_err() {
        return Ev::V(Value::Error(ErrorKind::Num));
    }
    let pick = |r: usize, c: usize, (n_r, n_c, d): &(usize, usize, Vec<Value>)| -> Value {
        let rr = if *n_r == 1 { 0 } else { r };
        let cc = if *n_c == 1 { 0 } else { c };
        if rr >= *n_r || cc >= *n_c {
            Value::Error(ErrorKind::NA)
        } else {
            d[rr * n_c + cc].clone()
        }
    };
    let mut data = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        for c in 0..cols {
            data.push(f(mats.iter().map(|m| pick(r, c, m)).collect()));
        }
    }
    Ev::A(Array { rows, cols, data }, o)
}

fn to_arg(expr: &Expr, ctx: &mut dyn Ctx) -> Arg {
    match expr {
        Expr::Missing => Arg::Omitted,
        e => match eval(e, ctx) {
            Ev::V(v) => Arg::Value(v),
            Ev::R(r) => Arg::Range(r),
            Ev::A(a, _) => Arg::Array(a),
        },
    }
}

/// A branch result: an omitted branch (`IF(A1,,1)`) is 0.
fn branch(expr: &Expr, ctx: &mut dyn Ctx) -> Ev {
    match expr {
        Expr::Missing => Ev::V(Value::Number(0.0)),
        e => eval(e, ctx),
    }
}

fn call(name: &str, args: &[Expr], ctx: &mut dyn Ctx) -> Ev {
    let err = |e| Ev::V(Value::Error(e));
    match name {
        "IF" => {
            let cond = eval(&args[0], ctx);
            let otherwise = |ctx: &mut dyn Ctx| match args.get(2) {
                Some(a) => branch(a, ctx),
                None => Ev::V(Value::Bool(false)),
            };
            if is_multi(&cond) {
                // An array condition picks element by element
                // (`SUM(IF(A1:A4>1, A1:A4, 0))`). A branch no element picks
                // isn't evaluated: what a formula reads is what it depends
                // on, and an unread branch mustn't make a cycle.
                let cond = as_array(cond, ctx);
                let yes = if picks(&cond, true) {
                    branch(&args[1], ctx)
                } else {
                    Ev::V(Value::Empty)
                };
                let no = if picks(&cond, false) {
                    otherwise(ctx)
                } else {
                    Ev::V(Value::Empty)
                };
                return map_n(vec![cond, yes, no], ctx, |v| {
                    let [c, y, n] = <[Value; 3]>::try_from(v).unwrap_or_else(|_| unreachable!());
                    match c.to_bool() {
                        Err(e) => Value::Error(e),
                        Ok(true) => y,
                        Ok(false) => n,
                    }
                });
            }
            match to_scalar(cond, ctx).to_bool() {
                Err(e) => err(e),
                Ok(true) => branch(&args[1], ctx),
                Ok(false) => otherwise(ctx),
            }
        }
        "IFS" => {
            if !args.len().is_multiple_of(2) {
                return err(ErrorKind::NA);
            }
            // Conditions in order, each only once the ones before it are
            // known false — so the first true one ends it, and nothing after
            // is read. An array condition switches to picking element by
            // element over it and the conditions after it.
            let mut picked: Vec<Ev> = Vec::new();
            for pair in args.chunks(2) {
                let cond = eval(&pair[0], ctx);
                if picked.is_empty() && !is_multi(&cond) {
                    match to_scalar(cond, ctx).to_bool() {
                        Err(e) => return err(e),
                        Ok(true) => return branch(&pair[1], ctx),
                        Ok(false) => continue,
                    }
                }
                let multi = is_multi(&cond);
                let cond = if multi { as_array(cond, ctx) } else { cond };
                let wanted = if multi {
                    picks(&cond, true)
                } else {
                    to_scalar(cond.clone(), ctx).to_bool() == Ok(true)
                };
                let value = if wanted {
                    branch(&pair[1], ctx)
                } else {
                    Ev::V(Value::Empty)
                };
                picked.push(cond);
                picked.push(value);
                if wanted && !multi {
                    break; // true everywhere: nothing after it is reached
                }
            }
            if picked.is_empty() {
                return err(ErrorKind::NA);
            }
            map_n(picked, ctx, |v| {
                for pair in v.chunks(2) {
                    match pair[0].to_bool() {
                        Err(e) => return Value::Error(e),
                        Ok(true) => return pair[1].clone(),
                        Ok(false) => {}
                    }
                }
                Value::Error(ErrorKind::NA)
            })
        }
        "IFERROR" | "IFNA" => {
            let only_na = name == "IFNA";
            let catches = |v: &Value| match v {
                Value::Error(ErrorKind::NA) => true,
                Value::Error(_) => !only_na,
                _ => false,
            };
            let ev = eval(&args[0], ctx);
            if is_multi(&ev) {
                let alt = to_scalar(branch(&args[1], ctx), ctx);
                return map1(ev, ctx, |v| if catches(&v) { alt.clone() } else { v });
            }
            let v = to_scalar(ev.clone(), ctx);
            if catches(&v) {
                branch(&args[1], ctx)
            } else {
                ev
            }
        }
        "CHOOSE" => {
            let index = to_scalar(eval(&args[0], ctx), ctx);
            let i = match index.to_number_in(ctx.date1904()) {
                Ok(n) => n.trunc(),
                Err(e) => return err(e),
            };
            if i < 1.0 || i as usize >= args.len() {
                return err(ErrorKind::Value);
            }
            branch(&args[i as usize], ctx)
        }
        "SWITCH" => {
            let target = to_scalar(eval(&args[0], ctx), ctx);
            if let Value::Error(e) = target {
                return err(e);
            }
            let rest = &args[1..];
            let pairs = rest.len() / 2;
            for i in 0..pairs {
                let v = to_scalar(eval(&rest[2 * i], ctx), ctx);
                if compare(&target, &v).is_eq() && !v.is_error() {
                    return branch(&rest[2 * i + 1], ctx);
                }
            }
            if rest.len() % 2 == 1 {
                branch(&rest[rest.len() - 1], ctx)
            } else {
                err(ErrorKind::NA)
            }
        }
        _ => {
            let args: Vec<Arg> = args.iter().map(|a| to_arg(a, ctx)).collect();
            if let Some(f) = func::lookup(name) {
                return match f.array {
                    Some(array) => match array(&args, ctx) {
                        Ok(a) => Ev::A(a, None),
                        Err(e) => err(e),
                    },
                    None => Ev::V((f.f)(&args, ctx)),
                };
            }
            match ctx.call_external(name, &args) {
                Some(v) => Ev::V(v),
                None => err(ErrorKind::Name),
            }
        }
    }
}

/// Whether `expr` calls a volatile function (`NOW`, `RAND`, …).
pub fn is_volatile(expr: &Expr) -> bool {
    let mut names = Vec::new();
    expr.calls(&mut names);
    names
        .iter()
        .any(|n| func::lookup(n).is_some_and(|f| f.volatile))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::formula::parse::parse;

    /// An in-memory grid for tests.
    #[derive(Default)]
    pub struct MockCtx {
        pub cells: HashMap<(usize, usize), Value>,
        pub here: (usize, usize),
    }

    impl MockCtx {
        pub fn with(cells: &[(&str, Value)]) -> Self {
            let mut m = MockCtx::default();
            for (name, v) in cells {
                let (r, c) = crate::grid::view::parse_cell_name(name).unwrap();
                m.cells.insert((r, c), v.clone());
            }
            m
        }
    }

    impl Ctx for MockCtx {
        fn cell(&self, sheet: Option<&str>, row: usize, col: usize) -> Value {
            if sheet.is_some() {
                return Value::Error(ErrorKind::Ref);
            }
            self.cells.get(&(row, col)).cloned().unwrap_or(Value::Empty)
        }
        fn extent(&self, sheet: Option<&str>) -> Option<(usize, usize)> {
            if sheet.is_some() {
                return None;
            }
            let rows = self.cells.keys().map(|k| k.0 + 1).max().unwrap_or(0);
            let cols = self.cells.keys().map(|k| k.1 + 1).max().unwrap_or(0);
            Some((rows, cols))
        }
        fn here(&self) -> (usize, usize) {
            self.here
        }
        fn now(&self) -> f64 {
            45000.5
        }
        fn random(&mut self) -> f64 {
            0.25
        }
    }

    fn ev(src: &str, ctx: &mut MockCtx) -> Value {
        eval_cell(&parse(src).unwrap(), ctx)
    }

    fn n(x: f64) -> Value {
        Value::Number(x)
    }

    #[test]
    fn the_largest_parseable_trees_evaluate_without_overflow() {
        // The parser's caps (nesting 256, tree height 1024) bound the trees
        // eval, print and drop recurse over; the extremes, and both at once,
        // must fit a test thread's stack (2 MiB, smaller than the UI thread's).
        let mut c = MockCtx::default();
        let long = format!("={}1", "1+".repeat(1000));
        assert_eq!(ev(&long, &mut c), n(1001.0));
        let both = format!(
            "={}{}1{}",
            "1+".repeat(700),
            "ABS(".repeat(250),
            ")".repeat(250)
        );
        assert_eq!(ev(&both, &mut c), n(701.0));
        let deep = format!("={}1{}", "ABS(".repeat(250), ")".repeat(250));
        assert_eq!(ev(&deep, &mut c), n(1.0));
        let parens = format!("={}1{}", "(".repeat(250), ")".repeat(250));
        assert_eq!(ev(&parens, &mut c), n(1.0));
    }

    #[test]
    fn arithmetic_and_precedence() {
        let mut c = MockCtx::default();
        assert_eq!(ev("=1+2*3", &mut c), n(7.0));
        assert_eq!(ev("=-2^2", &mut c), n(4.0));
        assert_eq!(ev("=2^3^2", &mut c), n(64.0));
        assert_eq!(ev("=50%", &mut c), n(0.5));
        assert_eq!(ev("=\"3\"+1", &mut c), n(4.0));
        assert_eq!(ev("=TRUE+1", &mut c), n(2.0));
        assert_eq!(ev("=1/0", &mut c), Value::Error(ErrorKind::Div0));
        assert_eq!(ev("=\"a\"+1", &mut c), Value::Error(ErrorKind::Value));
        assert_eq!(ev("=1<2<3", &mut c), Value::Bool(false));
        assert_eq!(ev("=\"abc\"=\"ABC\"", &mut c), Value::Bool(true));
        assert_eq!(ev("=1&2", &mut c), Value::text("12"));
        assert_eq!(ev("=0^0", &mut c), Value::Error(ErrorKind::Num));
        assert_eq!(ev("=(-8)^(1/3)", &mut c), Value::Error(ErrorKind::Num));
        assert_eq!(ev("=NOSUCH(1)", &mut c), Value::Error(ErrorKind::Name));
        assert_eq!(ev("=FOO", &mut c), Value::Error(ErrorKind::Name));
    }

    #[test]
    fn references_and_intersection() {
        let mut c = MockCtx::with(&[("A1", n(1.0)), ("A2", n(2.0)), ("A3", Value::text("x"))]);
        assert_eq!(ev("=A1+A2", &mut c), n(3.0));
        assert_eq!(ev("=B9", &mut c), n(0.0), "empty cell reads as 0");
        assert_eq!(
            ev("=SUM(A1:A3)", &mut c),
            n(3.0),
            "text in ranges is skipped"
        );
        assert_eq!(ev("=SUM(A:A)", &mut c), n(3.0));
        c.here = (1, 4);
        assert_eq!(
            ev("=A1:A3*10", &mut c),
            n(20.0),
            "implicit intersection on row 2"
        );
        c.here = (7, 4);
        assert_eq!(ev("=A1:A3", &mut c), Value::Error(ErrorKind::Value));
        assert_eq!(ev("=Other!A1", &mut c), Value::Error(ErrorKind::Ref));
    }

    #[test]
    fn short_circuit_forms() {
        let mut c = MockCtx::with(&[("A1", n(0.0))]);
        assert_eq!(ev("=IF(A1=0,\"zero\",1/0)", &mut c), Value::text("zero"));
        assert_eq!(ev("=IF(FALSE,1)", &mut c), Value::Bool(false));
        assert_eq!(ev("=IF(TRUE,,1)", &mut c), n(0.0));
        assert_eq!(ev("=IFERROR(1/A1,-1)", &mut c), n(-1.0));
        assert_eq!(ev("=IFNA(1/A1,-1)", &mut c), Value::Error(ErrorKind::Div0));
        assert_eq!(ev("=IFNA(#N/A,7)", &mut c), n(7.0));
        assert_eq!(ev("=IFS(A1>1,\"a\",A1=0,\"b\")", &mut c), Value::text("b"));
        assert_eq!(ev("=IFS(A1>1,\"a\")", &mut c), Value::Error(ErrorKind::NA));
        assert_eq!(ev("=CHOOSE(2,\"a\",\"b\",\"c\")", &mut c), Value::text("b"));
        assert_eq!(
            ev("=CHOOSE(4,\"a\",\"b\",\"c\")", &mut c),
            Value::Error(ErrorKind::Value)
        );
        assert_eq!(
            ev("=SWITCH(2,1,\"one\",2,\"two\",\"other\")", &mut c),
            Value::text("two")
        );
        assert_eq!(
            ev("=SWITCH(9,1,\"one\",\"other\")", &mut c),
            Value::text("other")
        );
        assert_eq!(
            ev("=SWITCH(9,1,\"one\")", &mut c),
            Value::Error(ErrorKind::NA)
        );
    }

    #[test]
    fn arrays_broadcast() {
        let mut c = MockCtx::default();
        assert_eq!(
            ev("={1,2,3}*2", &mut c),
            n(2.0),
            "a cell shows the top-left element"
        );
        assert_eq!(ev("=SUM({1,2;3,4}*10)", &mut c), n(100.0));
        assert_eq!(ev("=SUM({1,2,3}+{10;20})", &mut c), n(102.0));
        assert_eq!(ev("=SUM(IFERROR({1,0}/{1,0},5))", &mut c), n(6.0));
    }

    #[test]
    fn function_args_intersect_ranges() {
        let mut c = MockCtx::with(&[("A1", n(-1.0)), ("A2", n(-2.0)), ("A3", n(-3.0))]);
        c.here = (1, 4);
        // Legacy Excel: ABS(A1:A3) in row 2 is ABS(A2).
        assert_eq!(ev("=ABS(A1:A3)", &mut c), n(2.0));
    }

    #[test]
    fn misc_excel_semantics() {
        let mut c = MockCtx::with(&[("A1", n(1.0))]);
        assert_eq!(ev("=-A1^2", &mut c), n(1.0));
        assert_eq!(ev("=2^-1", &mut c), n(0.5));
        assert_eq!(ev("=\"1\"=1", &mut c), Value::Bool(false));
        assert_eq!(ev("=B1=\"\"", &mut c), Value::Bool(true));
        assert_eq!(ev("=B1=0", &mut c), Value::Bool(true));
        assert_eq!(ev("=B1=FALSE", &mut c), Value::Bool(true));
        assert_eq!(ev("=-B1", &mut c), n(0.0));
        assert_eq!(ev("=B1&\"x\"", &mut c), Value::text("x"));
        assert_eq!(ev("=5%%", &mut c), n(0.0005));
        assert_eq!(ev("=\"a\"<\"B\"", &mut c), Value::Bool(true));
        assert_eq!(ev("=TRUE>1000", &mut c), Value::Bool(true));
        assert_eq!(ev("=\"x\">1000", &mut c), Value::Bool(true));
        assert_eq!(ev("=CHOOSE(1.9,\"a\",\"b\")", &mut c), Value::text("a"));
        assert_eq!(ev("=IF(\"x\",1,2)", &mut c), Value::Error(ErrorKind::Value));
        assert_eq!(ev("=SUM(A1:A3*2)", &mut c), n(2.0));
    }

    #[test]
    #[ignore = "known limitation: an open-ended range is clamped to the data before arithmetic, so implicit intersection below the data has no element to pick"]
    fn whole_column_arithmetic_below_the_data() {
        let mut c = MockCtx::with(&[("A1", n(1.0)), ("A2", n(2.0)), ("A3", n(3.0))]);
        c.here = (9, 4);
        // Excel: implicit intersection picks A10 (empty) → 0.
        assert_eq!(ev("=A:A*2", &mut c), n(0.0));
    }
}
