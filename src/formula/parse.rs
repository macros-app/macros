//! Formula text → [`Expr`]: a hand-written lexer and recursive-descent
//! parser for Excel's formula language (en-US form, as Excel stores it).
//!
//! Excel's precedence, loosest to tightest: comparison (`= <> < > <= >=`),
//! `&`, `+ -`, `* /`, `^`, unary `- +`, `%`, then the range operator `:`.
//! The quirks that follow from it are deliberate: `-2^2` is 4 (unary minus
//! binds tighter than `^`), `2^3^2` is 64 (`^` is left-associative), and
//! `1<2<3` compares `TRUE` with 3.

use std::sync::Arc;

use super::ast::{Area, BinOp, Coord, Expr, Ref, UnOp};
use super::func;
use super::value::{ErrorKind, parse_number};

/// Columns a reference may name: `A`..`XFD`, as in Excel (so `TAX2024` is a
/// cell, while `TAXES2024` is a name).
pub const MAX_COL: usize = crate::a1::MAX_COLS - 1;
/// Rows a reference may name. Excel stops at 1,048,576; the grid doesn't.
pub const MAX_ROW: usize = (1 << 31) - 1;

/// Why a formula didn't parse, with the byte offset where it went wrong.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub at: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at character {})", self.message, self.at + 1)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Err(ErrorKind),
    /// An identifier-like run: a name, function name, `TRUE`, a cell
    /// (`B3`, `$B$3`), a column (`A`, `$A`), or a row number (`3`, `$3`).
    Word(String),
    /// `Sheet1!` / `'My sheet'!`: the sheet a reference is on.
    Sheet(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
    Semi,
    LBrace,
    RBrace,
    Colon,
    End,
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            at: self.pos,
            message: message.into(),
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn tokens(mut self) -> Result<Vec<(usize, Tok)>, ParseError> {
        let mut out = Vec::new();
        loop {
            // Whitespace is insignificant (Excel's space-as-intersection
            // operator isn't supported).
            while let Some(c) = self.peek_char() {
                if c.is_whitespace() {
                    self.pos += c.len_utf8();
                } else {
                    break;
                }
            }
            let start = self.pos;
            let Some(c) = self.peek_char() else {
                out.push((start, Tok::End));
                return Ok(out);
            };
            let tok = match c {
                '"' => Tok::Str(self.string()?),
                '\'' => {
                    let name = self.quoted_sheet()?;
                    Tok::Sheet(name)
                }
                // Straight after a cell reference, `#` is the spill-range
                // operator (`A1#`); anywhere else it starts an error value.
                '#' if matches!(out.last(), Some((at, Tok::Word(w))) if at + w.len() == start) => {
                    self.one(Tok::Op("#"))
                }
                '#' => Tok::Err(self.error_literal()?),
                '@' => self.one(Tok::Op("@")),
                '0'..='9' | '.' => {
                    // A number, unless it's a row reference (`3:5`, `3!`
                    // can't happen) — rows are lexed as words when followed
                    // by a colon below.
                    self.number_or_word()?
                }
                '(' => self.one(Tok::LParen),
                ')' => self.one(Tok::RParen),
                ',' => self.one(Tok::Comma),
                ';' => self.one(Tok::Semi),
                '{' => self.one(Tok::LBrace),
                '}' => self.one(Tok::RBrace),
                ':' => self.one(Tok::Colon),
                '+' | '-' | '*' | '/' | '^' | '&' | '%' => {
                    self.pos += 1;
                    Tok::Op(match c {
                        '+' => "+",
                        '-' => "-",
                        '*' => "*",
                        '/' => "/",
                        '^' => "^",
                        '&' => "&",
                        _ => "%",
                    })
                }
                '=' => self.one(Tok::Op("=")),
                '<' => {
                    self.pos += 1;
                    match self.peek_char() {
                        Some('=') => self.one(Tok::Op("<=")),
                        Some('>') => self.one(Tok::Op("<>")),
                        _ => Tok::Op("<"),
                    }
                }
                '>' => {
                    self.pos += 1;
                    match self.peek_char() {
                        Some('=') => self.one(Tok::Op(">=")),
                        _ => Tok::Op(">"),
                    }
                }
                c if c.is_alphabetic() || c == '_' || c == '$' || c == '\\' => {
                    let word = self.word();
                    if self.peek_char() == Some('!') {
                        self.pos += 1;
                        Tok::Sheet(word)
                    } else {
                        Tok::Word(word)
                    }
                }
                other => return Err(self.err(format!("unexpected character '{other}'"))),
            };
            out.push((start, tok));
        }
    }

    fn one(&mut self, tok: Tok) -> Tok {
        self.pos += 1;
        tok
    }

    /// `"text"` with `""` as an escaped quote.
    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek_char() else {
                return Err(self.err("unterminated text (missing closing \")"));
            };
            self.pos += c.len_utf8();
            if c == '"' {
                if self.peek_char() == Some('"') {
                    self.pos += 1;
                    out.push('"');
                } else {
                    return Ok(out);
                }
            } else {
                out.push(c);
            }
        }
    }

    /// `'My sheet'!` with `''` as an escaped quote; the `!` is required.
    fn quoted_sheet(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek_char() else {
                return Err(self.err("unterminated sheet name (missing closing ')"));
            };
            self.pos += c.len_utf8();
            if c == '\'' {
                if self.peek_char() == Some('\'') {
                    self.pos += 1;
                    out.push('\'');
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        if self.peek_char() != Some('!') {
            return Err(self.err("expected ! after a quoted sheet name"));
        }
        self.pos += 1;
        Ok(out)
    }

    fn error_literal(&mut self) -> Result<ErrorKind, ParseError> {
        // Longest match among the known literals (`#N/A`, `#DIV/0!`, …).
        let rest = &self.src[self.pos..];
        for len in (2..=8).rev() {
            if let Some(cand) = rest.get(..len)
                && let Some(e) = ErrorKind::parse(cand)
            {
                self.pos += len;
                return Ok(e);
            }
        }
        Err(self.err("unknown error value"))
    }

    fn word(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c.is_alphanumeric() || c == '_' || c == '.' || c == '$' || c == '\\' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        self.src[start..self.pos].to_string()
    }

    /// A number literal, or — when digits run straight into `:` and another
    /// row (`3:5`) — a row word for the parser to pair up.
    fn number_or_word(&mut self) -> Result<Tok, ParseError> {
        let start = self.pos;
        let bytes = self.src.as_bytes();
        let mut i = self.pos;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let int_end = i;
        // `3:5` / `3:$5`: a whole-row reference.
        if int_end > start && bytes.get(int_end) == Some(&b':') {
            let after = &self.src[int_end + 1..];
            let after = after.strip_prefix('$').unwrap_or(after);
            if after.starts_with(|c: char| c.is_ascii_digit()) {
                self.pos = int_end;
                return Ok(Tok::Word(self.src[start..int_end].to_string()));
            }
        }
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_digit() {
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                i = j;
            }
        }
        let text = &self.src[start..i];
        let n = parse_number(text).ok_or_else(|| self.err(format!("bad number '{text}'")))?;
        self.pos = i;
        Ok(Tok::Num(n))
    }
}

