//! `GridElement`: paints one pane showing a grid buffer — formula bar,
//! column/row headers, gridlines, cell text, selection, the cursor cell, and
//! the inline cell editor. Only cells on screen are touched, so the cost of a
//! frame is independent of the file's size.
//!
//! GPUI's line-layout cache already reuses a shaped line whose text, font and
//! size match one from the previous frame, so repeated values (a `status`
//! column, scrolling a few rows) don't re-shape; the element just keeps every
//! string it shapes short (truncated to what the cell can show).

use gpui::{
    App, Bounds, ContentMask, CursorStyle, Element, ElementId, ElementInputHandler, Entity, Font,
    FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, LayoutId,
    PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun, Window, fill, font, point,
    prelude::*, px, relative, rgb, rgba, size,
};

use super::Editor;
use super::grid::{BAR_HEIGHT, GridDoc, GridDrag, GridGeom, NAME_BOX_WIDTH, SHEET};
use crate::formula::refs::Axis;
use crate::grid::cellfmt::{self, CellFmt};
use crate::grid::view::{self, CellEdit, CellRange, VisibleLine, col_name};
use crate::theme::Theme;

/// Horizontal padding inside a cell, unzoomed px.
const CELL_PAD: f32 = 3.0;
/// Thickness of the cursor-cell / cell-editor outline, screen px. It is drawn
/// over the cell's gridlines (not inside the cell), so it reads as a crisp
/// frame without eating into the text.
const CURSOR_BORDER: f32 = 1.0;

pub struct GridElement {
    pub editor: Entity<Editor>,
    pub pane: usize,
}

/// A shaped string and where to draw it, clipped to `clip`.
struct Text {
    line: ShapedLine,
    origin: Point<Pixels>,
    clip: Bounds<Pixels>,
    line_height: Pixels,
}

pub struct GridPrepaint {
    /// Fills drawn under everything in the cells area (selection, blocks).
    under: Vec<PaintQuad>,
    /// Gridlines.
    lines: Vec<PaintQuad>,
    /// Cell text, clipped per cell (scrolled rows).
    cells: Vec<Text>,
    /// Frozen rows: their fills and text, drawn over the scrolled rows.
    frozen_under: Vec<PaintQuad>,
    frozen_cells: Vec<Text>,
    /// Headers, formula bar, cursor, editor, guides: drawn last.
    over: Vec<PaintQuad>,
    over_text: Vec<Text>,
    /// Cursor / editor / range outlines: painted after the headers, so an
    /// outline on row 1 or column A keeps its top/left edge (which sits on
    /// the header separator line).
    outlines: Vec<PaintQuad>,
    /// Where outlines may draw: the cells area plus the 1px line above and
    /// left of it, so they never spill over the header labels.
    outline_bounds: Bounds<Pixels>,
    cells_bounds: Bounds<Pixels>,
    scroll_bounds: Bounds<Pixels>,
    hitboxes: Vec<(Hitbox, CursorStyle)>,
    /// The function-name dropdown while typing a formula (the editor's
    /// LSP completion popup, reused).
    popup: Option<crate::element::PopupLayout>,
    background: PaintQuad,
}

impl IntoElement for GridElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

/// Theme colors the grid uses, resolved once per frame.
struct Palette {
    /// Colours for the references of a formula being edited, in order
    /// (each reference and the cells it covers share one), as Excel does.
    refs: Vec<Hsla>,
    /// Outline of `grid-trace-precedents` / `-dependents`.
    trace: Hsla,
    bg: Hsla,
    fg: Hsla,
    dim: Hsla,
    line: Hsla,
    header_bg: Hsla,
    header_active: Hsla,
    /// Column letters / row numbers: quiet, so the data reads first.
    header_label: Hsla,
    /// The active row/column's label: a step up from `header_label`.
    header_label_active: Hsla,
    selection: Hsla,
    cursor: Hsla,
    block: Hsla,
    warn: Hsla,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let bg = theme.background();
        let fg = theme.foreground();
        let dim = theme.color("dim", Hsla { a: 0.55, ..fg });
        let mut line = theme.color("grid-line", fg);
        if theme.background_of("grid-line").is_none() {
            line.a = 0.14;
        }
        // Headers sit on the plain background unless a theme sets the
        // `grid-header` face: they're reference marks, not content.
        let mut header_bg = theme.color("grid-header", fg);
        if theme.background_of("grid-header").is_none() {
            header_bg.a = 0.0;
        }
        let selection = theme.color("region", rgba(0x3a5fcd55).into());
        let cursor = theme.color("cursor", rgb(0x528bff).into());
        let header_active = Hsla { a: 0.07, ..fg };
        let refs: Vec<Hsla> = [0x4f8bf5, 0xe0533c, 0x9b59d0, 0x2fa36b, 0xd4882a, 0xd14b9b]
            .iter()
            .enumerate()
            .map(|(i, &fallback)| theme.color(&format!("plot-series-{i}"), rgb(fallback).into()))
            .collect();
        let trace = theme.color("warning", rgb(0xe5c07b).into());
        Self {
            refs,
            trace,
            bg,
            fg,
            dim,
            line,
            header_bg,
            header_active,
            header_label: Hsla { a: 0.30, ..fg },
            header_label_active: Hsla { a: 0.70, ..fg },
            selection,
            cursor,
            block: Hsla { a: 0.45, ..fg },
            warn: theme.color("warning", rgb(0xe5c07b).into()),
        }
    }
}

/// Default colors of the hand-formatting palette ([`cellfmt::COLORS`]):
/// mid tones that read on light and dark backgrounds. A theme overrides one
/// with the face `grid-<name>` (its background, if set, is the fill).
const PALETTE_DEFAULTS: [&str; 6] = [
    "#e5484d", "#f76b15", "#d6a400", "#30a46c", "#3e8ef7", "#8e4ec6",
];

fn palette_color(theme: &Theme, i: u8) -> Hsla {
    let fallback = PALETTE_DEFAULTS
        .get(i as usize)
        .and_then(|hex| crate::theme::parse_color(hex))
        .unwrap_or_default();
    theme.color(&format!("grid-{}", cellfmt::color_name(i)), fallback)
}

/// A fill: the face's background when the theme sets one, else the color
/// as a wash light enough to read text over.
fn palette_fill(theme: &Theme, i: u8) -> Hsla {
    theme
        .background_of(&format!("grid-{}", cellfmt::color_name(i)))
        .unwrap_or_else(|| Hsla {
            a: 0.4,
            ..palette_color(theme, i)
        })
}

