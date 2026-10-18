//! Declarative cell styling for a grid, set from Scheme and evaluated in
//! Rust at paint time: per-column faces, per-column number formats, and
//! conditional highlight rules.
//!
//! Declarative because the renderer can't call Scheme per frame — the engine
//! lives on its own thread — so Scheme registers *rules* once and the painter
//! applies them to whatever cells are on screen. Everything here changes only
//! how a cell is displayed; the stored value (and the saved file) is untouched.

use std::collections::{HashMap, HashSet};

use regex::Regex;

/// A number display format: `#,##0.00`, `0.0%`, `$#,##0`, `0.000 kg`.
///
/// The leading text before the first `#`/`0` is a literal prefix, the
/// trailing text after the number pattern a literal suffix. A `,` in the
/// pattern groups thousands; the `0`s after a `.` fix the decimal places; a
/// `%` in the suffix multiplies by 100 (as in Excel).
#[derive(Debug, Clone, PartialEq)]
pub struct NumFormat {
    prefix: String,
    suffix: String,
    decimals: usize,
    thousands: bool,
    percent: bool,
}

impl NumFormat {
    pub fn parse(pattern: &str) -> Option<Self> {
        let start = pattern.find(['#', '0'])?;
        let body_len = pattern[start..]
            .find(|c: char| !matches!(c, '#' | '0' | ',' | '.'))
            .unwrap_or(pattern.len() - start);
        let body = &pattern[start..start + body_len];
        let suffix = &pattern[start + body_len..];
        let decimals = body.split_once('.').map_or(0, |(_, frac)| {
            frac.chars().filter(|&c| c == '0' || c == '#').count()
        });
        Some(Self {
            prefix: pattern[..start].to_string(),
            suffix: suffix.to_string(),
            decimals,
            thousands: body.contains(','),
            percent: suffix.contains('%'),
        })
    }

    /// `value` formatted, or `None` when it isn't a number (text is shown
    /// as-is, the way Excel leaves text in a numeric column alone).
    pub fn apply(&self, value: &str) -> Option<String> {
        let mut n: f64 = value.trim().parse().ok().filter(|n: &f64| n.is_finite())?;
        if self.percent {
            n *= 100.0;
        }
        // Round half away from zero on the 15 digits Excel shows, as Excel
        // does: 1.005 → "1.01" (its double is just under 1.005), 1234.5 →
        // "1235", and no overflow for huge values.
        use crate::formula::decimal::{Dec, Mode};
        let (int, frac) = Dec::from_f64(n)
            .round(self.decimals as i32, Mode::Half)
            .fixed_parts(self.decimals);
        let int = if int.is_empty() { "0".to_string() } else { int };
        let sign = if n < 0.0 && (int != "0" || frac.bytes().any(|b| b != b'0')) {
            "-"
        } else {
            ""
        };
        let int = if self.thousands {
            super::cellfmt::group_digits(&int)
        } else {
            int
        };
        let num = if frac.is_empty() {
            int
        } else {
            format!("{int}.{frac}")
        };
        Some(format!("{sign}{}{num}{}", self.prefix, self.suffix))
    }
}

/// What a highlight rule tests a cell's value against.
#[derive(Debug, Clone)]
pub enum RuleTest {
    /// Numeric comparisons; a non-numeric cell never matches.
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    /// Inclusive numeric range (`"between" "10..20"`).
    Between(f64, f64),
    /// Exact text equality / inequality (numbers compare numerically when
    /// both sides parse, so `"7"` equals `"7.0"`).
    Eq(String),
    Ne(String),
    Contains(String),
    Matches(Regex),
    Empty,
    NotEmpty,
    /// A value that occurs more than once in its column, compared as
    /// Remove duplicates compares ([`super::dedup::key`]: case ignored,
    /// numbers by value).
    Duplicate,
    /// One of the column's `n` largest / smallest numbers.
    Top(usize),
    Bottom(usize),
}

