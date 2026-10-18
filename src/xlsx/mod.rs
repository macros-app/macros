//! A minimal `.xlsx` (Office Open XML SpreadsheetML) reader and writer.
//!
//! Scope: cell values and formulas — what a grid buffer holds. Everything is
//! written from scratch on `std` alone, so the package format is fully under
//! our control and adds no dependencies:
//!
//! - [`crc32`], [`zip`], [`inflate`] and [`deflate`]: the ZIP container.
//! - [`xml`]: a pull tokenizer and the escaping SpreadsheetML needs.
//! - [`read`](mod@read): package → [`Book`] — every worksheet's cells with
//!   cached values and formulas, plus a list of what a rewrite would lose.
//! - [`write`](mod@write): sheets of cells → a minimal package that Excel,
//!   LibreOffice and Google Sheets open without repair prompts.
//!
//! Formula text is passed through untouched (without the leading `=`); the
//! formula engine owns parsing, and shared formulas are handed to it unexpanded
//! because only its parser can shift their relative references.
//!
//! The reader is honest about loss rather than clever about preservation:
//! [`Book::not_preserved`] names every feature found in the package that
//! [`write`] can't reproduce (styles, merged cells, charts, …), so the editor
//! can warn before a save would drop them. A file this module wrote reads
//! back with that list empty.

pub mod crc32;
pub mod deflate;
pub mod inflate;
pub mod read;
pub mod write;
pub mod xml;
pub mod zip;

use std::fmt;

// The entry points, re-exported for the grid integration (not yet wired).
#[allow(unused_imports)]
pub use read::read;
#[allow(unused_imports)]
pub use write::{WriteCell, write, write_book, write_book_rows};

/// A cell's value — for a formula cell, the result cached in the file.
#[derive(Clone, Debug, PartialEq)]
pub enum XValue {
    Number(f64),
    Text(String),
    Bool(bool),
    /// An error value as Excel spells it: `"#N/A"`, `"#DIV/0!"`, ….
    Error(String),
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XFormula {
    /// Plain formula text WITHOUT the leading '=' exactly as stored in `<f>`.
    Normal(String),
    /// A shared formula: the master cell carries text + the range it applies
    /// to; other cells only carry the group index. The caller expands them
    /// (it owns the formula parser that translates relative references).
    SharedMaster {
        si: u32,
        text: String,
        range: String,
    },
    SharedChild {
        si: u32,
    },
    /// An array (CSE) formula: text and its range.
    Array {
        text: String,
        range: String,
    },
    /// A dynamic-array formula (an array formula with Excel's
    /// dynamic-array cell metadata, `cm`): text and the range its result
    /// spilled into when saved. The other cells of the range hold the
    /// spilled values, cached.
    Dynamic {
        text: String,
        range: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct XCell {
    /// 0-based.
    pub row: usize,
    /// 0-based.
    pub col: usize,
    pub value: XValue,
    pub formula: Option<XFormula>,
    /// Index into [`Book::styles`]; 0 is the default (no formatting).
    pub style: usize,
}

/// The cell formatting this module reads and writes: bold, a font color and
/// a solid fill (ARGB), a number-format code, and Wrap Text. A file's cell
/// format holding anything more (italics, borders, other alignment, a named
/// style, …) is reported in [`Book::not_preserved`] as "cell styles"
/// instead.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct XStyle {
    pub bold: bool,
    pub color: Option<u32>,
    pub fill: Option<u32>,
    /// `None` is General.
    pub num_fmt: Option<String>,
    /// `<alignment wrapText="1"/>`.
    pub wrap: bool,
}

impl XStyle {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct XSheet {
    pub name: String,
    /// Row-major order.
    pub cells: Vec<XCell>,
    /// Rows with a height set by hand (`customHeight`), in order. A height
    /// Excel worked out itself isn't kept: it is worked out again.
    pub rows: Vec<XRow>,
}

/// A row's height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XRow {
    /// 0-based.
    pub row: usize,
    /// In points (`ht`).
    pub height: f64,
    /// Set by hand (`customHeight`); otherwise written so other apps show
    /// it before they lay the row out themselves.
    pub custom: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Book {
    /// Worksheets in workbook (tab) order. Chart sheets and macro sheets
    /// aren't included; they show up in `not_preserved`.
    pub sheets: Vec<XSheet>,
    pub date1904: bool,
    /// The cell formats cells refer to by [`XCell::style`] (the file's
    /// `cellXfs`, in order; one this module can't represent reads as the
    /// default and is noted in `not_preserved`).
    pub styles: Vec<XStyle>,
    /// Value of `docProps/app.xml` `<Application>`, if any ("Macros" for
    /// files we wrote).
    pub application: Option<String>,
    /// Everything in the package this module would NOT preserve on a
    /// rewrite, as short human-readable reasons ("charts",
    /// "cell styles", "merged cells", "unknown part xl/foo.xml", …), each
    /// listed once in order of discovery. Empty means a rewrite with
    /// [`write`] loses nothing.
    pub not_preserved: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XlsxError {
    /// Not a ZIP archive at all.
    NotZip,
    /// A ZIP archive or package part that is damaged or malformed.
    Corrupt(String),
    /// Password-protected (an OLE-wrapped encrypted package, or encrypted
    /// ZIP entries).
    Encrypted,
    /// Valid, but uses something this module doesn't handle (ZIP64, `.xlsb`,
    /// a compression method other than deflate, …).
    Unsupported(String),
}

impl fmt::Display for XlsxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XlsxError::NotZip => write!(f, "not an .xlsx file (no ZIP archive found)"),
            XlsxError::Corrupt(why) => write!(f, "damaged .xlsx file: {why}"),
            XlsxError::Encrypted => write!(f, "the file is password-protected"),
            XlsxError::Unsupported(what) => write!(f, "unsupported .xlsx feature: {what}"),
        }
    }
}

impl std::error::Error for XlsxError {}

// A1 notation and Excel's sheet size live in `crate::a1`, shared with the
// formula engine; re-exported here for the reader, the writer and callers.
#[allow(unused_imports)]
pub use crate::a1::{MAX_COLS, MAX_ROWS, cell_name, col_name, parse_col_name, parse_ref};
