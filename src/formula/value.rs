//! Formula values, Excel's error values, and the coercion rules between them.
//!
//! Everything here follows Excel's documented behaviour, so a formula gives
//! the same answer in a grid as in Excel: `"3"+1` is 4, `TRUE+1` is 2, an
//! empty cell is 0 in arithmetic and `""` in text, and text compares without
//! regard to case.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

/// An Excel error value. `Circ` (a circular reference) and `Calc` (a Scheme
/// function still running, failed, or timed out) are this engine's own: Excel
/// has no error code for either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Null,
    Div0,
    Value,
    Ref,
    Name,
    Num,
    NA,
    /// A formula that failed to parse (`=SUM(`).
    Parse,
    Circ,
    Calc,
    /// A dynamic-array result that can't spill: its range is blocked by
    /// non-empty cells or runs off the sheet.
    Spill,
}

impl ErrorKind {
    /// The text Excel shows (and the formula grammar accepts as a literal).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Null => "#NULL!",
            ErrorKind::Div0 => "#DIV/0!",
            ErrorKind::Value => "#VALUE!",
            ErrorKind::Ref => "#REF!",
            ErrorKind::Name => "#NAME?",
            ErrorKind::Num => "#NUM!",
            ErrorKind::NA => "#N/A",
            ErrorKind::Parse => "#ERROR!",
            ErrorKind::Circ => "#CIRC!",
            ErrorKind::Calc => "#CALC!",
            ErrorKind::Spill => "#SPILL!",
        }
    }

    /// Every error kind.
    pub const ALL: [ErrorKind; 11] = [
        ErrorKind::Null,
        ErrorKind::Div0,
        ErrorKind::Value,
        ErrorKind::Ref,
        ErrorKind::Name,
        ErrorKind::Num,
        ErrorKind::NA,
        ErrorKind::Parse,
        ErrorKind::Circ,
        ErrorKind::Calc,
        ErrorKind::Spill,
    ];

    /// Parse an error literal (case-insensitive, as Excel accepts `#n/a`).
    pub fn parse(s: &str) -> Option<ErrorKind> {
        s.parse().ok()
    }

    /// Excel's error number (`ERROR.TYPE`). This engine's own errors get
    /// numbers past Excel's: `#ERROR!` (parse) 101, `#CIRC!` 102; `#SPILL!`
    /// and `#CALC!` are Excel's 9 and 14.
    pub fn code(self) -> u8 {
        match self {
            ErrorKind::Null => 1,
            ErrorKind::Div0 => 2,
            ErrorKind::Value => 3,
            ErrorKind::Ref => 4,
            ErrorKind::Name => 5,
            ErrorKind::Num => 6,
            ErrorKind::NA => 7,
            ErrorKind::Spill => 9,
            ErrorKind::Calc => 14,
            ErrorKind::Parse => 101,
            ErrorKind::Circ => 102,
        }
    }
}

/// Text that isn't an error literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownError;

impl fmt::Display for UnknownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not an error value")
    }
}

impl std::error::Error for UnknownError {}

impl std::str::FromStr for ErrorKind {
    type Err = UnknownError;

    /// An error literal, case-insensitively (`#n/a` is `#N/A`).
    fn from_str(s: &str) -> Result<ErrorKind, UnknownError> {
        ErrorKind::ALL
            .into_iter()
            .find(|e| e.as_str().eq_ignore_ascii_case(s))
            .ok_or(UnknownError)
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One formula value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Text(Arc<str>),
    Bool(bool),
    Error(ErrorKind),
    /// An empty cell (or an omitted argument).
    Empty,
}

impl From<ErrorKind> for Value {
    fn from(e: ErrorKind) -> Value {
        Value::Error(e)
    }
}

impl Value {
    pub fn text(s: &str) -> Value {
        Value::Text(Arc::from(s))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Value::Error(_))
    }