impl RuleTest {
    /// Build from Scheme's `(op value)`: `">" "1000"`, `"contains" "error"`,
    /// `"matches" "^ERR"`, `"empty" ""`, `"between" "10..20"`, `"top" "5"`.
    pub fn parse(op: &str, value: &str) -> Result<Self, String> {
        let num = || {
            value
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("{op} needs a number, got {value:?}"))
        };
        let count = || {
            value
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| format!("{op} needs a positive count, got {value:?}"))
        };
        Ok(match op.trim() {
            ">" => Self::Gt(num()?),
            ">=" => Self::Ge(num()?),
            "<" => Self::Lt(num()?),
            "<=" => Self::Le(num()?),
            "between" => {
                let nums: Vec<f64> = value
                    .split([',', ' '])
                    .flat_map(|p| p.split(".."))
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| p.trim().parse::<f64>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("between needs \"LOW..HIGH\", got {value:?}"))?;
                match nums[..] {
                    [a, b] => Self::Between(a.min(b), a.max(b)),
                    _ => return Err(format!("between needs \"LOW..HIGH\", got {value:?}")),
                }
            }
            "=" | "==" => Self::Eq(value.to_string()),
            "!=" | "/=" | "<>" => Self::Ne(value.to_string()),
            "contains" => Self::Contains(value.to_string()),
            "matches" => Self::Matches(Regex::new(value).map_err(|e| e.to_string())?),
            "empty" => Self::Empty,
            "not-empty" => Self::NotEmpty,
            "duplicate" => Self::Duplicate,
            "top" => Self::Top(count()?),
            "bottom" => Self::Bottom(count()?),
            other => return Err(format!("unknown rule operator {other:?}")),
        })
    }

    /// Whether this test needs the whole column (computed by
    /// [`GridStyle::refresh`]) rather than just the cell.
    pub fn columnar(&self) -> bool {
        matches!(self, Self::Duplicate | Self::Top(_) | Self::Bottom(_))
    }

    /// Test `value`; `derived` is what [`GridStyle::refresh`] computed for a
    /// columnar test.
    fn test_with(&self, value: &str, derived: &Derived) -> bool {
        let num = || super::infer::cell_number(value);
        let eq = |want: &str| match (num(), super::infer::cell_number(want)) {
            (Some(a), Some(b)) => a == b,
            _ => value == want,
        };
        match self {
            Self::Gt(n) => num().is_some_and(|v| v > *n),
            Self::Ge(n) => num().is_some_and(|v| v >= *n),
            Self::Lt(n) => num().is_some_and(|v| v < *n),
            Self::Le(n) => num().is_some_and(|v| v <= *n),
            Self::Between(lo, hi) => num().is_some_and(|v| v >= *lo && v <= *hi),
            Self::Eq(s) => eq(s),
            Self::Ne(s) => !eq(s),
            Self::Contains(s) => value.contains(s.as_str()),
            Self::Matches(re) => re.is_match(value),
            Self::Empty => value.is_empty(),
            Self::NotEmpty => !value.is_empty(),
            Self::Duplicate => {
                matches!(derived, Derived::Set(set) if set.contains(super::dedup::key(value).as_ref()))
            }
            Self::Top(_) => {
                matches!(derived, Derived::Threshold(t) if num().is_some_and(|v| v >= *t))
            }
            Self::Bottom(_) => {
                matches!(derived, Derived::Threshold(t) if num().is_some_and(|v| v <= *t))
            }
        }
    }

    /// Test `value` on its own (a columnar test never passes this way).
    pub fn test_value(&self, value: &str) -> bool {
        self.test_with(value, &Derived::None)
    }

    #[cfg(test)]
    pub fn test(&self, value: &str) -> bool {
        self.test_value(value)
    }
}

/// Column-wide data a columnar rule tests against, recomputed when the
/// grid's data changes.
#[derive(Debug, Clone, Default)]
pub enum Derived {
    #[default]
    None,
    /// The values occurring more than once.
    Set(HashSet<String>),
    /// The cut-off value for top / bottom n.
    Threshold(f64),
}