/// Split `$AB$12` into its column and row parts (each optionally `$`).
fn split_cell(word: &str) -> Option<(Coord, Coord)> {
    let (col_abs, rest) = match word.strip_prefix('$') {
        Some(r) => (true, r),
        None => (false, word),
    };
    let letters = rest.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
    if letters == 0 || letters > 3 {
        return None;
    }
    let col = col_index(&rest[..letters])?;
    let rest = &rest[letters..];
    let (row_abs, digits) = match rest.strip_prefix('$') {
        Some(d) => (true, d),
        None => (false, rest),
    };
    let row = row_index(digits)?;
    Some((Coord::new(row, row_abs), Coord::new(col, col_abs)))
}

/// `A`/`$A` as a column.
fn split_col(word: &str) -> Option<Coord> {
    let (abs, rest) = match word.strip_prefix('$') {
        Some(r) => (true, r),
        None => (false, word),
    };
    if rest.is_empty() || rest.len() > 3 || !rest.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some(Coord::new(col_index(rest)?, abs))
}

/// `3`/`$3` as a row.
fn split_row(word: &str) -> Option<Coord> {
    let (abs, rest) = match word.strip_prefix('$') {
        Some(r) => (true, r),
        None => (false, word),
    };
    Some(Coord::new(row_index(rest)?, abs))
}

