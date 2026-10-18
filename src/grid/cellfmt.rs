//! Formatting set by hand on single cells: bold, a text color and a fill
//! color from a small fixed palette (Excel's font-color and fill buttons),
//! a number format (an Excel format code: `$#,##0.00`, `yyyy-mm-dd`, …), and
//! Wrap Text (the cell's text wraps within its column, its row grows to fit).
//!
//! Unlike [`super::style`], which is declarative styling written in Scheme,
//! this is what the user applies while moving around the grid, so it follows
//! the cells it was set on: sorting, moving, inserting and deleting rows and
//! columns carry it along, and every change is an undo step. A number format
//! only changes how a cell is shown; the stored value is untouched. CSV can't
//! hold any of it, so a formatted CSV grid saves as `.xlsx`, which does.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock, RwLock};

/// The palette, in the order the color commands cycle through it. Each name
/// is also a theme face (`grid-red`, …) that overrides the default color.
pub const COLORS: &[&str] = &["red", "orange", "yellow", "green", "blue", "purple"];

/// The color each palette entry saves as in an `.xlsx` (ARGB): Excel's
/// "Standard Colors" row, so a file colored from Excel's palette reads back
/// as these.
pub const ARGB: &[u32] = &[
    0xFFFF0000, 0xFFFFC000, 0xFFFFFF00, 0xFF00B050, 0xFF0070C0, 0xFF7030A0,
];

/// A run of ASCII digits with a `,` between each group of three from the
/// right: `1234567` → `1,234,567`. No sign or decimals: callers add those
/// (the grid's summaries, number formats, and the formula `FIXED`
/// function all share this).
pub fn group_digits(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The palette index of an `.xlsx` ARGB color, if it is one of [`ARGB`].
pub fn argb_index(argb: u32) -> Option<u8> {
    ARGB.iter().position(|&a| a == argb).map(|i| i as u8)
}

/// One cell's hand-set formatting. A color is an index into [`COLORS`]; a
/// number format is an interned format code (see [`code`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellFmt {
    pub bold: bool,
    pub text: Option<u8>,
    pub fill: Option<u8>,
    pub num: Option<NumCode>,
    /// Wrap Text: word-wrap within the column width, showing every line.
    pub wrap: bool,
}

/// An interned number-format code. Interning is process-wide, so a code's
/// id stays valid in every grid's undo history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NumCode(u32);

/// Every interned code, by id, and the id of each.
type Codes = (Vec<Arc<str>>, HashMap<Arc<str>, u32>);

fn codes() -> &'static RwLock<Codes> {
    static CODES: OnceLock<RwLock<Codes>> = OnceLock::new();
    CODES.get_or_init(Default::default)
}

impl NumCode {
    /// The id for a format code; `General` (any case) and blank mean no
    /// format, so they give `None`.
    pub fn intern(code: &str) -> Option<Self> {
        let code = code.trim();
        if code.is_empty() || code.eq_ignore_ascii_case("general") {
            return None;
        }
        if let Some(&id) = codes().read().unwrap().1.get(code) {
            return Some(Self(id));
        }
        let mut t = codes().write().unwrap();
        if let Some(&id) = t.1.get(code) {
            return Some(Self(id));
        }
        let id = t.0.len() as u32;
        let code: Arc<str> = code.into();
        t.0.push(code.clone());
        t.1.insert(code, id);
        Some(Self(id))
    }

    pub fn code(self) -> Arc<str> {
        codes().read().unwrap().0[self.0 as usize].clone()
    }

    /// The `@` (text) format: shown exactly as stored, never as a number.
    pub fn is_text(self) -> bool {
        &*self.code() == "@"
    }
}

/// The named number formats `grid-set-number-format` offers, and their codes
/// (`general` is no format at all).
pub const PRESETS: &[(&str, &str)] = &[
    ("general", "General"),
    ("number", "#,##0.00"),
    ("currency", "$#,##0.00"),
    ("percent", "0.00%"),
    ("date", "yyyy-mm-dd"),
    ("text", "@"),
];

/// A preset's name → its code; anything else is taken as a format code.
pub fn preset_code(name: &str) -> &str {
    PRESETS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
        .map_or(name, |(_, c)| c)
}

/// A preset's name for `code`, for messages (`"currency"`), else the code.
pub fn describe(code: Option<NumCode>) -> String {
    let code = code.map_or_else(|| Arc::from("General"), NumCode::code);
    PRESETS
        .iter()
        .find(|(_, c)| **c == *code)
        .map_or_else(|| code.to_string(), |(n, _)| n.to_string())
}