/// A conditional highlight: cells in `col` (every column when `None`) whose
/// value passes `test` are drawn with `face`. A `row` rule tests `col`'s
/// value and, when it passes, colors that whole row.
#[derive(Debug, Clone)]
pub struct Rule {
    pub col: Option<usize>,
    pub test: RuleTest,
    pub face: String,
    pub row: bool,
    pub derived: Derived,
}

impl Rule {
    pub fn new(col: Option<usize>, test: RuleTest, face: String) -> Self {
        Self {
            col,
            test,
            face,
            row: false,
            derived: Derived::None,
        }
    }

    fn matches(&self, value: &str) -> bool {
        self.test.test_with(value, &self.derived)
    }
}

/// Horizontal alignment of a column's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

impl Align {
    pub fn parse(s: &str) -> Result<Option<Self>, String> {
        Ok(match s {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "center" | "centre" => Some(Self::Center),
            "auto" | "" => None,
            other => {
                return Err(format!(
                    "alignment is left, right, center or auto, not {other:?}"
                ));
            }
        })
    }
}

/// All of one grid's styling.
#[derive(Debug, Clone, Default)]
pub struct GridStyle {
    col_faces: HashMap<usize, String>,
    formats: HashMap<usize, NumFormat>,
    rules: Vec<Rule>,
    /// Faces set on single cells, by column then row (like overlays, but
    /// keyed by position: they don't move when rows are inserted).
    cell_faces: HashMap<usize, HashMap<usize, String>>,
    align: HashMap<usize, Align>,
    /// Face whose background tints every other (shown) row.
    pub stripe: Option<String>,
    /// Face for the header row's text and background.
    pub header_face: Option<String>,
    /// Data generation the columnar rules were last computed for.
    derived_gen: Option<u64>,
}

impl GridStyle {
    pub fn set_column_face(&mut self, col: usize, face: &str) {
        if face.is_empty() {
            self.col_faces.remove(&col);
        } else {
            self.col_faces.insert(col, face.to_string());
        }
    }

    /// Set (or, with an empty pattern, clear) a column's number format.
    pub fn set_format(&mut self, col: usize, pattern: &str) -> Result<(), String> {
        if pattern.is_empty() {
            self.formats.remove(&col);
            return Ok(());
        }
        let fmt = NumFormat::parse(pattern)
            .ok_or_else(|| format!("not a number format: {pattern:?} (try \"#,##0.00\")"))?;
        self.formats.insert(col, fmt);
        Ok(())
    }

    /// Add a rule. A columnar test (duplicate/top/bottom) needs one column.
    pub fn add_rule(&mut self, rule: Rule) -> Result<(), String> {
        if rule.col.is_none() && (rule.row || rule.test.columnar()) {
            return Err("this rule needs a column".into());
        }
        if rule.test.columnar() {
            self.derived_gen = None; // compute it on the next refresh
        }
        self.rules.push(rule);
        Ok(())
    }

    /// Set (an empty face clears) one cell's face.
    pub fn set_cell_face(&mut self, row: usize, col: usize, face: &str) {
        if face.is_empty() {
            if let Some(rows) = self.cell_faces.get_mut(&col) {
                rows.remove(&row);
                if rows.is_empty() {
                    self.cell_faces.remove(&col);
                }
            }
        } else {
            self.cell_faces
                .entry(col)
                .or_default()
                .insert(row, face.to_string());
        }
    }

    pub fn clear_cell_faces(&mut self) {
        self.cell_faces.clear();
    }

    pub fn set_align(&mut self, col: usize, align: Option<Align>) {
        match align {
            Some(a) => self.align.insert(col, a),
            None => self.align.remove(&col),
        };
    }

