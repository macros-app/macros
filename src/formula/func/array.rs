//! Dynamic-array functions: `FILTER`, `SORT`, `SORTBY`, `UNIQUE` and
//! `SEQUENCE`. Each returns a whole [`Array`], which a formula spills into
//! the cells below and to the right of it (see the engine), and which other
//! functions take like any array (`SUM(FILTER(…))`).
//!
//! Comparisons follow Excel's: text case-insensitively, and in a sort
//! numbers before text before booleans before errors, with blanks last in
//! either direction. An empty result is `#CALC!` (Excel's "empty array").

use std::cmp::Ordering;
use std::collections::HashMap;

use super::args::{MANY, area, def, dense_cells, given, opt_bool, trunc_i64};
use super::{Arg, Array, Ctx, FuncDef, num, scalar};
use crate::formula::value::{ErrorKind, Value, cmp_text};

pub(super) const FUNCS: &[FuncDef] = &[
    def!(
        array "FILTER",
        2,
        3,
        filter,
        "array, include, [if_empty]"
    ),
    def!(
        array "SORT",
        1,
        4,
        sort,
        "array, [sort_index], [sort_order], [by_col]"
    ),
    def!(
        array "SORTBY",
        2,
        MANY,
        sortby,
        "array, by_array1, [sort_order1], [by_array2, sort_order2], …"
    ),
    def!(
        array "UNIQUE",
        1,
        3,
        unique,
        "array, [by_col], [exactly_once]"
    ),
    def!(
        array "SEQUENCE",
        1,
        4,
        sequence,
        "rows, [columns], [start], [step]"
    ),
];

/// An argument as an array: a range's cells (open ends clamped to the
/// data), an array, or a lone value as 1×1. A lone error is that error.
fn to_array(arg: &Arg, ctx: &mut dyn Ctx) -> Result<Array, ErrorKind> {
    match arg {
        Arg::Array(a) => Ok(a.clone()),
        Arg::Value(Value::Error(e)) => Err(*e),
        Arg::Omitted => Ok(Array {
            rows: 1,
            cols: 1,
            data: vec![Value::Empty],
        }),
        _ => {
            let a = area(arg, ctx)?;
            let (rows, cols) = (a.rows(), a.cols());
            let data = a.values(ctx)?;
            Ok(Array { rows, cols, data })
        }
    }
}

/// Row `r` of `a`.
fn row(a: &Array, r: usize) -> &[Value] {
    &a.data[r * a.cols..(r + 1) * a.cols]
}

/// `a` with rows and columns swapped (so by-column work reuses the row
/// code).
fn transpose(a: &Array) -> Array {
    let mut data = Vec::with_capacity(a.data.len());
    for c in 0..a.cols {
        for r in 0..a.rows {
            data.push(a.get(r, c).clone());
        }
    }
    Array {
        rows: a.cols,
        cols: a.rows,
        data,
    }
}

/// The rows of `a` at `picked`, in that order.
fn take_rows(a: &Array, picked: &[usize]) -> Array {
    let mut data = Vec::with_capacity(picked.len() * a.cols);
    for &r in picked {
        data.extend_from_slice(row(a, r));
    }
    Array {
        rows: picked.len(),
        cols: a.cols,
        data,
    }
}

/// Rows picked out of `a` (by column when `by_col`), or `#CALC!` for none.
fn pick(a: &Array, by_col: bool, picked: &[usize]) -> Result<Array, ErrorKind> {
    if picked.is_empty() {
        return Err(ErrorKind::Calc);
    }
    Ok(if by_col {
        transpose(&take_rows(&transpose(a), picked))
    } else {
        take_rows(a, picked)
    })
}

