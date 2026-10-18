//! Writing: sheets of cells → a minimal `.xlsx` package.
//!
//! The package has exactly the parts Excel needs to open a file without a
//! repair prompt — content types, package and workbook relationships, app
//! and core properties, the workbook, a stylesheet holding just the cell
//! formats used (bold, font and fill colors, number formats, Wrap Text) and
//! the sheets with their row heights — and nothing that would need keeping
//! in sync.
//!
//! Cell encoding:
//!
//! - Numbers use Rust's shortest round-trip form, so reading the file back
//!   gives the identical `f64`; very large or small magnitudes use Excel's
//!   `1.5E+300` notation rather than hundreds of digits. NaN and infinities
//!   aren't valid SpreadsheetML numbers and become `#NUM!`.
//! - Strings are inline (`t="inlineStr"`), so there's no shared-string table
//!   to build; `xml:space="preserve"` keeps edge whitespace and newlines, and
//!   characters XML can't hold use Excel's `_xHHHH_` escapes.
//! - A formula is written with its cached result so other readers show
//!   values without recalculating. The engine's own errors (`#CIRC!`,
//!   `#CALC!`, a parse failure's `#ERROR!` — anything Excel 2007 doesn't
//!   know) cache as 0 instead; the workbook asks for a full recalculation on
//!   load, so Excel replaces them with its own verdict at once.
//! - A dynamic-array formula is written as Excel 365 writes one: an array
//!   formula over the range it spills into, marked with the dynamic-array
//!   cell metadata (`cm="1"`, defined in `xl/metadata.xml`); the spilled
//!   cells are plain cached values. Excel before 365 reads it as an
//!   ordinary array formula.
//!
//! The output depends only on the input: no timestamps, so saving the same
//! cells twice gives identical bytes. Cells are sorted row-major (a later
//! duplicate of a position wins); keeping within Excel's 1,048,576 ×
//! 16,384 limit is the caller's job.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::xml::{escape_attr, escape_xstring};
use super::zip;
use super::{XRow, XStyle, XValue, cell_name};

/// One cell to write. `formula` is formula text without the leading `=`. A
/// cell with a style but no value or formula is written as formatting only.
#[derive(Clone, Copy, Debug)]
pub struct WriteCell<'a> {
    pub row: usize,
    pub col: usize,
    pub value: &'a XValue,
    pub formula: Option<&'a str>,
    /// For a dynamic-array formula, the range it spills into (`B2:B9`; its
    /// own cell when it shows one value).
    pub array: Option<&'a str>,
    pub style: Option<&'a XStyle>,
}

/// Error values every Excel version reads; anything else is written as
/// described in the module docs.
const EXCEL_ERRORS: [&str; 9] = [
    "#NULL!",
    "#DIV/0!",
    "#VALUE!",
    "#REF!",
    "#NAME?",
    "#NUM!",
    "#N/A",
    "#GETTING_DATA",
    "#SPILL!",
];

const DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const NS_MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const NS_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// A complete `.xlsx` file holding one sheet named `sheet_name` (sanitized
/// to what Excel accepts) with `cells`.
#[allow(dead_code)] // the one-sheet shorthand, used by tests
pub fn write<'a>(
    sheet_name: &str,
    cells: impl IntoIterator<Item = WriteCell<'a>>,
    date1904: bool,
) -> Vec<u8> {
    write_book(vec![(sheet_name, cells)], date1904)
}

/// A complete `.xlsx` file holding `sheets` in tab order, each a name
/// (sanitized to what Excel accepts; keeping them distinct is the caller's
/// job) and its cells. The cell formats of every sheet share one stylesheet.
pub fn write_book<'a, I>(sheets: Vec<(&str, I)>, date1904: bool) -> Vec<u8>
where
    I: IntoIterator<Item = WriteCell<'a>>,
{
    let sheets = sheets
        .into_iter()
        .map(|(name, cells)| (name, cells, &[][..]))
        .collect();
    write_book_rows(sheets, date1904)
}