    /// The column's explicit alignment, if one was set.
    pub fn align(&self, col: usize) -> Option<Align> {
        self.align.get(&col).copied()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether the columnar rules are stale for data generation `gen`.
    pub fn needs_refresh(&self, generation: u64) -> bool {
        self.derived_gen != Some(generation) && self.rules.iter().any(|r| r.test.columnar())
    }

    /// Recompute the columnar rules' data for generation `gen`. `values`
    /// visits a column's data values (header excluded) in row order.
    pub fn refresh(
        &mut self,
        generation: u64,
        mut values: impl FnMut(usize, &mut dyn FnMut(&str)),
    ) {
        for rule in &mut self.rules {
            let Some(col) = rule.col else { continue };
            rule.derived = match rule.test {
                RuleTest::Duplicate => {
                    let mut seen: HashMap<String, u32> = HashMap::new();
                    values(col, &mut |v| {
                        if !v.is_empty() {
                            *seen.entry(super::dedup::key(v).into_owned()).or_default() += 1;
                        }
                    });
                    Derived::Set(
                        seen.into_iter()
                            .filter(|(_, n)| *n > 1)
                            .map(|(v, _)| v)
                            .collect(),
                    )
                }
                RuleTest::Top(n) | RuleTest::Bottom(n) => {
                    let mut nums = Vec::new();
                    values(col, &mut |v| {
                        if let Some(x) = super::infer::cell_number(v) {
                            nums.push(x);
                        }
                    });
                    if nums.is_empty() {
                        Derived::None
                    } else {
                        let top = matches!(rule.test, RuleTest::Top(_));
                        let k = n.min(nums.len()) - 1;
                        let (_, nth, _) = nums.select_nth_unstable_by(k, |a, b| {
                            if top { b.total_cmp(a) } else { a.total_cmp(b) }
                        });
                        Derived::Threshold(*nth)
                    }
                }
                _ => continue,
            };
        }
        self.derived_gen = Some(generation);
    }

    /// How `value` in column `col` should be shown, when a format applies.
    pub fn display(&self, col: usize, value: &str) -> Option<String> {
        self.formats.get(&col)?.apply(value)
    }

    /// The face for a cell: the cell's own face, else the last matching cell
    /// rule (later rules refine earlier ones), else the last matching row rule
    /// (`row_value(c)` reads column `c` of the same row), else the column's
    /// face.
    pub fn face<'a>(
        &self,
        row: usize,
        col: usize,
        value: &str,
        row_value: impl Fn(usize) -> &'a str,
    ) -> Option<&str> {
        if let Some(f) = self.cell_faces.get(&col).and_then(|rows| rows.get(&row)) {
            return Some(f);
        }
        self.rules
            .iter()
            .rev()
            .find(|r| !r.row && r.col.is_none_or(|c| c == col) && r.matches(value))
            .or_else(|| {
                self.rules.iter().rev().find(|r| {
                    r.row
                        && r.col
                            .is_some_and(|c| r.matches(if c == col { value } else { row_value(c) }))
                })
            })
            .map(|r| r.face.as_str())
            .or_else(|| self.col_faces.get(&col).map(String::as_str))
    }