/// `FILTER(array, include, [if_empty])`: the rows (for an `include`
/// column as tall as `array`) or columns (for an `include` row as wide)
/// whose `include` value is true.
fn filter(args: &[Arg], ctx: &mut dyn Ctx) -> Result<Array, ErrorKind> {
    let a = to_array(&args[0], ctx)?;
    let inc = to_array(&args[1], ctx)?;
    let by_col = if inc.cols == 1 && inc.rows == a.rows {
        false
    } else if inc.rows == 1 && inc.cols == a.cols {
        true
    } else {
        return Err(ErrorKind::Value);
    };
    let mut picked = Vec::new();
    for (i, v) in inc.data.iter().enumerate() {
        let keep = match v {
            Value::Error(e) => return Err(*e),
            Value::Empty => false,
            Value::Text(_) => return Err(ErrorKind::Value),
            v => v.to_bool()?,
        };
        if keep {
            picked.push(i);
        }
    }
    if picked.is_empty()
        && let Some(alt) = given(args.get(2))
    {
        return to_array(alt, ctx);
    }
    pick(&a, by_col, &picked)
}

/// Excel's sort order for one key: numbers, text (case-insensitive),
/// booleans, errors; `descending` reverses that, and blanks sort last
/// either way.
fn sort_cmp(x: &Value, y: &Value, descending: bool) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            Value::Error(_) => 3,
            Value::Empty => 4,
        }
    }
    match (x, y) {
        (Value::Empty, Value::Empty) => return Ordering::Equal,
        (Value::Empty, _) => return Ordering::Greater,
        (_, Value::Empty) => return Ordering::Less,
        _ => {}
    }
    let ord = match (x, y) {
        (Value::Number(a), Value::Number(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Text(a), Value::Text(b)) => cmp_text(a, b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Error(a), Value::Error(b)) => a.code().cmp(&b.code()),
        _ => rank(x).cmp(&rank(y)),
    };
    if descending { ord.reverse() } else { ord }
}

/// A sort order argument: 1 ascending (the default), -1 descending.
fn order(arg: Option<&Arg>, ctx: &mut dyn Ctx) -> Result<bool, ErrorKind> {
    match given(arg) {
        None => Ok(false),
        Some(a) => direction(num(a, ctx)?),
    }
}

/// A sort order number: 1 ascending, -1 descending (anything else is
/// `#VALUE!`). Returns whether it's descending.
fn direction(n: f64) -> Result<bool, ErrorKind> {
    if n == 1.0 {
        Ok(false)
    } else if n == -1.0 {
        Ok(true)
    } else {
        Err(ErrorKind::Value)
    }
}

/// The values of a number-or-array argument (`SORT`'s `{1,2}` keys).
fn numbers_of(arg: &Arg, ctx: &mut dyn Ctx) -> Result<Vec<f64>, ErrorKind> {
    to_array(arg, ctx)?
        .data
        .iter()
        .map(|v| v.to_number_in(ctx.date1904()))
        .collect()
}

/// Row indices of `keys` (one column per key) sorted stably, each key
/// ascending or descending as `desc` says.
fn sorted_rows(n: usize, keys: &[(Vec<Value>, bool)]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&i, &j| {
        keys.iter()
            .map(|(k, desc)| sort_cmp(&k[i], &k[j], *desc))
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    });
    idx
}

/// `SORT(array, [sort_index], [sort_order], [by_col])`: rows ordered by
/// column `sort_index` (or columns by row, `by_col`). `sort_index` and
/// `sort_order` may be arrays, for sorting on several keys.
fn sort(args: &[Arg], ctx: &mut dyn Ctx) -> Result<Array, ErrorKind> {
    let a = to_array(&args[0], ctx)?;
    let by_col = opt_bool(args.get(3), ctx, false)?;
    let a = if by_col { transpose(&a) } else { a };
    let indexes = match given(args.get(1)) {
        None => vec![1.0],
        Some(arg) => numbers_of(arg, ctx)?,
    };
    let orders: Vec<bool> = match given(args.get(2)) {
        None => vec![false],
        Some(arg) => numbers_of(arg, ctx)?
            .into_iter()
            .map(direction)
            .collect::<Result<_, _>>()?,
    };
    if orders.len() != 1 && orders.len() != indexes.len() {
        return Err(ErrorKind::Value);
    }
    let mut keys = Vec::with_capacity(indexes.len());
    for (k, &i) in indexes.iter().enumerate() {
        let i = trunc_i64(i)?;
        if i < 1 || i as usize > a.cols {
            return Err(ErrorKind::Value);
        }
        let col: Vec<Value> = (0..a.rows)
            .map(|r| a.get(r, i as usize - 1).clone())
            .collect();
        keys.push((col, orders[if orders.len() == 1 { 0 } else { k }]));
    }
    let sorted = take_rows(&a, &sorted_rows(a.rows, &keys));
    Ok(if by_col { transpose(&sorted) } else { sorted })
}