    /// The value of a cell holding `text`, read as Excel reads typed input:
    /// a number when it parses as one (`007` is 7, `1e3` is 1000, `50%` is
    /// 0.5), `TRUE`/`FALSE` as booleans (any case), an error literal as that
    /// error, empty as [`Value::Empty`], anything else as text.
    pub fn from_cell_text(text: &str) -> Value {
        if text.is_empty() {
            return Value::Empty;
        }
        if let Some(n) = parse_number(text) {
            return Value::Number(n);
        }
        if text.eq_ignore_ascii_case("TRUE") {
            return Value::Bool(true);
        }
        if text.eq_ignore_ascii_case("FALSE") {
            return Value::Bool(false);
        }
        if text.starts_with('#')
            && let Some(e) = ErrorKind::parse(text)
        {
            return Value::Error(e);
        }
        Value::text(text)
    }

    /// Coerce to a number for arithmetic: numbers as-is, booleans as 1/0,
    /// empty as 0, text that parses as a number as that number, other text
    /// as `#VALUE!`. Errors pass through.
    pub fn to_number(&self) -> Result<f64, ErrorKind> {
        self.to_number_in(false)
    }

    /// [`Self::to_number`] in a workbook on the 1904 date system (`date1904`),
    /// which matters for date text (`"2024-01-01"+0`).
    pub fn to_number_in(&self, date1904: bool) -> Result<f64, ErrorKind> {
        match self {
            Value::Number(n) => Ok(*n),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Empty => Ok(0.0),
            // As Excel does: `"1,000"+1` is 1001, `"$5"*2` is 10.
            Value::Text(s) => parse_number(s.trim())
                .or_else(|| parse_typed_number(s, None, date1904))
                .ok_or(ErrorKind::Value),
            Value::Error(e) => Err(*e),
        }
    }

    /// Coerce to a boolean (`IF`, `AND`, …): numbers are true when non-zero,
    /// empty is false, `"TRUE"`/`"FALSE"` text is that boolean, other text is
    /// `#VALUE!`.
    pub fn to_bool(&self) -> Result<bool, ErrorKind> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Number(n) => Ok(*n != 0.0),
            Value::Empty => Ok(false),
            Value::Text(s) if s.eq_ignore_ascii_case("TRUE") => Ok(true),
            Value::Text(s) if s.eq_ignore_ascii_case("FALSE") => Ok(false),
            Value::Text(_) => Err(ErrorKind::Value),
            Value::Error(e) => Err(*e),
        }
    }

    /// Coerce to text (`&`, text functions): numbers in General format,
    /// booleans as `TRUE`/`FALSE`, empty as `""`. Errors pass through.
    pub fn to_text(&self) -> Result<Arc<str>, ErrorKind> {
        match self {
            Value::Text(s) => Ok(s.clone()),
            Value::Number(n) => Ok(Arc::from(general_format(*n))),
            Value::Bool(b) => Ok(Arc::from(if *b { "TRUE" } else { "FALSE" })),
            Value::Empty => Ok(Arc::from("")),
            Value::Error(e) => Err(*e),
        }
    }
}

/// How the value is shown in a cell.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Number(n) => f.write_str(&general_format(*n)),
            Value::Text(s) => f.write_str(s),
            Value::Bool(b) => f.write_str(if *b { "TRUE" } else { "FALSE" }),
            Value::Error(e) => f.write_str(e.as_str()),
            Value::Empty => Ok(()),
        }
    }
}