    /// Whether any style can apply to column `col` (lets the painter skip
    /// the per-cell checks for plain columns).
    pub fn touches(&self, col: usize) -> bool {
        self.col_faces.contains_key(&col)
            || self.formats.contains_key(&col)
            || self.cell_faces.contains_key(&col)
            || self.align.contains_key(&col)
            || self
                .rules
                .iter()
                .any(|r| r.row || r.col.is_none_or(|c| c == col))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats() {
        let f = |p: &str, v: &str| NumFormat::parse(p).unwrap().apply(v);
        assert_eq!(
            f("#,##0.00", "1234567.891").as_deref(),
            Some("1,234,567.89")
        );
        assert_eq!(f("#,##0", "-1234.5").as_deref(), Some("-1,235"));
        assert_eq!(f("0.0%", "0.256").as_deref(), Some("25.6%"));
        assert_eq!(f("$#,##0.00", "12").as_deref(), Some("$12.00"));
        assert_eq!(f("0.000 kg", "1.5").as_deref(), Some("1.500 kg"));
        assert_eq!(f("0", "-0.2").as_deref(), Some("0"), "no negative zero");
        assert_eq!(f("#,##0", "n/a"), None, "text is left alone");
        assert!(NumFormat::parse("abc").is_none());
    }

    #[test]
    fn rules_and_faces() {
        let mut s = GridStyle::default();
        s.set_column_face(1, "keyword");
        let rule = |col, op, v: &str, face: &str| {
            Rule::new(col, RuleTest::parse(op, v).unwrap(), face.into())
        };
        s.add_rule(rule(Some(1), ">", "100", "error")).unwrap();
        s.add_rule(rule(None, "matches", "^ERR", "warning"))
            .unwrap();
        let none = |_| "";
        assert_eq!(s.face(0, 1, "50", none), Some("keyword"));
        assert_eq!(s.face(0, 1, "150", none), Some("error"));
        assert_eq!(s.face(0, 4, "ERR: disk", none), Some("warning"));
        assert_eq!(s.face(0, 4, "ok", none), None);
        assert!(s.touches(4) && s.touches(1));
        assert!(RuleTest::parse(">", "x").is_err());
        assert!(RuleTest::parse("~", "x").is_err());
        assert!(RuleTest::parse("=", "7").unwrap().test("7.0"));
        assert!(RuleTest::parse("empty", "").unwrap().test(""));
        assert!(RuleTest::parse("between", "10..20").unwrap().test("15"));
        assert!(!RuleTest::parse("between", "20 10").unwrap().test("21"));
        assert!(RuleTest::parse("between", "x").is_err());
        assert!(RuleTest::parse("top", "0").is_err());
    }

    #[test]
    fn row_rules_cell_faces_and_columnar_rules() {
        let mut s = GridStyle::default();
        let rule = |col, op, v: &str, face: &str| {
            Rule::new(col, RuleTest::parse(op, v).unwrap(), face.into())
        };
        // Whole row red when column 2 says "late".
        s.add_rule(Rule {
            row: true,
            ..rule(Some(2), "=", "late", "error")
        })
        .unwrap();
        let row = ["a", "1", "late"];
        assert_eq!(s.face(3, 0, "a", |c| row[c]), Some("error"));
        assert_eq!(s.face(3, 2, "late", |c| row[c]), Some("error"));
        assert_eq!(s.face(3, 0, "a", |_| "on time"), None);
        assert!(s.touches(7), "a row rule touches every column");
        // A cell's own face beats every rule.
        s.set_cell_face(3, 0, "success");
        assert_eq!(s.face(3, 0, "a", |c| row[c]), Some("success"));
        s.set_cell_face(3, 0, "");
        assert_eq!(s.face(3, 0, "a", |c| row[c]), Some("error"));
        // Columnar: duplicates and top 2, computed by refresh.
        s.add_rule(rule(Some(1), "duplicate", "", "warning"))
            .unwrap();
        s.add_rule(rule(Some(0), "top", "2", "success")).unwrap();
        assert!(s.needs_refresh(1));
        let cols = [
            vec!["5", "9", "1", "7"],
            vec!["x", "y", "X", "", "1", "1.0"],
        ];
        s.refresh(1, |c, f| cols[c].iter().for_each(|v| f(v)));
        assert!(!s.needs_refresh(1) && s.needs_refresh(2));
        let none = |_| "";
        assert_eq!(s.face(0, 1, "x", none), Some("warning"));
        assert_eq!(s.face(0, 1, "X", none), Some("warning"), "case ignored");
        assert_eq!(
            s.face(0, 1, "1.00", none),
            Some("warning"),
            "numbers by value"
        );
        assert_eq!(s.face(0, 1, "y", none), None);
        assert_eq!(s.face(0, 0, "9", none), Some("success"));
        assert_eq!(s.face(0, 0, "7", none), Some("success"));
        assert_eq!(s.face(0, 0, "5", none), None);
        assert!(s.add_rule(rule(None, "duplicate", "", "x")).is_err());
    }
}