fn col_index(letters: &str) -> Option<usize> {
    crate::a1::parse_col_name(letters).filter(|&c| c <= MAX_COL)
}

fn row_index(digits: &str) -> Option<usize> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.starts_with('0') {
        return None;
    }
    let n: usize = digits.parse().ok()?;
    (1..=MAX_ROW + 1).contains(&n).then(|| n - 1)
}

struct Parser {
    toks: Vec<(usize, Tok)>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].1
    }

    fn at(&self) -> usize {
        self.toks[self.i].0
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.i].1.clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            at: self.at(),
            message: message.into(),
        }
    }

    fn expect(&mut self, tok: Tok, what: &str) -> Result<(), ParseError> {
        if *self.peek() == tok {
            self.next();
            Ok(())
        } else {
            Err(self.err(format!("expected {what}")))
        }
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.binary(1)
    }

    /// Precedence climbing over the binary operators (all left-associative,
    /// `^` included, as in Excel).
    fn binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.unary()?;
        while let Tok::Op(s) = self.peek() {
            let op = match *s {
                "+" => BinOp::Add,
                "-" => BinOp::Sub,
                "*" => BinOp::Mul,
                "/" => BinOp::Div,
                "^" => BinOp::Pow,
                "&" => BinOp::Concat,
                "=" => BinOp::Eq,
                "<>" => BinOp::Ne,
                "<" => BinOp::Lt,
                ">" => BinOp::Gt,
                "<=" => BinOp::Le,
                ">=" => BinOp::Ge,
                _ => break,
            };
            let prec = op.precedence();
            if prec < min_prec {
                break;
            }
            self.next();
            let rhs = self.binary(prec + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Prefix `-`/`+` bind tighter than every binary operator (even `^`).
    fn unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Tok::Op("-") => {
                self.next();
                Ok(Expr::Unary(UnOp::Neg, Box::new(self.unary()?)))
            }
            Tok::Op("+") => {
                self.next();
                Ok(Expr::Unary(UnOp::Plus, Box::new(self.unary()?)))
            }
            Tok::Op("@") => {
                self.next();
                Ok(Expr::Unary(UnOp::At, Box::new(self.unary()?)))
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::Op("%") => {
                    self.next();
                    e = Expr::Percent(Box::new(e));
                }
                Tok::Op("#") => {
                    if !is_cell(&e) {
                        return Err(self.err("'#' follows a single cell (A1#)"));
                    }
                    self.next();
                    e = Expr::Spill(Box::new(e));
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let at = self.at();
        let here = |message: String| ParseError { at, message };
        match self.next() {
            Tok::Num(n) => Ok(Expr::Number(n)),
            Tok::Str(s) => Ok(Expr::Text(Arc::from(s))),
            Tok::Err(e) => Ok(Expr::Error(e)),
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(Tok::RParen, ")")?;
                Ok(Expr::Paren(Box::new(e)))
            }
            Tok::LBrace => self.array(),
            Tok::Sheet(sheet) => {
                let Tok::Word(word) = self.next() else {
                    return Err(self.err("expected a reference after the sheet name"));
                };
                self.reference(Some(Arc::from(sheet)), &word)?
                    .ok_or_else(|| self.err(format!("'{word}' is not a cell reference")))
            }
            Tok::Word(word) => {
                if *self.peek() == Tok::LParen {
                    self.next();
                    return self.call(&word);
                }
                if let Some(r) = self.reference(None, &word)? {
                    return Ok(r);
                }
                if word.eq_ignore_ascii_case("TRUE") {
                    return Ok(Expr::Bool(true));
                }
                if word.eq_ignore_ascii_case("FALSE") {
                    return Ok(Expr::Bool(false));
                }
                Ok(Expr::Name(Arc::from(word.to_ascii_uppercase())))
            }
            Tok::Op(op) => Err(here(format!("unexpected '{op}'"))),
            Tok::End => Err(here("formula ends early".into())),
            other => Err(here(format!("unexpected {}", describe(&other)))),
        }
    }

    /// A cell, range, column range, or row range starting with `word`
    /// (already consumed). `None` when `word` isn't reference-shaped.
    fn reference(
        &mut self,
        sheet: Option<Arc<str>>,
        word: &str,
    ) -> Result<Option<Expr>, ParseError> {
        let colon = *self.peek() == Tok::Colon;
        let mk = |area| {
            Expr::Ref(Ref {
                sheet: sheet.clone(),
                area,
            })
        };
        if let Some((row, col)) = split_cell(word) {
            if !colon {
                return Ok(Some(mk(Area::Cell { row, col })));
            }
            self.next();
            let Tok::Word(w2) = self.next() else {
                return Err(self.err("expected a cell after ':'"));
            };
            let (r1, c1) =
                split_cell(&w2).ok_or_else(|| self.err(format!("'{w2}' is not a cell")))?;
            return Ok(Some(mk(Area::Range {
                r0: row,
                c0: col,
                r1,
                c1,
            })));
        }
        if colon && let Some(c0) = split_col(word) {
            let save = self.i;
            self.next();
            if let Tok::Word(w2) = self.next()
                && let Some(c1) = split_col(&w2)
            {
                return Ok(Some(mk(Area::Cols { c0, c1 })));
            }
            self.i = save;
        }
        if colon && let Some(r0) = split_row(word) {
            self.next();
            // The second row lexes as a word (`$3`) or a plain number (`3`).
            let w2 = match self.next() {
                Tok::Word(w) => w,
                Tok::Num(n) if n.fract() == 0.0 && n >= 1.0 => format!("{n}"),
                _ => return Err(self.err("expected a row after ':'")),
            };
            let r1 = split_row(&w2).ok_or_else(|| self.err(format!("'{w2}' is not a row")))?;
            return Ok(Some(mk(Area::Rows { r0, r1 })));
        }
        Ok(None)
    }

    fn call(&mut self, name: &str) -> Result<Expr, ParseError> {
        let name = name.to_ascii_uppercase();
        // Excel writes newer functions with an `_xlfn.` prefix in files
        // (`_xlfn._xlws.` for a few: `FILTER`, `SORT`).
        let future = name.starts_with("_XLFN.");
        let name = name.strip_prefix("_XLFN.").unwrap_or(&name);
        let name = name.strip_prefix("_XLWS.").unwrap_or(name).to_string();
        let mut args = Vec::new();
        if *self.peek() == Tok::RParen {
            self.next();
        } else {
            loop {
                if matches!(self.peek(), Tok::Comma | Tok::RParen) {
                    args.push(Expr::Missing);
                } else {
                    args.push(self.expr()?);
                }
                match self.next() {
                    Tok::Comma => continue,
                    Tok::RParen => break,
                    _ => return Err(self.err("expected ',' or ')' in the argument list")),
                }
            }
        }
        // The file forms of the `@` and `#` operators.
        if future && args.len() == 1 {
            match name.as_str() {
                "SINGLE" => {
                    let arg = match args.remove(0) {
                        e @ (Expr::Ref(_) | Expr::Call(..) | Expr::Paren(_) | Expr::Name(_)) => e,
                        e => Expr::Paren(Box::new(e)),
                    };
                    return Ok(Expr::Unary(UnOp::At, Box::new(arg)));
                }
                "ANCHORARRAY" if is_cell(&args[0]) => {
                    return Ok(Expr::Spill(Box::new(args.remove(0))));
                }
                _ => {}
            }
        }
        if let Some((min, max)) = arity(&name)
            && (args.len() < min || args.len() > max)
        {
            let want = if min == max {
                format!("{min}")
            } else if max == usize::MAX {
                format!("at least {min}")
            } else {
                format!("{min} to {max}")
            };
            return Err(self.err(format!(
                "{name} takes {want} argument{}, not {}",
                if want == "1" { "" } else { "s" },
                args.len()
            )));
        }
        Ok(Expr::Call(Arc::from(name), args))
    }

    /// `{1,2;3,4}`: constants only, rows separated by `;`.
    fn array(&mut self) -> Result<Expr, ParseError> {
        let mut rows = vec![Vec::new()];
        loop {
            let neg = if *self.peek() == Tok::Op("-") {
                self.next();
                true
            } else {
                false
            };
            let item = match self.next() {
                Tok::Num(n) => Expr::Number(if neg { -n } else { n }),
                Tok::Str(s) if !neg => Expr::Text(Arc::from(s)),
                Tok::Err(e) if !neg => Expr::Error(e),
                Tok::Word(w) if !neg && w.eq_ignore_ascii_case("TRUE") => Expr::Bool(true),
                Tok::Word(w) if !neg && w.eq_ignore_ascii_case("FALSE") => Expr::Bool(false),
                _ => {
                    return Err(
                        self.err("array constants hold numbers, text, TRUE/FALSE, or errors")
                    );
                }
            };
            rows.last_mut().unwrap().push(item);
            match self.next() {
                Tok::Comma => {}
                Tok::Semi => rows.push(Vec::new()),
                Tok::RBrace => break,
                _ => return Err(self.err("expected ',', ';' or '}' in an array")),
            }
        }
        let width = rows[0].len();
        if rows.iter().any(|r| r.len() != width) {
            return Err(self.err("array rows must have the same length"));
        }
        Ok(Expr::Array(rows))
    }
}

/// A single-cell reference (what `#` may follow).
fn is_cell(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Ref(Ref {
            area: Area::Cell { .. },
            ..
        })
    )
}

