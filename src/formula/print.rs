//! [`Expr`] → canonical formula text, the form a formula is stored and shown
//! in: upper-case function names, no spaces, `$` as anchored, numbers in
//! General format, the leading `=` included. [`print_file`] gives the form
//! `.xlsx` files store instead.

use super::ast::{Area, Coord, Expr, Ref, UnOp};
use super::value::general_format;

pub fn print(e: &Expr) -> String {
    let mut out = String::from("=");
    write_expr(e, false, &mut out);
    out
}

/// Formula text as an `.xlsx` file stores it (no leading `=`): functions
/// newer than Excel 2007 carry Excel's `_xlfn.` prefix (so Excel doesn't
/// read them as unknown names), and the `@` and `#` operators are written
/// as the `_xlfn.SINGLE(…)` and `_xlfn.ANCHORARRAY(…)` calls Excel uses.
/// [`super::parse::parse`] reads both forms back.
pub fn print_file(e: &Expr) -> String {
    let mut out = String::new();
    write_expr(e, true, &mut out);
    out
}

/// Functions this engine has that files spell with a prefix: `_xlfn.` for
/// those added after Excel 2007, `_xlfn._xlws.` for the two worksheet-only
/// dynamic-array functions.
fn file_prefix(name: &str) -> &'static str {
    const FUTURE: &[&str] = &[
        "CEILING.MATH",
        "CONCAT",
        "DAYS",
        "FLOOR.MATH",
        "IFNA",
        "IFS",
        "MAXIFS",
        "MINIFS",
        "MODE.SNGL",
        "NUMBERVALUE",
        "PERCENTILE.INC",
        "QUARTILE.INC",
        "RANK.AVG",
        "RANK.EQ",
        "SEQUENCE",
        "SORTBY",
        "STDEV.P",
        "STDEV.S",
        "SWITCH",
        "TEXTJOIN",
        "UNIQUE",
        "VAR.P",
        "VAR.S",
        "XLOOKUP",
        "XOR",
    ];
    match name {
        "FILTER" | "SORT" => "_xlfn._xlws.",
        n if FUTURE.contains(&n) => "_xlfn.",
        _ => "",
    }
}

fn write_expr(e: &Expr, file: bool, out: &mut String) {
    match e {
        Expr::Number(n) => out.push_str(&general_format(*n)),
        Expr::Text(s) => {
            out.push('"');
            out.push_str(&s.replace('"', "\"\""));
            out.push('"');
        }
        Expr::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
        Expr::Error(e) => out.push_str(e.as_str()),
        Expr::Array(rows) => {
            out.push('{');
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                for (j, item) in row.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    write_expr(item, file, out);
                }
            }
            out.push('}');
        }
        Expr::Ref(r) => write_ref(r, out),
        Expr::Unary(UnOp::At, e) if file => {
            out.push_str("_xlfn.SINGLE(");
            match &**e {
                Expr::Paren(inner) => write_expr(inner, file, out),
                e => write_expr(e, file, out),
            }
            out.push(')');
        }
        Expr::Unary(op, e) => {
            out.push(match op {
                UnOp::Neg => '-',
                UnOp::Plus => '+',
                UnOp::At => '@',
            });
            write_expr(e, file, out);
        }
        Expr::Percent(e) => {
            write_expr(e, file, out);
            out.push('%');
        }
        Expr::Spill(e) if file => {
            out.push_str("_xlfn.ANCHORARRAY(");
            write_expr(e, file, out);
            out.push(')');
        }
        Expr::Spill(e) => {
            write_expr(e, file, out);
            out.push('#');
        }
        Expr::Binary(op, a, b) => {
            write_expr(a, file, out);
            out.push_str(op.symbol());
            write_expr(b, file, out);
        }
        Expr::Call(name, args) => {
            if file {
                out.push_str(file_prefix(name));
            }
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_expr(a, file, out);
            }
            out.push(')');
        }
        Expr::Missing => {}
        Expr::Paren(e) => {
            out.push('(');
            write_expr(e, file, out);
            out.push(')');
        }
        Expr::Name(n) => out.push_str(n),
    }
}

pub fn write_ref(r: &Ref, out: &mut String) {
    if let Some(sheet) = &r.sheet {
        write_sheet(sheet, out);
        out.push('!');
    }
    match r.area {
        Area::Cell { row, col } => write_cell(row, col, out),
        Area::Range { r0, c0, r1, c1 } => {
            write_cell(r0, c0, out);
            out.push(':');
            write_cell(r1, c1, out);
        }
        Area::Cols { c0, c1 } => {
            write_col(c0, out);
            out.push(':');
            write_col(c1, out);
        }
        Area::Rows { r0, r1 } => {
            write_row(r0, out);
            out.push(':');
            write_row(r1, out);
        }
    }
}

/// A sheet name, quoted when it isn't a plain identifier (spaces,
/// punctuation, a leading digit, or text that looks like a cell).
fn write_sheet(name: &str, out: &mut String) {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && super::parse::parse(&format!("={name}")).map_or(true, |e| !matches!(e, Expr::Ref(_)));
    if plain {
        out.push_str(name);
    } else {
        out.push('\'');
        out.push_str(&name.replace('\'', "''"));
        out.push('\'');
    }
}

fn write_cell(row: Coord, col: Coord, out: &mut String) {
    write_col(col, out);
    write_row(row, out);
}

fn write_col(col: Coord, out: &mut String) {
    if col.abs {
        out.push('$');
    }
    out.push_str(&crate::grid::view::col_name(col.index));
}

fn write_row(row: Coord, out: &mut String) {
    if row.abs {
        out.push('$');
    }
    out.push_str(&(row.index + 1).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::parse::parse;

    #[test]
    fn file_form_prefixes_newer_functions() {
        for (typed, file) in [
            ("=SUM(A1:A3)", "SUM(A1:A3)"),
            ("=XLOOKUP(A1,B:B,C:C)", "_xlfn.XLOOKUP(A1,B:B,C:C)"),
            (
                "=SORT(UNIQUE(A1:A9))",
                "_xlfn._xlws.SORT(_xlfn.UNIQUE(A1:A9))",
            ),
            (
                "=FILTER(A1:A3,B1:B3>1)",
                "_xlfn._xlws.FILTER(A1:A3,B1:B3>1)",
            ),
            (
                "=SEQUENCE(3)+STDEV.S(A:A)",
                "_xlfn.SEQUENCE(3)+_xlfn.STDEV.S(A:A)",
            ),
            ("=SUM(A1#)", "SUM(_xlfn.ANCHORARRAY(A1))"),
            ("=@A1:A3*2", "_xlfn.SINGLE(A1:A3)*2"),
            ("=@(A1:A3*2)", "_xlfn.SINGLE(A1:A3*2)"),
        ] {
            let e = parse(typed).unwrap();
            assert_eq!(print_file(&e), file, "{typed}");
            // And it reads back as the same formula.
            assert_eq!(print(&parse(file).unwrap()), typed, "{file}");
        }
    }
}