/// `code` with one decimal place more (`up`) or fewer, in every numeric
/// section: `#,##0` → `#,##0.0` → `#,##0.00`, and back. `None` when the
/// code has no number in it to change (a date, `@`).
pub fn step_decimals(code: &str, up: bool) -> Option<String> {
    let mut out = String::new();
    let mut changed = false;
    for (i, sec) in split_sections(code).into_iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        match step_section(sec, up) {
            Some(s) => {
                out.push_str(&s);
                changed = true;
            }
            None => out.push_str(sec),
        }
    }
    changed.then_some(out)
}

/// A format code's sections (`pos;neg;zero;text`), splitting on `;` outside
/// quotes and brackets.
fn split_sections(code: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quote, mut bracket, mut escape) = (0, false, false, false);
    for (i, ch) in code.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if !quote => escape = true,
            '"' if !bracket => quote = !quote,
            '[' if !quote => bracket = true,
            ']' if !quote => bracket = false,
            ';' if !quote && !bracket => {
                out.push(&code[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&code[start..]);
    out
}

/// One section with a decimal place more or fewer; `None` when it holds no
/// digit placeholders (or is a date/time section, whose `0`s are seconds).
fn step_section(sec: &str, up: bool) -> Option<String> {
    // Byte positions of digit placeholders and the point, outside literals.
    let (mut digits, mut point) = (Vec::new(), None);
    let (mut quote, mut bracket, mut escape) = (false, false, false);
    for (i, ch) in sec.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' | '_' | '*' if !quote && !bracket => escape = true,
            '"' if !bracket => quote = !quote,
            '[' if !quote => bracket = true,
            ']' if !quote => bracket = false,
            _ if quote || bracket => {}
            // Scientific (`0.00E+00`): the exponent's digits aren't decimals.
            'E' | 'e' => break,
            '0' | '#' | '?' => digits.push(i),
            '.' if point.is_none() => point = Some(i),
            c if "yYmMdDhHsS".contains(c) => return None,
            _ => {}
        }
    }
    let last = *digits.last()?;
    let mut s = sec.to_string();
    match point {
        Some(p) => {
            let frac: Vec<usize> = digits.iter().copied().filter(|&i| i > p).collect();
            if up {
                s.insert(frac.last().map_or(p, |&i| i) + 1, '0');
            } else if let Some(&i) = frac.last() {
                s.remove(i);
                if frac.len() == 1 {
                    s.remove(p);
                }
            } else {
                s.remove(p);
            }
        }
        None if up => s.insert_str(last + 1, ".0"),
        None => return None,
    }
    Some(s)
}

impl CellFmt {
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

/// The palette index of a color name (`"red"`), if it is one.
pub fn color_index(name: &str) -> Option<u8> {
    COLORS
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name.trim()))
        .map(|i| i as u8)
}

/// The color after `current` in the cycle: none → red → … → purple → none.
pub fn next_color(current: Option<u8>) -> Option<u8> {
    match current {
        None => Some(0),
        Some(i) if (i as usize) + 1 < COLORS.len() => Some(i + 1),
        Some(_) => None,
    }
}

/// The color name for a palette index.
pub fn color_name(i: u8) -> &'static str {
    COLORS.get(i as usize).copied().unwrap_or("none")
}

/// Every formatted cell of a grid, by `(row, col)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellFormats {
    cells: HashMap<(usize, usize), CellFmt>,
    /// Formatted cells per column.
    per_col: HashMap<usize, usize>,
    /// The wrapped cells, row-major, so a row's are found without a scan.
    wrapped: BTreeSet<(usize, usize)>,
}

impl CellFormats {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn get(&self, row: usize, col: usize) -> CellFmt {
        self.cells.get(&(row, col)).copied().unwrap_or_default()
    }

    /// Set a cell's formatting; plain formatting removes the entry. Returns
    /// what it was.
    pub fn set(&mut self, row: usize, col: usize, fmt: CellFmt) -> CellFmt {
        let old = if fmt.is_plain() {
            self.cells.remove(&(row, col))
        } else {
            self.cells.insert((row, col), fmt)
        };
        match (old.is_some(), fmt.is_plain()) {
            (false, false) => *self.per_col.entry(col).or_default() += 1,
            (true, true) => {
                if let Some(n) = self.per_col.get_mut(&col) {
                    *n -= 1;
                    if *n == 0 {
                        self.per_col.remove(&col);
                    }
                }
            }
            _ => {}
        }
        if fmt.wrap {
            self.wrapped.insert((row, col));
        } else {
            self.wrapped.remove(&(row, col));
        }
        old.unwrap_or_default()
    }

    /// Whether any cell wraps.
    pub fn any_wrapped(&self) -> bool {
        !self.wrapped.is_empty()
    }