fn run(len: usize, font: &Font, color: Hsla) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// The first `max_chars` chars of `s`'s first line, with a `↵` marking any
/// further lines (cells keep a uniform row height; the bar shows it all).
fn display_text(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut more_lines = false;
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            more_lines = true;
            break;
        }
        out.extend(line.trim_end_matches('\r').chars().take(max_chars));
    }
    if more_lines {
        out.push_str(" ↵");
    }
    out
}

impl Element for GridElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<GridPrepaint>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let theme = cx.global::<Theme>();
        let settings = cx.global::<crate::settings::Settings>();
        let popup = PopupSettings {
            rows: settings.completion_max_rows(),
            detail: settings.completion_show_detail(),
            kind: settings.completion_show_kind(),
        };
        let editor = self.editor.read(cx);
        let g = editor.pane_grid(self.pane)?;
        let truncate = editor.grid_ui.opts.truncate_indicator.as_str();
        let f = Frame::new(g, theme, settings, truncate, bounds, window);

        let mut p = f.empty_prepaint();
        f.cells(&mut p, window);
        f.gridlines(&mut p);
        // A formula being edited: each reference gets a colour, and the
        // cells it covers an outline in that colour.
        let ref_spans = f
            .editing()
            .map_or_else(Vec::new, |e| formula_refs(&e.text, &f.pal));
        f.ref_outlines(&ref_spans, &mut p);
        match f.editing() {
            Some(e) => f.cell_editor(e, &ref_spans, &popup, &mut p, window),
            None => {
                f.cursor_outline(&mut p);
                f.spill_outline(&mut p);
            }
        }
        f.range_outline(&mut p);
        f.headers(&mut p, window);
        f.scroll_indicators(&mut p);
        if let Some(GridDrag::Resize {
            pane,
            axis,
            start,
            size: sz,
            ..
        }) = editor.grid_ui.drag
            && pane == self.pane
        {
            f.resize_guide(
                axis == Axis::Cols,
                start + (sz as f32) * f.zoom,
                &mut p,
                window,
            );
        }
        let bar_text = if f.bar_h > 0.0 {
            f.formula_bar(&mut p, window)
        } else {
            (0.0, 0.0)
        };

        let geom = f.geom(bar_text);
        let pane = self.pane;
        let hit = p.popup.as_ref().map(|pp| pp.hit);
        self.editor.update(cx, |editor, _| {
            editor.set_grid_geometry(pane, geom);
            editor.grid_ui.completion_hit = hit;
        });
        Some(p)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        // Claim the typed-text input handler for the selected grid pane, so
        // IME composition (dead keys, CJK) reaches the cell editor.
        if self.editor.read(cx).pane_is_selected(self.pane) {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }
        let Some(p) = prepaint.as_mut() else {
            return;
        };
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(p.background.clone());
            window.with_content_mask(
                Some(ContentMask {
                    bounds: p.scroll_bounds,
                }),
                |window| {
                    for q in p.under.drain(..) {
                        window.paint_quad(q);
                    }
                    for t in p.cells.drain(..) {
                        paint_text(t, window, cx);
                    }
                },
            );
            window.with_content_mask(
                Some(ContentMask {
                    bounds: p.cells_bounds,
                }),
                |window| {
                    for q in p.frozen_under.drain(..) {
                        window.paint_quad(q);
                    }
                    for t in p.frozen_cells.drain(..) {
                        paint_text(t, window, cx);
                    }
                    for q in p.lines.drain(..) {
                        window.paint_quad(q);
                    }
                },
            );
            for q in p.over.drain(..) {
                window.paint_quad(q);
            }
            for t in p.over_text.drain(..) {
                paint_text(t, window, cx);
            }
            window.with_content_mask(
                Some(ContentMask {
                    bounds: p.outline_bounds,
                }),
                |window| {
                    for q in p.outlines.drain(..) {
                        window.paint_quad(q);
                    }
                },
            );
        });
        if let Some(popup) = p.popup.take() {
            // Over everything, unclipped by the cells area.
            for quad in popup.quads {
                window.paint_quad(quad);
            }
            for (line, origin) in popup.labels {
                line.paint(origin, popup.hit.row_height, window, cx).ok();
            }
        }
        for (hitbox, style) in &p.hitboxes {
            window.set_cursor_style(*style, hitbox);
        }
        let pane = self.pane;
        self.editor.update(cx, |editor, _| {
            editor.report_pane_bounds(pane, bounds);
        });
    }
}

/// How the function-name dropdown is laid out (from the settings).
struct PopupSettings {
    rows: usize,
    detail: bool,
    kind: bool,
}

/// Everything one frame's layout shares: the grid, the theme, fonts, and
/// the pane's geometry (screen px), worked out once in [`Frame::new`].
struct Frame<'a> {
    g: &'a GridDoc,
    v: &'a view::GridView,
    theme: &'a Theme,
    pal: Palette,
    regular: Font,
    bold: Font,
    /// No bold face is installed: bold is drawn twice, offset.
    fake_bold: bool,
    /// The editor font size, unzoomed.
    base_size: f32,
    zoom: f32,
    /// Cell text size (zoomed).
    font_size: Pixels,
    /// One monospace advance at `font_size`.
    char_w: f32,
    /// The formula bar's (unzoomed chrome) font size and advance.
    chrome_size: Pixels,
    chrome_char_w: f32,
    /// Drawn at the end of text cut off by its cell.
    truncate: &'a str,
    used_rows: usize,
    used_cols: usize,
    bounds: Bounds<Pixels>,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    bar_h: f32,
    header_h: f32,
    header_w: f32,
    /// Top-left and size of the cells area (past bar and headers).
    ox: f32,
    oy: f32,
    cw: f32,
    ch: f32,
    /// Height of the frozen rows band.
    frozen_h: f32,
    /// The columns and rows on screen, at screen positions.
    cols: Vec<VisibleLine>,
    rows: Vec<VisibleLine>,
    sel: CellRange,
    /// More than one cell is selected.
    has_range: bool,
    /// Zoomed out past reading: cells draw as tinted blocks.
    blocks: bool,
}

/// One row being laid out.
#[derive(Clone, Copy)]
struct RowAt {
    r: usize,
    y: f32,
    h: f32,
    /// The sheet's header row (drawn bold, never formatted).
    header_row: bool,
}

/// Where a row's fills and text go: the scrolled layer or the frozen one.
struct Layer<'p> {
    under: &'p mut Vec<PaintQuad>,
    texts: &'p mut Vec<Text>,
}