/// [`write_book`], with each sheet's row heights (`rows` in any order, one
/// per row).
pub fn write_book_rows<'a, I>(sheets: Vec<(&str, I, &[XRow])>, date1904: bool) -> Vec<u8>
where
    I: IntoIterator<Item = WriteCell<'a>>,
{
    let n = sheets.len().max(1);
    let names: Vec<&str> = sheets.iter().map(|(name, ..)| *name).collect();
    let mut styles = Styles::default();
    let mut dynamic = false;
    let mut parts: Vec<String> = sheets
        .into_iter()
        .map(|(_, cells, rows)| {
            let (xml, d) = sheet_xml(cells, rows, &mut styles);
            dynamic |= d;
            xml
        })
        .collect();
    if parts.is_empty() {
        parts.push(sheet_xml(std::iter::empty(), &[], &mut styles).0);
    }
    let mut z = zip::Writer::new();
    z.add("[Content_Types].xml", content_types(n, dynamic).as_bytes());
    z.add("_rels/.rels", ROOT_RELS.as_bytes());
    z.add("docProps/app.xml", APP.as_bytes());
    z.add("docProps/core.xml", CORE.as_bytes());
    z.add("xl/workbook.xml", workbook_xml(&names, date1904).as_bytes());
    z.add(
        "xl/_rels/workbook.xml.rels",
        workbook_rels(n, dynamic).as_bytes(),
    );
    z.add("xl/styles.xml", styles.xml().as_bytes());
    for (i, sheet) in parts.iter().enumerate() {
        z.add(
            &format!("xl/worksheets/sheet{}.xml", i + 1),
            sheet.as_bytes(),
        );
    }
    // The dynamic-array metadata part, only when a formula uses it.
    if dynamic {
        z.add("xl/metadata.xml", METADATA.as_bytes());
    }
    z.finish()
}

/// Excel's dynamic-array cell metadata: cell metadata record 1 (`cm="1"`)
/// marks a formula as a dynamic array, exactly as Excel 365 writes it.
const METADATA: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    "<metadata xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" ",
    "xmlns:xda=\"http://schemas.microsoft.com/office/spreadsheetml/2017/dynamicarray\">",
    "<metadataTypes count=\"1\"><metadataType name=\"XLDAPR\" minSupportedVersion=\"120000\" ",
    "copy=\"1\" pasteAll=\"1\" pasteValues=\"1\" merge=\"1\" splitFirst=\"1\" rowColShift=\"1\" ",
    "clearFormats=\"1\" clearComments=\"1\" assign=\"1\" coerce=\"1\" cellMeta=\"1\"/></metadataTypes>",
    "<futureMetadata name=\"XLDAPR\" count=\"1\"><bk><extLst>",
    "<ext uri=\"{bdbb8cdc-fa1e-496e-a857-3c3f30c029c3}\">",
    "<xda:dynamicArrayProperties fDynamic=\"1\" fCollapsed=\"0\"/></ext></extLst></bk></futureMetadata>",
    "<cellMetadata count=\"1\"><bk><rc t=\"1\" v=\"0\"/></bk></cellMetadata>",
    "</metadata>"
);

/// `[Content_Types].xml` for a package of `n` sheets (and `xl/metadata.xml`
/// when a formula is a `dynamic` array).
fn content_types(n: usize, dynamic: bool) -> String {
    let mut s = String::from(CONTENT_TYPES_HEAD);
    for i in 1..=n {
        let _ = write!(
            s,
            "<Override PartName=\"/xl/worksheets/sheet{i}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        );
    }
    if dynamic {
        s.push_str("<Override PartName=\"/xl/metadata.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheetMetadata+xml\"/>");
    }
    s.push_str(CONTENT_TYPES_TAIL);
    s
}

/// The workbook's relationships: sheets `rId1`…`rIdn`, then the styles, then
/// the dynamic-array metadata when it's written.
fn workbook_rels(n: usize, dynamic: bool) -> String {
    let mut s = String::from(DECL);
    s.push_str(
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for i in 1..=n {
        let _ = write!(
            s,
            "<Relationship Id=\"rId{i}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{i}.xml\"/>"
        );
    }
    let _ = write!(
        s,
        "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
        n + 1
    );
    if dynamic {
        let _ = write!(
            s,
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/sheetMetadata\" Target=\"metadata.xml\"/>",
            n + 2
        );
    }
    s.push_str("</Relationships>");
    s
}

const CONTENT_TYPES_HEAD: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
    "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
    "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
    "<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>",
);

const CONTENT_TYPES_TAIL: &str = concat!(
    "<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>",
    "<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>",
    "<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/>",
    "</Types>"
);

const ROOT_RELS: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    "<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>",
    "<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>",
    "<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/>",
    "</Relationships>"
);