fn describe(t: &Tok) -> &'static str {
    match t {
        Tok::RParen => "')'",
        Tok::Comma => "','",
        Tok::Semi => "';'",
        Tok::RBrace => "'}'",
        Tok::Colon => "':'",
        _ => "token",
    }
}

/// `(min, max)` argument counts for a known function — built-in, or one of
/// the evaluator's own forms. `None` for unknown names (they parse, and
/// evaluate to `#NAME?` unless a Scheme function of that name exists).
pub fn arity(name: &str) -> Option<(usize, usize)> {
    if let Some(f) = func::lookup(name) {
        return Some((f.min_args, f.max_args));
    }
    func::special(name)
}

/// The references in formula text as typed so far (it needn't parse yet):
/// each one's byte span in `text` and what it covers. For colouring
/// references while a formula is edited, and point mode.
pub fn ref_spans(text: &str) -> Vec<(std::ops::Range<usize>, Ref)> {
    let body = text.strip_prefix('=').unwrap_or(text);
    let offset = text.len() - body.len();
    let Ok(toks) = (Lexer { src: body, pos: 0 }).tokens() else {
        return Vec::new();
    };
    // Each token's end: a word ends after its text; a sheet prefix runs on.
    let end_of = |i: usize| -> usize {
        match &toks[i].1 {
            Tok::Word(w) => toks[i].0 + w.len(),
            Tok::Num(_) => toks.get(i + 1).map_or(body.len(), |t| t.0),
            _ => toks[i].0 + 1,
        }
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let start = toks[i].0;
        let (sheet, j) = match &toks[i].1 {
            Tok::Sheet(name) => (Some(Arc::from(name.as_str())), i + 1),
            _ => (None, i),
        };
        let Some((_, Tok::Word(w))) = toks.get(j) else {
            i += 1;
            continue;
        };
        // A function name isn't a reference.
        if matches!(toks.get(j + 1), Some((_, Tok::LParen))) {
            i = j + 1;
            continue;
        }
        let colon = matches!(toks.get(j + 1), Some((_, Tok::Colon)));
        let second = toks.get(j + 2).and_then(|(_, t)| match t {
            Tok::Word(w) => Some(w.clone()),
            Tok::Num(n) if n.fract() == 0.0 && *n >= 1.0 => Some(format!("{n}")),
            _ => None,
        });
        let area = match (split_cell(w), colon, second.as_deref()) {
            (Some((r0, c0)), true, Some(w2)) => {
                split_cell(w2).map(|(r1, c1)| (Area::Range { r0, c0, r1, c1 }, j + 2))
            }
            (Some((row, col)), _, _) => Some((Area::Cell { row, col }, j)),
            (None, true, Some(w2)) => {
                match (split_col(w), split_col(w2), split_row(w), split_row(w2)) {
                    (Some(c0), Some(c1), _, _) => Some((Area::Cols { c0, c1 }, j + 2)),
                    (_, _, Some(r0), Some(r1)) => Some((Area::Rows { r0, r1 }, j + 2)),
                    _ => None,
                }
            }
            _ => None,
        };
        match area {
            Some((area, last)) => {
                out.push((offset + start..offset + end_of(last), Ref { sheet, area }));
                i = last + 1;
            }
            None => i = j + 1,
        }
    }
    out
}