impl<'a> Frame<'a> {
    fn new(
        g: &'a GridDoc,
        theme: &'a Theme,
        settings: &crate::settings::Settings,
        truncate: &'a str,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> Self {
        let pal = Palette::new(theme);
        let base_size = settings.font_size();
        let v = &g.view;
        let zoom = v.zoom.max(0.1);
        let font_size = px((base_size * zoom).max(3.0));
        let chrome_size = px(base_size.min(14.0) * 0.92);
        let regular = font(settings.font_family().to_string());
        let bold = Font {
            weight: FontWeight::BOLD,
            ..regular.clone()
        };
        // Without a bold face installed (the bundled Lilex is regular only),
        // bold resolves to the regular face: hand-set bold is then drawn
        // twice, a fraction of a pixel apart.
        let fake_bold = {
            let ts = window.text_system();
            ts.resolve_font(&bold) == ts.resolve_font(&regular)
        };
        let advance = |window: &mut Window, size: Pixels| {
            let zero = window.text_system().shape_line(
                "0".into(),
                size,
                &[run(1, &regular, pal.fg)],
                None,
            );
            f32::from(zero.x_for_index(1)).max(1.0)
        };
        let char_w = advance(window, font_size);
        let chrome_char_w = advance(window, chrome_size);

        let (used_rows, used_cols) = g.shown_extent();
        let bar_h = if v.bar_visible { BAR_HEIGHT } else { 0.0 };
        let header_h = (view::DEFAULT_ROW_HEIGHT as f32 * zoom).max(12.0);
        // Row header: wide enough for the largest row number on screen.
        let approx_last_row = v.top_row() + v.page_rows() + 2;
        let digits = approx_last_row.to_string().len().max(2) as f32;
        let header_w = (digits * char_w + 12.0).max(40.0 * zoom);

        let left = f32::from(bounds.left());
        let top = f32::from(bounds.top());
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        let ox = left + header_w;
        let oy = top + bar_h + header_h;
        let cw = (width - header_w).max(0.0);
        let ch = (height - bar_h - header_h).max(0.0);
        let frozen_h = (v.frozen_height() * zoom as f64) as f32;
        let mut cols = v.visible_cols(cw);
        cols.iter_mut().for_each(|l| l.pos += ox);
        let mut rows = v.visible_rows(ch);
        rows.iter_mut().for_each(|l| l.pos += oy);
        let sel = g.selection();
        Self {
            g,
            v,
            theme,
            pal,
            regular,
            bold,
            fake_bold,
            base_size,
            zoom,
            font_size,
            char_w,
            chrome_size,
            chrome_char_w,
            truncate,
            used_rows,
            used_cols,
            bounds,
            left,
            top,
            width,
            height,
            bar_h,
            header_h,
            header_w,
            ox,
            oy,
            cw,
            ch,
            frozen_h,
            cols,
            rows,
            sel,
            has_range: !sel.is_single(),
            blocks: zoom < view::BLOCK_ZOOM,
        }
    }

