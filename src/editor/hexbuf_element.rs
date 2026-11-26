//! `HexElement`: paints one pane showing a hex buffer — the column header,
//! one row per `width` bytes (offset, hex, ASCII), the cursor and selection
//! in both columns, and the data inspector strip at the bottom. Only the rows
//! on screen are shaped, so a frame costs the same for any file size.

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, Font,
    GlobalElementId, Hsla, InspectorElementId, LayoutId, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextAlign, TextRun, Window, fill, font, point, prelude::*, px, relative,
    size,
};

use crate::color_compat::rgb;
use crate::hexview::{self, ByteClass, inspect};
use crate::theme::Theme;

use super::Editor;
use super::hexbuf::{HexDoc, HexGeom, hex_char_x, hex_chars, human_size, offset_digits};

pub struct HexElement {
    pub editor: Entity<Editor>,
    pub pane: usize,
}

impl IntoElement for HexElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

/// A shaped line and where it goes.
struct Text {
    line: ShapedLine,
    origin: Point<Pixels>,
}

pub struct HexPrepaint {
    background: PaintQuad,
    /// Row highlight, selection, cursor: under the text.
    under: Vec<PaintQuad>,
    texts: Vec<Text>,
    /// Separators and the inspector strip's background.
    over: Vec<PaintQuad>,
    footer: Vec<Text>,
    line_height: Pixels,
}

/// Theme colors, resolved once per frame.
struct Palette {
    bg: Hsla,
    fg: Hsla,
    dim: Hsla,
    null: Hsla,
    whitespace: Hsla,
    control: Hsla,
    high: Hsla,
    cursor: Hsla,
    selection: Hsla,
    row: Hsla,
    rule: Hsla,
    footer_bg: Hsla,
    modified: Hsla,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let fg = theme.foreground();
        let dim = theme.color("dim", Hsla { alpha: 0.55, ..fg });
        Self {
            bg: theme.background(),
            fg,
            dim,
            null: theme.color("hex-null", Hsla { alpha: 0.35, ..fg }),
            whitespace: theme.color("hex-whitespace", theme.color("string", rgb(0x98c379))),
            control: theme.color("hex-control", theme.color("number", rgb(0xd19a66))),
            high: theme.color("hex-high", theme.color("keyword", rgb(0xc678dd))),
            cursor: theme.color("cursor", rgb(0x528bff)),
            selection: theme.color(
                "region",
                Hsla {
                    alpha: 0.25,
                    ..rgb(0x3a5fcd)
                },
            ),
            row: theme.color("hl-line", Hsla { alpha: 0.05, ..fg }),
            rule: Hsla { alpha: 0.12, ..fg },
            footer_bg: theme.color("mode-line-bg", Hsla { alpha: 0.04, ..fg }),
            modified: theme.color("diagnostic-warning", rgb(0xe5c07b)),
        }
    }

    fn byte(&self, b: u8) -> Hsla {
        match ByteClass::of(b) {
            ByteClass::Null => self.null,
            ByteClass::Printable => self.fg,
            ByteClass::Whitespace => self.whitespace,
            ByteClass::Control => self.control,
            ByteClass::High => self.high,
        }
    }
}

fn run(len: usize, font: &Font, color: Hsla) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    }
}

/// Builds one line of text with per-span colours.
struct Line<'a> {
    text: String,
    runs: Vec<TextRun>,
    font: &'a Font,
}

impl<'a> Line<'a> {
    fn new(font: &'a Font) -> Self {
        Self {
            text: String::new(),
            runs: Vec::new(),
            font,
        }
    }

    fn push(&mut self, s: &str, color: Hsla) {
        if s.is_empty() {
            return;
        }
        self.text.push_str(s);
        match self.runs.last_mut() {
            Some(r) if r.color == color => r.len += s.len(),
            _ => self.runs.push(run(s.len(), self.font, color)),
        }
    }

    /// Pad with spaces to `chars` characters (ASCII-only lines).
    fn pad_to(&mut self, chars: usize, color: Hsla) {
        let have = self.text.chars().count();
        if have < chars {
            self.push(&" ".repeat(chars - have), color);
        }
    }

    fn shape(self, size: Pixels, window: &mut Window) -> Option<ShapedLine> {
        if self.text.is_empty() {
            return None;
        }
        Some(
            window
                .text_system()
                .shape_line(SharedString::from(self.text), size, &self.runs, None),
        )
    }
}