const APP: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    "<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\" ",
    "xmlns:vt=\"http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes\">",
    "<Application>Macros</Application></Properties>"
);

const CORE: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
    "<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" ",
    "xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:dcterms=\"http://purl.org/dc/terms/\" ",
    "xmlns:dcmitype=\"http://purl.org/dc/dcmitype/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"/>"
);

/// The stylesheet, built from the styles cells use. Index 0 of each list is
/// Excel's required default: one font, the two fills it reserves (none,
/// gray125), one border, and the "Normal" cell format.
#[derive(Default)]
struct Styles {
    /// Cell formats after the default, in first-use order.
    xfs: Vec<XStyle>,
    index: HashMap<XStyle, usize>,
}

/// Every font's face: the default's, with only bold and color varying.
const FONT_FACE: &str = "<name val=\"Calibri\"/><family val=\"2\"/>";

impl Styles {
    /// The `s` index for `style` (0: the default).
    fn id(&mut self, style: Option<&XStyle>) -> usize {
        let Some(style) = style.filter(|s| !s.is_default()) else {
            return 0;
        };
        if let Some(&i) = self.index.get(style) {
            return i;
        }
        self.xfs.push(style.clone());
        let i = self.xfs.len();
        self.index.insert(style.clone(), i);
        i
    }

    fn xml(&self) -> String {
        // Fonts, fills and number formats, each deduplicated.
        let mut fonts: Vec<(bool, Option<u32>)> = vec![(false, None)];
        let mut fills: Vec<u32> = Vec::new();
        let mut codes: Vec<&str> = Vec::new();
        let mut xfs = String::new();
        for st in &self.xfs {
            let font = (st.bold, st.color);
            let font_id = fonts.iter().position(|&f| f == font).unwrap_or_else(|| {
                fonts.push(font);
                fonts.len() - 1
            });
            let fill_id = st.fill.map_or(0, |argb| {
                2 + fills.iter().position(|&f| f == argb).unwrap_or_else(|| {
                    fills.push(argb);
                    fills.len() - 1
                })
            });
            let num_id = st.num_fmt.as_deref().map_or(0, |code| {
                164 + codes.iter().position(|&c| c == code).unwrap_or_else(|| {
                    codes.push(code);
                    codes.len() - 1
                })
            });
            let _ = write!(
                xfs,
                "<xf numFmtId=\"{num_id}\" fontId=\"{font_id}\" fillId=\"{fill_id}\" borderId=\"0\" xfId=\"0\""
            );
            if num_id != 0 {
                xfs.push_str(" applyNumberFormat=\"1\"");
            }
            if font_id != 0 {
                xfs.push_str(" applyFont=\"1\"");
            }
            if fill_id != 0 {
                xfs.push_str(" applyFill=\"1\"");
            }
            if st.wrap {
                xfs.push_str(" applyAlignment=\"1\"><alignment wrapText=\"1\"/></xf>");
            } else {
                xfs.push_str("/>");
            }
        }
        let mut s = String::from(DECL);
        let _ = write!(s, "<styleSheet xmlns=\"{NS_MAIN}\">");
        if !codes.is_empty() {
            let _ = write!(s, "<numFmts count=\"{}\">", codes.len());
            for (i, code) in codes.iter().enumerate() {
                let _ = write!(s, "<numFmt numFmtId=\"{}\" formatCode=\"", 164 + i);
                escape_attr(&mut s, code);
                s.push_str("\"/>");
            }
            s.push_str("</numFmts>");
        }
        let _ = write!(s, "<fonts count=\"{}\">", fonts.len());
        for (bold, color) in &fonts {
            s.push_str("<font>");
            if *bold {
                s.push_str("<b/>");
            }
            s.push_str("<sz val=\"11\"/>");
            if let Some(argb) = color {
                let _ = write!(s, "<color rgb=\"{argb:08X}\"/>");
            }
            s.push_str(FONT_FACE);
            s.push_str("</font>");
        }
        s.push_str("</fonts>");
        let _ = write!(
            s,
            "<fills count=\"{}\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill>",
            2 + fills.len()
        );
        for argb in &fills {
            let _ = write!(
                s,
                "<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{argb:08X}\"/><bgColor indexed=\"64\"/></patternFill></fill>"
            );
        }
        s.push_str("</fills>");
        s.push_str("<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>");
        s.push_str("<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>");
        let _ = write!(
            s,
            "<cellXfs count=\"{}\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>{xfs}</cellXfs>",
            1 + self.xfs.len()
        );
        s.push_str("<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>");
        s.push_str("</styleSheet>");
        s
    }
}