    fn editing(&self) -> Option<&'a CellEdit> {
        self.v.edit.as_ref()
    }

    fn empty_prepaint(&self) -> GridPrepaint {
        let (ox, oy, cw, ch) = (self.ox, self.oy, self.cw, self.ch);
        GridPrepaint {
            under: Vec::new(),
            lines: Vec::new(),
            cells: Vec::new(),
            frozen_under: Vec::new(),
            frozen_cells: Vec::new(),
            over: Vec::new(),
            over_text: Vec::new(),
            outlines: Vec::new(),
            outline_bounds: rect(
                ox - CURSOR_BORDER,
                oy - CURSOR_BORDER,
                cw + CURSOR_BORDER,
                ch + CURSOR_BORDER,
            ),
            cells_bounds: rect(ox, oy, cw, ch),
            scroll_bounds: rect(
                ox,
                oy + self.frozen_h.min(ch),
                cw,
                (ch - self.frozen_h).max(0.0),
            ),
            hitboxes: Vec::new(),
            popup: None,
            background: fill(self.bounds, self.pal.bg),
        }
    }

    /// What the element reports for hit-testing.
    fn geom(&self, (bar_text_x, bar_shift): (f32, f32)) -> GridGeom {
        GridGeom {
            origin: self.bounds.origin,
            width: self.width,
            height: self.height,
            bar_h: self.bar_h,
            header_w: self.header_w,
            header_h: self.header_h,
            char_w: self.char_w,
            bar_text_x,
            bar_shift,
            bar_char_w: self.chrome_char_w,
        }
    }

    // --- cells -------------------------------------------------------------

    /// Every visible row's fills and cell text.
    fn cells(&self, p: &mut GridPrepaint, window: &mut Window) {
        // Zebra stripes: the face's background, or a faint wash of the text
        // color when the theme gives the face none.
        let stripe = self.g.style.stripe.as_deref().map(|f| {
            self.theme.background_of(f).unwrap_or(Hsla {
                a: 0.035,
                ..self.pal.fg
            })
        });
        for line in &self.rows {
            let (r, y, h) = (line.idx, line.pos, line.size);
            let mut layer = if line.frozen {
                Layer {
                    under: &mut p.frozen_under,
                    texts: &mut p.frozen_cells,
                }
            } else {
                Layer {
                    under: &mut p.under,
                    texts: &mut p.cells,
                }
            };
            if line.frozen {
                layer
                    .under
                    .push(fill(rect(self.ox, y, self.cw, h), self.pal.bg));
            }
            // The header row reads as a header.
            let header_row = r == 0 && self.g.wb.header(SHEET);
            if let Some(bg) = stripe
                && !header_row
                && self.v.rows.rank(r) % 2 == 1
            {
                layer
                    .under
                    .push(fill(rect(self.ox, y, self.cw, h - 1.0), bg));
            }
            let row = RowAt {
                r,
                y,
                h,
                header_row,
            };
            let mut skip_until = 0usize; // columns already covered by overflow
            for (i, col) in self.cols.iter().enumerate() {
                let (c, x, w) = (col.idx, col.pos, col.size);
                // Bold and colors set by hand; a fill shows on any cell,
                // empty or not, and under the selection's tint.
                let fmt = if self.g.formats.touches_col(c) {
                    self.g.formats.get(r, c)
                } else {
                    CellFmt::default()
                };
                if let Some(i) = fmt.fill {
                    layer.under.push(fill(
                        rect(x, y, w - 1.0, h - 1.0),
                        palette_fill(self.theme, i),
                    ));
                }
                if self.has_range && self.sel.contains(r, c) {
                    layer.under.push(fill(rect(x, y, w, h), self.pal.selection));
                }
                if i < skip_until || r >= self.used_rows || c >= self.used_cols {
                    continue;
                }
                if self.editing().is_some_and(|e| e.row == r && e.col == c) {
                    continue; // the editor draws this cell
                }
                if let Some(end) = self.cell_content(row, i, fmt, &mut layer, window) {
                    skip_until = end;
                }
            }
        }
    }

    /// One cell's face fill and text (column `i` of [`Self::cols`]). Returns
    /// the column index its text overflows up to, when it spills into empty
    /// cells to its right.
    fn cell_content(
        &self,
        row: RowAt,
        i: usize,
        fmt: CellFmt,
        layer: &mut Layer,
        window: &mut Window,
    ) -> Option<usize> {
        let (g, theme, pal) = (self.g, self.theme, &self.pal);
        let (zoom, char_w) = (self.zoom, self.char_w);
        let RowAt {
            r,
            y,
            h,
            header_row,
        } = row;
        let VisibleLine {
            idx: c,
            pos: x,
            size: w,
            ..
        } = self.cols[i];
        let value = g.shown_text(r, c);
        // Scheme-set styling (faces, rules, number formats); the header row
        // is left plain.
        let styled = !header_row && g.style.touches(c);
        let face = if styled {
            g.style.face(r, c, value, |k| g.cell(r, k))
        } else if header_row {
            g.style.header_face.as_deref()
        } else {
            None
        };
        if let Some(bg) = face.and_then(|f| theme.background_of(f)) {
            layer.under.push(fill(rect(x, y, w - 1.0, h - 1.0), bg));
        }
        if value.is_empty() {
            return None;
        }
        // A number format set on the cell wins over a column's.
        let by_cell = fmt.num.and_then(|code| g.number_formatted(r, c, code));
        let formatted = match &by_cell {
            Some((text, _)) => Some(text.clone()),
            None if styled => g.style.display(c, value),
            None => None,
        };
        let value = formatted.as_deref().unwrap_or(value);
        if self.blocks {
            layer
                .under
                .push(fill(rect(x + 1.0, y + 1.0, w - 2.0, h - 2.0), pal.block));
            return None;
        }
        let pad = CELL_PAD * zoom;
        // A formula aligns by its result: numbers right, as Excel does, even
        // in a text column.
        let result = g.formula_value(r, c);
        let right = match (&by_cell, result) {
            (Some((_, numeric)), _) => *numeric,
            (None, Some(v)) => matches!(v, crate::formula::value::Value::Number(_)),
            (None, None) => g.right_aligned(c),
        } && !header_row;
        let center = !header_row && g.style.align(c) == Some(crate::grid::style::Align::Center);
        // Left-aligned text spills into empty cells to its right, as in
        // Excel; right-aligned numbers and centered text never do.
        let mut avail = w - 2.0 * pad;
        let mut clip_w = w;
        let mut overflow = None;
        if !right && !center {
            let cols = &self.cols;
            let mut j = i + 1;
            while (avail / char_w) < value.chars().count() as f32
                && j < cols.len()
                // Into empty cells only — a formula cell isn't empty even
                // when its result is "".
                && (cols[j].idx >= self.used_cols
                    || (g.cell(r, cols[j].idx).is_empty() && !g.is_formula_at(r, cols[j].idx)))
                && !self
                    .editing()
                    .is_some_and(|e| e.row == r && e.col == cols[j].idx)
                && !(self.has_range && self.sel.contains(r, cols[j].idx))
            {
                avail += cols[j].size;
                clip_w += cols[j].size;
                j += 1;
            }
            overflow = Some(j);
        }
        let max_chars = ((avail / char_w).ceil().max(1.0) as usize).saturating_add(1);
        let mut shown = display_text(value, max_chars);
        let fits = (avail / char_w).floor().max(1.0) as usize;
        let truncate_len = self.truncate.chars().count();
        if !right && truncate_len > 0 && shown.chars().count() > fits {
            // Cut off: end with the indicator, inside the cell.
            let keep = fits.saturating_sub(truncate_len);
            shown = shown.chars().take(keep).collect::<String>() + self.truncate;
        }
        let mut color = face.map_or(pal.fg, |f| theme.color(f, pal.fg));
        if face.is_none() && matches!(result, Some(crate::formula::value::Value::Error(_))) {
            color = theme.color("error", pal.warn);
        }
        if let Some(i) = fmt.text {
            color = palette_color(theme, i);
        }
        let row_font = if header_row {
            &self.bold
        } else {
            &self.regular
        };
        let cell_font = if fmt.bold { &self.bold } else { row_font };
        // A number too wide shrinks or turns to ###; text (right-aligned by
        // its column) is clipped like any text.
        let numeric = match result {
            Some(v) => matches!(v, crate::formula::value::Value::Number(_)),
            None => crate::grid::infer::cell_number(&shown).is_some(),
        };
        if right && numeric && shown.chars().count() as f32 * char_w > avail {
            let n = (avail / char_w).floor().max(1.0) as usize;
            // Like Excel's General format: drop decimal places until it
            // fits; only a number whose whole part can't fit shows as ###,
            // never cut off.
            match fit_number(&shown, n) {
                Some(fitted) => shown = fitted,
                None => {
                    shown = "#".repeat(n);
                    color = pal.dim;
                }
            }
        }
        let line = window.text_system().shape_line(
            SharedString::from(shown.clone()),
            self.font_size,
            &[run(shown.len(), cell_font, color)],
            None,
        );
        let tw = f32::from(line.width);
        let tx = if right {
            x + w - pad - tw
        } else if center {
            x + ((w - tw) / 2.0).max(pad)
        } else {
            x + pad
        };
        let ty = y + (h - self.header_h) / 2.0;
        let clip = rect(x, y, clip_w, h);
        let cell_lh = px(self.header_h.max(1.0));
        if fmt.bold && self.fake_bold {
            layer.texts.push(Text {
                line: line.clone(),
                origin: point(px(tx + (0.6 * zoom).max(0.4)), px(ty)),
                clip,
                line_height: cell_lh,
            });
        }
        layer.texts.push(Text {
            line,
            origin: point(px(tx), px(ty)),
            clip,
            line_height: cell_lh,
        });
        overflow
    }

    /// Gridlines, and the stronger line under the frozen band.
    fn gridlines(&self, p: &mut GridPrepaint) {
        let (ox, oy, cw, ch) = (self.ox, self.oy, self.cw, self.ch);
        if self.zoom >= 0.25 {
            for l in &self.cols {
                p.lines
                    .push(fill(rect(l.pos + l.size - 1.0, oy, 1.0, ch), self.pal.line));
            }
            for l in &self.rows {
                p.lines
                    .push(fill(rect(ox, l.pos + l.size - 1.0, cw, 1.0), self.pal.line));
            }
        }
        if self.frozen_h > 0.0 && self.frozen_h < ch {
            p.over.push(fill(
                rect(ox, oy + self.frozen_h - 1.0, cw, 2.0),
                Hsla {
                    a: 0.35,
                    ..self.pal.fg
                },
            ));
        }
    }

    // --- cursor cell, inline editor, outlines -----------------------------

    /// Cell `(r, c)`'s rectangle `(x, y, w, h)`, when any of it is on
    /// screen (and not scrolled under the frozen rows).
    fn cell_rect(&self, r: usize, c: usize) -> Option<(f32, f32, f32, f32)> {
        let (ox, oy, cw, ch, v) = (self.ox, self.oy, self.cw, self.ch, self.v);
        let x = ox + v.col_x(c) as f32;
        let y = oy + v.row_y(r) as f32;
        let w = v.col_px(c) as f32;
        let h = v.row_px(r) as f32;
        let visible_y = if r < v.frozen_rows {
            true
        } else {
            y + h > oy + self.frozen_h
        };
        (x + w > ox && x < ox + cw && y + h > oy && y < oy + ch && visible_y)
            .then_some((x, y, w, h))
    }

    /// Outlines around each reference of the formula being edited, and
    /// around the cells `grid-trace-*` found.
    fn ref_outlines(&self, ref_spans: &[RefSpan], p: &mut GridPrepaint) {
        let v = self.v;
        let mut outline = |a: CellRange, color: Hsla| {
            let x0 = self.ox + v.col_x(a.c0) as f32;
            let y0 = self.oy + v.row_y(a.r0) as f32;
            let x1 = self.ox + (v.col_x(a.c1) + v.col_px(a.c1)) as f32;
            let y1 = self.oy + (v.row_y(a.r1) + v.row_px(a.r1)) as f32;
            if x1 > x0 && y1 > y0 {
                push_border(
                    &mut p.outlines,
                    x0 - 1.0,
                    y0 - 1.0,
                    x1 - x0 + 1.0,
                    y1 - y0 + 1.0,
                    1.5,
                    color,
                );
            }
        };
        for rs in ref_spans {
            // Whole rows/columns stop at the data.
            let mut a = rs.area;
            a.r1 = a.r1.min(self.used_rows.max(a.r0 + 1) - 1);
            a.c1 = a.c1.min(self.used_cols.max(a.c0 + 1) - 1);
            outline(a, rs.color);
        }
        for &a in &v.trace {
            outline(a, self.pal.trace);
        }
    }

    /// The inline cell editor over the cell being edited: its text, caret,
    /// frame, and the function-name dropdown.
    fn cell_editor(
        &self,
        e: &CellEdit,
        ref_spans: &[RefSpan],
        popup: &PopupSettings,
        p: &mut GridPrepaint,
        window: &mut Window,
    ) {
        let Some((x, y, w, h)) = self.cell_rect(e.row, e.col) else {
            return;
        };
        let pal = &self.pal;
        let shown = e.text.replace('\n', "↵");
        let caret_disp = e.caret + 2 * e.text[..e.caret].matches('\n').count();
        let runs = coloured_runs(&shown, ref_spans, &self.regular, pal.fg);
        let line = window.text_system().shape_line(
            SharedString::from(shown.clone()),
            self.font_size,
            &runs,
            None,
        );
        let pad = CELL_PAD * self.zoom;
        // The editor grows to the right to fit its text, like Excel.
        let ew = (f32::from(line.width) + 2.0 * pad + self.char_w)
            .max(w)
            .min(self.ox + self.cw - x);
        p.over.push(fill(rect(x, y, ew, h), pal.bg));
        push_border(
            &mut p.outlines,
            x - 1.0,
            y - 1.0,
            ew + 1.0,
            h + 1.0,
            CURSOR_BORDER,
            pal.cursor,
        );
        let caret_x = x + pad + f32::from(line.x_for_index(caret_disp));
        if !e.in_bar {
            p.over
                .push(fill(rect(caret_x, y + 2.0, 1.5, h - 4.0), pal.cursor));
        }
        // The function-name dropdown hangs below the caret.
        if let Some(view) = Editor::grid_completion_view(self.g) {
            // Rows at the base font's height whatever the zoom (its text
            // isn't zoomed), the list starting under the cell.
            let row_h = (self.base_size * 1.4).round().max(4.0);
            p.popup = Some(crate::element::build_popup(
                point(px(caret_x), px(y + h - row_h)),
                view,
                popup.rows,
                popup.detail,
                popup.kind,
                &self.regular,
                px((self.base_size).max(3.0)),
                px(row_h),
                self.bounds,
                self.theme,
                window,
            ));
        }
        p.over_text.push(Text {
            line,
            origin: point(px(x + pad), px(y + (h - self.header_h) / 2.0)),
            clip: rect(x, y, ew, h),
            line_height: px(self.header_h.max(1.0)),
        });
    }

    /// The cursor cell's frame. A multi-cell selection shows only its range
    /// outline; the cursor cell gets its own frame only when it's the
    /// selection.
    fn cursor_outline(&self, p: &mut GridPrepaint) {
        let (cr, cc) = self.v.cursor;
        if !self.has_range
            && let Some((x, y, w, h)) = self.cell_rect(cr, cc)
        {
            push_border(
                &mut p.outlines,
                x - 1.0,
                y - 1.0,
                w + 1.0,
                h + 1.0,
                CURSOR_BORDER,
                self.pal.cursor,
            );
        }
    }

    /// A thin frame around the range the cursor cell's formula spills
    /// into (or that spills over the cursor cell), as Excel draws one.
    fn spill_outline(&self, p: &mut GridPrepaint) {
        if self.has_range {
            return;
        }
        let Some(a) = self.g.cursor_spill() else {
            return;
        };
        let (ox, oy, cw, ch, v) = (self.ox, self.oy, self.cw, self.ch, self.v);
        let x0 = (ox + v.col_x(a.c0) as f32).max(ox - 2.0);
        let y0 = (oy + v.row_y(a.r0) as f32).max(oy - 2.0);
        let x1 = (ox + (v.col_x(a.c1) + v.col_px(a.c1)) as f32).min(ox + cw + 2.0);
        let y1 = (oy + (v.row_y(a.r1) + v.row_px(a.r1)) as f32).min(oy + ch + 2.0);
        if x1 > x0 && y1 > y0 {
            push_border(
                &mut p.outlines,
                x0 - 1.0,
                y0 - 1.0,
                x1 - x0 + 1.0,
                y1 - y0 + 1.0,
                1.0,
                self.pal.cursor,
            );
        }
    }

    /// Outline the whole selected range (clamped to the view).
    fn range_outline(&self, p: &mut GridPrepaint) {
        if !self.has_range {
            return;
        }
        let (ox, oy, cw, ch, v, sel) = (self.ox, self.oy, self.cw, self.ch, self.v, self.sel);
        let x0 = (ox + v.col_x(sel.c0) as f32).max(ox - 2.0);
        // A range below the frozen rows, scrolled up under them, stops at
        // the frozen band's edge rather than drawing across it.
        let top_limit = if sel.r0 >= v.frozen_rows {
            oy + self.frozen_h + 1.0
        } else {
            oy - 2.0
        };
        let y0 = (oy + v.row_y(sel.r0) as f32).max(top_limit);
        let x1 = (ox + (v.col_x(sel.c1) + v.col_px(sel.c1)) as f32).min(ox + cw + 2.0);
        let y1 = (oy + (v.row_y(sel.r1) + v.row_px(sel.r1)) as f32).min(oy + ch + 2.0);
        if x1 > x0 && y1 > y0 {
            push_border(
                &mut p.outlines,
                x0 - 1.0,
                y0 - 1.0,
                x1 - x0 + 1.0,
                y1 - y0 + 1.0,
                CURSOR_BORDER,
                self.pal.cursor,
            );
        }
    }

    // --- headers, scroll indicators, resize guide --------------------------

    /// Column letters and row numbers, with their resize grab bands.
    fn headers(&self, p: &mut GridPrepaint, window: &mut Window) {
        let (left, oy, width, header_w, header_h) =
            (self.left, self.oy, self.width, self.header_w, self.header_h);
        let (pal, ch, ox) = (&self.pal, self.ch, self.ox);
        let hy = self.top + self.bar_h;
        p.over.push(fill(rect(left, hy, width, header_h), pal.bg));
        p.over
            .push(fill(rect(left, hy, width, header_h), pal.header_bg));
        p.over.push(fill(rect(left, oy, header_w, ch), pal.bg));
        p.over
            .push(fill(rect(left, oy, header_w, ch), pal.header_bg));
        let header_font_size = px((self.base_size * self.zoom * 0.8).max(3.0));
        let (cr, cc) = self.v.cursor;
        let sel = self.sel;
        let active_cols = if self.has_range {
            (sel.c0, sel.c1)
        } else {
            (cc, cc)
        };
        let active_rows = if self.has_range {
            (sel.r0, sel.r1)
        } else {
            (cr, cr)
        };
        let label = |window: &mut Window, text: &str, color: Hsla| {
            window.text_system().shape_line(
                SharedString::from(text.to_string()),
                header_font_size,
                &[run(text.len(), &self.regular, color)],
                None,
            )
        };
        for &VisibleLine {
            idx: c,
            pos: x,
            size: w,
            ..
        } in &self.cols
        {
            let active = (active_cols.0..=active_cols.1).contains(&c);
            if active {
                p.over
                    .push(fill(rect(x, hy, w, header_h), pal.header_active));
            }
            if !self.blocks || c % 5 == 0 {
                let color = if active {
                    pal.header_label_active
                } else {
                    pal.header_label
                };
                let line = label(window, &col_name(c), color);
                let tx = x + (w - f32::from(line.width)) / 2.0;
                p.over_text.push(Text {
                    line,
                    origin: point(px(tx), px(hy)),
                    clip: rect(x, hy, w, header_h),
                    line_height: px(header_h),
                });
            }
            // A grab band on the border: resize cursor.
            let hb =
                window.insert_hitbox(rect(x + w - 3.0, hy, 6.0, header_h), HitboxBehavior::Normal);
            p.hitboxes.push((hb, CursorStyle::ResizeLeftRight));
        }
        for &VisibleLine {
            idx: r,
            pos: y,
            size: h,
            ..
        } in &self.rows
        {
            let active = (active_rows.0..=active_rows.1).contains(&r);
            if active {
                p.over
                    .push(fill(rect(left, y, header_w, h), pal.header_active));
            }
            if !self.blocks || r % 10 == 0 {
                let problem = self.g.wb.problems(SHEET).binary_search(&r).is_ok();
                let color = if problem {
                    pal.warn
                } else if active {
                    pal.header_label_active
                } else {
                    pal.header_label
                };
                let line = label(window, &(r + 1).to_string(), color);
                let tx = left + header_w - 6.0 - f32::from(line.width);
                p.over_text.push(Text {
                    line,
                    origin: point(px(tx), px(y + (h - header_h) / 2.0)),
                    clip: rect(left, y, header_w, h),
                    line_height: px(header_h),
                });
            }
            let hb = window.insert_hitbox(
                rect(left, y + h - 3.0, header_w, 6.0),
                HitboxBehavior::Normal,
            );
            p.hitboxes.push((hb, CursorStyle::ResizeUpDown));
        }
        // Header separators: the same hairline as the gridlines.
        p.over
            .push(fill(rect(left, oy - 1.0, width, 1.0), pal.line));
        p.over.push(fill(
            rect(ox - 1.0, hy, 1.0, self.height - self.bar_h),
            pal.line,
        ));
    }

    /// Scroll position indicators along the right and bottom edges.
    fn scroll_indicators(&self, p: &mut GridPrepaint) {
        let (ox, oy, cw, ch, v) = (self.ox, self.oy, self.cw, self.ch, self.v);
        let (zoom, frozen_h) = (self.zoom as f64, self.frozen_h);
        let thumb_color = Hsla {
            a: 0.25,
            ..self.pal.fg
        };
        let content_h = (v.rows.start(self.used_rows.max(v.cursor.0 + 1)) - v.frozen_height())
            .max(v.scroll_y + (ch - frozen_h) as f64 / zoom)
            * zoom;
        if content_h > (ch - frozen_h) as f64 + 1.0 {
            let view_h = (ch - frozen_h) as f64;
            let thumb = (view_h / content_h * view_h).max(18.0) as f32;
            let pos = (v.scroll_y * zoom / content_h * view_h) as f32;
            p.over.push(fill(
                rect(
                    ox + cw - 6.0,
                    oy + frozen_h + pos.min((view_h as f32 - thumb).max(0.0)),
                    4.0,
                    thumb,
                ),
                thumb_color,
            ));
        }
        let content_w = v
            .cols
            .start(self.used_cols.max(v.cursor.1 + 1))
            .max(v.scroll_x + cw as f64 / zoom)
            * zoom;
        if content_w > cw as f64 + 1.0 {
            let thumb = (cw as f64 / content_w * cw as f64).max(18.0) as f32;
            let pos = (v.scroll_x * zoom / content_w * cw as f64) as f32;
            p.over.push(fill(
                rect(
                    ox + pos.min((cw - thumb).max(0.0)),
                    oy + ch - 6.0,
                    thumb,
                    4.0,
                ),
                thumb_color,
            ));
        }
    }

    /// The guide line of a header-border drag, `at` px into the cells area
    /// (a column border when `is_col`), and a resize cursor over the pane.
    fn resize_guide(&self, is_col: bool, at: f32, p: &mut GridPrepaint, window: &mut Window) {
        let guide = if is_col {
            rect(
                self.ox + at - 1.0,
                self.top + self.bar_h,
                2.0,
                self.height - self.bar_h,
            )
        } else {
            rect(self.left, self.oy + at - 1.0, self.width, 2.0)
        };
        p.over.push(fill(guide, self.pal.cursor));
        let whole = window.insert_hitbox(self.bounds, HitboxBehavior::Normal);
        let style = if is_col {
            CursorStyle::ResizeLeftRight
        } else {
            CursorStyle::ResizeUpDown
        };
        p.hitboxes.push((whole, style));
    }

    // --- formula bar ---------------------------------------------------------

    /// The formula bar: name box, `fx`, and the cursor cell's contents (or
    /// the edit, with its caret). Returns where its text starts (from the
    /// pane's left) and how far long text is scrolled, so a click lands on
    /// the character under it.
    fn formula_bar(&self, p: &mut GridPrepaint, window: &mut Window) -> (f32, f32) {
        let (g, pal, regular) = (self.g, &self.pal, &self.regular);
        let (font_size, char_w) = (self.chrome_size, self.chrome_char_w);
        let (left, top, width) = (self.left, self.top, self.width);
        let h = BAR_HEIGHT;
        p.over.push(fill(rect(left, top, width, h), pal.bg));
        p.over
            .push(fill(rect(left, top + h - 1.0, width, 1.0), pal.line));
        p.over.push(fill(
            rect(left + NAME_BOX_WIDTH, top + 4.0, 1.0, h - 8.0),
            pal.line,
        ));
        let (name, content) = g.bar_text();
        // Another cell's formula (the cursor is on a spilled value) greyed.
        let content_color = if g.bar_ghost() { pal.dim } else { pal.fg };
        let lh = px(h);
        let shape = |window: &mut Window, s: &str, color: Hsla| {
            window.text_system().shape_line(
                SharedString::from(s.to_string()),
                font_size,
                &[run(s.len(), regular, color)],
                None,
            )
        };
        let name_line = shape(window, &name, pal.fg);
        p.over_text.push(Text {
            line: name_line,
            origin: point(px(left + 8.0), px(top)),
            clip: rect(left, top, NAME_BOX_WIDTH - 4.0, h),
            line_height: lh,
        });
        let fx_x = left + NAME_BOX_WIDTH + 8.0;
        let fx = shape(window, "fx", pal.dim);
        p.over_text.push(Text {
            line: fx,
            origin: point(px(fx_x), px(top)),
            clip: rect(fx_x, top, 24.0, h),
            line_height: lh,
        });
        let text_x = left + NAME_BOX_WIDTH + 34.0;
        let avail = (width - (text_x - left) - 8.0).max(0.0);
        // Loading progress takes the right end of the bar.
        let mut avail_text = avail;
        if let Some(l) = &g.loading {
            let pct = (l.progress.load(std::sync::atomic::Ordering::Relaxed) as f64
                / l.total.max(1) as f64
                * 100.0)
                .min(99.0);
            let label = format!("Loading… {pct:.0}%");
            let line = shape(window, &label, pal.warn);
            let lw = f32::from(line.width);
            avail_text = (avail - lw - 16.0).max(0.0);
            let lx = left + width - 8.0 - lw;
            p.over_text.push(Text {
                line,
                origin: point(px(lx), px(top)),
                clip: rect(lx, top, lw + 4.0, h),
                line_height: lh,
            });
        }
        let editing = g.view.edit.as_ref();
        let max_chars = (avail_text / char_w) as usize + 2;
        let (shown, caret) = match editing {
            Some(e) => {
                let shown = e.text.replace('\n', "↵");
                let caret = e.caret + 2 * e.text[..e.caret].matches('\n').count();
                (shown, Some(caret))
            }
            None => {
                let mut s: String = content.replace('\n', "↵").chars().take(max_chars).collect();
                if content.chars().count() > max_chars {
                    s.pop();
                    s.push('…');
                }
                (s, None)
            }
        };
        let line = match editing {
            Some(e) => {
                let spans = formula_refs(&e.text, pal);
                window.text_system().shape_line(
                    SharedString::from(shown.clone()),
                    font_size,
                    &coloured_runs(&shown, &spans, regular, pal.fg),
                    None,
                )
            }
            None => shape(window, &shown, content_color),
        };
        // Keep the caret in view when the edited text is wider than the bar.
        let caret_x = caret.map(|c| f32::from(line.x_for_index(c)));
        let shift = caret_x.map_or(0.0, |x| (x - avail_text + char_w).max(0.0));
        if let (Some(x), Some(e)) = (caret_x, editing)
            && e.in_bar
        {
            p.over.push(fill(
                rect(text_x + x - shift, top + 4.0, 1.5, h - 8.0),
                pal.cursor,
            ));
        }
        if let Some(e) = editing
            && !e.in_bar
        {
            // A subtle marker that the cell is being edited.
            p.over.push(fill(
                rect(text_x - 6.0, top + 6.0, 2.0, h - 12.0),
                pal.cursor,
            ));
        }
        p.over_text.push(Text {
            line,
            origin: point(px(text_x - shift), px(top)),
            clip: rect(text_x, top, avail_text, h),
            line_height: lh,
        });
        (text_x - left, shift)
    }
}