/// `SORTBY(array, by_array1, [sort_order1], …)`: rows (for `by` columns
/// as tall as `array`) or columns (for `by` rows as wide) ordered by other
/// arrays.
fn sortby(args: &[Arg], ctx: &mut dyn Ctx) -> Result<Array, ErrorKind> {
    let a = to_array(&args[0], ctx)?;
    let mut keys = Vec::new();
    let mut by_col = None;
    for pair in args[1..].chunks(2) {
        let by = to_array(&pair[0], ctx)?;
        let this = if by.cols == 1 && by.rows == a.rows && by_col != Some(true) {
            false
        } else if by.rows == 1 && by.cols == a.cols && by_col != Some(false) {
            true
        } else {
            return Err(ErrorKind::Value);
        };
        by_col = Some(this);
        keys.push((by.data, order(pair.get(1), ctx)?));
    }
    let by_col = by_col.unwrap_or(false);
    let a = if by_col { transpose(&a) } else { a };
    let sorted = take_rows(&a, &sorted_rows(a.rows, &keys));
    Ok(if by_col { transpose(&sorted) } else { sorted })
}

/// A value as a hashable key for `UNIQUE`: numbers by value, text
/// case-folded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Num(u64),
    Text(String),
    Bool(bool),
    Error(u8),
    Empty,
}

fn key(v: &Value) -> Key {
    match v {
        Value::Number(n) => Key::Num(if *n == 0.0 { 0 } else { n.to_bits() }),
        Value::Text(s) => Key::Text(s.to_lowercase()),
        Value::Bool(b) => Key::Bool(*b),
        Value::Error(e) => Key::Error(e.code()),
        Value::Empty => Key::Empty,
    }
}

/// `UNIQUE(array, [by_col], [exactly_once])`: the distinct rows (or
/// columns) in order of first appearance; with `exactly_once`, only those
/// that appear once.
fn unique(args: &[Arg], ctx: &mut dyn Ctx) -> Result<Array, ErrorKind> {
    let a = to_array(&args[0], ctx)?;
    let by_col = opt_bool(args.get(1), ctx, false)?;
    let once = opt_bool(args.get(2), ctx, false)?;
    let t = if by_col { transpose(&a) } else { a.clone() };
    let mut first: Vec<usize> = Vec::new();
    let mut count: HashMap<Vec<Key>, (usize, usize)> = HashMap::new();
    for r in 0..t.rows {
        let k: Vec<Key> = row(&t, r).iter().map(key).collect();
        let e = count.entry(k).or_insert_with(|| {
            first.push(r);
            (r, 0)
        });
        e.1 += 1;
    }
    if once {
        let mut seen = vec![0usize; t.rows];
        for &(r, n) in count.values() {
            seen[r] = n;
        }
        first.retain(|&r| seen[r] == 1);
    }
    pick(&a, by_col, &first)
}

/// `SEQUENCE(rows, [columns], [start], [step])`: numbers counting from
/// `start` by `step`, filling each row before the next.
fn sequence(args: &[Arg], ctx: &mut dyn Ctx) -> Result<Array, ErrorKind> {
    let dim = |arg: Option<&Arg>, ctx: &mut dyn Ctx| -> Result<usize, ErrorKind> {
        let n = match given(arg) {
            None => 1,
            Some(a) => match scalar(a, ctx) {
                Value::Empty => 1,
                v => trunc_i64(v.to_number_in(ctx.date1904())?)?,
            },
        };
        match n {
            ..0 => Err(ErrorKind::Value),
            0 => Err(ErrorKind::Calc),
            n => usize::try_from(n).map_err(|_| ErrorKind::Num),
        }
    };
    let rows = dim(args.first(), ctx)?;
    let cols = dim(args.get(1), ctx)?;
    let start = super::args::opt_num(args.get(2), ctx, 1.0)?;
    let step = super::args::opt_num(args.get(3), ctx, 1.0)?;
    let n = dense_cells(rows, cols)?;
    let data = (0..n)
        .map(|i| super::IntoValue::into_value(start + step * i as f64))
        .collect();
    Ok(Array { rows, cols, data })
}