/// Excel's comparison: numbers < text < booleans; text case-insensitively;
/// empty equals both 0 and `""` (and `FALSE`). Errors are handled by the
/// caller (they propagate before comparing).
pub fn compare(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) | Value::Empty => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            Value::Error(_) => 3,
        }
    }
    match (a, b) {
        (Value::Empty, Value::Text(s)) => "".cmp(&*s.to_lowercase()),
        (Value::Text(s), Value::Empty) => s.to_lowercase().as_str().cmp(""),
        (Value::Empty, Value::Bool(x)) => false.cmp(x),
        (Value::Bool(x), Value::Empty) => x.cmp(&false),
        _ if rank(a) != rank(b) => rank(a).cmp(&rank(b)),
        (Value::Text(x), Value::Text(y)) => cmp_text(x, y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => {
            let x = a.to_number().unwrap_or(0.0);
            let y = b.to_number().unwrap_or(0.0);
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
    }
}

/// Case-insensitive text order (Unicode lower-casing, then the raw text as a
/// tie-break so the order is total).
pub fn cmp_text(a: &str, b: &str) -> Ordering {
    let la = a.to_lowercase();
    let lb = b.to_lowercase();
    la.cmp(&lb)
}

/// Text as the number typing it into a cell would make, as Excel coerces
/// text in arithmetic and numeric arguments: plain numbers, thousands
/// separators (`1,000`), currency (`$5.25`), accounting negatives
/// (`(12)`), percentages, and dates and times (`2024-01-01`, `12:30`). A
/// date without a year takes `current_year`; with `None` it isn't a date.
pub fn parse_typed_number(s: &str, current_year: Option<i64>, date1904: bool) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(n) = parse_number(t) {
        return Some(n);
    }
    let (neg_paren, body) = match t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        Some(inner) => (true, inner.trim()),
        None => (false, t),
    };
    let mut b = body;
    let mut neg = false;
    if let Some(r) = b.strip_prefix('-') {
        neg = true;
        b = r;
    } else if let Some(r) = b.strip_prefix('+') {
        b = r;
    }
    if let Some(r) = b.strip_prefix('$') {
        b = r.trim_start();
        if let Some(r) = b.strip_prefix('-') {
            neg = !neg;
            b = r;
        }
    }
    // A parenthesized negative carries no sign of its own: `(-5)` isn't one.
    if neg_paren && body.starts_with(['-', '+']) {
        return None;
    }
    // Thousands separators: only in the integer part, in groups of three
    // after the first (`1,234`, not `1,2,3` or `5,`).
    let int_end = b.find(['.', 'e', 'E', '%']).unwrap_or(b.len());
    let (int, rest) = b.split_at(int_end);
    let grouped = {
        let mut groups = int.split(',');
        let first = groups.next().unwrap_or("");
        (1..=3).contains(&first.len()) && groups.all(|g| g.len() == 3)
    };
    if (grouped || !int.contains(','))
        && !rest.contains(',')
        && (int.contains(',') || b != body || neg_paren)
    {
        let cleaned = format!("{}{}", int.replace(',', ""), rest);
        if let Some(n) = parse_number(&cleaned)
            && !cleaned.starts_with(['-', '+'])
        {
            let n = if neg { -n } else { n };
            return Some(if neg_paren { -n } else { n });
        }
    }
    if neg_paren || neg {
        return None;
    }
    super::date::parse_datetime(t, current_year, date1904).map(|d| d.serial())
}

/// Parse a number the way Excel reads typed input: optional sign, digits with
/// an optional fraction, optional exponent, and an optional trailing `%`
/// (dividing by 100). No thousands separators, no surrounding spaces, and no
/// `inf`/`nan` (which `f64::from_str` would otherwise accept).
pub fn parse_number(s: &str) -> Option<f64> {
    let (body, percent) = match s.strip_suffix('%') {
        Some(b) => (b, true),
        None => (s, false),
    };
    let bytes = body.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - digits_start;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - frac_start;
    }
    if digits == 0 {
        return None;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return None;
        }
    }
    if i != bytes.len() {
        return None;
    }
    let n: f64 = body.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    Some(if percent { n / 100.0 } else { n })
}