/// The function call the caret at byte `caret` is inside, and which of its
/// arguments (0-based) — for the signature hint. Skips text in quotes and
/// array braces. `None` outside any call.
pub fn call_context(text: &str, caret: usize) -> Option<(String, usize)> {
    let before = text.get(..caret)?;
    // Walk back over the text, tracking nesting, to the innermost open `(`.
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (paren byte, commas)
    let mut in_str = false;
    let mut in_sheet = false; // inside a `'quoted sheet'` name
    let mut braces = 0usize;
    for (i, ch) in before.char_indices() {
        match ch {
            '"' if !in_sheet => in_str = !in_str,
            '\'' if !in_str => in_sheet = !in_sheet,
            _ if in_str || in_sheet => {}
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            '(' => stack.push((i, 0)),
            ')' => {
                stack.pop();
            }
            ',' if braces == 0 => {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
            }
            _ => {}
        }
    }
    let &(paren, commas) = stack.last()?;
    let name_start = before[..paren]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '.' || *c == '_')
        .last()
        .map(|(i, _)| i)?;
    let name = &before[name_start..paren];
    name.starts_with(|c: char| c.is_alphabetic())
        .then(|| (name.to_ascii_uppercase(), commas))
}

/// The partial identifier ending at byte `caret` (`=SU|` → `SU`), when it
/// could be the start of a function name, with its start offset.
pub fn word_before(text: &str, caret: usize) -> Option<(usize, &str)> {
    let before = text.get(..caret)?;
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '.' || *c == '_')
        .last()
        .map(|(i, _)| i)?;
    let word = &before[start..];
    let prev = before[..start].chars().next_back();
    // Not a function name: text inside a string or a quoted sheet name, or
    // the column after `:` in a range (`A:A`).
    (word.starts_with(|c: char| c.is_alphabetic())
        && prev.is_none_or(|c| !matches!(c, '"' | '!' | '$' | '\'' | ':'))
        && !inside_quotes(&before[..start]))
    .then_some((start, word))
}