/// The rectangle at `(x, y)` of size `w × h`, in screen px.
fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
}

/// `text` (a number as shown) rounded to fewer decimal places so it fits in
/// `width` characters, or `None` when even its whole part doesn't fit (or it
/// isn't a plain decimal number).
fn fit_number(text: &str, width: usize) -> Option<String> {
    let (_, frac) = text.split_once('.')?;
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: f64 = text.parse().ok()?;
    (0..frac.len()).rev().find_map(|d| {
        // Half away from zero on the decimal value, as Excel rounds
        // (`format!` alone rounds half to even), and never "-0".
        let r = crate::formula::decimal::round(n, d as i32, crate::formula::decimal::Mode::Half);
        let r = if r == 0.0 { 0.0 } else { r };
        let s = format!("{r:.d$}");
        (s.chars().count() <= width).then_some(s)
    })
}

/// A reference in a formula being edited.
struct RefSpan {
    /// Its byte span in the text.
    span: std::ops::Range<usize>,
    /// The cells it covers.
    area: CellRange,
    color: Hsla,
}

/// The references in edited formula `text`, each with its byte span, the
/// rectangle it covers, and its colour. Empty for anything but a formula
/// (and for multi-line text, whose display shifts byte offsets).
fn formula_refs(text: &str, pal: &Palette) -> Vec<RefSpan> {
    if !text.starts_with('=') || text.contains('\n') {
        return Vec::new();
    }
    crate::formula::parse::ref_spans(text)
        .into_iter()
        .filter(|(_, r)| r.sheet.is_none())
        .enumerate()
        .map(|(i, (span, r))| RefSpan {
            span,
            area: CellRange::from_bounds(r.area.bounds()),
            color: pal.refs[i % pal.refs.len()],
        })
        .collect()
}