/// Excel's "General" number format as text (what `=x&""` produces): up to
/// 15 significant digits, so `0.1+0.2` is `0.3`; trailing zeros trimmed;
/// fixed notation for 1e-9 ≤ |x| < 1e15 and scientific outside it
/// (`1.23456789012346E+17`). Cells narrower than the text shorten it at paint
/// time; this is the full form.
pub fn general_format(n: f64) -> String {
    if n.is_nan() {
        return "#NUM!".into();
    }
    if n == 0.0 || !n.is_finite() {
        return "0".into();
    }
    // Round to 15 significant digits, as Excel does.
    let rounded: f64 = format!("{:.14e}", n).parse().unwrap_or(n);
    let abs = rounded.abs();
    if (1e-9..1e15).contains(&abs) {
        let magnitude = abs.log10().floor() as i32; // 2 for 123.4, -3 for 0.0012
        let decimals = (14 - magnitude).max(0) as usize;
        let s = format!("{:.*}", decimals, rounded);
        let s = if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        };
        if s == "-0" { "0".into() } else { s }
    } else {
        let s = format!("{:.14e}", rounded);
        let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mant = if mant.contains('.') {
            mant.trim_end_matches('0').trim_end_matches('.')
        } else {
            mant
        };
        let exp: i32 = exp.parse().unwrap_or(0);
        format!("{mant}E{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_text_reads_like_typed_input() {
        assert_eq!(Value::from_cell_text("007"), Value::Number(7.0));
        assert_eq!(Value::from_cell_text("1e3"), Value::Number(1000.0));
        assert_eq!(Value::from_cell_text("50%"), Value::Number(0.5));
        assert_eq!(Value::from_cell_text("true"), Value::Bool(true));
        assert_eq!(Value::from_cell_text("#n/a"), Value::Error(ErrorKind::NA));
        assert_eq!(Value::from_cell_text(""), Value::Empty);
        assert_eq!(Value::from_cell_text("inf"), Value::text("inf"));
        assert_eq!(Value::from_cell_text(" 3"), Value::text(" 3"));
        assert_eq!(Value::from_cell_text("1,234"), Value::text("1,234"));
    }

    #[test]
    fn coercions() {
        assert_eq!(Value::text(" 3 ").to_number(), Ok(3.0));
        assert_eq!(Value::text("x").to_number(), Err(ErrorKind::Value));
        assert_eq!(Value::Bool(true).to_number(), Ok(1.0));
        assert_eq!(Value::Empty.to_number(), Ok(0.0));
        assert_eq!(Value::Number(0.0).to_bool(), Ok(false));
        assert_eq!(&*Value::Number(2.5).to_text().unwrap(), "2.5");
        assert_eq!(&*Value::Bool(false).to_text().unwrap(), "FALSE");
    }

    #[test]
    fn comparison_order() {
        use Ordering::*;
        assert_eq!(compare(&Value::Number(5.0), &Value::text("a")), Less);
        assert_eq!(compare(&Value::text("z"), &Value::Bool(false)), Less);
        assert_eq!(compare(&Value::text("ABC"), &Value::text("abc")), Equal);
        assert_eq!(compare(&Value::Empty, &Value::Number(0.0)), Equal);
        assert_eq!(compare(&Value::Empty, &Value::text("")), Equal);
        assert_eq!(compare(&Value::Bool(true), &Value::Number(3.0)), Greater);
    }

    #[test]
    fn general_format_matches_excel() {
        assert_eq!(general_format(0.1 + 0.2), "0.3");
        assert_eq!(general_format(0.1 + 0.2 - 0.3), "5.55111512312578E-17");
        assert_eq!(general_format(1234567.0), "1234567");
        assert_eq!(general_format(-2.5), "-2.5");
        assert_eq!(general_format(1.0 / 3.0), "0.333333333333333");
        assert_eq!(general_format(2.0 / 3.0), "0.666666666666667");
        assert_eq!(general_format(123456789012.0), "123456789012");
        assert_eq!(general_format(123456789012345678.0), "1.23456789012346E+17");
        assert_eq!(general_format(0.000000001), "0.000000001");
        assert_eq!(general_format(1e-10), "1E-10");
        assert_eq!(general_format(0.0012), "0.0012");
        assert_eq!(general_format(100.0), "100");
        assert_eq!(general_format(-0.0), "0");
    }
}