/// Whether `text` ends inside an open `"string"` or `'sheet name'`.
fn inside_quotes(text: &str) -> bool {
    let (mut in_str, mut in_sheet) = (false, false);
    for ch in text.chars() {
        match ch {
            '"' if !in_sheet => in_str = !in_str,
            '\'' if !in_str => in_sheet = !in_sheet,
            _ => {}
        }
    }
    in_str || in_sheet
}

/// Parse formula text. A leading `=` is optional (it's stripped).
pub fn parse(text: &str) -> Result<Expr, ParseError> {
    let body = text.strip_prefix('=').unwrap_or(text);
    let offset = text.len() - body.len();
    let toks = Lexer { src: body, pos: 0 }.tokens().map_err(|mut e| {
        e.at += offset;
        e
    })?;
    let mut p = Parser { toks, i: 0 };
    let e = p.expr().map_err(|mut e| {
        e.at += offset;
        e
    })?;
    if *p.peek() != Tok::End {
        let mut err = p.err(match p.peek() {
            Tok::RParen => "unmatched ')'".to_string(),
            _ => "unexpected text after the formula".to_string(),
        });
        err.at += offset;
        return Err(err);
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formula::print::print;

    fn round(src: &str) -> String {
        print(&parse(src).unwrap_or_else(|e| panic!("{src}: {e}")))
    }

    #[test]
    fn round_trips_canonical_text() {
        for src in [
            "=1+2*3",
            "=SUM(A1:B2,$C$3,D:D,4:4)",
            "=-2^2",
            "=(1+2)*3",
            "=A1&\" \"&B1",
            "=IF(A1>=10,\"big\",\"small\")",
            "=Sheet2!A1+'My data'!$B$2",
            "=SUM({1,2;3,4})",
            "=50%",
            "=IF(A1,,1)",
            "=#N/A",
            "=TRUE",
            "=STDEV.P(A1:A9)",
            "=1E+20",
            "=\"say \"\"hi\"\"\"",
            "=A1:B2",
            "=$A:$A",
            "=$1:3",
        ] {
            assert_eq!(round(src), src, "round trip of {src}");
        }
        // Lower case and spacing normalise to Excel's stored form.
        assert_eq!(round("= sum( a1 , b$2 )"), "=SUM(A1,B$2)");
        assert_eq!(round("=_xlfn.xlookup(A1,B:B,C:C)"), "=XLOOKUP(A1,B:B,C:C)");
    }

    #[test]
    fn dynamic_array_operators() {
        for src in [
            "=A1#",
            "=SUM(Sheet2!$B$3#)",
            "=@A1:A3*10",
            "=-@B:B",
            "=A1#*2",
        ] {
            assert_eq!(round(src), src, "round trip of {src}");
        }
        assert!(matches!(parse("=A1#").unwrap(), Expr::Spill(_)));
        assert!(matches!(
            parse("=@A1:A3").unwrap(),
            Expr::Unary(UnOp::At, _)
        ));
        // `#` after anything but a cell is an error; spaced, it's an error value.
        assert!(parse("=A1:B2#").is_err());
        assert!(parse("=(A1)#").is_err());
        assert!(matches!(parse("=IF(A1,#N/A)").unwrap(), Expr::Call(..)));
        // The file forms Excel writes.
        assert_eq!(round("=_xlfn.ANCHORARRAY(A1)"), "=A1#");
        assert_eq!(round("=_xlfn.SINGLE(A1:A3)+1"), "=@A1:A3+1");
        assert_eq!(round("=_xlfn.SINGLE(A1:A3*2)"), "=@(A1:A3*2)");
        assert_eq!(round("=_xlfn._xlws.SORT(A1:A3)"), "=SORT(A1:A3)");
        assert_eq!(
            round("=_xlfn._xlws.FILTER(A1:A3,B1:B3>1)"),
            "=FILTER(A1:A3,B1:B3>1)"
        );
    }

    #[test]
    fn precedence_quirks() {
        let e = parse("=-2^2").unwrap();
        assert!(matches!(e, Expr::Binary(BinOp::Pow, ref l, _) if matches!(**l, Expr::Unary(..))));
        let e = parse("=2^3^2").unwrap();
        assert!(
            matches!(e, Expr::Binary(BinOp::Pow, ref l, _) if matches!(**l, Expr::Binary(BinOp::Pow, ..)))
        );
        let e = parse("=1<2<3").unwrap();
        assert!(
            matches!(e, Expr::Binary(BinOp::Lt, ref l, _) if matches!(**l, Expr::Binary(BinOp::Lt, ..)))
        );
        let e = parse("=1+2&3").unwrap();
        assert!(matches!(e, Expr::Binary(BinOp::Concat, ..)));
        let e = parse("=1=1&\"\"").unwrap();
        assert!(matches!(e, Expr::Binary(BinOp::Eq, ..)));
    }

    #[test]
    fn references_and_names() {
        assert!(matches!(parse("=TAX2024").unwrap(), Expr::Ref(_)));
        assert!(matches!(parse("=TAXES2024").unwrap(), Expr::Name(_)));
        assert!(matches!(parse("=XFD1").unwrap(), Expr::Ref(_)));
        assert!(matches!(parse("=XFE1").unwrap(), Expr::Name(_)));
        assert!(matches!(parse("=A0").unwrap(), Expr::Name(_)));
        let Expr::Ref(r) = parse("=$B3").unwrap() else {
            panic!()
        };
        assert_eq!(
            r.area,
            Area::Cell {
                row: Coord::new(2, false),
                col: Coord::new(1, true)
            }
        );
    }

    #[test]
    fn caret_context() {
        assert_eq!(
            call_context("=SUMIF(A:A,\"x\",", 15),
            Some(("SUMIF".into(), 2))
        );
        assert_eq!(call_context("=IF(A1,SUM(B1,", 14), Some(("SUM".into(), 1)));
        assert_eq!(call_context("=IF(A1,SUM(B1),", 15), Some(("IF".into(), 2)));
        assert_eq!(call_context("=A1+1", 5), None);
        assert_eq!(call_context("=SUM({1,2},", 11), Some(("SUM".into(), 1)));
        assert_eq!(word_before("=SU", 3), Some((1, "SU")));
        assert_eq!(word_before("=1+vl", 5), Some((3, "vl")));
        assert_eq!(word_before("=\"ab", 4), None);
        assert_eq!(word_before("=12", 3), None);
    }

    #[test]
    fn reference_spans_while_typing() {
        let text = "=SUM(A1:B2,$C$3)+Sheet2!D4+E:E+";
        let spans: Vec<&str> = ref_spans(text)
            .iter()
            .map(|(r, _)| &text[r.clone()])
            .collect();
        assert_eq!(spans, ["A1:B2", "$C$3", "Sheet2!D4", "E:E"]);
        // Unfinished text still yields what's there.
        let spans = ref_spans("=A1*(B");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 1..3);
        assert!(ref_spans("=SUM(").is_empty());
    }

    #[test]
    fn errors_point_at_the_problem() {
        for bad in [
            "=SUM(", "=1+", "=)", "=\"open", "=A1:", "=1 2", "={1,2;3}", "=SUM(1,2", "=@", "=SUM()",
        ] {
            assert!(parse(bad).is_err(), "{bad} should fail");
        }
        let e = parse("=1+*2").unwrap_err();
        assert_eq!(e.at, 3);
        // Garbage never panics.
        for s in [
            "",
            "=",
            "''!",
            "=#",
            "={",
            "=A:",
            "=1:",
            "=$",
            "=-",
            "=%%",
            "=\u{1F600}",
        ] {
            let _ = parse(s);
        }
    }

    #[test]
    fn more_round_trips() {
        for src in [
            "=Sheet2!1:3",
            "=SUM('My Sheet'!A:A)",
            "='It''s'!A1",
            "={-1,2;3,-4.5}",
            "=--A1",
            "=1-(-1)",
            "=A1%^2",
            "=-(1+2)",
            "=1E-10",
            "=0.000001",
            "=123456789012345",
            "=SUM(A1:B2 )",
            "='2024'!A1",
            "='A1'!B2",
            "=TRUE!A1",
            "=\"\"",
            "=LOG10(100)",
            "=$A$1:$B$2",
            "=1/3",
            "=0.1",
        ] {
            let p1 = parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
            let printed = print(&p1);
            let p2 = parse(&printed).unwrap_or_else(|e| panic!("{src} -> {printed}: {e}"));
            assert_eq!(p1, p2, "{src} -> {printed}");
        }
    }
}