/// Text runs colouring each reference span; the rest in `default`.
fn coloured_runs(text: &str, spans: &[RefSpan], font: &Font, default: Hsla) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut at = 0;
    for RefSpan { span, color, .. } in spans {
        if span.start < at || span.end > text.len() {
            continue;
        }
        if span.start > at {
            runs.push(run(span.start - at, font, default));
        }
        runs.push(run(span.end - span.start, font, *color));
        at = span.end;
    }
    if at < text.len() || runs.is_empty() {
        runs.push(run(text.len() - at, font, default));
    }
    runs
}

/// Four quads forming a rectangle outline `t` px thick.
fn push_border(out: &mut Vec<PaintQuad>, x: f32, y: f32, w: f32, h: f32, t: f32, color: Hsla) {
    out.push(fill(rect(x, y, w, t), color));
    out.push(fill(rect(x, y + h - t, w, t), color));
    out.push(fill(rect(x, y, t, h), color));
    out.push(fill(rect(x + w - t, y, t, h), color));
}

fn paint_text(t: Text, window: &mut Window, cx: &mut App) {
    window.with_content_mask(Some(ContentMask { bounds: t.clip }), |window| {
        t.line.paint(t.origin, t.line_height, window, cx).ok();
    });
}

#[cfg(test)]
mod tests {
    use super::fit_number;

