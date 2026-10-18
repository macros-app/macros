//! Remove duplicates (Excel's Data › Remove Duplicates): which rows repeat
//! an earlier row on a set of key columns.
//!
//! Values compare the way Excel's Remove Duplicates compares them, on what
//! the cell shows (a formula by its result):
//! - text ignores case: `Ann` = `ANN`;
//! - a value that reads as a number compares by value: `1` = `1.0` = `1e0`
//!   = ` 1 ` (and `-0` = `0`); `1` ≠ `one`;
//! - other whitespace is significant: `a ` ≠ `a` (trim the cells first,
//!   `grid-trim-cells`, to ignore it);
//! - empty cells equal each other;
//! - anything else (dates, booleans, error values) compares as its text.
//!
//! The `duplicate` highlight rule uses the same comparison ([`key`]), so a
//! highlighted value is exactly one Remove duplicates would act on.
//!
//! Detection is one pass with a hash of each row's key, so a million rows
//! take well under a second: no row is compared with more than the few rows
//! that share its hash.

use std::borrow::Cow;
use std::hash::Hasher;

use rustc_hash::{FxHashMap, FxHasher};

use super::infer::cell_number;

/// One cell's value as Remove duplicates sees it.
#[derive(Clone, Copy, Debug)]
enum Part<'a> {
    /// A number's bits (`-0` folded into `0`).
    Num(u64),
    Text(&'a str),
}

impl<'a> Part<'a> {
    fn of(value: &'a str) -> Self {
        match cell_number(value) {
            // `+ 0.0` turns -0 into 0, the only two equal floats with
            // different bits (NaN and infinities never parse).
            Some(n) => Part::Num((n + 0.0).to_bits()),
            None => Part::Text(value),
        }
    }

    fn hash(self, h: &mut FxHasher) {
        match self {
            Part::Num(bits) => {
                h.write_u8(1);
                h.write_u64(bits);
            }
            Part::Text(s) => {
                h.write_u8(2);
                if s.is_ascii() {
                    for b in s.bytes() {
                        h.write_u8(b.to_ascii_lowercase());
                    }
                } else {
                    for c in s.chars().flat_map(char::to_lowercase) {
                        h.write_u32(c as u32);
                    }
                }
                h.write_u8(0xff);
            }
        }
    }

    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Part::Num(a), Part::Num(b)) => a == b,
            (Part::Text(a), Part::Text(b)) => text_eq(a, b),
            _ => false,
        }
    }
}

/// Text equality ignoring case.
fn text_eq(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.chars()
            .flat_map(char::to_lowercase)
            .eq(b.chars().flat_map(char::to_lowercase))
    }
}

/// `value` reduced to what Remove duplicates compares: two values are
/// duplicates exactly when their keys are equal. (The `duplicate` highlight
/// rule keys its counts with this.)
pub fn key(value: &str) -> Cow<'_, str> {
    match Part::of(value) {
        // Marked with a control character, so a number's key can't equal
        // ordinary text.
        Part::Num(bits) => Cow::Owned(format!("\u{1}{:016x}", bits)),
        Part::Text(s) if s.chars().any(char::is_uppercase) => Cow::Owned(s.to_lowercase()),
        Part::Text(s) => Cow::Borrowed(s),
    }
}

/// The rows of `rows` (in order) that duplicate an earlier one there on
/// columns `keys`, keeping each first occurrence. `cell(r, c)` is the value
/// a cell shows. Returns them in ascending order when `rows` is ascending.
pub fn duplicate_rows<'a>(
    rows: impl IntoIterator<Item = usize>,
    keys: &[usize],
    cell: impl Fn(usize, usize) -> &'a str,
) -> Vec<usize> {
    let row_hash = |r: usize| {
        let mut h = FxHasher::default();
        for &c in keys {
            Part::of(cell(r, c)).hash(&mut h);
        }
        h.finish()
    };
    let same_row = |a: usize, b: usize| {
        keys.iter()
            .all(|&c| Part::of(cell(a, c)).same(Part::of(cell(b, c))))
    };
    dedup_by(rows, row_hash, same_row)
}

/// [`duplicate_rows`] given each row's hash and an exact comparison.
fn dedup_by(
    rows: impl IntoIterator<Item = usize>,
    row_hash: impl Fn(usize) -> u64,
    same_row: impl Fn(usize, usize) -> bool,
) -> Vec<usize> {
    // The first row seen with each hash; rows whose key differs from it but
    // whose hash doesn't (a collision) go in `others`, rarely used.
    let mut first: FxHashMap<u64, usize> = FxHashMap::default();
    let mut others: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
    let mut dups = Vec::new();
    for r in rows {
        let h = row_hash(r);
        match first.get(&h) {
            None => {
                first.insert(h, r);
            }
            Some(&f) if same_row(f, r) => dups.push(r),
            Some(_) => {
                let list = others.entry(h).or_default();
                if list.iter().any(|&o| same_row(o, r)) {
                    dups.push(r);
                } else {
                    list.push(r);
                }
            }
        }
    }
    dups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dups(rows: &[&[&str]], keys: &[usize]) -> Vec<usize> {
        duplicate_rows(0..rows.len(), keys, |r, c| {
            rows[r].get(c).copied().unwrap_or("")
        })
    }

    #[test]
    fn keeps_the_first_occurrence() {
        let rows: &[&[&str]] = &[
            &["a", "1"],
            &["b", "2"],
            &["a", "1"],
            &["a", "3"],
            &["b", "2"],
        ];
        assert_eq!(dups(rows, &[0, 1]), vec![2, 4]);
        assert_eq!(dups(rows, &[0]), vec![2, 3, 4]);
        assert_eq!(dups(rows, &[1]), vec![2, 4]);
    }

    #[test]
    fn comparison() {
        let same = |a: &str, b: &str| dups(&[&[a], &[b]], &[0]) == vec![1];
        // Case is ignored, ASCII or not.
        assert!(same("Ann", "aNN"));
        assert!(same("ÉCOLE", "école"));
        assert!(same("TRUE", "true"));
        // Numbers by value.
        assert!(same("1", "1.0"));
        assert!(same("1", "1e0"));
        assert!(same("1", " 1 "));
        assert!(same("-0", "0"));
        assert!(same("0.10", ".1"));
        assert!(!same("1", "1.0001"));
        assert!(!same("1", "one"));
        // Whitespace in text matters, as in Excel.
        assert!(!same("a ", "a"));
        assert!(!same("a b", "a  b"));
        // Empty equals empty, not a space.
        assert!(same("", ""));
        assert!(!same("", " "));
        // Dates compare as their text.
        assert!(!same("2024-01-05", "1/5/2024"));
        // Errors as their text.
        assert!(same("#N/A", "#n/a"));
    }

    #[test]
    fn key_matches_the_comparison() {
        for (a, b) in [("Ann", "ANN"), ("1", "1.00"), ("-0", "0"), ("", "")] {
            assert_eq!(key(a), key(b), "{a:?} {b:?}");
        }
        for (a, b) in [("a ", "a"), ("1", "one"), ("1", "\u{1}")] {
            assert_ne!(key(a), key(b), "{a:?} {b:?}");
        }
        assert!(matches!(key("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn hash_collisions_still_compare_values() {
        // Every row hashes alike: equality alone must decide.
        let rows = ["x", "y", "x", "z", "y", "Y"];
        let out = dedup_by(0..rows.len(), |_| 7, |a, b| text_eq(rows[a], rows[b]));
        assert_eq!(out, vec![2, 4, 5]);
    }
}
