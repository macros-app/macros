//! Test support: an in-memory [`Ctx`] and a tiny call syntax for
//! table-driven function tests.
//!
//! `eval(&mut ctx, "ROUND(2.675, 2)")` calls the registered function with
//! literal arguments: numbers, `"text"` (`""` escapes a quote), `TRUE`,
//! `#N/A`, `{1,2;3,4}` arrays, `A1`/`B2:C9`/`A:A`/`Sheet2!A1` references,
//! an empty slot for an omitted argument, and nested calls (evaluated to a
//! value first). Expected results use the same literal syntax and compare
//! numbers at Excel's 15 significant digits.

use std::collections::HashMap;
use std::sync::Arc;

use super::lookup::LookupIndex;
use super::{Arg, Array, Ctx, RangeRef};
use crate::a1::{parse_col_name as col_index, parse_ref as cell};
use crate::formula::value::{ErrorKind, Value, general_format, parse_number};

/// A grid of cells on sheet `None` (and optionally `Sheet2`).
pub struct Mock {
    pub cells: HashMap<(Option<String>, usize, usize), Value>,
    pub here: (usize, usize),
    pub now: f64,
    pub rand: Vec<f64>,
    pub d1904: bool,
    /// Build and hand out lookup indexes (exercising the hash path).
    pub use_index: bool,
    pub index_builds: usize,
}

impl Mock {
    pub fn new() -> Mock {
        Mock {
            cells: HashMap::new(),
            here: (9, 9),
            // 2024-03-05 14:30 (serial 45356 + 14.5/24).
            now: 45356.0 + 14.5 / 24.0,
            rand: vec![0.25, 0.5, 0.75, 0.0, 0.999],
            d1904: false,
            use_index: false,
            index_builds: 0,
        }
    }

    /// Fill rows from A1 down. Each cell reads like typed input (`"3"` is a
    /// number); a leading `'` forces text, `""` leaves it empty.
    pub fn grid(rows: &[&[&str]]) -> Mock {
        let mut m = Mock::new();
        for (r, row) in rows.iter().enumerate() {
            for (c, s) in row.iter().enumerate() {
                m.put(None, r, c, s);
            }
        }
        m
    }

    pub fn put(&mut self, sheet: Option<&str>, r: usize, c: usize, s: &str) {
        let v = match s.strip_prefix('\'') {
            Some(t) => Value::text(t),
            None => Value::from_cell_text(s),
        };
        let key = (sheet.map(str::to_string), r, c);
        if v == Value::Empty {
            self.cells.remove(&key);
        } else {
            self.cells.insert(key, v);
        }
    }
}

impl Ctx for Mock {
    fn cell(&self, sheet: Option<&str>, row: usize, col: usize) -> Value {
        self.cells
            .get(&(sheet.map(str::to_string), row, col))
            .cloned()
            .unwrap_or(Value::Empty)
    }

    fn extent(&self, sheet: Option<&str>) -> Option<(usize, usize)> {
        let sheet = sheet.map(str::to_string);
        if sheet.as_deref().is_some_and(|s| s != "Sheet2") {
            return None;
        }
        let mut dims = (0, 0);
        for (s, r, c) in self.cells.keys() {
            if *s == sheet {
                dims.0 = dims.0.max(r + 1);
                dims.1 = dims.1.max(c + 1);
            }
        }
        Some(dims)
    }

    fn here(&self) -> (usize, usize) {
        self.here
    }

    fn now(&self) -> f64 {
        self.now
    }

    fn random(&mut self) -> f64 {
        let x = self.rand.remove(0);
        self.rand.push(x);
        x
    }

    fn date1904(&self) -> bool {
        self.d1904
    }

    fn lookup_index(&mut self, range: &RangeRef) -> Option<Arc<LookupIndex>> {
        if !self.use_index {
            return None;
        }
        self.index_builds += 1;
        Some(Arc::new(LookupIndex::build(self, range)))
    }
}

