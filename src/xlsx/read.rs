//! Reading: an `.xlsx` package → [`Book`].
//!
//! The package is navigated the way OPC intends, through relationships, not
//! by guessing part names: `_rels/.rels` names the workbook part, the
//! workbook's relationships name its sheets, shared strings and styles, and
//! `<sheets>` in the workbook gives tab order. Fallbacks (the content-types
//! list, then `xl/workbook.xml`) only apply when a package lacks the
//! relationship outright.
//!
//! Alongside the cells, every part and every worksheet element is checked
//! against what [`write`](super::write) reproduces, and anything else
//! becomes a reason in [`Book::not_preserved`]. The checks err toward
//! reporting: an unknown part or sheet element is listed by name. A few
//! things Excel writes into every file and that carry no user content are
//! deliberately not reported — the theme, `calcChain.xml` (rebuilt on
//! load), core properties (author/dates), `<pageMargins>`, `<sheetFormatPr>`
//! and the workbook's view settings.

use super::xml::{Event, Reader, Tag, XmlError, decode_xstring};
use super::zip::Archive;
use super::{Book, MAX_ROWS, XCell, XFormula, XRow, XSheet, XStyle, XValue, XlsxError, parse_ref};

/// Read a whole workbook from the bytes of an `.xlsx` file.
pub fn read(bytes: &[u8]) -> Result<Book, XlsxError> {
    let zip = Archive::open(bytes)?;
    let pkg = Package { zip };
    if pkg.has("xl/workbook.bin") {
        return Err(XlsxError::Unsupported("binary workbook (.xlsb)".into()));
    }
    let mut notes = Notes::default();

    let root_rels = match pkg.get("_rels/.rels")? {
        Some(xml) => parse_rels(&xml, "").map_err(|e| bad("_rels/.rels", e))?,
        None => Vec::new(),
    };
    let wb_part = match find_rel(&root_rels, "/officeDocument") {
        Some(r) => r.target.clone(),
        None => workbook_from_content_types(&pkg)?.unwrap_or_else(|| "xl/workbook.xml".into()),
    };
    let wb_xml = pkg
        .get(&wb_part)?
        .ok_or_else(|| XlsxError::Corrupt(format!("workbook part {wb_part} is missing")))?;
    let wb = parse_workbook(&wb_xml, &mut notes).map_err(|e| bad(&wb_part, e))?;
    if wb.sheets.len() > 1 {
        notes.add_front(format!("{} sheets", wb.sheets.len()));
    }

    let app_part = find_rel(&root_rels, "/extended-properties")
        .map_or_else(|| "docProps/app.xml".to_string(), |r| r.target.clone());
    let application = match pkg.get(&app_part)? {
        Some(xml) => parse_application(&xml).map_err(|e| bad(&app_part, e))?,
        None => None,
    };

    let wb_rels_part = rels_path(&wb_part);
    let wb_rels = match pkg.get(&wb_rels_part)? {
        Some(xml) => parse_rels(&xml, &wb_part).map_err(|e| bad(&wb_rels_part, e))?,
        None => Vec::new(),
    };

    let mut styles = vec![Some(XStyle::default())];
    if let Some(rel) = find_rel(&wb_rels, "/styles")
        && let Some(xml) = pkg.get(&rel.target)?
    {
        styles = parse_styles(&xml).map_err(|e| bad(&rel.target, e))?;
    }

    let shared = match find_rel(&wb_rels, "/sharedStrings") {
        Some(rel) => match pkg.get(&rel.target)? {
            Some(xml) => parse_shared_strings(&xml, &mut notes).map_err(|e| bad(&rel.target, e))?,
            None => Vec::new(),
        },
        None => Vec::new(),
    };

    let mut sheets = Vec::new();
    for s in &wb.sheets {
        let rel = wb_rels.iter().find(|r| r.id == s.rid).ok_or_else(|| {
            XlsxError::Corrupt(format!("sheet {:?} has no relationship {}", s.name, s.rid))
        })?;
        if rel.typ.ends_with("/worksheet") {
            let xml = pkg.get(&rel.target)?.ok_or_else(|| {
                XlsxError::Corrupt(format!("sheet part {} is missing", rel.target))
            })?;
            let (cells, rows) = parse_sheet_rows(&xml, &shared, &styles, &mut notes)
                .map_err(|e| bad(&rel.target, e))?;
            sheets.push(XSheet {
                name: s.name.clone(),
                cells,
                rows,
            });
        } else if rel.typ.ends_with("/chartsheet") {
            notes.add("chart sheets");
        } else if rel.typ.ends_with("/dialogsheet") {
            notes.add("dialog sheets");
        } else {
            notes.add("macro sheets");
        }
    }

    for e in &pkg.zip.entries {
        if let Some(reason) = classify_part(&e.name) {
            notes.add(&reason);
        }
    }
    // Drawings hold the anchors of charts and images, and VML drawings the
    // boxes of comments: reporting both says the same thing twice.
    if notes.has("charts") || notes.has("images") || notes.has("chart sheets") {
        notes.remove("drawings");
    }
    if notes.has("comments") {
        notes.remove("legacy drawings");
    }

    Ok(Book {
        sheets,
        date1904: wb.date1904,
        styles: styles.into_iter().map(Option::unwrap_or_default).collect(),
        application,
        not_preserved: notes.0,
    })
}

fn bad(part: &str, e: XmlError) -> XlsxError {
    XlsxError::Corrupt(format!("{part}: {e}"))
}

struct Package<'a> {
    zip: Archive<'a>,
}

impl Package<'_> {
    fn has(&self, name: &str) -> bool {
        self.zip.find(name).is_some()
    }

    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, XlsxError> {
        match self.zip.find(name) {
            Some(e) => self.zip.read(e).map(Some),
            None => Ok(None),
        }
    }
}

/// Reasons in discovery order, each once.
#[derive(Default)]
struct Notes(Vec<String>);

impl Notes {
    fn add(&mut self, reason: &str) {
        if !self.has(reason) {
            self.0.push(reason.to_string());
        }
    }

    fn add_front(&mut self, reason: String) {
        self.0.insert(0, reason);
    }

    fn has(&self, reason: &str) -> bool {
        self.0.iter().any(|r| r == reason)
    }

    fn remove(&mut self, reason: &str) {
        self.0.retain(|r| r != reason);
    }
}

// ---------------------------------------------------------------------------
// Relationships and package structure