    #[test]
    fn numbers_lose_decimals_before_hashes() {
        assert_eq!(fit_number("18958.56", 7).as_deref(), Some("18958.6"));
        assert_eq!(fit_number("18958.56", 5).as_deref(), Some("18959"));
        assert_eq!(fit_number("18958.56", 4), None);
        assert_eq!(fit_number("123456", 3), None);
    }

    #[test]
    fn regression_fit_number_rounding() {
        // Excel rounds half away from zero: 18958.5 in a 5-char cell is 18959.
        eprintln!("18958.5 -> {:?}", fit_number("18958.5", 5));
        eprintln!("2.5 -> {:?}", fit_number("2.5", 1));
        eprintln!("-0.4 -> {:?}", fit_number("-0.4", 2));
        eprintln!("0.125 -> {:?}", fit_number("0.125", 4));
        assert_eq!(fit_number("18958.5", 5).as_deref(), Some("18959"));
        assert_eq!(fit_number("-0.4", 2).as_deref(), Some("0"));
    }
}

/// Audit probe: wraps a `GridElement` and records what its prepaint
/// produced, before paint drains it.
#[cfg(test)]
pub(crate) mod probe {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    #[derive(Default, Debug, Clone)]
    pub struct Seen {
        /// (text, origin x, origin y, clip x, clip w, frozen?)
        pub cells: Vec<(String, f32, f32, f32, f32, bool)>,
        pub over_text: Vec<String>,
        /// outline quads: (x, y, w, h)
        pub outlines: Vec<(f32, f32, f32, f32)>,
        pub popup_row_height: Option<f32>,
        pub cells_top: f32,
        pub frozen_bottom: f32,
    }