fn workbook_xml(sheet_names: &[&str], date1904: bool) -> String {
    let mut s = String::from(DECL);
    let _ = write!(s, "<workbook xmlns=\"{NS_MAIN}\" xmlns:r=\"{NS_REL}\">");
    s.push_str(if date1904 {
        "<workbookPr date1904=\"1\"/>"
    } else {
        "<workbookPr/>"
    });
    s.push_str("<bookViews><workbookView/></bookViews><sheets>");
    let fallback = ["Sheet1"];
    let names = if sheet_names.is_empty() {
        &fallback[..]
    } else {
        sheet_names
    };
    for (i, name) in names.iter().enumerate() {
        s.push_str("<sheet name=\"");
        escape_attr(&mut s, &sanitize_sheet_name(name));
        let _ = write!(s, "\" sheetId=\"{0}\" r:id=\"rId{0}\"/>", i + 1);
    }
    s.push_str("</sheets>");
    // Cached values may be stand-ins (see the module docs), so have Excel
    // recalculate everything on open.
    s.push_str("<calcPr calcId=\"0\" fullCalcOnLoad=\"1\"/></workbook>");
    s
}

/// A sheet name Excel accepts: none of `[]:*?/\` or control characters, no
/// apostrophe at either end, at most 31 UTF-16 units, not blank.
pub fn sanitize_sheet_name(name: &str) -> String {
    let mut out = String::new();
    let mut units = 0;
    for c in name.chars() {
        let c = if "[]:*?/\\".contains(c) || c.is_control() || c == '\u{FFFE}' || c == '\u{FFFF}' {
            '_'
        } else {
            c
        };
        units += c.len_utf16();
        if units > 31 {
            break;
        }
        out.push(c);
    }
    if out.trim().is_empty() {
        return "Sheet1".into();
    }
    if out.starts_with('\'') {
        out.replace_range(..1, "_");
    }
    if out.ends_with('\'') {
        out.pop();
        out.push('_');
    }
    out
}

/// `f64` → SpreadsheetML number text, exactly round-tripping (finite only).
fn number_text(v: f64) -> String {
    if v == 0.0 {
        return "0".into(); // also -0, which Excel doesn't distinguish
    }
    let a = v.abs();
    if (1e-5..1e15).contains(&a) {
        return format!("{v}");
    }
    // `{:e}` is also shortest round-trip: "1.5e300" → Excel's "1.5E+300".
    let s = format!("{v:e}");
    match s.split_once('e') {
        Some((m, e)) if e.starts_with('-') => format!("{m}E{e}"),
        Some((m, e)) => format!("{m}E+{e}"),
        None => s,
    }
}

fn needs_preserve(s: &str) -> bool {
    let ws = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r');
    s.starts_with(ws) || s.ends_with(ws) || s.contains('\n')
}

/// A `<row>` start tag: its number and, when it has one, its height.
fn open_row(s: &mut String, row: usize, height: Option<&XRow>) {
    let _ = write!(s, "<row r=\"{}\"", row + 1);
    if let Some(h) = height {
        // Points, to the hundredth (Excel's own precision).
        let pt = (h.height * 100.0).round() / 100.0;
        let _ = write!(s, " ht=\"{pt}\"");
        if h.custom {
            s.push_str(" customHeight=\"1\"");
        }
    }
}