#[cfg(test)]
mod tests {
    use super::super::testing::Mock;
    use crate::formula::eval::{Outcome, eval_formula};
    use crate::formula::value::{Value, general_format};

    /// A formula's whole result as array-constant text (`{1,2;3,4}`), or
    /// its one value.
    fn spill(m: &mut Mock, src: &str) -> String {
        let e = crate::formula::parse::parse(src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        let show = |v: &Value| match v {
            Value::Number(n) => general_format(*n),
            Value::Text(s) => format!("\"{s}\""),
            v => v.to_string(),
        };
        match eval_formula(&e, m) {
            Outcome::Value(v) => show(&v),
            Outcome::Array(a) => {
                let rows: Vec<String> = a
                    .data
                    .chunks(a.cols)
                    .map(|r| r.iter().map(show).collect::<Vec<_>>().join(","))
                    .collect();
                format!("{{{}}}", rows.join(";"))
            }
        }
    }

    fn check(m: &mut Mock, cases: &[(&str, &str)]) {
        let bad: Vec<String> = cases
            .iter()
            .filter_map(|(src, want)| {
                let got = spill(m, src);
                (got != *want).then(|| format!("  {src}\n    got  {got}\n    want {want}"))
            })
            .collect();
        assert!(
            bad.is_empty(),
            "{} case(s) failed:\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    /// Names and scores, with a blank score and mixed-case duplicates.
    fn people() -> Mock {
        Mock::grid(&[
            &["bob", "3", "x"],
            &["Ann", "1", "y"],
            &["cy", "", "x"],
            &["BOB", "3", "x"],
            &["dee", "2", "y"],
        ])
    }

    #[test]
    fn sequence_counts() {
        check(
            &mut Mock::new(),
            &[
                ("=SEQUENCE(3)", "{1;2;3}"),
                ("=SEQUENCE(2,3)", "{1,2,3;4,5,6}"),
                ("=SEQUENCE(2,2,10,-5)", "{10,5;0,-5}"),
                ("=SEQUENCE(1,3,,0.5)", "{1,1.5,2}"),
                ("=SEQUENCE(2.9)", "{1;2}"),
                ("=SEQUENCE(1)", "1"),
                ("=SEQUENCE(0)", "#CALC!"),
                ("=SEQUENCE(-1)", "#VALUE!"),
                ("=SEQUENCE(\"a\")", "#VALUE!"),
                ("=SEQUENCE(100000,100000)", "#NUM!"),
                ("=SUM(SEQUENCE(100))", "5050"),
                ("=ROWS(SEQUENCE(4,2))", "4"),
            ],
        );
    }

    #[test]
    fn filter_rows_and_columns() {
        let mut m = people();
        check(
            &mut m,
            &[
                ("=FILTER(A1:A5,B1:B5>1)", "{\"bob\";\"BOB\";\"dee\"}"),
                (
                    "=FILTER(A1:B5,C1:C5=\"X\")",
                    "{\"bob\",3;\"cy\",0;\"BOB\",3}",
                ),
                ("=FILTER({1,2,3},{TRUE,FALSE,TRUE})", "{1,3}"),
                ("=FILTER(A1:A5,B1:B5>9)", "#CALC!"),
                ("=FILTER(A1:A5,B1:B5>9,\"none\")", "\"none\""),
                ("=FILTER(A1:A5,B1:B5>9,{0,0})", "{0,0}"),
                // The include array must match the array's height or width.
                ("=FILTER(A1:A5,B1:B4>1)", "#VALUE!"),
                ("=FILTER(A1:A5,A1:A5)", "#VALUE!"),
                ("=FILTER(A1:A2,{1;#N/A})", "#N/A"),
                ("=FILTER(A1:A2,{1;0})", "\"bob\""),
                ("=COUNTA(FILTER(A1:A5,C1:C5=\"y\"))", "2"),
            ],
        );
    }

    #[test]
    fn sort_orders_like_excel() {
        let mut m = people();
        check(
            &mut m,
            &[
                // Case-insensitive, stable: "bob" stays before "BOB".
                ("=SORT(A1:A5)", "{\"Ann\";\"bob\";\"BOB\";\"cy\";\"dee\"}"),
                (
                    "=SORT(A1:A5,1,-1)",
                    "{\"dee\";\"cy\";\"bob\";\"BOB\";\"Ann\"}",
                ),
                // Blanks sort last in either direction.
                ("=SORT(B1:B5)", "{1;2;3;3;0}"),
                ("=SORT(B1:B5,,-1)", "{3;3;2;1;0}"),
                (
                    "=SORT(A1:B5,2,1)",
                    "{\"Ann\",1;\"dee\",2;\"bob\",3;\"BOB\",3;\"cy\",0}",
                ),
                // Several keys: group, then score descending.
                (
                    "=SORT(B1:C5,{2,1},{1,-1})",
                    "{3,\"x\";3,\"x\";0,\"x\";2,\"y\";1,\"y\"}",
                ),
                ("=SORT({3,1,2},1,1,TRUE)", "{1,2,3}"),
                // Numbers before text before booleans before errors.
                ("=SORT({TRUE;\"a\";2;#N/A;1})", "{1;2;\"a\";TRUE;#N/A}"),
                ("=SORT(A1:A5,3)", "#VALUE!"),
                ("=SORT(A1:A5,0)", "#VALUE!"),
                ("=SORT(A1:A5,1,2)", "#VALUE!"),
                ("=SORT(#N/A)", "#N/A"),
                ("=SORT(5)", "5"),
            ],
        );
    }

    #[test]
    fn sortby_other_arrays() {
        let mut m = people();
        check(
            &mut m,
            &[
                (
                    "=SORTBY(A1:A5,B1:B5)",
                    "{\"Ann\";\"dee\";\"bob\";\"BOB\";\"cy\"}",
                ),
                (
                    "=SORTBY(A1:A5,C1:C5,-1,B1:B5,1)",
                    "{\"Ann\";\"dee\";\"bob\";\"BOB\";\"cy\"}",
                ),
                (
                    "=SORTBY({\"a\",\"b\",\"c\"},{3,1,2})",
                    "{\"b\",\"c\",\"a\"}",
                ),
                ("=SORTBY(A1:A5,B1:B4)", "#VALUE!"),
                ("=SORTBY(A1:A5,B1:B5,0)", "#VALUE!"),
                // Every by-array must run the same way.
                ("=SORTBY({1,2;3,4},{1;2},1,{1,2},1)", "#VALUE!"),
            ],
        );
    }

    #[test]
    fn unique_values_and_rows() {
        let mut m = people();
        check(
            &mut m,
            &[
                // Case-insensitive; the first spelling is kept.
                ("=UNIQUE(A1:A5)", "{\"bob\";\"Ann\";\"cy\";\"dee\"}"),
                ("=UNIQUE(A1:B5)", "{\"bob\",3;\"Ann\",1;\"cy\",0;\"dee\",2}"),
                ("=UNIQUE(C1:C5)", "{\"x\";\"y\"}"),
                ("=UNIQUE(A1:A5,,TRUE)", "{\"Ann\";\"cy\";\"dee\"}"),
                ("=UNIQUE({1,2,1,\"1\"},TRUE)", "{1,2,\"1\"}"),
                ("=UNIQUE({1,1},TRUE,TRUE)", "#CALC!"),
                ("=SORT(UNIQUE(C1:C5),,-1)", "{\"y\";\"x\"}"),
                ("=ROWS(UNIQUE(B1:B5))", "4"),
            ],
        );
    }
}