fn reference(s: &str) -> Option<RangeRef> {
    let (sheet, body) = match s.split_once('!') {
        Some((sh, b)) => (Some(Arc::from(sh)), b),
        None => (None, s),
    };
    let (a, b) = body.split_once(':').unwrap_or((body, body));
    if let (Some((r0, c0)), Some((r1, c1))) = (cell(a), cell(b)) {
        return Some(RangeRef {
            sheet,
            r0,
            c0,
            r1,
            c1,
        });
    }
    if let (Some(c0), Some(c1)) = (col_index(a), col_index(b)) {
        return Some(RangeRef {
            sheet,
            r0: 0,
            c0,
            r1: usize::MAX,
            c1,
        });
    }
    if let (Ok(r0), Ok(r1)) = (a.parse::<usize>(), b.parse::<usize>()) {
        return Some(RangeRef {
            sheet,
            r0: r0 - 1,
            c0: 0,
            r1: r1 - 1,
            c1: usize::MAX,
        });
    }
    None
}

/// Split at top-level commas (outside quotes, braces and parentheses).
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote = false;
    for ch in s.chars() {
        match ch {
            '"' => quote = !quote,
            '(' | '{' if !quote => depth += 1,
            ')' | '}' if !quote => depth -= 1,
            c if c == sep && !quote && depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out
}

/// A literal value: number, text, boolean, error.
pub fn literal(s: &str) -> Option<Value> {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return Some(Value::text(&s[1..s.len() - 1].replace("\"\"", "\"")));
    }
    if s.eq_ignore_ascii_case("TRUE") {
        return Some(Value::Bool(true));
    }
    if s.eq_ignore_ascii_case("FALSE") {
        return Some(Value::Bool(false));
    }
    if s.starts_with('#') {
        return ErrorKind::parse(s).map(Value::Error);
    }
    if s.is_empty() {
        return Some(Value::Empty);
    }
    let neg = s.strip_prefix('-');
    match neg {
        Some(rest) => parse_number(rest).map(|n| Value::Number(-n)),
        None => parse_number(s).map(Value::Number),
    }
}

fn arg(s: &str, ctx: &mut Mock) -> Arg {
    let s = s.trim();
    if s.is_empty() {
        return Arg::Omitted;
    }
    if let Some(inner) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
        let rows: Vec<Vec<Value>> = split_top(inner, ';')
            .iter()
            .map(|r| {
                split_top(r, ',')
                    .iter()
                    .map(|v| literal(v).expect("array literal"))
                    .collect()
            })
            .collect();
        let cols = rows[0].len();
        return Arg::Array(Array {
            rows: rows.len(),
            cols,
            data: rows.into_iter().flatten().collect(),
        });
    }
    if let Some(v) = literal(s) {
        return Arg::Value(v);
    }
    if s.ends_with(')') && s.contains('(') {
        return Arg::Value(eval(ctx, s));
    }
    Arg::Range(reference(s).unwrap_or_else(|| panic!("bad argument {s:?}")))
}

/// Call `NAME(args…)`.
pub fn eval(ctx: &mut Mock, src: &str) -> Value {
    let src = src.trim();
    let open = src.find('(').expect("call");
    let name = &src[..open];
    let inner = &src[open + 1..src.len() - 1];
    let def = super::lookup(name).unwrap_or_else(|| panic!("no function {name}"));
    let args: Vec<Arg> = if inner.trim().is_empty() {
        Vec::new()
    } else {
        split_top(inner, ',').iter().map(|a| arg(a, ctx)).collect()
    };
    assert!(
        args.len() >= def.min_args && args.len() <= def.max_args,
        "{src}: {} args outside {}..={}",
        args.len(),
        def.min_args,
        def.max_args
    );
    (def.f)(&args, ctx)
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => general_format(*x) == general_format(*y),
        _ => a == b,
    }
}