/// One `0`'s advance in `font` at `size`.
fn char_advance(window: &mut Window, font: &Font, size: Pixels) -> f32 {
    let zero =
        window
            .text_system()
            .shape_line("0".into(), size, &[run(1, font, Hsla::default())], None);
    f32::from(zero.x_for_index(1)).max(1.0)
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(px(x), px(y)),
        size: size(px(w.max(0.0)), px(h.max(0.0))),
    }
}

fn outline(out: &mut Vec<PaintQuad>, x: f32, y: f32, w: f32, h: f32, color: Hsla) {
    let t = 1.0;
    out.push(fill(rect(x, y, w, t), color));
    out.push(fill(rect(x, y + h - t, w, t), color));
    out.push(fill(rect(x, y, t, h), color));
    out.push(fill(rect(x + w - t, y, t, h), color));
}

/// Gap between columns, in characters.
const GAP: usize = 2;

/// Bytes per row that fit `avail` characters, at most 256.
fn fit_width(avail: usize, digits: usize, group: usize) -> usize {
    let need = |w: usize| digits + GAP + hex_chars(w, group) + GAP + w;
    let mut w = 1;
    while w < 256 && need(w + 1) <= avail {
        w += 1;
    }
    w
}

impl Element for HexElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<HexPrepaint>;

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
        let pal = Palette::new(theme);
        let mono = font(settings.font_family().to_string());
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let lh = f32::from(line_height).max(1.0);
        let cw = char_advance(window, &mono, font_size);

        let editor = self.editor.read(cx);
        let h: &HexDoc = editor.pane_hex(self.pane)?;
        let selected = editor.pane_is_selected(self.pane);
        let read_only = editor.pane_doc(self.pane).read_only;
        let modified = h.data.modified();
        let len = h.data.len();
        let v = &h.view;

        let (x0, y0) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let pad = cw;
        let digits = offset_digits(len);
        let group = v.group.max(1);
        let avail = ((bw - 2.0 * pad) / cw).max(0.0) as usize;
        let fit = fit_width(avail, digits, group);
        let mut view = v.clone();
        view.fit_width = fit;
        let w = view.row_width();
        let footer_lines = if v.inspector { 2 } else { 1 };
        let footer_h = footer_lines as f32 * lh + 6.0;
        let header_h = lh + 2.0;
        let body_h = (bh - header_h - footer_h).max(lh);
        let rows = ((body_h / lh) as usize).max(1);
        let last_row = len / w;
        let top = v.top.min(last_row);

        let hex_x = pad + (digits + GAP) as f32 * cw;
        let ascii_x = hex_x + (hex_chars(w, group) + GAP) as f32 * cw;
        let rows_y = header_h;

        let mut p = HexPrepaint {
            background: fill(bounds, pal.bg),
            under: Vec::new(),
            texts: Vec::new(),
            over: Vec::new(),
            footer: Vec::new(),
            line_height,
        };

        // Header: column offsets over the hex and ASCII columns.
        {
            let col = v.cursor % w;
            let mut line = Line::new(&mono);
            line.push("offset", pal.dim);
            line.pad_to(digits + GAP, pal.dim);
            for i in 0..w {
                line.pad_to(digits + GAP + hex_char_x(i, group), pal.dim);
                let c = if i == col { pal.fg } else { pal.dim };
                line.push(&format!("{:02x}", i % 256), c);
            }
            line.pad_to(digits + GAP + hex_chars(w, group) + GAP, pal.dim);
            for i in 0..w {
                let c = if i == col { pal.fg } else { pal.dim };
                line.push(&format!("{:x}", i % 16), c);
            }
            if let Some(shaped) = line.shape(font_size, window) {
                p.texts.push(Text {
                    line: shaped,
                    origin: point(px(x0 + pad), px(y0)),
                });
            }
            p.over
                .push(fill(rect(x0, y0 + header_h - 1.0, bw, 1.0), pal.rule));
        }

        let sel = v.selection(len);
        let cursor_row = v.cursor / w;
        let data = h.data.bytes();
        for r in 0..rows {
            let row = top + r;
            if row > last_row {
                break;
            }
            let start = row * w;
            let y = y0 + rows_y + r as f32 * lh;
            if row == cursor_row && selected {
                p.under.push(fill(rect(x0, y, bw, lh), pal.row));
            }
            let end = (start + w).min(len);
            let bytes = &data[start.min(len)..end];

            // Selection: one band per column across the row's selected bytes.
            if let Some(sel) = &sel {
                let a = sel.start.max(start);
                let b = sel.end.min(end);
                if a < b {
                    let (ia, ib) = (a - start, b - 1 - start);
                    let hx = x0 + hex_x + hex_char_x(ia, group) as f32 * cw;
                    let hw = (hex_char_x(ib, group) + 2 - hex_char_x(ia, group)) as f32 * cw;
                    p.under.push(fill(rect(hx, y, hw, lh), pal.selection));
                    let ax = x0 + ascii_x + ia as f32 * cw;
                    p.under.push(fill(
                        rect(ax, y, (ib - ia + 1) as f32 * cw, lh),
                        pal.selection,
                    ));
                }
            }

            // Cursor: a solid block in the focused column (on the nibble being
            // typed in the hex column), an outline in the other.
            let at_cursor = row == cursor_row;
            if at_cursor {
                let i = v.cursor - start;
                let hx = x0 + hex_x + hex_char_x(i, group) as f32 * cw;
                let ax = x0 + ascii_x + i as f32 * cw;
                let solid = Hsla {
                    alpha: if selected { 0.9 } else { 0.35 },
                    ..pal.cursor
                };
                let faint = Hsla {
                    alpha: 0.6,
                    ..pal.cursor
                };
                if v.ascii {
                    p.under.push(fill(rect(ax, y, cw, lh), solid));
                    outline(&mut p.under, hx, y, 2.0 * cw, lh, faint);
                } else {
                    let (nx, nw) = if v.low_nibble {
                        (hx + cw, cw)
                    } else if v.editing || !editor.evil_intercepts() {
                        (hx, cw)
                    } else {
                        (hx, 2.0 * cw)
                    };
                    p.under.push(fill(rect(nx, y, nw, lh), solid));
                    outline(&mut p.under, ax, y, cw, lh, faint);
                }
            }
            let cursor_text = |i: usize, hex_col: bool| -> bool {
                selected && at_cursor && v.cursor - start == i && hex_col != v.ascii
            };

            // Offset.
            let mut off = Line::new(&mono);
            let off_color = if at_cursor { pal.fg } else { pal.dim };
            off.push(&format!("{start:0digits$x}"), off_color);
            // Hex column.
            let mut hexl = Line::new(&mono);
            for (i, &b) in bytes.iter().enumerate() {
                hexl.pad_to(hex_char_x(i, group), pal.dim);
                let s = format!("{b:02x}");
                if cursor_text(i, true) {
                    // Over the solid cursor: background-coloured, split per
                    // nibble so only the block's digit inverts.
                    let (hi, lo) = s.split_at(1);
                    let block_full = !v.low_nibble && !(v.editing || !editor.evil_intercepts());
                    if v.low_nibble {
                        hexl.push(hi, pal.byte(b));
                        hexl.push(lo, pal.bg);
                    } else if block_full {
                        hexl.push(&s, pal.bg);
                    } else {
                        hexl.push(hi, pal.bg);
                        hexl.push(lo, pal.byte(b));
                    }
                } else {
                    hexl.push(&s, pal.byte(b));
                }
            }
            // ASCII column.
            let mut asc = Line::new(&mono);
            for (i, &b) in bytes.iter().enumerate() {
                let c = hexview::ascii_char(b);
                let color = if cursor_text(i, false) {
                    pal.bg
                } else if c == '.' && b != b'.' {
                    Hsla {
                        alpha: pal.dim.alpha * 0.8,
                        ..pal.byte(b)
                    }
                } else {
                    pal.byte(b)
                };
                let mut buf = [0u8; 4];
                asc.push(c.encode_utf8(&mut buf), color);
            }
            for (line, x) in [(off, pad), (hexl, hex_x), (asc, ascii_x)] {
                if let Some(shaped) = line.shape(font_size, window) {
                    p.texts.push(Text {
                        line: shaped,
                        origin: point(px(x0 + x), px(y)),
                    });
                }
            }
        }

        // Column rules between offset | hex | ascii.
        let rule_top = y0 + header_h;
        let rule_h = (bh - header_h - footer_h).max(0.0);
        for x in [hex_x - cw, ascii_x - cw] {
            p.over
                .push(fill(rect(x0 + x, rule_top, 1.0, rule_h), pal.rule));
        }

        // Footer: position, mode, then the inspector.
        {
            let fy = y0 + bh - footer_h;
            p.over.push(fill(rect(x0, fy, bw, footer_h), pal.footer_bg));
            p.over.push(fill(rect(x0, fy, bw, 1.0), pal.rule));
            let mut l1 = Line::new(&mono);
            l1.push(&format!("0x{:0digits$x}", v.cursor), pal.fg);
            l1.push(&format!(" ({})", v.cursor), pal.dim);
            l1.push(&format!("  of {}", human_size(len)), pal.dim);
            if let Some(sel) = &sel {
                l1.push("  sel ", pal.dim);
                l1.push(
                    &format!(
                        "0x{:x}–0x{:x} ({} bytes)",
                        sel.start,
                        sel.end - 1,
                        sel.len()
                    ),
                    pal.fg,
                );
            }
            l1.push("   ", pal.dim);
            l1.push(if v.insert { "INS" } else { "OVR" }, pal.fg);
            l1.push(
                if v.ascii {
                    "  text column"
                } else {
                    "  hex column"
                },
                pal.dim,
            );
            if editor.evil_intercepts() {
                l1.push(
                    if v.editing {
                        "  editing"
                    } else {
                        "  (i to edit)"
                    },
                    if v.editing { pal.modified } else { pal.dim },
                );
            }
            if read_only {
                l1.push("  read-only", pal.modified);
            } else if modified {
                l1.push("  modified", pal.modified);
            }
            if let Some(shaped) = l1.shape(font_size, window) {
                p.footer.push(Text {
                    line: shaped,
                    origin: point(px(x0 + pad), px(fy + 3.0)),
                });
            }
            if v.inspector {
                let mut l2 = Line::new(&mono);
                let fields = inspect::fields(data, v.cursor, v.big_endian);
                if fields.is_empty() {
                    l2.push("end of buffer — typing appends", pal.dim);
                } else {
                    l2.push(if v.big_endian { "BE " } else { "LE " }, pal.dim);
                    for (label, value) in fields {
                        l2.push(&format!(" {label} "), pal.dim);
                        l2.push(&value, pal.fg);
                    }
                }
                if let Some(shaped) = l2.shape(font_size, window) {
                    p.footer.push(Text {
                        line: shaped,
                        origin: point(px(x0 + pad), px(fy + 3.0 + lh)),
                    });
                }
            }
        }

        let geom = HexGeom {
            origin: bounds.origin,
            char_w: cw,
            line_h: lh,
            rows_y,
            rows,
            hex_x,
            ascii_x,
            width: w,
            group,
            top,
        };
        let pane = self.pane;
        self.editor.update(cx, |editor, _| {
            editor.set_hex_geometry(pane, geom, fit);
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
        // Claim the typed-text input handler for the selected hex pane, as
        // every editor surface does: the minibuffer (save prompts, search,
        // goto) receives typed characters through it.
        if self.editor.read(cx).pane_is_selected(self.pane) {
            let focus_handle = self.editor.read(cx).focus_handle.clone();
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }
        let Some(p) = prepaint.as_mut() else {
            return;
        };
        let lh = p.line_height;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(p.background.clone());
            for q in p.under.drain(..) {
                window.paint_quad(q);
            }
            for t in p.texts.drain(..) {
                t.line
                    .paint(t.origin, lh, TextAlign::Left, None, window, cx)
                    .ok();
            }
            for q in p.over.drain(..) {
                window.paint_quad(q);
            }
            for t in p.footer.drain(..) {
                t.line
                    .paint(t.origin, lh, TextAlign::Left, None, window, cx)
                    .ok();
            }
        });
        let pane = self.pane;
        self.editor.update(cx, |editor, _| {
            editor.report_pane_bounds(pane, bounds);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::fit_width;

    #[test]
    fn fit_width_counts_every_column() {
        // 8 offset + 2 + 48 hex + 2 + 16 ascii = 76 characters for 16 bytes.
        assert_eq!(fit_width(76, 8, 1), 16);
        assert_eq!(fit_width(75, 8, 1), 15);
        assert_eq!(fit_width(0, 8, 1), 1);
    }
}