    /// The columns of row `row`'s wrapped cells, in order.
    pub fn wrapped_in_row(&self, row: usize) -> impl Iterator<Item = usize> + '_ {
        self.wrapped.range((row, 0)..(row + 1, 0)).map(|&(_, c)| c)
    }

    /// Every formatted cell.
    pub fn positions(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.cells.keys().copied()
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.per_col.clear();
        self.wrapped.clear();
    }

    /// Whether any cell in column `col` is formatted (lets the painter skip
    /// the lookup for plain columns).
    pub fn touches_col(&self, col: usize) -> bool {
        self.per_col.contains_key(&col)
    }

    /// Move every cell's formatting where a structural edit moved the cell:
    /// `row`/`col` give a row's or column's new index, or `None` when it was
    /// deleted (its formatting goes with it).
    pub fn remap(
        &mut self,
        row: impl Fn(usize) -> Option<usize>,
        col: impl Fn(usize) -> Option<usize>,
    ) {
        let old = std::mem::take(&mut self.cells);
        self.per_col.clear();
        self.wrapped.clear();
        for ((r, c), f) in old {
            if let (Some(r), Some(c)) = (row(r), col(c)) {
                self.set(r, c, f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_grouping() {
        assert_eq!(group_digits(""), "");
        assert_eq!(group_digits("0"), "0");
        assert_eq!(group_digits("123"), "123");
        assert_eq!(group_digits("1234"), "1,234");
        assert_eq!(group_digits("123456"), "123,456");
        assert_eq!(group_digits("1234567"), "1,234,567");
    }

    #[test]
    fn colors_cycle_through_the_palette_and_back_to_none() {
        let mut c = None;
        let mut seen = Vec::new();
        for _ in 0..=COLORS.len() {
            c = next_color(c);
            seen.push(c.map(color_name));
        }
        assert_eq!(seen.first(), Some(&Some("red")));
        assert_eq!(seen[COLORS.len() - 1], Some("purple"));
        assert_eq!(seen.last(), Some(&None));
        assert_eq!(color_index("Yellow"), Some(2));
        assert_eq!(color_index("teal"), None);
    }

    #[test]
    fn plain_formatting_is_not_stored_and_remap_follows_cells() {
        let mut f = CellFormats::default();
        let bold = CellFmt {
            bold: true,
            ..Default::default()
        };
        assert!(f.set(1, 1, bold).is_plain());
        f.set(2, 0, bold);
        assert_eq!(f.set(2, 0, CellFmt::default()), bold);
        assert_eq!(f.get(2, 0), CellFmt::default());
        assert!(f.touches_col(1) && !f.touches_col(0));
        // Row 0 deleted: row 1 moves up.
        f.remap(|r| r.checked_sub(1), Some);
        assert_eq!(f.get(0, 1), bold);
        f.remap(|_| None, Some);
        assert!(f.is_empty());
    }

    #[test]
    fn number_codes_intern_presets_and_decimal_steps() {
        assert_eq!(NumCode::intern("General"), None);
        assert_eq!(NumCode::intern(" "), None);
        let a = NumCode::intern("$#,##0.00").unwrap();
        assert_eq!(NumCode::intern("$#,##0.00"), Some(a));
        assert_eq!(&*a.code(), "$#,##0.00");
        assert_eq!(describe(Some(a)), "currency");
        assert_eq!(describe(None), "general");
        assert!(NumCode::intern("@").unwrap().is_text());
        assert_eq!(preset_code("Percent"), "0.00%");
        assert_eq!(preset_code("0.0"), "0.0");

        let up = |c: &str| step_decimals(c, true);
        let down = |c: &str| step_decimals(c, false);
        assert_eq!(up("#,##0").as_deref(), Some("#,##0.0"));
        assert_eq!(up("$#,##0.00").as_deref(), Some("$#,##0.000"));
        assert_eq!(up("0%").as_deref(), Some("0.0%"));
        assert_eq!(down("0.0%").as_deref(), Some("0%"));
        assert_eq!(down("#,##0.00").as_deref(), Some("#,##0.0"));
        assert_eq!(down("0"), None);
        assert_eq!(
            up("#,##0.00;(#,##0.00)").as_deref(),
            Some("#,##0.000;(#,##0.000)")
        );
        assert_eq!(up("0.00\" kg\"").as_deref(), Some("0.000\" kg\""));
        assert_eq!(up("0.00E+00").as_deref(), Some("0.000E+00"));
        assert_eq!(down("0.00E+00").as_deref(), Some("0.0E+00"));
        assert_eq!(up("yyyy-mm-dd"), None);
        assert_eq!(up("@"), None);
    }
}