/// Run `(call, expected)` cases, reporting every mismatch at once.
pub fn check(ctx: &mut Mock, cases: &[(&str, &str)]) {
    let mut bad = Vec::new();
    for (src, want) in cases {
        let want_v = literal(want).unwrap_or_else(|| panic!("bad expectation {want:?}"));
        let got = eval(ctx, src);
        if !same(&got, &want_v) {
            bad.push(format!("  {src}\n    got  {got:?}\n    want {want_v:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} case(s) failed:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// [`check`] on an empty grid.
pub fn check0(cases: &[(&str, &str)]) {
    check(&mut Mock::new(), cases);
}

#[test]
fn registry_is_consistent() {
    let mut seen = std::collections::HashSet::new();
    for f in super::all() {
        assert!(seen.insert(f.name), "duplicate {}", f.name);
        assert_eq!(f.name, f.name.to_ascii_uppercase());
        assert!(f.min_args <= f.max_args, "{}", f.name);
    }
    // The short-circuiting forms belong to the evaluator.
    for name in ["IF", "IFS", "IFERROR", "IFNA", "CHOOSE", "SWITCH"] {
        assert!(super::lookup(name).is_none(), "{name} is evaluator-owned");
    }
    // Arity exactly as Excel's.
    for (name, min, max) in [
        ("SUM", 1, usize::MAX),
        ("ROUND", 2, 2),
        ("ROUNDUP", 2, 2),
        ("LEFT", 1, 2),
        ("MID", 3, 3),
        ("VLOOKUP", 3, 4),
        ("XLOOKUP", 3, 6),
        ("INDEX", 2, 4),
        ("TEXTJOIN", 3, usize::MAX),
        ("SUMIFS", 3, usize::MAX),
        ("COUNTIFS", 2, usize::MAX),
        ("NOW", 0, 0),
        ("ROW", 0, 1),
        ("FILTER", 2, 3),
        ("SORT", 1, 4),
        ("SORTBY", 2, usize::MAX),
        ("UNIQUE", 1, 3),
        ("SEQUENCE", 1, 4),
    ] {
        let f = super::lookup(name).unwrap();
        assert_eq!((f.min_args, f.max_args), (min, max), "{name}");
    }
    assert!(super::lookup("stdev.p").is_some());
    // Exactly the dynamic-array functions return whole arrays.
    let arrays: Vec<_> = super::all()
        .filter(|f| f.array.is_some())
        .map(|f| f.name)
        .collect();
    assert_eq!(arrays, ["FILTER", "SORT", "SORTBY", "UNIQUE", "SEQUENCE"]);
    let volatile: Vec<_> = super::all()
        .filter(|f| f.volatile)
        .map(|f| f.name)
        .collect();
    assert_eq!(volatile, ["RAND", "RANDBETWEEN", "TODAY", "NOW"]);
}

#[test]
fn time_rejects_huge_components() {
    check0(&[
        ("TIME(-900000000000000000, 0, 0)", "#NUM!"),
        ("TIME(0, -900000000000000000, 0)", "#NUM!"),
        ("TIME(-1, 120, 0)", "0.041666666666666664"),
    ]);
}

/// A whole formula (`=…`, operators and special forms included) on `m`.
fn formula(m: &mut Mock, src: &str) -> Value {
    let e = crate::formula::parse::parse(src).unwrap_or_else(|e| panic!("{src}: {e:?}"));
    crate::formula::eval::eval_cell(&e, m)
}

#[test]
fn text_coerces_to_numbers_as_typed() {
    check0(&[
        ("SUM(\"1,000\")", "1000"),
        ("ROUND(\"$5.25\", 0)", "5"),
        ("ABS(\"1/1/2024\")", "45292"),
        ("COUNT(\"1/1/2024\")", "1"),
    ]);
    let mut m = Mock::new();
    for (src, want) in [
        ("=\"1,000\"+1", Value::Number(1001.0)),
        ("=\"$5\"*2", Value::Number(10.0)),
        ("=\"2024-01-01\"+0", Value::Number(45292.0)),
        ("=\"(12)\"+0", Value::Number(-12.0)),
        ("=\"abc\"+1", Value::Error(ErrorKind::Value)),
    ] {
        assert_eq!(formula(&mut m, src), want, "{src}");
    }
}

#[test]
fn if_and_ifs_map_over_arrays() {
    let mut m = Mock::grid(&[&["1"], &["2"], &["3"], &["4"]]);
    for (src, want) in [
        ("=SUM(IF(A1:A4>1, A1:A4, 0))", Value::Number(9.0)),
        ("=SUMPRODUCT(IF(A1:A4>2, 1, 0))", Value::Number(2.0)),
        ("=SUM(IF(A1:A4>2, 10))", Value::Number(20.0)),
        (
            "=SUM(IFS(A1:A4<2, 100, A1:A4<4, 10, TRUE, 1))",
            Value::Number(121.0),
        ),
        ("=IF(A1>0, \"y\", \"n\")", Value::text("y")),
    ] {
        assert_eq!(formula(&mut m, src), want, "{src}");
    }
}