/// The worksheet part, and whether a dynamic-array formula is in it.
fn sheet_xml<'a>(
    cells: impl IntoIterator<Item = WriteCell<'a>>,
    rows: &[XRow],
    styles: &mut Styles,
) -> (String, bool) {
    let heights: std::collections::BTreeMap<usize, &XRow> = rows
        .iter()
        .filter(|r| r.height.is_finite() && r.height >= 0.0)
        .map(|r| (r.row, r))
        .collect();
    let mut cells: Vec<WriteCell> = cells
        .into_iter()
        .filter(|c| {
            c.formula.is_some()
                || *c.value != XValue::Empty
                || c.style.is_some_and(|s| !s.is_default())
        })
        .collect();
    // Stable sort, then keep the last of each position: reversing puts it
    // first for `dedup_by`.
    cells.sort_by_key(|c| (c.row, c.col));
    cells.reverse();
    cells.dedup_by(|a, b| (a.row, a.col) == (b.row, b.col));
    cells.reverse();

    let mut s = String::with_capacity(64 + cells.len() * 32);
    s.push_str(DECL);
    let _ = write!(s, "<worksheet xmlns=\"{NS_MAIN}\" xmlns:r=\"{NS_REL}\">");
    let dim = match (cells.first(), cells.last()) {
        (Some(first), Some(last)) => {
            let min_col = cells.iter().map(|c| c.col).min().unwrap_or(0);
            let max_col = cells.iter().map(|c| c.col).max().unwrap_or(0);
            let (a, b) = ((first.row, min_col), (last.row, max_col));
            let tl = cell_name(a.0, a.1);
            if a == b {
                tl
            } else {
                format!("{tl}:{}", cell_name(b.0, b.1))
            }
        }
        _ => "A1".into(),
    };
    let _ = write!(s, "<dimension ref=\"{dim}\"/>");
    if cells.is_empty() && heights.is_empty() {
        s.push_str("<sheetData/></worksheet>");
        return (s, false);
    }
    let dynamic = cells
        .iter()
        .any(|c| c.formula.is_some() && c.array.is_some());
    s.push_str("<sheetData>");
    // Rows with a height but no cells go in order among the others.
    let mut sized = heights.iter().peekable();
    let mut empty_rows_before = |s: &mut String, row: usize| {
        while let Some((&r, h)) = sized.next_if(|(r, _)| **r < row) {
            open_row(s, r, Some(h));
            s.push_str("/>");
        }
    };
    let mut row = None;
    for c in &cells {
        if row != Some(c.row) {
            if row.is_some() {
                s.push_str("</row>");
            }
            empty_rows_before(&mut s, c.row);
            open_row(&mut s, c.row, heights.get(&c.row).copied());
            s.push('>');
            row = Some(c.row);
        }
        push_cell(&mut s, c, styles.id(c.style));
    }
    if row.is_some() {
        s.push_str("</row>");
    }
    empty_rows_before(&mut s, usize::MAX);
    s.push_str("</sheetData></worksheet>");
    (s, dynamic)
}

fn push_cell(s: &mut String, c: &WriteCell, style: usize) {
    // The cell's position, and its style when it has one.
    let r = match style {
        0 => format!("r=\"{}\"", cell_name(c.row, c.col)),
        n => format!("r=\"{}\" s=\"{n}\"", cell_name(c.row, c.col)),
    };
    let excel_error = |e: &str| EXCEL_ERRORS.contains(&e);
    let Some(f) = c.formula else {
        match c.value {
            XValue::Number(n) if n.is_finite() => {
                let _ = write!(s, "<c {r}><v>{}</v></c>", number_text(*n));
            }
            XValue::Number(_) => {
                let _ = write!(s, "<c {r} t=\"e\"><v>#NUM!</v></c>");
            }
            XValue::Bool(b) => {
                let _ = write!(s, "<c {r} t=\"b\"><v>{}</v></c>", *b as u8);
            }
            XValue::Error(e) if excel_error(e) => {
                let _ = write!(s, "<c {r} t=\"e\"><v>{e}</v></c>");
            }
            // An error Excel can't read is kept as its text.
            XValue::Text(t) | XValue::Error(t) => {
                let _ = write!(s, "<c {r} t=\"inlineStr\"><is><t");
                if needs_preserve(t) {
                    s.push_str(" xml:space=\"preserve\"");
                }
                s.push('>');
                escape_xstring(s, t);
                s.push_str("</t></is></c>");
            }
            XValue::Empty if style != 0 => {
                let _ = write!(s, "<c {r}/>");
            }
            XValue::Empty => {}
        }
        return;
    };
    let (typ, cached): (&str, Option<String>) = match c.value {
        XValue::Number(n) if n.is_finite() => ("", Some(number_text(*n))),
        XValue::Number(_) => (" t=\"e\"", Some("#NUM!".into())),
        XValue::Text(t) => {
            let mut v = String::new();
            escape_xstring(&mut v, t);
            (" t=\"str\"", Some(v))
        }
        XValue::Bool(b) => (" t=\"b\"", Some((*b as u8).to_string())),
        XValue::Error(e) if excel_error(e) => (" t=\"e\"", Some(e.clone())),
        XValue::Error(_) => ("", Some("0".into())),
        XValue::Empty => ("", None),
    };
    match c.array {
        Some(range) => {
            let _ = write!(s, "<c {r}{typ} cm=\"1\"><f t=\"array\" ref=\"");
            escape_attr(s, range);
            s.push_str("\">");
        }
        None => {
            let _ = write!(s, "<c {r}{typ}><f>");
        }
    }
    escape_xstring(s, f);
    s.push_str("</f>");
    if let Some(v) = cached {
        let _ = write!(s, "<v>{v}</v>");
    }
    s.push_str("</c>");
}