struct Rel {
    id: String,
    /// The relationship type URI; matched by suffix so the transitional and
    /// strict (ISO) namespaces both work.
    typ: String,
    /// The target part name, resolved to a package path without leading `/`.
    target: String,
}

fn find_rel<'r>(rels: &'r [Rel], type_suffix: &str) -> Option<&'r Rel> {
    rels.iter().find(|r| r.typ.ends_with(type_suffix))
}

/// `xl/workbook.xml` → `xl/_rels/workbook.xml.rels`.
fn rels_path(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Resolve a relationship target against the part that owns the
/// relationship: absolute targets start at the package root, relative ones
/// at the source part's directory, with `.` and `..` segments applied.
fn resolve(source: &str, target: &str) -> String {
    let mut segs: Vec<&str> = Vec::new();
    let target = match target.strip_prefix('/') {
        Some(abs) => abs,
        None => {
            if let Some((dir, _)) = source.rsplit_once('/') {
                segs.extend(dir.split('/'));
            }
            target
        }
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs.join("/")
}

fn parse_rels(xml: &[u8], source: &str) -> Result<Vec<Rel>, XmlError> {
    let mut r = Reader::new(xml)?;
    let mut rels = Vec::new();
    while let Some(ev) = r.next()? {
        if let Event::Start(t) = ev
            && t.name == "Relationship"
        {
            if t.attr("TargetMode").is_some_and(|m| m == "External") {
                continue;
            }
            let (Some(id), Some(typ), Some(target)) =
                (t.attr("Id"), t.attr("Type"), t.attr("Target"))
            else {
                return Err(r.err("relationship missing Id, Type or Target"));
            };
            rels.push(Rel {
                id: id.into_owned(),
                typ: typ.into_owned(),
                target: resolve(source, &target),
            });
        }
    }
    Ok(rels)
}

/// Without `_rels/.rels`, the content-types list still says which part is
/// the workbook (`…sheet.main+xml`, or the macro-enabled/template variants).
fn workbook_from_content_types(pkg: &Package) -> Result<Option<String>, XlsxError> {
    let Some(xml) = pkg.get("[Content_Types].xml")? else {
        return Ok(None);
    };
    let found = (|| -> Result<Option<String>, XmlError> {
        let mut r = Reader::new(&xml)?;
        while let Some(ev) = r.next()? {
            if let Event::Start(t) = ev
                && t.name == "Override"
                && t.attr("ContentType")
                    .is_some_and(|c| c.ends_with(".main+xml"))
                && let Some(p) = t.attr("PartName")
            {
                return Ok(Some(resolve("", &p)));
            }
        }
        Ok(None)
    })();
    found.map_err(|e| bad("[Content_Types].xml", e))
}

fn parse_application(xml: &[u8]) -> Result<Option<String>, XmlError> {
    let mut r = Reader::new(xml)?;
    while let Some(ev) = r.next()? {
        if let Event::Start(t) = ev
            && t.name == "Application"
        {
            return Ok(Some(r.text()?.trim().to_string()));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Workbook and styles

struct SheetEntry {
    name: String,
    rid: String,
}

struct Workbook {
    date1904: bool,
    sheets: Vec<SheetEntry>,
}

fn is_true(v: &str) -> bool {
    v == "1" || v == "true"
}

fn parse_workbook(xml: &[u8], notes: &mut Notes) -> Result<Workbook, XmlError> {
    let mut r = Reader::new(xml)?;
    let mut wb = Workbook {
        date1904: false,
        sheets: Vec::new(),
    };
    let mut depth = 0;
    while let Some(ev) = r.next()? {
        let t = match ev {
            Event::Start(t) => t,
            Event::End(_) => {
                depth -= 1;
                continue;
            }
            Event::Text(_) => continue,
        };
        depth += 1;
        match (depth, t.name) {
            (2, "workbookPr") => wb.date1904 = t.attr("date1904").is_some_and(|v| is_true(&v)),
            (3, "sheet") => {
                let (Some(name), Some(rid)) = (t.attr("name"), t.attr("id")) else {
                    return Err(r.err("<sheet> missing name or r:id"));
                };
                if t.attr("state").is_some_and(|s| s != "visible") {
                    notes.add("hidden sheets");
                }
                wb.sheets.push(SheetEntry {
                    name: name.into_owned(),
                    rid: rid.into_owned(),
                });
            }
            (2, "definedNames") => notes.add("defined names"),
            (2, "externalReferences") => notes.add("external links"),
            (2, "pivotCaches") => notes.add("pivot tables"),
            (2, "workbookProtection") => notes.add("workbook protection"),
            (2, "customWorkbookViews") => notes.add("custom views"),
            _ => {}
        }
    }
    Ok(wb)
}

/// The cell formats (`cellXfs`), in order: each as an [`XStyle`], or `None`
/// when it holds something an `XStyle` can't (italics, a font size, borders,
/// alignment, a theme color, a named cell style, …) — a cell using one is
/// what gets reported as "cell styles".
fn parse_styles(xml: &[u8]) -> Result<Vec<Option<XStyle>>, XmlError> {
    let tables = StyleTables::collect(xml)?;
    let mut out: Vec<Option<XStyle>> = tables.xfs.iter().map(|x| tables.resolve(x)).collect();
    if out.is_empty() {
        out.push(Some(XStyle::default()));
    }
    Ok(out)
}

/// A `<font>` of the stylesheet, as far as [`XStyle`] cares.
#[derive(Default)]
struct Font {
    bold: bool,
    color: Option<u32>,
    size: Option<String>,
    name: Option<String>,
    /// Anything else: italics, underline, a theme color, ….
    other: bool,
}

/// A `<fill>`.
#[derive(Default)]
struct Fill {
    solid: bool,
    color: Option<u32>,
    /// A pattern or gradient, or a color given other than as RGB.
    other: bool,
}

/// A cell format (`<xf>` of `cellXfs`): indexes into the other tables.
#[derive(Default)]
struct Xf {
    num: u32,
    font: usize,
    fill: usize,
    border: usize,
    /// `<alignment wrapText="1"/>`.
    wrap: bool,
    /// Other alignment, protection, a named style, a quote prefix.
    other: bool,
}

/// The stylesheet's tables, as read.
#[derive(Default)]
struct StyleTables {
    /// Custom number formats by id.
    codes: std::collections::HashMap<u32, String>,
    fonts: Vec<Font>,
    fills: Vec<Fill>,
    /// Per border: whether it draws nothing.
    borders: Vec<bool>,
    xfs: Vec<Xf>,
}

impl StyleTables {
    fn collect(xml: &[u8]) -> Result<StyleTables, XmlError> {
        let mut r = Reader::new(xml)?;
        let mut stack: Vec<&str> = Vec::new();
        let mut tables = StyleTables::default();
        let StyleTables {
            codes,
            fonts,
            fills,
            borders,
            xfs,
        } = &mut tables;
        let flag = |t: &Tag| t.attr("val").is_none_or(|v| is_true(&v));
        while let Some(ev) = r.next()? {
            let t = match ev {
                Event::Start(t) => t,
                Event::End(_) => {
                    stack.pop();
                    continue;
                }
                Event::Text(_) => continue,
            };
            let parent = stack.last().copied();
            let grand = stack.len().checked_sub(2).map(|i| stack[i]);
            match (grand, parent, t.name) {
                (_, Some("numFmts"), "numFmt") => {
                    if let (Some(id), Some(code)) = (t.attr("numFmtId"), t.attr("formatCode"))
                        && let Ok(id) = id.trim().parse()
                    {
                        codes.insert(id, code.into_owned());
                    }
                }
                (_, Some("fonts"), "font") => fonts.push(Font::default()),
                (Some("fonts"), Some("font"), name) => {
                    let Some(f) = fonts.last_mut() else { continue };
                    match name {
                        "b" => f.bold = flag(&t),
                        "sz" => f.size = t.attr("val").map(|v| v.into_owned()),
                        "name" => f.name = t.attr("val").map(|v| v.into_owned()),
                        "color" => match font_color(&t) {
                            Some(c) => f.color = c,
                            None => f.other = true,
                        },
                        "family" | "charset" | "scheme" => {}
                        _ => f.other |= flag(&t),
                    }
                }
                (_, Some("fills"), "fill") => fills.push(Fill::default()),
                (Some("fills"), Some("fill"), kind) => {
                    if let Some(f) = fills.last_mut() {
                        match (kind, t.attr("patternType").as_deref()) {
                            ("patternFill", None | Some("none")) => {}
                            ("patternFill", Some("solid")) => f.solid = true,
                            _ => f.other = true,
                        }
                    }
                }
                // Only a `<fills>` fill: a `<dxf>`'s fill (conditional
                // formatting) has the same shape but isn't in the table.
                (Some("fill"), Some("patternFill"), "fgColor")
                    if stack.len() >= 3 && stack[stack.len() - 3] == "fills" =>
                {
                    if let Some(f) = fills.last_mut() {
                        match t.attr("rgb").and_then(|v| argb(&v)) {
                            Some(c) => f.color = Some(c),
                            None => f.other = true,
                        }
                    }
                }
                (_, Some("borders"), "border") => borders.push(true),
                (Some("borders"), Some("border"), _) => {
                    if t.attr("style").is_some_and(|s| s != "none")
                        && let Some(b) = borders.last_mut()
                    {
                        *b = false;
                    }
                }
                (_, Some("cellXfs"), "xf") => {
                    let id = |name: &str| t.attr(name).and_then(|v| v.trim().parse::<usize>().ok());
                    let other = id("xfId").is_some_and(|i| i != 0)
                        || t.attr("quotePrefix").is_some_and(|v| is_true(&v));
                    xfs.push(Xf {
                        num: id("numFmtId").unwrap_or(0) as u32,
                        font: id("fontId").unwrap_or(0),
                        fill: id("fillId").unwrap_or(0),
                        border: id("borderId").unwrap_or(0),
                        wrap: false,
                        other,
                    });
                }
                (Some("cellXfs"), Some("xf"), name) => {
                    // Alignment other than Wrap Text, or protection other
                    // than Excel's default (locked, not hidden).
                    if let Some(x) = xfs.last_mut() {
                        x.other |= match name {
                            "protection" => {
                                t.attr("locked").is_some_and(|v| !is_true(&v))
                                    || t.attr("hidden").is_some_and(|v| is_true(&v))
                            }
                            "alignment" => {
                                x.wrap = t.attr("wrapText").is_some_and(|v| is_true(&v));
                                t.attrs().any(|(q, _)| q != "wrapText")
                            }
                            _ => t.attrs().next().is_some(),
                        };
                    }
                }
                _ => {}
            }
            // An empty element gets its End event too.
            stack.push(t.name);
        }
        Ok(tables)
    }

    /// One cell format as an [`XStyle`], or `None` if it holds more.
    fn resolve(&self, x: &Xf) -> Option<XStyle> {
        let base = self.fonts.first();
        if x.other || !self.borders.get(x.border).copied().unwrap_or(x.border == 0) {
            return None;
        }
        let (bold, color) = match self.fonts.get(x.font) {
            Some(f) => {
                let same_face = base.is_none_or(|b| b.size == f.size && b.name == f.name);
                if f.other || !same_face {
                    return None;
                }
                (f.bold, f.color)
            }
            None if x.font == 0 => (false, None),
            None => return None,
        };
        let fill = match self.fills.get(x.fill) {
            Some(f) if f.other || (f.solid && f.color.is_none()) => return None,
            Some(f) => f.color.filter(|_| f.solid),
            None if x.fill == 0 => None,
            None => return None,
        };
        let num_fmt = match x.num {
            0 => None,
            id => Some(
                self.codes
                    .get(&id)
                    .cloned()
                    .or_else(|| builtin_format(id).map(String::from))?,
            ),
        };
        Some(XStyle {
            bold,
            color,
            fill,
            num_fmt: num_fmt.filter(|c| !c.eq_ignore_ascii_case("general")),
            wrap: x.wrap,
        })
    }
}

/// A font's `<color>`: `Some(None)` for the default text color (automatic,
/// black, or the theme's text color untinted), `Some(Some(argb))` for an RGB
/// color, `None` for any other theme or indexed color.
fn font_color(t: &Tag) -> Option<Option<u32>> {
    if t.attr("auto").is_some_and(|v| is_true(&v)) {
        return Some(None);
    }
    if let Some(rgb) = t.attr("rgb") {
        return argb(&rgb).map(|c| (c & 0xFF_FFFF != 0).then_some(c));
    }
    let untinted = t
        .attr("tint")
        .is_none_or(|v| v.trim().parse::<f64>() == Ok(0.0));
    match (t.attr("theme").as_deref(), t.attr("indexed").as_deref()) {
        (Some("1"), None) if untinted => Some(None),
        (None, Some("8" | "64")) => Some(None),
        _ => None,
    }
}

/// `"FFFF0000"` (or `"FF0000"`) → `0xFFFF0000`.
fn argb(v: &str) -> Option<u32> {
    let v = v.trim();
    let n = u32::from_str_radix(v, 16).ok()?;
    match v.len() {
        8 => Some(n),
        6 => Some(0xFF00_0000 | n),
        _ => None,
    }
}

/// The code of a built-in number format (`numFmtId` below 164 that a file
/// uses without defining), as Excel shows it in an en-US locale. `None` for
/// the locale-specific ids.
fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "m/d/yyyy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yyyy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Strings

/// The text of an `<si>` or `<is>` whose start tag was just read: its `<t>`,
/// or its rich-text runs' `<t>`s joined. Phonetic runs (`<rPh>`) are
/// annotations, not text, and are skipped.
fn rich_text(r: &mut Reader, notes: &mut Notes) -> Result<String, XmlError> {
    let mut out = String::new();
    // Elements open inside it (a run, `<r>`, is read through), counted
    // rather than recursed into so deep nesting can't overflow the stack.
    let mut depth = 1;
    while depth > 0 {
        let Some(t) = r.next_child()? else {
            depth -= 1;
            continue;
        };
        match t.name {
            "t" => out.push_str(&t_text(r, &t)?),
            "rPh" | "phoneticPr" => r.skip()?,
            // A run's font: the text is kept, its formatting isn't.
            "rPr" => {
                notes.add("rich text formatting");
                r.skip()?
            }
            _ => depth += 1,
        }
    }
    Ok(out)
}

/// One `<t>`: XML whitespace at the ends is dropped unless the element says
/// `xml:space="preserve"` (as Excel itself reads it), then `_xHHHH_` escapes
/// are decoded.
fn t_text(r: &mut Reader, t: &Tag) -> Result<String, XmlError> {
    let preserve = t.qattr("xml:space").is_some_and(|v| v == "preserve");
    let text = r.text()?;
    let text = if preserve {
        &text[..]
    } else {
        text.trim_matches([' ', '\t', '\n', '\r'])
    };
    Ok(decode_xstring(text).into_owned())
}

fn parse_shared_strings(xml: &[u8], notes: &mut Notes) -> Result<Vec<String>, XmlError> {
    let mut r = Reader::new(xml)?;
    let mut out = Vec::new();
    while let Some(ev) = r.next()? {
        if let Event::Start(t) = ev
            && t.name == "si"
        {
            out.push(rich_text(&mut r, notes)?);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Worksheets

/// What a worksheet's top-level element means for a rewrite: `None` for
/// what we write or can safely drop.
fn sheet_element_reason(name: &str) -> Option<&'static str> {
    Some(match name {
        "sheetPr" | "dimension" | "sheetViews" | "sheetFormatPr" | "sheetData" | "pageMargins"
        | "sheetCalcPr" | "phoneticPr" | "ignoredErrors" => return None,
        "cols" => "column widths",
        "mergeCells" => "merged cells",
        "conditionalFormatting" => "conditional formatting",
        "dataValidations" => "data validation",
        "hyperlinks" => "hyperlinks",
        "drawing" => "drawings",
        "legacyDrawing" | "legacyDrawingHF" => "legacy drawings",
        "tableParts" => "tables",
        "autoFilter" | "sortState" => "filters",
        "sheetProtection" | "protectedRanges" => "sheet protection",
        "printOptions" | "pageSetup" | "headerFooter" | "rowBreaks" | "colBreaks" => {
            "print settings"
        }
        "picture" => "background image",
        "oleObjects" => "embedded objects",
        "controls" => "form controls",
        "scenarios" => "scenarios",
        "dataConsolidate" => "data consolidation",
        "customSheetViews" => "custom views",
        "extLst" => "Excel extensions (extLst)",
        _ => "other sheet features",
    })
}

/// Parse a worksheet's cells, noting features a rewrite would lose.
#[cfg(test)]
fn parse_sheet(
    xml: &[u8],
    shared: &[String],
    styles: &[Option<XStyle>],
    notes: &mut Notes,
) -> Result<Vec<XCell>, XmlError> {
    parse_sheet_rows(xml, shared, styles, notes).map(|(cells, _)| cells)
}

/// A worksheet's cells and the rows sized by hand, noting features a
/// rewrite would lose.
fn parse_sheet_rows(
    xml: &[u8],
    shared: &[String],
    styles: &[Option<XStyle>],
    notes: &mut Notes,
) -> Result<(Vec<XCell>, Vec<XRow>), XmlError> {
    let mut r = Reader::new(xml)?;
    let mut cells = Vec::new();
    let mut rows = Vec::new();
    let mut depth = 0;
    while let Some(ev) = r.next()? {
        let t = match ev {
            Event::Start(t) => t,
            Event::End(_) => {
                depth -= 1;
                continue;
            }
            Event::Text(_) => continue,
        };
        if depth == 0 {
            depth = 1; // <worksheet>
            continue;
        }
        if let Some(reason) = sheet_element_reason(t.name) {
            notes.add(reason);
        }
        match t.name {
            "sheetData" => sheet_data(&mut r, shared, styles, notes, &mut cells, &mut rows)?,
            "sheetPr" | "sheetViews" => view_state(&mut r, notes)?,
            _ => r.skip()?,
        }
    }
    cells.sort_by_key(|c| (c.row, c.col));
    rows.sort_by_key(|r| r.row);
    rows.dedup_by_key(|r| r.row);
    Ok((cells, rows))
}

/// The rest of a `<sheetPr>` or `<sheetViews>`: mostly view state, except a
/// tab color and frozen panes (at any depth).
fn view_state(r: &mut Reader, notes: &mut Notes) -> Result<(), XmlError> {
    let mut depth = 1;
    while depth > 0 {
        let Some(c) = r.next_child()? else {
            depth -= 1;
            continue;
        };
        match c.name {
            "tabColor" => notes.add("tab color"),
            "pane" => notes.add("frozen panes"),
            _ => {}
        }
        depth += 1;
    }
    Ok(())
}

fn sheet_data(
    r: &mut Reader,
    shared: &[String],
    styles: &[Option<XStyle>],
    notes: &mut Notes,
    cells: &mut Vec<XCell>,
    rows: &mut Vec<XRow>,
) -> Result<(), XmlError> {
    // Rows and cells may omit `r`; each then follows the previous one.
    let mut next_row = 0usize;
    while let Some(t) = r.next_child()? {
        if t.name != "row" {
            r.skip()?;
            continue;
        }
        let row = match t.attr("r") {
            Some(v) => match v.trim().parse::<usize>() {
                Ok(n) if (1..=MAX_ROWS).contains(&n) => n - 1,
                _ => return Err(r.err("invalid row number")),
            },
            None => next_row,
        };
        if row >= MAX_ROWS {
            return Err(r.err("too many rows"));
        }
        next_row = row + 1;
        if t.attr("customHeight").is_some_and(|v| is_true(&v)) {
            match t.attr("ht").and_then(|v| v.trim().parse::<f64>().ok()) {
                Some(height) if height.is_finite() && height >= 0.0 => rows.push(XRow {
                    row,
                    height,
                    custom: true,
                }),
                _ => notes.add("row heights"),
            }
        }
        if t.attr("hidden").is_some_and(|v| is_true(&v)) {
            notes.add("hidden rows");
        }
        if t.attr("outlineLevel").is_some_and(|v| v != "0") {
            notes.add("outline groups");
        }
        if t.attr("customFormat").is_some_and(|v| is_true(&v)) {
            notes.add("cell styles");
        }
        let mut next_col = 0usize;
        while let Some(c) = r.next_child()? {
            if c.name != "c" {
                r.skip()?;
                continue;
            }
            let (crow, col) = match c.attr("r") {
                Some(v) => parse_ref(&v).ok_or_else(|| r.err("invalid cell reference"))?,
                None => (row, next_col),
            };
            if col >= super::MAX_COLS {
                return Err(r.err("too many columns"));
            }
            next_col = col + 1;
            if let Some(cell) = cell(r, &c, crow, col, shared, styles, notes)? {
                cells.push(cell);
            }
        }
    }
    Ok(())
}

/// One `<c>` whose start tag was just read. Cells with no value, formula
/// or formatting yield `None`.
fn cell(
    r: &mut Reader,
    c: &Tag,
    row: usize,
    col: usize,
    shared: &[String],
    styles: &[Option<XStyle>],
    notes: &mut Notes,
) -> Result<Option<XCell>, XmlError> {
    let typ = c.attr("t");
    let style = match c.attr("s").map(|v| v.trim().parse::<usize>()) {
        None | Some(Ok(0)) => 0,
        Some(Ok(i)) => match styles.get(i) {
            Some(Some(st)) if st.is_default() => 0,
            Some(Some(_)) => i,
            _ => {
                notes.add("cell styles");
                0
            }
        },
        Some(Err(_)) => return Err(r.err("invalid style index")),
    };
    let mut v: Option<String> = None;
    let mut inline: Option<String> = None;
    let mut formula = None;
    while let Some(t) = r.next_child()? {
        match t.name {
            "v" => v = Some(r.text()?),
            "is" => inline = Some(rich_text(r, notes)?),
            "f" => formula = formula_of(r, &t, row, col, notes)?,
            _ => r.skip()?,
        }
    }
    let value = match typ.as_deref().unwrap_or("n") {
        "n" => match v.as_deref().map(str::trim) {
            None | Some("") => XValue::Empty,
            Some(s) => XValue::Number(parse_number(s).ok_or_else(|| r.err("invalid number"))?),
        },
        "s" => {
            let idx = v
                .as_deref()
                .map(str::trim)
                .and_then(|s| s.parse::<usize>().ok());
            let s = idx
                .and_then(|i| shared.get(i))
                .ok_or_else(|| r.err("invalid shared string index"))?;
            XValue::Text(s.clone())
        }
        "str" => v.map_or(XValue::Empty, |s| {
            XValue::Text(decode_xstring(&s).into_owned())
        }),
        "inlineStr" => match (inline, v) {
            (Some(s), _) => XValue::Text(s),
            (None, Some(s)) => XValue::Text(decode_xstring(&s).into_owned()),
            (None, None) => XValue::Empty,
        },
        "b" => match v.as_deref().map(str::trim) {
            None | Some("") => XValue::Empty,
            Some("1" | "true") => XValue::Bool(true),
            Some("0" | "false") => XValue::Bool(false),
            Some(_) => return Err(r.err("invalid boolean")),
        },
        "e" => v.map_or(XValue::Empty, |s| XValue::Error(s.trim().to_string())),
        // ISO 8601 date text; the caller decides whether to interpret it.
        "d" => v.map_or(XValue::Empty, |s| XValue::Text(s.trim().to_string())),
        _ => return Err(r.err("unknown cell type")),
    };
    if value == XValue::Empty && formula.is_none() && style == 0 {
        return Ok(None);
    }
    Ok(Some(XCell {
        row,
        col,
        value,
        formula,
        style,
    }))
}

fn formula_of(
    r: &mut Reader,
    f: &Tag,
    row: usize,
    col: usize,
    notes: &mut Notes,
) -> Result<Option<XFormula>, XmlError> {
    let text = decode_xstring(&r.text()?).into_owned();
    let own_ref = || super::cell_name(row, col);
    let range = f.attr("ref").map_or_else(own_ref, |s| s.into_owned());
    Ok(match f.attr("t").as_deref().unwrap_or("normal") {
        "normal" if text.is_empty() => None,
        "normal" => Some(XFormula::Normal(text)),
        "shared" => {
            let si = f.attr("si").and_then(|s| s.trim().parse::<u32>().ok());
            let si = si.ok_or_else(|| r.err("shared formula without a valid si"))?;
            if text.is_empty() {
                Some(XFormula::SharedChild { si })
            } else {
                Some(XFormula::SharedMaster { si, text, range })
            }
        }
        "array" => {
            // Written back as a plain formula: Excel would read it with
            // implicit intersection, and a multi-cell one's other cells as
            // constants.
            notes.add("array formulas");
            Some(XFormula::Array { text, range })
        }
        "dataTable" => {
            notes.add("data tables");
            None
        }
        _ => return Err(r.err("unknown formula type")),
    })
}

/// A number as SpreadsheetML writes one (xsd:double without INF/NaN):
/// `[+-]digits[.digits][(e|E)[+-]digits]`. Rust's parser alone would also
/// take "inf", "NaN" and "infinity".
fn parse_number(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut mantissa = digits(&mut i);
    if b.get(i) == Some(&b'.') {
        i += 1;
        mantissa += digits(&mut i);
    }
    if mantissa == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    s.parse().ok().filter(|f: &f64| f.is_finite())
}

/// Why a package part would be lost on a rewrite, or `None` if it's one we
/// write, read, or can drop without losing anything the user made.
fn classify_part(name: &str) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let prefix = |p: &str| n.starts_with(p);
    let reason = match () {
        _ if n.ends_with('/') => return None, // directory entries
        _ if matches!(
            n.as_str(),
            "[content_types].xml"
                | "_rels/.rels"
                | "docprops/app.xml"
                | "docprops/core.xml"
                | "xl/workbook.xml"
                | "xl/_rels/workbook.xml.rels"
                | "xl/styles.xml"
                | "xl/sharedstrings.xml"
                | "xl/calcchain.xml"
                | "xl/volatiledependencies.xml"
        ) =>
        {
            return None;
        }
        _ if prefix("docprops/thumbnail") || prefix("xl/theme/") || prefix("xl/persons/") => {
            return None;
        }
        _ if prefix("xl/worksheets/") => return None,
        _ if n == "docprops/custom.xml" => "custom document properties",
        _ if prefix("xl/vbaproject") => "VBA macros",
        _ if prefix("xl/comments") || prefix("xl/threadedcomments/") => "comments",
        _ if prefix("xl/drawings/vmldrawing") || prefix("xl/drawings/_rels/vmldrawing") => {
            "legacy drawings"
        }
        _ if prefix("xl/drawings/") => "drawings",
        _ if prefix("xl/charts/") => "charts",
        _ if prefix("xl/chartsheets/") => "chart sheets",
        _ if prefix("xl/dialogsheets/") => "dialog sheets",
        _ if prefix("xl/macrosheets/") => "macro sheets",
        _ if prefix("xl/media/") => "images",
        _ if prefix("xl/pivottables/") || prefix("xl/pivotcache/") => "pivot tables",
        _ if prefix("xl/tables/") => "tables",
        _ if prefix("xl/querytables/") => "query tables",
        _ if n == "xl/connections.xml" => "data connections",
        _ if prefix("xl/externallinks/") => "external links",
        _ if prefix("xl/slicers/") || prefix("xl/slicercaches/") => "slicers",
        _ if prefix("xl/timelines/") || prefix("xl/timelinecaches/") => "timelines",
        _ if prefix("xl/richdata/") => "pictures in cells",
        _ if n == "xl/metadata.xml" => "dynamic array metadata",
        _ if prefix("xl/ctrlprops/") => "form controls",
        _ if prefix("xl/activex/") => "ActiveX controls",
        _ if prefix("xl/embeddings/") => "embedded objects",
        _ if prefix("xl/printersettings/") => "print settings",
        _ if prefix("xl/model/") => "data model",
        _ if prefix("xl/revisions/") || n == "xl/usernames.xml" => "shared workbook revisions",
        _ if prefix("customxml/") => "custom XML parts",
        _ => return Some(format!("unknown part {name}")),
    };
    Some(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::{Book, XlsxError};
    use super::*;

    fn fixture(name: &str) -> Book {
        let bytes: &[u8] = match name {
            "basic" => include_bytes!("testdata/pkg/basic.xlsx"),
            "multi" => include_bytes!("testdata/pkg/multi.xlsx"),
            "features" => include_bytes!("testdata/pkg/features.xlsx"),
            "numfmt" => include_bytes!("testdata/pkg/numfmt.xlsx"),
            "norels" => include_bytes!("testdata/pkg/norels.xlsx"),
            other => panic!("unknown fixture {other}"),
        };
        read(bytes).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn at(sheet: &XSheet, a1: &str) -> XCell {
        let (row, col) = parse_ref(a1).unwrap();
        sheet
            .cells
            .iter()
            .find(|c| c.row == row && c.col == col)
            .unwrap_or_else(|| panic!("no cell {a1}"))
            .clone()
    }

    fn text(s: &str) -> XValue {
        XValue::Text(s.into())
    }

    #[test]
    fn basic_package() {
        let book = fixture("basic");
        assert!(book.date1904);
        assert_eq!(book.application.as_deref(), Some("Microsoft Excel"));
        // Its bold/italic runs and its array formula can't be written back.
        assert_eq!(
            book.not_preserved,
            ["rich text formatting", "array formulas"]
        );
        assert_eq!(book.sheets.len(), 1);
        let s = &book.sheets[0];
        assert_eq!(s.name, "Data & <Stuff>");

        // Shared strings: plain, rich runs joined, phonetic run skipped,
        // xml:space honored, _xHHHH_ decoded.
        assert_eq!(at(s, "A1").value, text("plain"));
        assert_eq!(at(s, "B1").value, text("bold and italic"));
        assert_eq!(at(s, "C1").value, text("  padded  "));
        assert_eq!(at(s, "D1").value, text("trimmed"));
        assert_eq!(at(s, "E1").value, text("tab\there\r_x0041_"));
        assert_eq!(at(s, "F1").value, text("漢字"));

        assert_eq!(at(s, "A2").value, XValue::Number(1.5));
        assert_eq!(at(s, "B2").value, XValue::Number(-2e-300));
        assert_eq!(at(s, "C2").value, XValue::Bool(true));
        assert_eq!(at(s, "D2").value, XValue::Bool(false));
        assert_eq!(at(s, "E2").value, XValue::Error("#DIV/0!".into()));
        assert_eq!(at(s, "F2").value, text("2024-03-01T00:00:00"));
        assert_eq!(at(s, "G2").value, text(" inline\nrich"));

        // Formulas.
        let a3 = at(s, "A3");
        assert_eq!(a3.formula, Some(XFormula::Normal("SUM(A2:B2)".into())));
        assert_eq!(a3.value, XValue::Number(1.5));
        assert_eq!(
            at(s, "B3").formula,
            Some(XFormula::Normal("A1&\"<x>\"".into()))
        );
        assert_eq!(at(s, "B3").value, text("plain<x>"));
        assert_eq!(
            at(s, "C3").formula,
            Some(XFormula::SharedMaster {
                si: 0,
                text: "A2*2".into(),
                range: "C3:C5".into()
            })
        );
        assert_eq!(at(s, "C4").formula, Some(XFormula::SharedChild { si: 0 }));
        assert_eq!(at(s, "C4").value, XValue::Number(6.0));
        assert_eq!(at(s, "C5").formula, Some(XFormula::SharedChild { si: 0 }));
        assert_eq!(
            at(s, "D3").formula,
            Some(XFormula::Array {
                text: "SUM(A2:B2*2)".into(),
                range: "D3".into()
            })
        );
        // A formula with no cached value.
        assert_eq!(at(s, "E3").formula, Some(XFormula::Normal("NOW()".into())));
        assert_eq!(at(s, "E3").value, XValue::Empty);

        // Missing `r` on rows and cells: row 7 follows row 6, cells follow.
        assert_eq!(at(s, "A7").value, XValue::Number(10.0));
        assert_eq!(at(s, "B7").value, XValue::Number(11.0));
        assert_eq!(at(s, "D7").value, XValue::Number(13.0));
        assert_eq!(at(s, "E7").value, XValue::Number(14.0));
        assert_eq!(at(s, "A8").value, XValue::Number(20.0));

        // Style-only cells are dropped; output is row-major.
        let pos: Vec<(usize, usize)> = s.cells.iter().map(|c| (c.row, c.col)).collect();
        let mut sorted = pos.clone();
        sorted.sort();
        assert_eq!(pos, sorted);
        assert!(!s.cells.iter().any(|c| (c.row, c.col) == (5, 0)));
    }

    #[test]
    fn multiple_sheets_in_workbook_order() {
        let book = fixture("multi");
        let names: Vec<&str> = book.sheets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["First", "Second", "Third"]);
        assert_eq!(book.sheets[0].cells[0].value, text("one"));
        assert_eq!(book.sheets[1].cells[0].value, text("two"));
        assert_eq!(book.sheets[2].cells[0].value, text("three"));
        assert!(!book.date1904);
        assert_eq!(
            book.not_preserved,
            ["3 sheets", "hidden sheets", "defined names"]
        );
    }

    #[test]
    fn features_reported() {
        let book = fixture("features");
        let mut got = book.not_preserved.clone();
        got.sort();
        // A1's date format is a number format, which is kept.
        let a1 = &book.sheets[0].cells[0];
        assert_eq!(
            book.styles[a1.style].num_fmt.as_deref(),
            Some("m/d/yyyy"),
            "built-in date format"
        );
        let mut want = vec![
            "column widths",
            "merged cells",
            "conditional formatting",
            "data validation",
            "hyperlinks",
            "tables",
            "comments",
            "charts",
            "images",
            "VBA macros",
            "frozen panes",
            "pivot tables",
            "unknown part xl/foo.xml",
        ];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(book.sheets[0].cells.len(), 2);
        // A row height set by hand is kept.
        assert_eq!(
            book.sheets[0].rows,
            [XRow {
                row: 0,
                height: 30.0,
                custom: true
            }]
        );
    }

    #[test]
    fn unused_custom_number_format_is_not_a_loss() {
        // Defined but on no cell: a rewrite loses nothing anyone sees.
        assert!(fixture("numfmt").not_preserved.is_empty());
    }

    #[test]
    fn workbook_found_without_root_rels() {
        let book = fixture("norels");
        assert_eq!(book.sheets[0].name, "Only");
        assert_eq!(book.sheets[0].cells[0].value, XValue::Number(42.0));
    }

    #[test]
    fn relationship_targets_resolve() {
        assert_eq!(
            resolve("xl/workbook.xml", "worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(
            resolve("xl/workbook.xml", "/xl/styles.xml"),
            "xl/styles.xml"
        );
        assert_eq!(
            resolve("xl/worksheets/sheet1.xml", "../drawings/d.xml"),
            "xl/drawings/d.xml"
        );
        assert_eq!(resolve("", "xl/workbook.xml"), "xl/workbook.xml");
        assert_eq!(resolve("", "./xl/./workbook.xml"), "xl/workbook.xml");
        assert_eq!(rels_path("xl/workbook.xml"), "xl/_rels/workbook.xml.rels");
    }

    #[test]
    fn number_tokens() {
        for (s, v) in [
            ("1", 1.0),
            ("-0.5", -0.5),
            ("+3", 3.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1E+3", 1000.0),
            ("2e-3", 0.002),
        ] {
            assert_eq!(parse_number(s), Some(v), "{s}");
        }
        for s in [
            "", "inf", "NaN", "infinity", "1e", "e5", ".", "1.2.3", "0x10", "1e999", " 1", "1_000",
        ] {
            assert_eq!(parse_number(s), None, "{s}");
        }
    }

    #[test]
    fn sheet_errors_are_reported_not_panics() {
        let shared = vec!["s".to_string()];
        let bad = [
            r#"<worksheet><sheetData><row r="0"/></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row><c r="A0"/></row></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row><c t="s"><v>1</v></c></row></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row><c><v>abc</v></c></row></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row><c t="b"><v>2</v></c></row></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row><c t="zz"><v>1</v></c></row></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row><c><f t="shared">A1</f></c></row></sheetData></worksheet>"#,
            r#"<worksheet><sheetData><row r="1048577"/></sheetData></worksheet>"#,
        ];
        for xml in bad {
            assert!(
                parse_sheet(xml.as_bytes(), &shared, &[], &mut Notes::default()).is_err(),
                "{xml}"
            );
        }
    }

    #[test]
    fn mutated_sheet_xml_never_panics() {
        let xml = br#"<worksheet xmlns="m"><sheetViews><sheetView><pane/></sheetView></sheetViews><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c t="inlineStr"><is><r><t xml:space="preserve"> x_x0041_</t></r></is></c><c r="C1"><f t="shared" si="0" ref="C1:C3">A1+1</f><v>2</v></c></row><row><c><f t="array" ref="A2">1</f><v>1e5</v></c><c t="b"><v>1</v></c><c t="e"><v>#N/A</v></c></row></sheetData><mergeCells><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#;
        let shared = vec!["s".to_string()];
        let ok = parse_sheet(xml, &shared, &[], &mut Notes::default()).unwrap();
        assert_eq!(ok.len(), 6);
        let alphabet = b"<>/=\"'&;#x_01AZ ";
        let mut rng = 0xC0FFEEu64;
        for _ in 0..5000 {
            let mut d = xml.to_vec();
            for _ in 0..2 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let i = (rng % d.len() as u64) as usize;
                d[i] = alphabet[(rng >> 35) as usize % alphabet.len()];
            }
            let _ = parse_sheet(&d, &shared, &[], &mut Notes::default());
        }
    }

    #[test]
    fn mutated_packages_never_panic() {
        let base: &[u8] = include_bytes!("testdata/pkg/basic.xlsx");
        let stored: &[u8] = include_bytes!("testdata/pkg/basic_stored.xlsx");
        let mut rng = 0xBADC0DEu64;
        for data in [base, stored] {
            for _ in 0..1500 {
                let mut d = data.to_vec();
                for _ in 0..1 + rng as usize % 4 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let i = (rng % d.len() as u64) as usize;
                    d[i] = (rng >> 24) as u8;
                }
                let _ = read(&d);
            }
            for cut in (0..data.len()).step_by(7) {
                let _ = read(&data[..cut]);
            }
        }
    }

    #[test]
    fn non_packages() {
        assert_eq!(read(b"a,b\n1,2\n"), Err(XlsxError::NotZip));
        let mut ole = super::super::zip::OLE_MAGIC.to_vec();
        ole.resize(4096, 0);
        assert_eq!(read(&ole), Err(XlsxError::Encrypted));
        // A ZIP that isn't a workbook.
        let mut w = super::super::zip::Writer::new();
        w.add("readme.txt", b"hi");
        assert!(matches!(read(&w.finish()), Err(XlsxError::Corrupt(_))));
        let mut w = super::super::zip::Writer::new();
        w.add("xl/workbook.bin", b"\0");
        assert!(matches!(read(&w.finish()), Err(XlsxError::Unsupported(_))));
    }

    #[test]
    fn array_formulas_are_reported_lossy() {
        let mut notes = Notes::default();
        let xml = br#"<worksheet><sheetData><row r="1"><c r="A1"><f t="array" ref="A1:A3">B1:B3*2</f><v>2</v></c></row><row r="2"><c r="A2"><v>4</v></c></row><row r="3"><c r="A3"><v>6</v></c></row></sheetData></worksheet>"#;
        let cells = parse_sheet(xml, &[], &[], &mut notes).unwrap();
        assert_eq!(cells.len(), 3);
        assert!(!notes.0.is_empty(), "multi-cell array formula not reported");
    }

    #[test]
    fn rich_text_formatting_is_reported_lossy() {
        let book = fixture("basic");
        assert_eq!(at(&book.sheets[0], "B1").value, text("bold and italic"));
        assert!(
            !book.not_preserved.is_empty(),
            "bold/italic runs dropped silently"
        );
    }

    #[test]
    fn dxf_fills_do_not_touch_cell_fills() {
        let styles = br#"<styleSheet>
            <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
            <fills count="3"><fill><patternFill patternType="none"/></fill>
              <fill><patternFill patternType="gray125"/></fill>
              <fill><patternFill patternType="solid"><fgColor rgb="FFFFC000"/></patternFill></fill></fills>
            <borders count="1"><border/></borders>
            <cellXfs count="2">
              <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
              <xf numFmtId="0" fontId="0" fillId="2" borderId="0" xfId="0"/>
            </cellXfs>
            <dxfs count="2">
              <dxf><fill><patternFill><fgColor rgb="FF00FF00"/></patternFill></fill></dxf>
              <dxf><fill><patternFill><fgColor theme="4"/></patternFill></fill></dxf>
            </dxfs></styleSheet>"#;
        let styles = parse_styles(styles).unwrap();
        assert_eq!(
            styles[1],
            Some(XStyle {
                fill: Some(0xFFFFC000),
                ..XStyle::default()
            })
        );
    }

    #[test]
    fn styles_beyond_bold_colors_and_number_formats_are_reported() {
        let sheet = |s: usize| {
            format!(
                r#"<worksheet><sheetData><row r="1"><c r="A1" s="{s}"><v>1</v></c></row></sheetData></worksheet>"#
            )
        };
        let styles = br#"<styleSheet>
            <numFmts count="1"><numFmt numFmtId="164" formatCode="0.0%"/></numFmts>
            <fonts count="4">
              <font><sz val="11"/><color theme="1"/><name val="Calibri"/></font>
              <font><b/><sz val="11"/><color rgb="FF0070C0"/><name val="Calibri"/></font>
              <font><i/><sz val="11"/><name val="Calibri"/></font>
              <font><sz val="14"/><name val="Calibri"/></font>
            </fonts>
            <fills count="3"><fill><patternFill patternType="none"/></fill>
              <fill><patternFill patternType="gray125"/></fill>
              <fill><patternFill patternType="solid"><fgColor rgb="FFFFC000"/></patternFill></fill></fills>
            <borders count="2"><border><left/><right/></border>
              <border><left style="thin"/><right/></border></borders>
            <cellXfs count="7">
              <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>
              <xf numFmtId="164" fontId="1" fillId="2" borderId="0" xfId="0"/>
              <xf numFmtId="0" fontId="2" fillId="0" borderId="0" xfId="0"/>
              <xf numFmtId="0" fontId="3" fillId="0" borderId="0" xfId="0"/>
              <xf numFmtId="0" fontId="0" fillId="0" borderId="1" xfId="0"/>
              <xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"><alignment horizontal="center"/></xf>
              <xf numFmtId="0" fontId="0" fillId="1" borderId="0" xfId="0"/>
            </cellXfs></styleSheet>"#;
        let styles = parse_styles(styles).unwrap();
        assert_eq!(
            styles[1],
            Some(XStyle {
                bold: true,
                color: Some(0xFF0070C0),
                fill: Some(0xFFFFC000),
                num_fmt: Some("0.0%".into()),
                wrap: false,
            })
        );
        assert_eq!(
            styles[0],
            Some(XStyle::default()),
            "theme text color is the default"
        );
        for (i, what) in [
            (2, "italics"),
            (3, "font size"),
            (4, "border"),
            (5, "alignment"),
            (6, "pattern"),
        ] {
            assert_eq!(styles[i], None, "{what}");
            let mut notes = Notes::default();
            let cells = parse_sheet(sheet(i).as_bytes(), &[], &styles, &mut notes).unwrap();
            assert_eq!(notes.0, ["cell styles"], "{what}");
            assert_eq!(cells[0].style, 0);
        }
        let mut notes = Notes::default();
        let cells = parse_sheet(sheet(1).as_bytes(), &[], &styles, &mut notes).unwrap();
        assert!(notes.0.is_empty());
        assert_eq!(cells[0].style, 1);
    }
}