    pub struct Probe {
        pub inner: GridElement,
        pub seen: Rc<RefCell<Seen>>,
    }

    impl IntoElement for Probe {
        type Element = Self;
        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Probe {
        type RequestLayoutState = ();
        type PrepaintState = Option<GridPrepaint>;
        fn id(&self) -> Option<ElementId> {
            None
        }
        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }
        fn request_layout(
            &mut self,
            id: Option<&GlobalElementId>,
            iid: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, ()) {
            self.inner.request_layout(id, iid, window, cx)
        }
        fn prepaint(
            &mut self,
            id: Option<&GlobalElementId>,
            iid: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            rl: &mut (),
            window: &mut Window,
            cx: &mut App,
        ) -> Option<GridPrepaint> {
            let p = self.inner.prepaint(id, iid, bounds, rl, window, cx);
            if let Some(p) = &p {
                let mut s = self.seen.borrow_mut();
                let rec = |t: &Text, frozen: bool| {
                    (
                        t.line.text.to_string(),
                        f32::from(t.origin.x),
                        f32::from(t.origin.y),
                        f32::from(t.clip.origin.x),
                        f32::from(t.clip.size.width),
                        frozen,
                    )
                };
                s.cells = p.cells.iter().map(|t| rec(t, false)).collect();
                s.cells.extend(p.frozen_cells.iter().map(|t| rec(t, true)));
                s.over_text = p
                    .over_text
                    .iter()
                    .map(|t| t.line.text.to_string())
                    .collect();
                s.outlines = p
                    .outlines
                    .iter()
                    .map(|q| {
                        (
                            f32::from(q.bounds.origin.x),
                            f32::from(q.bounds.origin.y),
                            f32::from(q.bounds.size.width),
                            f32::from(q.bounds.size.height),
                        )
                    })
                    .collect();
                s.popup_row_height = p.popup.as_ref().map(|pp| f32::from(pp.hit.row_height));
                s.cells_top = f32::from(p.cells_bounds.origin.y);
                s.frozen_bottom = f32::from(p.scroll_bounds.origin.y);
            }
            p
        }
        fn paint(
            &mut self,
            id: Option<&GlobalElementId>,
            iid: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            rl: &mut (),
            pp: &mut Option<GridPrepaint>,
            window: &mut Window,
            cx: &mut App,
        ) {
            self.inner.paint(id, iid, bounds, rl, pp, window, cx)
        }
    }
}