#[cfg(test)]
mod tests {
    use super::super::{XCell, XFormula, read};
    use super::*;

    fn cell(row: usize, col: usize, value: &XValue) -> WriteCell<'_> {
        WriteCell {
            row,
            col,
            value,
            formula: None,
            array: None,
            style: None,
        }
    }

    fn round_trip(cells: &[WriteCell]) -> Vec<XCell> {
        let bytes = write("Sheet", cells.iter().copied(), false);
        let book = read(&bytes).unwrap();
        assert_eq!(book.not_preserved, Vec::<String>::new());
        assert_eq!(book.application.as_deref(), Some("Macros"));
        assert_eq!(book.sheets.len(), 1);
        book.sheets.into_iter().next().unwrap().cells
    }

    #[test]
    fn values_round_trip() {
        let texts = [
            "plain",
            "  leading and trailing  ",
            "line one\nline two",
            "\ttab",
            "cr\r\nlf",
            "ctl \u{1}\u{2}\u{1F}\u{B}",
            "unicode 漢字 😀 é",
            "_x0041_ literal",
            "_x005F_",
            "<&>\"'",
            "",
            " ",
        ];
        let values: Vec<XValue> = texts
            .iter()
            .map(|t| XValue::Text(t.to_string()))
            .chain([
                XValue::Bool(true),
                XValue::Bool(false),
                XValue::Error("#N/A".into()),
                XValue::Error("#DIV/0!".into()),
                XValue::Number(0.1 + 0.2),
                XValue::Number(-1.0),
                XValue::Number(1e308),
                XValue::Number(f64::MAX),
                XValue::Number(f64::MIN_POSITIVE),
                XValue::Number(5e-324),
                XValue::Number(-2.5e-300),
                XValue::Number(123456789012345678.0),
                XValue::Number(1e15),
                XValue::Number(99999999999999.98),
                XValue::Number(0.00001),
                XValue::Number(0.000009999),
            ])
            .collect();
        let cells: Vec<WriteCell> = values
            .iter()
            .enumerate()
            .map(|(i, v)| cell(i, i % 3, v))
            .collect();
        let got = round_trip(&cells);
        assert_eq!(got.len(), values.len());
        for (g, v) in got.iter().zip(&values) {
            assert_eq!(&g.value, v);
            assert_eq!(g.formula, None);
        }
    }

    #[test]
    fn random_numbers_exact() {
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        let values: Vec<XValue> = (0..3000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                f64::from_bits(rng)
            })
            .filter(|f| f.is_finite())
            .map(XValue::Number)
            .collect();
        let cells: Vec<WriteCell> = values
            .iter()
            .enumerate()
            .map(|(i, v)| cell(i, 0, v))
            .collect();
        for (g, v) in round_trip(&cells).iter().zip(&values) {
            let (XValue::Number(a), XValue::Number(b)) = (&g.value, v) else {
                panic!()
            };
            assert!(a == b || (*a == 0.0 && *b == 0.0), "{a} vs {b}");
            if *b != 0.0 {
                assert_eq!(a.to_bits(), b.to_bits());
            }
        }
    }

    #[test]
    fn formulas_round_trip() {
        let num = XValue::Number(3.0);
        let txt = XValue::Text(" a\nb ".into());
        let t = XValue::Bool(true);
        let na = XValue::Error("#N/A".into());
        let empty = XValue::Empty;
        let f = |row, value, formula| WriteCell {
            row,
            col: 1,
            value,
            formula: Some(formula),
            array: None,
            style: None,
        };
        let cells = [
            f(0, &num, "1+2"),
            f(1, &txt, "\" a\"&CHAR(10)&\"b \""),
            f(2, &t, "A1>2"),
            f(3, &na, "NA()"),
            f(4, &empty, "NOW()"),
            f(5, &num, "IF(A1<2,\"<&>\",\"_x0041_\")"),
        ];
        let got = round_trip(&cells);
        for (g, c) in got.iter().zip(&cells) {
            assert_eq!(g.formula, Some(XFormula::Normal(c.formula.unwrap().into())));
            assert_eq!(&g.value, c.value);
        }
    }

    #[test]
    fn engine_errors_and_non_finite_numbers() {
        let circ = XValue::Error("#CIRC!".into());
        let calc = XValue::Error("#CALC!".into());
        let nan = XValue::Number(f64::NAN);
        let inf = XValue::Number(f64::INFINITY);
        let cells = [
            WriteCell {
                row: 0,
                col: 0,
                value: &circ,
                formula: Some("A1"),
                array: None,
                style: None,
            },
            WriteCell {
                row: 1,
                col: 0,
                value: &calc,
                formula: Some("SCHEME(1)"),
                array: None,
                style: None,
            },
            WriteCell {
                row: 2,
                col: 0,
                value: &nan,
                formula: Some("0/0"),
                array: None,
                style: None,
            },
            cell(3, 0, &inf),
            cell(4, 0, &circ),
        ];
        let got = round_trip(&cells);
        assert_eq!(got[0].value, XValue::Number(0.0));
        assert_eq!(got[0].formula, Some(XFormula::Normal("A1".into())));
        assert_eq!(got[1].value, XValue::Number(0.0));
        assert_eq!(got[2].value, XValue::Error("#NUM!".into()));
        assert_eq!(got[3].value, XValue::Error("#NUM!".into()));
        assert_eq!(got[4].value, XValue::Text("#CIRC!".into()));
    }

    #[test]
    fn cells_sorted_deduplicated_and_empties_dropped() {
        let (a, b, c, e) = (
            XValue::Number(1.0),
            XValue::Number(2.0),
            XValue::Number(3.0),
            XValue::Empty,
        );
        let cells = [
            cell(5, 2, &a),
            cell(0, 1, &b),
            cell(5, 0, &c),
            cell(0, 1, &a),
            cell(2, 2, &e),
        ];
        let got = round_trip(&cells);
        let pos: Vec<(usize, usize, XValue)> =
            got.into_iter().map(|c| (c.row, c.col, c.value)).collect();
        assert_eq!(
            pos,
            [(0, 1, a.clone()), (5, 0, c.clone()), (5, 2, a.clone())]
        );
        let (xml, _) = sheet_xml(cells.iter().copied(), &[], &mut Styles::default());
        assert!(xml.contains("<dimension ref=\"A1:C6\"/>"), "{xml}");
        assert!(
            sheet_xml(std::iter::empty(), &[], &mut Styles::default())
                .0
                .contains("<dimension ref=\"A1\"/><sheetData/>")
        );
        assert!(round_trip(&[]).is_empty());
    }

    #[test]
    fn sheet_names() {
        assert_eq!(
            sanitize_sheet_name("Q1 [draft]: a/b\\c*?"),
            "Q1 _draft__ a_b_c__"
        );
        assert_eq!(sanitize_sheet_name(""), "Sheet1");
        assert_eq!(sanitize_sheet_name("   "), "Sheet1");
        assert_eq!(sanitize_sheet_name("'quoted'"), "_quoted_");
        assert_eq!(sanitize_sheet_name(&"x".repeat(40)).len(), 31);
        assert_eq!(sanitize_sheet_name(&"😀".repeat(20)).chars().count(), 15);
        assert_eq!(sanitize_sheet_name("tab\there"), "tab_here");
        let bytes = write("R&D <2024> \"final\"", std::iter::empty(), true);
        let book = read(&bytes).unwrap();
        assert_eq!(book.sheets[0].name, "R&D <2024> \"final\"");
        assert!(book.date1904);
        assert_eq!(book.not_preserved, Vec::<String>::new());
    }

    #[test]
    fn number_text_forms() {
        assert_eq!(number_text(1.0), "1");
        assert_eq!(number_text(-0.0), "0");
        assert_eq!(number_text(0.1), "0.1");
        assert_eq!(number_text(1e300), "1E+300");
        assert_eq!(number_text(-1.5e-7), "-1.5E-7");
        assert_eq!(number_text(123456.789), "123456.789");
    }

    #[test]
    fn output_is_deterministic() {
        let v = XValue::Text("same".into());
        let a = write("S", [cell(0, 0, &v)], false);
        let b = write("S", [cell(0, 0, &v)], false);
        assert_eq!(a, b);
    }

    #[test]
    fn styles_round_trip_and_style_only_cells_are_kept() {
        let bold_red = XStyle {
            bold: true,
            color: Some(0xFFFF0000),
            ..Default::default()
        };
        let money = XStyle {
            num_fmt: Some("$#,##0.00".into()),
            fill: Some(0xFFFFFF00),
            ..Default::default()
        };
        let quoted = XStyle {
            num_fmt: Some("0.0 \"kg\"".into()),
            ..Default::default()
        };
        let n = XValue::Number(1234.5);
        let t = XValue::Text("hi".into());
        let empty = XValue::Empty;
        let plain = XStyle::default();
        let with = |row, value, style| WriteCell {
            row,
            col: 0,
            value,
            formula: None,
            array: None,
            style,
        };
        let cells = [
            with(0, &t, Some(&bold_red)),
            with(1, &n, Some(&money)),
            with(2, &n, Some(&money)),
            with(3, &empty, Some(&quoted)),
            with(4, &n, Some(&plain)),
            with(5, &empty, None),
        ];
        let bytes = write("S", cells.iter().copied(), false);
        let book = read(&bytes).unwrap();
        assert_eq!(book.not_preserved, Vec::<String>::new());
        let got: Vec<(usize, &XStyle)> = book.sheets[0]
            .cells
            .iter()
            .map(|c| (c.row, &book.styles[c.style]))
            .collect();
        assert_eq!(
            got,
            [
                (0, &bold_red),
                (1, &money),
                (2, &money),
                (3, &quoted),
                (4, &plain)
            ],
            "a style-only cell is kept; a default style and an empty cell are not"
        );
        // Two cells with one style share one cell format.
        assert_eq!(book.sheets[0].cells[1].style, book.sheets[0].cells[2].style);
    }

    /// Wrap Text and row heights read back as written; a height Excel would
    /// work out itself (not `custom`) is written but not read back, and a
    /// height on a row without cells still gets its row.
    #[test]
    fn wrap_and_row_heights_round_trip() {
        let wrap = XStyle {
            wrap: true,
            ..Default::default()
        };
        let bold_wrap = XStyle {
            bold: true,
            wrap: true,
            ..Default::default()
        };
        let t = XValue::Text("one\ntwo".into());
        let cells = [
            WriteCell {
                row: 1,
                col: 0,
                value: &t,
                formula: None,
                array: None,
                style: Some(&wrap),
            },
            WriteCell {
                row: 1,
                col: 1,
                value: &t,
                formula: None,
                array: None,
                style: Some(&bold_wrap),
            },
        ];
        let rows = [
            XRow {
                row: 4,
                height: 40.5,
                custom: true,
            },
            XRow {
                row: 1,
                height: 30.0,
                custom: false,
            },
            XRow {
                row: 0,
                height: 9.75,
                custom: true,
            },
        ];
        let bytes = write_book_rows(vec![("S", cells.iter().copied(), &rows[..])], false);
        let book = read(&bytes).unwrap();
        assert_eq!(book.not_preserved, Vec::<String>::new());
        let sheet = &book.sheets[0];
        assert_eq!(book.styles[sheet.cells[0].style], wrap);
        assert_eq!(book.styles[sheet.cells[1].style], bold_wrap);
        assert_eq!(sheet.rows, [rows[2], rows[0]]);
        let mut s = Styles::default();
        let (xml, _) = sheet_xml(cells.iter().copied(), &rows, &mut s);
        assert!(
            xml.contains("<row r=\"1\" ht=\"9.75\" customHeight=\"1\"/><row r=\"2\" ht=\"30\">"),
            "{xml}"
        );
        assert!(
            xml.ends_with("<row r=\"5\" ht=\"40.5\" customHeight=\"1\"/></sheetData></worksheet>")
        );
    }
}
