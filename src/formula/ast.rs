//! The formula syntax tree.
//!
//! References hold *absolute* 0-based coordinates plus each axis's `$`
//! anchoring. Absolute coordinates make inserting/deleting rows a plain shift
//! of every coordinate past the edit (Excel shifts `$` and non-`$` alike);
//! the anchoring flags only matter when a formula is copied or filled, where
//! [`super::refs::translate`] moves the unanchored axes. The printed A1 text
//! (`print.rs`) is the formula's canonical, stored form.

use std::sync::Arc;

use super::value::ErrorKind;

/// One axis of a reference: an index and whether it is `$`-anchored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Coord {
    pub index: usize,
    pub abs: bool,
}

impl Coord {
    pub fn new(index: usize, abs: bool) -> Self {
        Self { index, abs }
    }
}

/// The area a reference covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Area {
    /// `B3`, `$B$3`.
    Cell { row: Coord, col: Coord },
    /// `B3:D9` (corners as written; [`Area::bounds`] normalises).
    Range {
        r0: Coord,
        c0: Coord,
        r1: Coord,
        c1: Coord,
    },
    /// `A:C`, whole columns.
    Cols { c0: Coord, c1: Coord },
    /// `1:3`, whole rows.
    Rows { r0: Coord, r1: Coord },
}

impl Area {
    /// `(r0, c0, r1, c1)` inclusive and normalised; open ends are
    /// `usize::MAX` (`A:A` is rows `0..=MAX`).
    pub fn bounds(&self) -> (usize, usize, usize, usize) {
        match *self {
            Area::Cell { row, col } => (row.index, col.index, row.index, col.index),
            Area::Range { r0, c0, r1, c1 } => (
                r0.index.min(r1.index),
                c0.index.min(c1.index),
                r0.index.max(r1.index),
                c0.index.max(c1.index),
            ),
            Area::Cols { c0, c1 } => (
                0,
                c0.index.min(c1.index),
                usize::MAX,
                c0.index.max(c1.index),
            ),
            Area::Rows { r0, r1 } => (
                r0.index.min(r1.index),
                0,
                r0.index.max(r1.index),
                usize::MAX,
            ),
        }
    }
}

/// A reference, optionally on another sheet (`Sheet2!A1`, `'My data'!B:B`).
#[derive(Clone, Debug, PartialEq)]
pub struct Ref {
    pub sheet: Option<Arc<str>>,
    pub area: Area,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    /// `@`, implicit intersection: one value from a range or array (the
    /// cell in the formula's own row or column), as pre-dynamic-array Excel
    /// read every formula. Files store it as `_xlfn.SINGLE(…)`.
    At,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Pow => "^",
            BinOp::Concat => "&",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::Le => "<=",
            BinOp::Ge => ">=",
        }
    }

    /// Binding strength (higher binds tighter), Excel's order.
    pub fn precedence(self) -> u8 {
        match self {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => 1,
            BinOp::Concat => 2,
            BinOp::Add | BinOp::Sub => 3,
            BinOp::Mul | BinOp::Div => 4,
            BinOp::Pow => 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(f64),
    Text(Arc<str>),
    Bool(bool),
    Error(ErrorKind),
    /// An array constant `{1,2;3,4}`: rows of constants.
    Array(Vec<Vec<Expr>>),
    Ref(Ref),
    Unary(UnOp, Box<Expr>),
    /// Postfix `%`.
    Percent(Box<Expr>),
    /// Postfix `#` on a cell reference, the spill-range operator: `A1#` is
    /// the whole range the formula in `A1` spills into. Files store it as
    /// `_xlfn.ANCHORARRAY(A1)`.
    Spill(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// A function call; the name is upper-cased (as Excel stores it).
    Call(Arc<str>, Vec<Expr>),
    /// An omitted argument: `IF(A1,,1)`.
    Missing,
    /// Explicit parentheses, kept so the printed text matches what was typed.
    Paren(Box<Expr>),
    /// An identifier that isn't a function call or a reference: a defined
    /// name, which v1 doesn't support, so it evaluates to `#NAME?`.
    Name(Arc<str>),
}

impl Expr {
    /// Visit each direct sub-expression, in source order. Array constants
    /// count their elements (constants only, as the parser allows, so a
    /// walk finds nothing there — but every walk agrees).
    pub fn for_each_child(&self, f: &mut impl FnMut(&Expr)) {
        match self {
            Expr::Unary(_, e) | Expr::Percent(e) | Expr::Spill(e) | Expr::Paren(e) => f(e),
            Expr::Binary(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::Call(_, args) => args.iter().for_each(f),
            Expr::Array(rows) => rows.iter().flatten().for_each(f),
            Expr::Number(_)
            | Expr::Text(_)
            | Expr::Bool(_)
            | Expr::Error(_)
            | Expr::Ref(_)
            | Expr::Missing
            | Expr::Name(_) => {}
        }
    }

    /// [`Self::for_each_child`], mutably.
    pub fn for_each_child_mut(&mut self, f: &mut impl FnMut(&mut Expr)) {
        match self {
            Expr::Unary(_, e) | Expr::Percent(e) | Expr::Spill(e) | Expr::Paren(e) => f(e),
            Expr::Binary(_, a, b) => {
                f(a);
                f(b);
            }
            Expr::Call(_, args) => args.iter_mut().for_each(f),
            Expr::Array(rows) => rows.iter_mut().flatten().for_each(f),
            Expr::Number(_)
            | Expr::Text(_)
            | Expr::Bool(_)
            | Expr::Error(_)
            | Expr::Ref(_)
            | Expr::Missing
            | Expr::Name(_) => {}
        }
    }

    /// Every reference in the formula, in source order.
    pub fn refs(&self, out: &mut Vec<Ref>) {
        match self {
            Expr::Ref(r) => out.push(r.clone()),
            e => e.for_each_child(&mut |c| c.refs(out)),
        }
    }

    /// Every function name called, upper-case, in source order.
    pub fn calls(&self, out: &mut Vec<Arc<str>>) {
        if let Expr::Call(name, _) = self {
            out.push(name.clone());
        }
        self.for_each_child(&mut |c| c.calls(out));
    }

    /// Apply `f` to every reference, mutably (structural edits, copying).
    pub fn for_each_ref_mut(&mut self, f: &mut impl FnMut(&mut Expr)) {
        match self {
            Expr::Ref(_) => f(self),
            e => e.for_each_child_mut(&mut |c| c.for_each_ref_mut(f)),
        }
    }
}
