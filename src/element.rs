use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Corners, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, Font, FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, LayoutId, PaintQuad, Pixels, Point, RenderImage, ShapedLine, SharedString,
    Style, TextAlign, TextRun, TransformationMatrix, UnderlineStyle, Window, WrappedLine, fill,
    font, point, prelude::*, px, relative, size,
};

use crate::color_compat::{rgb, rgba};

use crate::editor::{CompletionView, Editor};
use crate::highlight::{HlSpan, face_name};
use crate::lsp::Severity;
use crate::theme::{Theme, Underline};

mod popup;
mod terminal;

pub(crate) use popup::{PopupLayout, build_popup};
pub use terminal::TerminalElement;

/// Horizontal padding on each side of the line-number gutter.
const GUTTER_PAD: Pixels = px(8.);
/// Width of the overlay scroll-bar track/thumb on the pane's right edge.
const SCROLLBAR_WIDTH: Pixels = px(6.);
/// Minimum height of the scroll-bar thumb so it stays grabbable on long buffers.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
/// Alpha multiplier applied to a block cursor so the glyph under it stays legible.
const BLOCK_CURSOR_ALPHA: f32 = 0.5;
/// File-tree icon size as a fraction of the line height.
const ICON_SCALE: f32 = 0.68;
/// Diameter of a diagnostic gutter dot.
const DIAG_DOT: f32 = 7.0;

/// A plain (unstyled) text run: a single font/color with no background,
/// underline, or strikethrough. The common case for shaped UI/text lines.
fn plain_run(len: usize, font: Font, color: Hsla) -> TextRun {
    TextRun {
        len,
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    }
}

/// The run for ghost (inline-suggestion) text: dim + italic. One constructor
/// so the spliced first line and the painted continuation lines can't drift
/// in style.
fn ghost_run(len: usize, font: &Font, color: Hsla) -> TextRun {
    let mut run = plain_run(len, font.clone(), color);
    run.font.style = FontStyle::Italic;
    run
}

/// Diagnostic colors, resolved from the theme once per prepaint. The faces are
/// `diagnostic-error` / `diagnostic-warning` / `diagnostic-info` /
/// `diagnostic-hint` (set by every bundled theme), with the previous fixed
/// hexes as fallbacks for themes that don't define them.
#[derive(Clone, Copy)]
struct DiagnosticColors {
    error: Hsla,
    warning: Hsla,
    info: Hsla,
    hint: Hsla,
}

impl DiagnosticColors {
    fn resolve(theme: &Theme) -> Self {
        let info = theme.color("diagnostic-info", rgb(0x3794ff));
        Self {
            error: theme.color("diagnostic-error", rgb(0xf14c4c)),
            warning: theme.color("diagnostic-warning", rgb(0xcca700)),
            info,
            hint: theme.color("diagnostic-hint", info),
        }
    }

    fn for_severity(&self, severity: Severity) -> Hsla {
        match severity {
            Severity::Error => self.error,
            Severity::Warning => self.warning,
            Severity::Info => self.info,
            Severity::Hint => self.hint,
        }
    }
}

/// A shaped, positioned line, cached after paint so the editor can do
/// hit-testing (mouse), IME bounds queries, and vertical cursor motion without
/// re-shaping. Only the currently visible lines are cached.
pub struct LineLayout {
    /// The shaped, soft-wrapped line. When word-wrap is off it has no wrap
    /// boundaries (a single visual row); when on it may span several rows.
    /// Shaped over the *display* text — `overlay-hide` byte ranges (`holes`) are
    /// spliced out before shaping, so indices into `wrapped` are display indices.
    pub wrapped: WrappedLine,
    /// Y offset of this line relative to the element's origin (already adjusted
    /// for the scroll position, so it may be negative for the top line).
    pub top: Pixels,
    /// Byte range of the line in the buffer (line break excluded).
    pub range: Range<usize>,
    /// Hidden byte ranges within this line, relative to `range.start`, sorted and
    /// non-overlapping (from `overlay-hide`). Empty for the common case, where the
    /// display text equals the buffer text and the maps below are the identity.
    pub holes: Vec<(usize, usize)>,
    /// Virtual left indent (px) the line text is shifted by (`set-line-indent`,
    /// the Org indent look). Subtracted from a click's x before hit-testing.
    pub indent: Pixels,
    /// Painted height of this line (rows it occupies × line height). Equals the
    /// shaped height for ordinary lines, but for a scaled heading it's the full
    /// reserved block — so click hit-testing matches what was painted.
    pub height: Pixels,
    /// Vertical offset from `top` at which the glyphs are painted, to center a
    /// scaled heading in its taller reserved block. Zero for ordinary lines.
    pub content_dy: Pixels,
    /// Ghost text spliced *into* the display text (the inverse of `holes`):
    /// `(display index it was inserted at, its byte length)`. The insertion
    /// point is where point sits, so the cursor renders before the ghost.
    pub ghost: Option<(usize, usize)>,
}

impl LineLayout {
    /// Buffer offset (relative to the line start) → index into the display text.
    pub fn to_display(&self, rel: usize) -> usize {
        ghost_to_display(to_display(rel, &self.holes), self.ghost)
    }
    /// Index into the display text → buffer offset (relative to the line start).
    // Named to pair with `to_display`, not a `From`-style constructor.
    #[allow(clippy::wrong_self_convention)]
    pub fn from_display(&self, disp: usize) -> usize {
        from_display(ghost_from_display(disp, self.ghost), &self.holes)
    }
}

/// Shift a display index past ghost text inserted at `(at, len)`. An index at
/// exactly `at` does NOT shift — that's point itself, which renders before the
/// ghost; everything after it moves right by the ghost's length.
fn ghost_to_display(disp: usize, ghost: Option<(usize, usize)>) -> usize {
    match ghost {
        Some((at, len)) if disp > at => disp + len,
        _ => disp,
    }
}

/// The display index past the end of a line whose real text ends at display
/// index `end`, with ghost text `(at, len)` spliced in at or before it.
fn display_end(end: usize, ghost: Option<(usize, usize)>) -> usize {
    match ghost {
        Some((at, len)) if at <= end => end + len,
        _ => end,
    }
}

/// Inverse of [`ghost_to_display`]: an index inside the ghost text collapses to
/// its insertion point (a click on the suggestion lands at point).
fn ghost_from_display(disp: usize, ghost: Option<(usize, usize)>) -> usize {
    match ghost {
        Some((at, len)) if disp > at => disp.saturating_sub(len).max(at),
        _ => disp,
    }
}

/// Map a line-relative buffer offset to its index in the display text, where the
/// byte ranges in `holes` have been removed. An offset inside a hole clamps to
/// that hole's start. `holes` must be sorted and non-overlapping.
fn to_display(rel: usize, holes: &[(usize, usize)]) -> usize {
    let mut removed = 0;
    for &(hs, he) in holes {
        if rel <= hs {
            break;
        }
        if rel < he {
            return hs - removed;
        }
        removed += he - hs;
    }
    rel - removed
}

/// Inverse of [`to_display`]: a display-text index back to a line-relative buffer
/// offset (landing just past any hole it sits at).
fn from_display(disp: usize, holes: &[(usize, usize)]) -> usize {
    let mut removed = 0;
    for &(hs, he) in holes {
        if disp < hs - removed {
            break;
        }
        removed += he - hs;
    }
    disp + removed
}

/// The line text with `holes` (line-relative byte ranges) spliced out.
fn remove_holes(text: &str, holes: &[(usize, usize)]) -> String {
    if holes.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for &(hs, he) in holes {
        if hs > pos {
            out.push_str(&text[pos..hs]);
        }
        pos = he.max(pos);
    }
    if pos < text.len() {
        out.push_str(&text[pos..]);
    }
    out
}

/// Shorten the per-line `TextRun`s so they cover the display text instead of the
/// full buffer line: each run loses the bytes it shares with a hole, and a run
/// fully inside a hole is dropped. Runs are in buffer-line order.
fn punch_runs(runs: Vec<TextRun>, holes: &[(usize, usize)]) -> Vec<TextRun> {
    if holes.is_empty() {
        return runs;
    }
    let mut out = Vec::with_capacity(runs.len());
    let mut pos = 0;
    for mut r in runs {
        let (start, end) = (pos, pos + r.len);
        pos = end;
        let mut overlap = 0;
        for &(hs, he) in holes {
            let (os, oe) = (hs.max(start), he.min(end));
            if oe > os {
                overlap += oe - os;
            }
        }
        if r.len > overlap {
            r.len -= overlap;
            out.push(r);
        }
    }
    out
}

/// Splice a run for ghost text into the display-text `runs` at display index
/// `at` (the inverse of `punch_runs`): the run covering `at` is split, and the
/// ghost's own run inserted between the halves. Runs must already cover the
/// ghost-less display text (holes punched).
fn inject_ghost_run(runs: Vec<TextRun>, at: usize, ghost_run: TextRun) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut pos = 0;
    let mut inserted = false;
    for mut r in runs {
        let start = pos;
        pos += r.len;
        if !inserted && at <= start {
            out.push(ghost_run.clone());
            inserted = true;
        }
        if !inserted && at < pos {
            let mut head = r.clone();
            head.len = at - start;
            r.len -= head.len;
            out.push(head);
            out.push(ghost_run.clone());
            inserted = true;
        }
        out.push(r);
    }
    if !inserted {
        out.push(ghost_run); // at end of line (or an empty line)
    }
    out
}

/// The custom GPUI element that draws the buffer. GPUI ships no multi-line text
/// widget, so we shape and paint each visible line ourselves, register the
/// editor as the platform input handler, and clip to the viewport.
pub struct EditorElement {
    pub editor: Entity<Editor>,
    /// Which window pane this element draws.
    pub pane: usize,
}

pub struct PrepaintState {
    lines: Vec<LineLayout>,
    cursor: Option<PaintQuad>,
    /// Full-width background band behind the cursor's line (Emacs `hl-line-mode`).
    hl_line: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
    /// Background highlight quads for the active search query's matches.
    search: Vec<PaintQuad>,
    /// Background highlight quads for the bracket at point and its match
    /// (Emacs `show-paren-mode`).
    paren: Vec<PaintQuad>,
    /// Scroll-bar track + thumb quads (Emacs scroll-bar-mode), right edge.
    scrollbar: Vec<PaintQuad>,
    /// Per-line SVG icons (file tree): (bounds, asset path, tint color).
    icons: Vec<(Bounds<Pixels>, SharedString, Hsla)>,
    /// Inline raster images (Org / LaTeX preview): (bounds, decoded bitmap).
    images: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
    /// Squiggle/underline quads for LSP diagnostics.
    diagnostics: Vec<PaintQuad>,
    /// git-gutter (diff-hl) change markers, painted at the gutter's far-left
    /// edge: full-height bars for added/modified lines, a short top-edge bar
    /// where lines were deleted.
    git_gutter: Vec<PaintQuad>,
    /// The completion popup overlay, if open.
    popup: Option<PopupLayout>,
    /// Left edge of the text area (past the line-number gutter, if any).
    text_left: Pixels,
    /// Where line text is painted: `text_left` shifted by the horizontal scroll
    /// (equal to `text_left` when word-wrap is on, since h-scroll is then 0).
    line_x: Pixels,
    /// One monospace character's advance, reported to the editor for h-scroll.
    char_width: Pixels,
    /// Shaped line-number labels + where to paint them (in the gutter).
    gutter: Vec<(ShapedLine, Point<Pixels>)>,
    /// Fold chevrons painted in the gutter: (bounds, asset path, tint color).
    fold_chevrons: Vec<(Bounds<Pixels>, SharedString, Hsla)>,
    /// Clickable region + line for each fold chevron, reported to the editor so a
    /// gutter click toggles the right fold.
    fold_hitboxes: Vec<crate::editor::FoldHitbox>,
    /// gpui hitboxes over the chevrons, so the pointer becomes a hand on hover
    /// (set during paint via `set_cursor_style`).
    fold_cursors: Vec<Hitbox>,
    /// Shaped virtual suffix text (inlay/blame) + where to paint it.
    virtual_text: Vec<(ShapedLine, Point<Pixels>)>,
    /// avy jump hints: a background quad + shaped label, painted on top of the
    /// text at a byte position (e.g. "a", "sd"). One per visible candidate.
    avy: Vec<(PaintQuad, ShapedLine, Point<Pixels>)>,
    /// Secondary cursor carets (multiple-cursors), painted like the primary one.
    extra_cursors: Vec<PaintQuad>,
    /// The primary cursor's top, relative to the viewport top (may be negative or
    /// past the bottom under wrapping). Fed back to the editor so it can follow the
    /// cursor against the real painted geometry. `None` when the cursor isn't laid
    /// out (e.g. scrolled fully off, or this isn't the selected pane).
    cursor_rel_y: Option<Pixels>,
}

/// Per-line geometry shared by the prepaint helpers: which buffer line it is,
/// where it sits, its byte range, and the display-text mapping (`holes`)
/// needed to position anything within it.
struct LineCtx<'a> {
    /// Buffer line index.
    i: usize,
    /// Byte range of the line in the buffer (line break excluded).
    range: Range<usize>,
    /// The shaped, soft-wrapped display text.
    wrapped: &'a WrappedLine,
    /// Hidden byte ranges spliced out of the display text (line-relative).
    holes: &'a [(usize, usize)],
    /// Y offset of the line's top relative to the element origin.
    top: Pixels,
    /// Painted height of the line (rows it occupies × line height).
    line_h: Pixels,
    /// Window-space top-left of the glyphs (indent, scroll, and any scaled-
    /// heading centering already applied); `.y` is the content top.
    line_origin: Point<Pixels>,
    /// Uniform row height used for positioning within the line.
    line_height: Pixels,
    /// Ghost text spliced into the display text at `(display index, byte len)`
    /// — see [`LineLayout::ghost`].
    ghost: Option<(usize, usize)>,
}

impl LineCtx<'_> {
    /// Display index of an absolute buffer offset at or after the line start.
    fn disp(&self, offset: usize) -> usize {
        ghost_to_display(
            to_display(offset.saturating_sub(self.range.start), self.holes),
            self.ghost,
        )
    }

    /// Display index just past *everything* on the line, including ghost text
    /// anchored at end of line — `disp(range.end)` stops before such a ghost
    /// (an index at the anchor is point, which renders before it), so virtual
    /// suffixes placed there would overlap the suggestion.
    fn disp_end(&self) -> usize {
        display_end(
            to_display(self.range.end - self.range.start, self.holes),
            self.ghost,
        )
    }

    /// Emit background quads for the absolute byte range `[s, e)` where it
    /// overlaps this line — the shared body of the selection / search / paren
    /// / diagnostic highlights.
    fn push_range(
        &self,
        out: &mut Vec<PaintQuad>,
        s: usize,
        e: usize,
        color: Hsla,
        min_width: Pixels,
    ) {
        if s > self.range.end || e < self.range.start {
            return;
        }
        push_range_quads(
            out,
            self.wrapped,
            self.disp(s.max(self.range.start)),
            self.disp(e.min(self.range.end)),
            self.line_origin,
            self.line_height,
            color,
            min_width,
        );
    }
}

/// The per-frame accumulators the per-line helpers append into, drained into
/// [`PrepaintState`] once the line loop finishes.
#[derive(Default)]
struct Accum {
    selections: Vec<PaintQuad>,
    search: Vec<PaintQuad>,
    paren: Vec<PaintQuad>,
    diagnostics: Vec<PaintQuad>,
    git_gutter: Vec<PaintQuad>,
    icons: Vec<(Bounds<Pixels>, SharedString, Hsla)>,
    avy: Vec<(PaintQuad, ShapedLine, Point<Pixels>)>,
    extra_cursors: Vec<PaintQuad>,
    gutter: Vec<(ShapedLine, Point<Pixels>)>,
    fold_chevrons: Vec<(Bounds<Pixels>, SharedString, Hsla)>,
    fold_hitboxes: Vec<crate::editor::FoldHitbox>,
    fold_cursors: Vec<Hitbox>,
    virtual_text: Vec<(ShapedLine, Point<Pixels>)>,
    lines: Vec<LineLayout>,
    /// The primary cursor's quad (`None` when blinked off or not laid out).
    cursor: Option<PaintQuad>,
    hl_line: Option<PaintQuad>,
    /// The primary cursor's origin (also the completion-popup anchor, tracked
    /// even when blinked off).
    cursor_px: Option<Point<Pixels>>,
    images: Vec<(Bounds<Pixels>, Arc<RenderImage>)>,
}

/// The theme colors a frame paints with, resolved once per prepaint (like
/// [`DiagnosticColors`]), each with a fixed fallback for themes that don't
/// define its face.
#[derive(Clone, Copy)]
struct FrameColors {
    cursor: Hsla,
    region: Hsla,
    /// Helix multiple selections: each secondary selection paints a dimmer
    /// span, and its caret a dimmer cursor, so the primary stays distinct.
    secondary_region: Hsla,
    secondary_cursor: Hsla,
    search: Hsla,
    paren: Hsla,
    hl_line: Hsla,
    line_number: Hsla,
    git_added: Hsla,
    git_modified: Hsla,
    git_removed: Hsla,
}

impl FrameColors {
    /// `text` is the frame's foreground, the line numbers' last fallback.
    fn resolve(theme: &Theme, text: Hsla) -> Self {
        let cursor = theme.color("cursor", rgb(0x528bff));
        let region = theme.color("region", rgba(0x3a5fcd55));
        // Themes can set the secondary colors explicitly; otherwise derive
        // them from the primary region/cursor by knocking the alpha down.
        let dimmed = |mut c: Hsla, by: f32| {
            c.alpha *= by;
            c
        };
        FrameColors {
            cursor,
            region,
            secondary_region: theme.color("secondary-region", dimmed(region, 0.6)),
            secondary_cursor: theme.color("secondary-cursor", dimmed(cursor, 0.7)),
            search: theme.color("search", rgba(0xffc83d66)),
            paren: theme.color("show-paren", rgba(0x5fafef66)),
            hl_line: theme.color("hl-line", rgba(0xffffff10)),
            line_number: theme.color("line-number", theme.color("dim", text)),
            git_added: theme.color("magit-gutter-added", rgb(0x6cae3e)),
            git_modified: theme.color("magit-gutter-modified", rgb(0xc7a23a)),
            git_removed: theme.color("magit-gutter-removed", rgb(0xc1574b)),
        }
    }
}

/// Everything [`FrameCtx::layout_line`] needs about the frame being laid out,
/// resolved once per prepaint: the editor state for this pane, fonts and
/// geometry, colors, and the visible-range highlight data.
struct FrameCtx<'a> {
    editor: &'a Editor,
    pane: usize,
    selected: bool,
    buffer: &'a crate::buffer::Buffer,
    /// The pane buffer's narrowing (accessible byte range), if any.
    narrow: Option<std::ops::Range<usize>>,
    theme: &'a Theme,
    colors: FrameColors,
    text_color: Hsla,
    font: Font,
    font_size: Pixels,
    /// One monospace character's advance.
    em: Pixels,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
    text_left: Pixels,
    /// `text_left` shifted by the horizontal scroll.
    text_x: Pixels,
    wrap_width: Option<Pixels>,
    scales: bool,
    images: bool,
    line_numbers: bool,
    hl_line: bool,
    cursor: usize,
    selection: Range<usize>,
    block_sel: Option<(usize, usize, usize, usize)>,
    extra_selections: Vec<(usize, usize)>,
    extra_cursor_positions: Vec<usize>,
    search: Vec<Range<usize>>,
    paren: Vec<Range<usize>>,
    highlights: Vec<HlSpan>,
    overlay_faces: &'a [(Range<usize>, SharedString)],
    ghost: Option<(usize, &'a str)>,
    ghost_first: &'a str,
    ghost_tail: Vec<&'a str>,
    ghost_color: Hsla,
    diagnostics: Vec<crate::lsp::Diagnostic>,
    diff_marks: &'a std::collections::HashMap<usize, crate::editor::DiffMark>,
    gutter_dots: &'a std::collections::HashMap<usize, SharedString>,
    diag_colors: DiagnosticColors,
    gutter: GutterCfg,
    cursor_cfg: CursorCfg,
}

impl FrameCtx<'_> {
    /// Lay out buffer line `i` with its top at `y` (advanced past the line,
    /// its ghost continuation rows and its inline image): shape it and collect
    /// its gutter, cursor, highlight and decoration output into `acc`.
    fn layout_line(&self, i: usize, y: &mut Pixels, acc: &mut Accum, window: &mut Window) {
        let lh = f32::from(self.line_height);
        let (mut range, mut text) = self.buffer.line(i);
        // A narrowing's boundary lines show only their accessible part.
        // `clip` is how many bytes that cut off the line's start.
        let (mut clipped, mut clip) = (false, 0);
        if let Some(n) = &self.narrow {
            let (s, e) = (range.start.max(n.start), range.end.min(n.end).max(n.start));
            if (s..e) != range {
                (clipped, clip) = (true, s - range.start);
                text = text[clip..e - range.start].to_string();
                range = s..e;
            }
        }
        let top = *y;

        // `overlay-hide` ranges intersecting this line, made line-relative and
        // restricted to char boundaries; spliced out before shaping so the
        // stars (etc.) collapse. Empty for almost every line. The line the
        // cursor sits on is revealed in full (Emacs-style) so hidden markup —
        // e.g. Org's leading stars — is visible and editable while point is
        // there; moving off the line re-hides it on the next frame.
        //
        // Selected pane only: `overlay-hide` ranges live on the *active*
        // buffer (`line_holes` reads `self.buffer`/`self.overlays`), so a
        // non-selected pane must not consult them — its rows index a different,
        // possibly longer buffer (e.g. a tall dired-sidebar tree beside a short
        // file), which would index the active buffer past its end. Matches the
        // `selected` gating on `overlay_faces` and the cursor-line reveal below.
        let mut holes = if self.selected {
            self.editor.line_holes(i)
        } else {
            Vec::new()
        };
        if clipped {
            holes = holes
                .into_iter()
                .map(|(s, e)| {
                    (
                        s.saturating_sub(clip),
                        e.saturating_sub(clip).min(text.len()),
                    )
                })
                .collect();
        }
        holes.retain(|&(s, e)| s < e && text.is_char_boundary(s) && text.is_char_boundary(e));
        // The cursor's own line is revealed in full so hidden markup — e.g.
        // Org's leading stars — is visible and editable while point is there.
        // The width those stars reclaim is taken back out of the indent below,
        // so the visible title stays put instead of jumping right as the
        // cursor arrives. Moving off the line re-hides it next frame.
        let revealed = self.selected && self.cursor >= range.start && self.cursor <= range.end;
        let reveal_cols: usize = if revealed {
            holes.iter().map(|&(s, e)| e - s).sum()
        } else {
            0
        };
        if revealed {
            holes.clear();
        }

        // Virtual left indent (Org `org-indent-mode`): shift this line's text
        // right by N columns. Display-only — the buffer text is untouched, so
        // there is no real whitespace to navigate (matches Emacs).
        let indent_px =
            px(self.editor.line_indent(i).saturating_sub(reveal_cols) as f32 * f32::from(self.em));
        let lx = self.text_x + indent_px;
        // Whole-line scale (markdown/org headings rendered larger). A scaled
        // line is shaped at `scale`× the base font and doesn't wrap; it
        // reserves `1 + scale_rows_at` display rows (claimed above), so the
        // bigger text sits in that taller block and the row math stays exact.
        let scale = if self.scales {
            self.editor.line_scale(i)
        } else {
            1.0
        };
        let line_font_size = self.font_size * scale;
        // Narrow the wrap width by the indent so wrapped rows stay in the pane.
        // Scaled heading lines never wrap (keeps them a single, taller block).
        let line_wrap_width = if scale > 1.0 {
            None
        } else {
            self.wrap_width
                .map(|w| px((f32::from(w) - f32::from(indent_px)).max(1.0)))
        };

        // Compute the styled runs, then shape once (the branches only
        // differ in which spans and base color tile the line).
        let runs = if text.is_empty() {
            Vec::new()
        } else if let Some(face) = self.editor.pane_line_face(self.pane, i) {
            // Special buffers (magit-style) paint whole lines by face name —
            // but sub-line overlay accents (magit's status labels, diff +/-
            // markers) still override their byte range, layered on top of the
            // whole-line color as the base (no syntax spans here). The face's
            // bold attribute rides along as the line's base weight.
            let color = self.theme.color(face, self.text_color);
            line_runs(
                &[],
                self.overlay_faces,
                range.clone(),
                &self.font,
                color,
                self.theme.is_bold(face),
                self.theme,
                Some(text.as_str()),
            )
        } else {
            line_runs(
                &self.highlights,
                self.overlay_faces,
                range.clone(),
                &self.font,
                self.text_color,
                false,
                self.theme,
                Some(text.as_str()),
            )
        };
        // Ghost anchored on this line (point's line): splice the
        // suggestion's first line into the display text at point — the
        // inverse of the `holes` splice-out — so the rest of the real line
        // is pushed right and soft-wrap measures the suggestion like real
        // text. Point's line is `revealed` above, so `holes` is empty here
        // and display indices equal line-relative buffer offsets.
        let ghost_here = self
            .ghost
            .filter(|(at, _)| *at >= range.start && *at <= range.end);
        let line_ghost = ghost_here
            .filter(|_| !self.ghost_first.is_empty())
            .map(|(at, _)| (to_display(at - range.start, &holes), self.ghost_first.len()));
        let mut display_text = remove_holes(&text, &holes);
        let mut display_runs = punch_runs(runs, &holes);
        if let Some((at, len)) = line_ghost {
            display_text.insert_str(at, self.ghost_first);
            display_runs = inject_ghost_run(
                display_runs,
                at,
                ghost_run(len, &self.font, self.ghost_color),
            );
        }
        let wrapped = window.text_system().shape_text(
            SharedString::from(display_text),
            line_font_size,
            &display_runs,
            line_wrap_width,
            None,
        );
        let wrapped = wrapped.ok().and_then(|mut v| v.pop()).unwrap_or_default();
        // A scaled line occupies the whole reserved block (its claimed rows ×
        // line height) so `y` advances by the rows it reserved above; an
        // unscaled line is just its shaped (possibly wrapped) height.
        let line_h = if scale > 1.0 {
            px((1 + self.editor.scale_rows_at(i)) as f32 * lh)
        } else {
            wrapped.size(self.line_height).height
        };

        // A scaled (heading) line reserves whole rows (`ceil(scale)`), taller
        // than the one base row a line normally occupies. `WrappedLine::paint`
        // vertically centers the glyph run within the `line_height` it's given
        // (here the base `lh`), so the glyphs' center sits one half-row below
        // `content_top`. To center them in the reserved block, offset the whole
        // line's content (glyphs, cursor, selection, inlays — and the line
        // number) down by half the slack: `(block − one base row) / 2`. Zero for
        // ordinary lines, which keeps wrapped multi-row lines top-aligned.
        let content_dy = if scale > 1.0 {
            (line_h - px(lh)) / 2.0
        } else {
            px(0.)
        };
        let content_top = self.bounds.top() + top + content_dy;
        let line_origin = point(lx, content_top);
        let lc = LineCtx {
            i,
            range: range.clone(),
            wrapped: &wrapped,
            holes: &holes,
            top,
            line_h,
            line_origin,
            line_height: self.line_height,
            ghost: line_ghost,
        };

        // Gutter: line number, fold chevron, diagnostic dot, git bar.
        if self.line_numbers {
            layout_gutter(
                &lc,
                &self.gutter,
                self.editor,
                self.theme,
                &self.diagnostics,
                self.diff_marks,
                self.gutter_dots,
                window,
                acc,
            );
        }

        if self.hl_line
            && self.cursor >= range.start
            && self.cursor <= range.end
            && acc.hl_line.is_none()
        {
            // Span the full pane width (gutter included), one line tall.
            let origin = point(self.bounds.left(), self.bounds.top() + top);
            acc.hl_line = Some(fill(
                Bounds::new(origin, size(self.bounds.size.width, line_h)),
                self.colors.hl_line,
            ));
        }

        // Cursors: the primary caret (also the completion-popup anchor,
        // tracked even when blinked off) and any secondary carets.
        if self.selected {
            if let Some((origin, quad)) =
                layout_cursor(&lc, self.buffer, self.cursor, &self.cursor_cfg)
            {
                acc.cursor_px = Some(origin);
                acc.cursor = quad;
            }
            layout_extra_cursors(
                &lc,
                &self.extra_cursor_positions,
                &self.cursor_cfg,
                &mut acc.extra_cursors,
            );
        }

        // Range highlights, all through the same per-row quad helper.
        if let Some((top_row, bottom_row, left_col, right_col)) = self.block_sel {
            // Block-wise Visual: highlight only the `[left_col, right_col]`
            // column strip on each row within the rectangle (rows shorter
            // than the strip contribute nothing).
            if i >= top_row && i <= bottom_row {
                let s = char_col_byte_in(&text, left_col);
                let e = char_col_byte_in(&text, right_col + 1);
                if e > s {
                    lc.push_range(
                        &mut acc.selections,
                        range.start + s,
                        range.start + e,
                        self.colors.region,
                        px(0.),
                    );
                }
            }
        } else if !self.selection.is_empty() {
            lc.push_range(
                &mut acc.selections,
                self.selection.start,
                self.selection.end,
                self.colors.region,
                px(0.),
            );
        }
        // Secondary selections (Helix multiple selections): one dimmer span
        // per selection that overlaps this line. Each carries its own color,
        // so they coexist with the primary highlight in the same quad list.
        for &(ss, se) in &self.extra_selections {
            if se > ss {
                lc.push_range(
                    &mut acc.selections,
                    ss,
                    se,
                    self.colors.secondary_region,
                    px(0.),
                );
            }
        }
        // Search matches: a translucent background highlight per match.
        for m in &self.search {
            lc.push_range(&mut acc.search, m.start, m.end, self.colors.search, px(0.));
        }
        // Matching-delimiter highlight: the bracket pair (each one char).
        for m in &self.paren {
            lc.push_range(&mut acc.paren, m.start, m.end, self.colors.paren, px(0.));
        }

        // Decorations: file-tree icon, diagnostic underlines, virtual
        // suffix text, avy hints.
        layout_decorations(
            &lc,
            self.editor,
            self.pane,
            self.selected,
            self.theme,
            self.text_color,
            &self.font,
            self.font_size,
            self.em,
            &self.diagnostics,
            self.diag_colors,
            window,
            acc,
        );

        acc.lines.push(LineLayout {
            wrapped,
            top,
            range,
            holes,
            indent: indent_px,
            height: line_h,
            content_dy,
            ghost: line_ghost,
        });
        *y += line_h;

        // Ghost continuation lines: painted on the display rows reserved
        // below point's line (the inline-image pattern), one per `\n` in
        // the suggestion. Not hit-testable — a click below the anchor line
        // maps to the following buffer line, and moving point dismisses
        // the ghost anyway.
        if ghost_here.is_some() {
            for t in &self.ghost_tail {
                if !t.is_empty() {
                    let shaped = window.text_system().shape_line(
                        SharedString::from(t.to_string()),
                        self.font_size,
                        &[ghost_run(t.len(), &self.font, self.ghost_color)],
                        None,
                    );
                    acc.virtual_text
                        .push((shaped, point(lx, self.bounds.top() + *y)));
                }
                *y += px(lh);
            }
        }

        // Inline image: paint it in the rows reserved below this line, and
        // advance past them. `image_rows_at` and the displayed size agree on
        // the fixed max width, so the reserved height matches the bitmap.
        if self.images
            && let Some((data, disp_w, disp_h)) = self.editor.line_image_at(i)
        {
            let img_origin = point(self.text_left + indent_px, self.bounds.top() + *y);
            acc.images
                .push((Bounds::new(img_origin, size(px(disp_w), px(disp_h))), data));
            *y += px(self.editor.image_rows_at(i) as f32 * lh);
        }
    }
}

/// Gutter configuration resolved once per frame: geometry, colors, and which
/// gutter features are enabled.
struct GutterCfg {
    bounds: Bounds<Pixels>,
    gutter_width: Pixels,
    gutter_pad: Pixels,
    /// Width of the fold-chevron column (0 when fold markers are off).
    fold_col: Pixels,
    chevron_sz: Pixels,
    fold_markers: bool,
    diag_gutter: bool,
    git_gutter: bool,
    git_gutter_w: Pixels,
    git_gutter_del: bool,
    gg_added: Hsla,
    gg_modified: Hsla,
    gg_removed: Hsla,
    number_color: Hsla,
    /// Buffer line shown as line 1 — the narrowing's first line, so numbers
    /// count from the accessible portion (Emacs's default).
    line_base: usize,
    font: Font,
    font_size: Pixels,
    diag_colors: DiagnosticColors,
}

/// Cursor styling resolved once per frame, shared by the primary and
/// secondary (multiple-cursors) carets.
struct CursorCfg {
    block: bool,
    width: Pixels,
    shown: bool,
    /// One monospace advance — block-cursor width past EOL.
    char_advance: Pixels,
    color: Hsla,
    secondary_color: Hsla,
}

/// Lay out one line's gutter: the line-number label, its fold chevron, the
/// diagnostic dot, and the git-gutter change bar. Only called when the
/// line-number gutter is shown (the other gutter features are gated on it).
#[allow(clippy::too_many_arguments)]
fn layout_gutter(
    line: &LineCtx,
    cfg: &GutterCfg,
    editor: &Editor,
    theme: &Theme,
    diagnostics: &[crate::lsp::Diagnostic],
    diff_marks: &std::collections::HashMap<usize, crate::editor::DiffMark>,
    gutter_dots: &std::collections::HashMap<usize, SharedString>,
    window: &mut Window,
    acc: &mut Accum,
) {
    let i = line.i;
    let bounds = cfg.bounds;

    // Line-number label, right-aligned in the gutter. Shares the line's
    // content offset, so on a scaled line it centers to the same midpoint.
    let label = SharedString::from((i - cfg.line_base + 1).to_string());
    let n = label.len();
    let shaped_num = window.text_system().shape_line(
        label,
        cfg.font_size,
        &[plain_run(n, cfg.font.clone(), cfg.number_color)],
        None,
    );
    let nx = bounds.left() + cfg.gutter_width - cfg.gutter_pad - shaped_num.x_for_index(n);
    acc.gutter.push((shaped_num, point(nx, line.line_origin.y)));

    // Fold chevron, centered in its reserved gutter column for any line Scheme
    // marked foldable: down = open (expanded), right = collapsed. The whole
    // column is the clickable/hover region, which is reported back so a click
    // toggles the fold.
    let marker = cfg.fold_markers.then(|| editor.fold_marker_at(i)).flatten();
    if let Some(open) = marker {
        let glyph = Bounds::new(
            point(
                bounds.left() + (cfg.fold_col - cfg.chevron_sz) / 2.0,
                bounds.top() + line.top + (line.line_h - cfg.chevron_sz) / 2.0,
            ),
            size(cfg.chevron_sz, cfg.chevron_sz),
        );
        let name = if open {
            "chevron-down"
        } else {
            "chevron-right"
        };
        acc.fold_chevrons.push((
            glyph,
            SharedString::from(format!("icons/{name}.svg")),
            cfg.number_color,
        ));
        let hit = Bounds::new(
            point(bounds.left(), bounds.top() + line.top),
            size(cfg.fold_col, line.line_h),
        );
        acc.fold_hitboxes.push(crate::editor::FoldHitbox {
            bounds: hit,
            line: i,
        });
        acc.fold_cursors
            .push(window.insert_hitbox(hit, HitboxBehavior::Normal));
    }

    // Breakpoint gutter dot (`set-gutter-dot`, dap.scm): same shape and spot
    // as the diagnostic dot, and it wins when both are present — the user put
    // it there, and a diagnostic still shows via its text-area underline.
    if let Some(face) = gutter_dots.get(&i) {
        let color = theme.color(face, cfg.gg_removed);
        let dot = px(DIAG_DOT);
        let dx = bounds.left() + px(3.);
        let dy = bounds.top() + line.top + px((f32::from(line.line_h) - DIAG_DOT) / 2.0);
        let mut quad = fill(Bounds::new(point(dx, dy), size(dot, dot)), color);
        quad.corner_radii = Corners::all(dot / 2.0);
        acc.diagnostics.push(quad);
    }
    // Diagnostic gutter dot: a colored circle on the far-left of the gutter
    // for any line carrying a diagnostic, colored by the worst severity
    // present (error > warning > info/hint).
    else if cfg.diag_gutter && marker.is_none() {
        let worst = diagnostics
            .iter()
            .filter(|d| d.range.start <= line.range.end && d.range.end >= line.range.start)
            .map(|d| d.severity)
            .min();
        if let Some(severity) = worst {
            let color = cfg.diag_colors.for_severity(severity);
            let dot = px(DIAG_DOT);
            let dx = bounds.left() + px(3.);
            let dy = bounds.top() + line.top + px((f32::from(line.line_h) - DIAG_DOT) / 2.0);
            let mut quad = fill(Bounds::new(point(dx, dy), size(dot, dot)), color);
            quad.corner_radii = Corners::all(dot / 2.0);
            acc.diagnostics.push(quad);
        }
    }

    // git-gutter (diff-hl): a full-height change bar at the gutter's left edge
    // for added/modified lines; a short top-edge bar for a deletion that
    // happened just above this line. Sits left of the diagnostic dot column,
    // so the two can coexist on a line.
    if cfg.git_gutter {
        use crate::editor::DiffMark;
        let gx = bounds.left() + px(1.);
        let gy = bounds.top() + line.top;
        match diff_marks.get(&i) {
            Some(DiffMark::Removed) if cfg.git_gutter_del => {
                let w = (cfg.git_gutter_w * 2.0).min(px(8.));
                let bar = Bounds::new(point(gx, gy), size(w, px(2.)));
                acc.git_gutter.push(fill(bar, cfg.gg_removed));
            }
            Some(mark @ (DiffMark::Added | DiffMark::Modified)) => {
                let color = if *mark == DiffMark::Added {
                    cfg.gg_added
                } else {
                    cfg.gg_modified
                };
                let bar = Bounds::new(point(gx, gy), size(cfg.git_gutter_w, line.line_h));
                acc.git_gutter.push(fill(bar, color));
            }
            // A deletion mark with the deleted-wedge option off, or no mark.
            Some(DiffMark::Removed) | None => {}
        }
    }
}

/// Lay out the primary cursor on this line: its pixel origin (the completion-
/// popup anchor, tracked even when blinked off) plus the paint quad when the
/// cursor is currently shown. `None` when the cursor isn't on this line.
fn layout_cursor(
    line: &LineCtx,
    buffer: &crate::buffer::Buffer,
    cursor: usize,
    cfg: &CursorCfg,
) -> Option<(Point<Pixels>, Option<PaintQuad>)> {
    if cursor < line.range.start || cursor > line.range.end {
        return None;
    }
    let cpos = line
        .wrapped
        .position_for_index(line.disp(cursor), line.line_height)
        .unwrap_or_default();
    let origin = point(line.line_origin.x + cpos.x, line.line_origin.y + cpos.y);
    let (width, color) = if cfg.block {
        // Width of the grapheme under the cursor (or one char at EOL), but
        // only if it sits on the same visual row. With ghost text at point the
        // next buffer grapheme renders past the whole suggestion — measuring
        // to it would stretch the block across the ghost, so fall back to one
        // character's advance (the block sits over the ghost's first char).
        let ghost_at_cursor = line.ghost.is_some_and(|(at, _)| line.disp(cursor) == at);
        let next = buffer.next_grapheme(cursor);
        let npos = (!ghost_at_cursor && next > cursor && next <= line.range.end)
            .then(|| {
                line.wrapped
                    .position_for_index(line.disp(next), line.line_height)
            })
            .flatten();
        let width = match npos {
            Some(np) if np.y == cpos.y && np.x > cpos.x => np.x - cpos.x,
            _ => cfg.char_advance,
        };
        // Translucent so the character under the block stays readable.
        let mut color = cfg.color;
        color.alpha *= BLOCK_CURSOR_ALPHA;
        (width, color)
    } else {
        (cfg.width, cfg.color)
    };
    let quad = cfg
        .shown
        .then(|| fill(Bounds::new(origin, size(width, line.line_height)), color));
    Some((origin, quad))
}

/// Secondary cursors (multiple-cursors): a thin caret at each position that
/// falls on this line.
fn layout_extra_cursors(
    line: &LineCtx,
    positions: &[usize],
    cfg: &CursorCfg,
    out: &mut Vec<PaintQuad>,
) {
    for &ec in positions {
        if ec >= line.range.start && ec <= line.range.end {
            let cpos = line
                .wrapped
                .position_for_index(line.disp(ec), line.line_height)
                .unwrap_or_default();
            let origin = point(line.line_origin.x + cpos.x, line.line_origin.y + cpos.y);
            out.push(fill(
                Bounds::new(origin, size(cfg.width, line.line_height)),
                cfg.secondary_color,
            ));
        }
    }
}

/// Lay out one line's decorations: the file-tree icon, diagnostic underlines,
/// virtual suffix text (inlay hints / blame), and avy jump hints. The latter
/// two only exist on the selected pane.
#[allow(clippy::too_many_arguments)] // one call site; bundles a paint pass
fn layout_decorations(
    line: &LineCtx,
    editor: &Editor,
    pane: usize,
    selected: bool,
    theme: &Theme,
    text_color: Hsla,
    font: &Font,
    font_size: Pixels,
    em: Pixels,
    diagnostics: &[crate::lsp::Diagnostic],
    diag_colors: DiagnosticColors,
    window: &mut Window,
    acc: &mut Accum,
) {
    let i = line.i;
    let range = &line.range;

    // File-tree icons, painted in reserved columns (the Scheme renders leading
    // space at `col` so they don't overlap the label).
    for (col, name, face) in editor.pane_line_icons(pane, i) {
        let ipos = line
            .wrapped
            .position_for_index(
                to_display(col.min(range.end - range.start), line.holes),
                line.line_height,
            )
            .unwrap_or_default();
        let lh_f = f32::from(line.line_height);
        let sz = lh_f * ICON_SCALE;
        let pad = (lh_f - sz) / 2.0;
        let origin = point(
            line.line_origin.x + ipos.x,
            line.line_origin.y + ipos.y + px(pad),
        );
        let color = face
            .or_else(|| editor.pane_line_face(pane, i))
            .map_or(text_color, |f| theme.color(f, text_color));
        acc.icons.push((
            Bounds::new(origin, size(px(sz), px(sz))),
            SharedString::from(format!("icons/{name}.svg")),
            color,
        ));
    }

    // Diagnostic underlines: a 2px bar along each diagnostic's span. Drawn as
    // a thin band at the bottom of each visual row the span covers.
    for diagnostic in diagnostics {
        let (d0, d1) = (diagnostic.range.start, diagnostic.range.end);
        if d0 <= range.end && d1 >= range.start {
            let color = diag_colors.for_severity(diagnostic.severity);
            let mut bars = Vec::new();
            line.push_range(&mut bars, d0, d1, color, px(4.));
            // Keep only the bottom 2px of each row's quad.
            for quad in bars {
                let b = quad.bounds;
                acc.diagnostics.push(fill(
                    Bounds::from_corners(
                        point(b.left(), b.bottom() - px(2.)),
                        point(b.right(), b.bottom()),
                    ),
                    color,
                ));
            }
        }
    }

    // Virtual suffix text (inlay hint / blame), after the line content.
    if selected && let Some((suffix, face)) = editor.line_suffix(i) {
        let color = theme.color(face, theme.color("dim", text_color));
        // Inlay-hint / code-lens text is LSP-sourced; collapse any newline so the
        // single-line `shape_line` below can't panic and abort the editor.
        let suffix = if suffix.contains(['\n', '\r']) {
            crate::util::single_line(suffix)
        } else {
            suffix.to_string()
        };
        let label = SharedString::from(format!("  {suffix}"));
        let n = label.len();
        let shaped_suffix = window.text_system().shape_line(
            label,
            font_size,
            &[plain_run(n, font.clone(), color)],
            None,
        );
        let epos = line
            .wrapped
            .position_for_index(line.disp_end(), line.line_height)
            .unwrap_or_default();
        acc.virtual_text.push((
            shaped_suffix,
            point(line.line_origin.x + epos.x, line.line_origin.y + epos.y),
        ));
    }

    // avy hints: paint each candidate's label over the buffer at its byte
    // position (e.g. "a", "sd"). Only the focused pane carries hints.
    if selected {
        let hint_bg = theme.color("avy-hint-bg", gpui::hsla(0.13, 0.95, 0.55, 1.0));
        let hint_fg = theme.color("avy-hint-fg", gpui::hsla(0.0, 0.0, 0.05, 1.0));
        for (byte, label) in editor.avy_hints() {
            if *byte >= range.start && *byte <= range.end {
                let pos = line
                    .wrapped
                    .position_for_index(line.disp(*byte), line.line_height)
                    .unwrap_or_default();
                let origin = point(line.line_origin.x + pos.x, line.line_origin.y + pos.y);
                let shaped = window.text_system().shape_line(
                    label.clone(),
                    font_size,
                    &[plain_run(label.len(), font.clone(), hint_fg)],
                    None,
                );
                let w = shaped.width.max(em);
                let bg = fill(Bounds::new(origin, size(w, line.line_height)), hint_bg);
                acc.avy.push((bg, shaped, origin));
            }
        }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

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
        // Fill the viewport; scrolling is handled internally, so the element is
        // viewport-sized rather than content-sized.
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
        let editor = self.editor.read(cx);
        let pane = self.pane;
        let selected = editor.pane_is_selected(pane);
        let buffer = editor.pane_buffer(pane);
        // Narrowed (`C-x n n`): only the accessible lines are drawn, the
        // boundary ones clipped (`layout_line`). Any pane showing the buffer.
        let narrow = editor.pane_narrow(pane);
        let narrow_lines = narrow.as_ref().map(|r| {
            (
                buffer.line_index_at_byte(r.start),
                buffer.line_index_at_byte(r.end),
            )
        });
        let theme = cx.global::<Theme>();
        let settings = cx.global::<crate::settings::Settings>();
        let popup_max_rows = settings.completion_max_rows();
        let popup_show_detail = settings.completion_show_detail();
        let popup_show_kind = settings.completion_show_kind();
        // Cursor shape: "bar" (cursor-width px), "block" (char width), or "auto"
        // (block in evil Normal/Visual, otherwise a bar).
        let block_cursor = match settings.cursor_style() {
            crate::settings::CursorStyle::Block => true,
            crate::settings::CursorStyle::Bar => false,
            crate::settings::CursorStyle::Auto => editor.wants_block_cursor(),
        };
        let cursor_width = px(settings.cursor_width());
        // When blinking is on, only paint the cursor in the "shown" half of the
        // cycle; a steady cursor (blink off) is always shown. A mode can hide it
        // outright (`cursor-visible:<mode>`).
        let cursor_shown = settings.cursor_visible_for(editor.pane_major_mode(pane))
            && (!settings.cursor_blink() || editor.cursor_blink_visible());

        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let text_color = style.color;
        let colors = FrameColors::resolve(theme, text_color);
        // The custom element doesn't inherit the div's font_family, so resolve the
        // configured `font-family` ourselves (defaults to the bundled "Lilex"). A
        // family GPUI can't resolve falls back to the platform default at shape
        // time. In Org buffers drop ligatures so prose markup like `**`/`***`
        // renders as plain asterisks instead of ligating into one vertically-
        // shifted glyph.
        let editor_font = {
            let mut f = font(settings.font_family().to_string());
            if editor.is_org() {
                f.features = gpui::FontFeatures::disable_ligatures();
            }
            f
        };

        // One monospace character's advance (block-cursor width past EOL, and the
        // line-number gutter's column width).
        let mk_run = |len: usize, color: Hsla| plain_run(len, editor_font.clone(), color);
        let em = window
            .text_system()
            .shape_line(
                SharedString::from("0"),
                font_size,
                &[mk_run(1, text_color)],
                None,
            )
            .x_for_index(1);
        let char_advance = if block_cursor { em } else { px(0.) };

        let lh = f32::from(line_height);
        let scroll = f32::from(editor.pane_scroll(pane));
        let total_lines = buffer.line_count();
        let viewport_h = f32::from(bounds.size.height);

        // Line-number gutter (Emacs display-line-numbers-mode). Per major mode:
        // on by default for every pane's buffer, off for modes that opt out
        // (e.g. the dired-sidebar tree). Resolved against this pane's own major
        // mode so a code pane shows numbers while the tree next to it doesn't.
        let line_numbers = settings.line_numbers_for(editor.pane_major_mode(pane));
        // hl-line: a background band on the cursor's row, selected pane only.
        let hl_line = selected && settings.hl_line_for(editor.pane_major_mode(pane));
        let gutter_digits = total_lines.max(1).to_string().len().max(3);
        let gutter_pad = GUTTER_PAD;
        // Gutter fold chevrons: selected pane only, and only when the gutter
        // exists (line numbers on) so they have somewhere to live. When shown they
        // claim a dedicated column on the left of the gutter so they never overlap
        // the line numbers; `chevron_sz` is the glyph size, `fold_col` its column.
        let fold_markers_ui = selected && line_numbers && editor.has_fold_markers();
        let chevron_sz = px((lh * 0.7).min(14.0));
        let fold_col = if fold_markers_ui {
            chevron_sz + px(6.)
        } else {
            px(0.)
        };
        let gutter_width = if line_numbers {
            fold_col + em * gutter_digits as f32 + gutter_pad * 2.0
        } else {
            px(0.)
        };
        let text_left = bounds.left() + gutter_width;
        // Diagnostic gutter dots ride alongside the line numbers, so they need the
        // gutter to exist; gate on both the option and line numbers being shown.
        let diag_gutter = line_numbers && settings.diagnostics_gutter();
        // git-gutter (diff-hl): change bars at the gutter's far-left edge. Like
        // the diagnostic dots, gated on the line-number gutter existing.
        let git_gutter_on = line_numbers && settings.magit_gutter();
        let git_gutter_w = px(settings.magit_gutter_width());
        let git_gutter_del = settings.magit_gutter_deleted();
        // Diagnostic colors (gutter dots + underlines), resolved once per frame.
        let diag_colors = DiagnosticColors::resolve(theme);

        // Copilot-style ghost text (selected pane only — it lives on the active
        // buffer, like `line_holes`). `ghost_at` validates at read time: the
        // suggestion is only visible while point still sits on its anchor byte.
        // The first line is spliced into the anchor line's display text; the
        // continuation lines paint on display rows reserved below it. The split
        // and face lookup only run while a ghost is showing — this path is
        // per-frame, and no ghost is the overwhelmingly common case.
        let ghost = if selected { editor.ghost_at() } else { None };
        let (ghost_first, ghost_tail, ghost_color) = match ghost {
            Some((_, t)) => {
                let mut parts = t.split('\n');
                let first = parts.next().unwrap_or("");
                // The `ghost` face, falling back to the theme's dim shade,
                // then to a faded foreground so the suggestion reads as
                // tentative even on a theme that defines neither.
                let mut faded = text_color;
                faded.alpha *= 0.55;
                let color = theme.color("ghost", theme.color("dim", faded));
                (first, parts.collect::<Vec<_>>(), color)
            }
            None => ("", Vec::new(), text_color),
        };
        // The ghost's anchor line + reserved tail rows, resolved once per frame
        // (folded-away anchors contribute nothing) — the row loop below must
        // not re-derive this per line.
        let ghost_rows = if ghost.is_some() {
            editor.ghost_rows()
        } else {
            None
        };

        // Folds collapse lines into fewer rows; inline images expand a line into
        // extra rows below it. Either way `rows[k]` is the buffer line drawn
        // starting at a known screen row. Without either, this is the identity
        // (the fast path).
        let folds = selected && editor.has_folds();
        let images = selected && editor.has_line_images();
        let scales = selected && editor.has_line_scales();
        let dynamic = folds || images || scales || ghost_rows.is_some() || narrow.is_some();
        let display_count = match narrow_lines {
            _ if dynamic && selected => editor.display_line_count(),
            Some((first, last)) => last - first + 1,
            None => total_lines,
        };
        let first_row = (scroll / lh).floor().max(0.0) as usize;
        let visible = (viewport_h / lh).ceil() as usize + 1;
        let last_row = (first_row + visible).min(display_count);

        // Cumulative display row of the first painted line — the base for `y`
        // when folds/images make display rows differ from buffer-line indices.
        let mut first_top_row = first_row;
        let rows: Vec<usize> = if dynamic {
            let mut v = Vec::new();
            let mut r = 0usize;
            let (lo, hi) = narrow_lines.unwrap_or((0, total_lines.saturating_sub(1)));
            for i in lo..(hi + 1).min(total_lines) {
                if folds && editor.is_line_invisible(i) {
                    continue;
                }
                // 1 row for the line's text + N reserved rows for its image, +
                // any extra rows a scaled (larger) line or a multi-line ghost
                // suggestion claims.
                let line_rows = 1
                    + if images { editor.image_rows_at(i) } else { 0 }
                    + if scales { editor.scale_rows_at(i) } else { 0 }
                    + match ghost_rows {
                        Some((g, rows)) if g == i => rows,
                        _ => 0,
                    };
                if r < last_row && r + line_rows > first_row {
                    if v.is_empty() {
                        first_top_row = r;
                    }
                    v.push(i);
                }
                r += line_rows;
                if r >= last_row {
                    break;
                }
            }
            v
        } else {
            (first_row..last_row).collect()
        };

        // Highlight only the visible byte range, queried on demand.
        let highlights = match (rows.first(), rows.last()) {
            (Some(&f), Some(&l)) => {
                let start = buffer.line_range(f).start;
                let end = buffer.line_range(l).end;
                editor.pane_highlights(pane, start..end)
            }
            _ => Vec::new(),
        };

        // Sub-line overlay faces (selected pane only).
        let empty_overlays: &[(std::ops::Range<usize>, SharedString)] = &[];
        let overlay_faces = if selected {
            editor.range_faces()
        } else {
            empty_overlays
        };

        let cursor = editor.pane_point(pane);
        let selection = editor.pane_selection(pane);
        // Block-wise Visual (`C-v`): a rectangle (top_row, bottom_row, left_col,
        // right_col) highlighted per line as a column strip rather than the
        // contiguous `selection` range. `None` unless in block-wise Visual.
        let block_sel = editor.pane_visual_block(pane);
        let diagnostics = editor.pane_diagnostics(pane);
        let diff_marks = editor.pane_diff_marks(pane);
        let gutter_dots = editor.pane_gutter_dots(pane);
        // Search matches highlight only in the selected pane (the active buffer),
        // and only over the visible line span — O(viewport), recomputed each
        // frame so it can't go stale.
        let search = match (selected, rows.first(), rows.last()) {
            (true, Some(&top), Some(&bot)) => {
                let mut vis_start = buffer.line_range(top).start;
                let mut vis_end = buffer.line_range(bot).end;
                if let Some(n) = &narrow {
                    vis_start = vis_start.max(n.start);
                    vis_end = vis_end.min(n.end).max(vis_start);
                }
                editor.search_match_ranges_in(vis_start, vis_end)
            }
            _ => Vec::new(),
        };
        // Matching-delimiter highlight (Emacs show-paren), selected pane only,
        // recomputed each frame so it tracks the cursor.
        let paren = if selected && settings.show_paren_for(editor.pane_major_mode(pane)) {
            editor.show_paren_ranges()
        } else {
            Vec::new()
        };

        // Word-wrap (Emacs visual-line-mode): when on, soft-wrap each line at the
        // text-area width; when off, leave lines unwrapped and shift them left by
        // the pane's horizontal scroll so off-screen text is reachable.
        let word_wrap = settings.word_wrap_for(editor.pane_major_mode(pane));
        let text_area_w = (f32::from(bounds.size.width) - f32::from(gutter_width)).max(1.0);
        let wrap_width = if word_wrap {
            Some(px(text_area_w))
        } else {
            None
        };
        let h_scroll = if selected && !word_wrap {
            editor.h_scroll()
        } else {
            px(0.)
        };
        let text_x = text_left - h_scroll;

        let mut acc = Accum::default();
        let extra_cursor_positions = editor.extra_cursor_positions();
        // Secondary selection spans (Helix multiple selections); selected pane
        // only, like the carets. Each is highlighted with `colors.secondary_region`.
        let extra_selections = if selected {
            editor.extra_selections()
        } else {
            Vec::new()
        };
        let gutter = GutterCfg {
            bounds,
            gutter_width,
            gutter_pad,
            fold_col,
            chevron_sz,
            fold_markers: fold_markers_ui,
            diag_gutter,
            git_gutter: git_gutter_on,
            git_gutter_w,
            git_gutter_del,
            gg_added: colors.git_added,
            gg_modified: colors.git_modified,
            gg_removed: colors.git_removed,
            number_color: colors.line_number,
            line_base: narrow_lines.map_or(0, |(first, _)| first),
            font: editor_font.clone(),
            font_size,
            diag_colors,
        };
        let cursor_cfg = CursorCfg {
            block: block_cursor,
            width: cursor_width,
            shown: cursor_shown,
            char_advance,
            color: colors.cursor,
            secondary_color: colors.secondary_cursor,
        };
        let frame = FrameCtx {
            editor,
            pane,
            selected,
            buffer,
            narrow,
            theme,
            colors,
            text_color,
            font: editor_font.clone(),
            font_size,
            em,
            line_height,
            bounds,
            text_left,
            text_x,
            wrap_width,
            scales,
            images,
            line_numbers,
            hl_line,
            cursor,
            selection,
            block_sel,
            extra_selections,
            extra_cursor_positions,
            search,
            paren,
            highlights,
            overlay_faces,
            ghost,
            ghost_first,
            ghost_tail,
            ghost_color,
            diagnostics,
            diff_marks,
            gutter_dots,
            diag_colors,
            gutter,
            cursor_cfg,
        };

        // Lines have variable height under wrapping (and under inline images), so
        // accumulate pixel offsets rather than multiplying a row index. Start at
        // the top of the first visible line (which may be partially scrolled off
        // the top); `first_top_row` is its cumulative display row.
        let mut y = px(first_top_row as f32 * lh - scroll);
        let viewport_bottom = px(viewport_h);
        for &i in &rows {
            // Stop laying out once below the viewport — but keep going until the
            // cursor's own line has been laid out, so `reconcile_cursor_scroll`
            // learns its real (wrapped) pixel position and can follow it. Under
            // word-wrap the lines above the cursor can fill the viewport before
            // the loop reaches the cursor's line (e.g. jumping to end-of-buffer),
            // so breaking at the first off-screen line would leave `cursor_px`
            // unset and the scroll a few rows short of revealing the cursor. Once
            // the cursor has been located — or in a non-selected pane, which has
            // no cursor to follow — the early-out stands. Off-screen lines laid
            // out in the interim are clipped to `bounds` anyway.
            if y >= viewport_bottom && (!selected || acc.cursor_px.is_some()) {
                break;
            }
            frame.layout_line(i, &mut y, &mut acc, window);
        }

        // The completion popup, anchored just below the cursor.
        let popup = acc
            .cursor_px
            .zip(editor.completion_view())
            .map(|(anchor, view)| {
                build_popup(
                    anchor,
                    view,
                    popup_max_rows,
                    popup_show_detail,
                    popup_show_kind,
                    &editor_font,
                    font_size,
                    line_height,
                    bounds,
                    theme,
                    window,
                )
            });

        // Scroll bar (Emacs scroll-bar-mode): a thumb on the pane's right edge,
        // sized/positioned by the visible fraction. Overlay — doesn't reserve
        // gutter space. Only when content overflows the viewport.
        let mut scrollbar = Vec::new();
        if settings.scroll_bar() {
            let content_h = display_count as f32 * lh;
            if content_h > viewport_h + 1.0 {
                let sb_w = SCROLLBAR_WIDTH;
                let x1 = bounds.right();
                let x0 = x1 - sb_w;
                let track_color = theme.color("scroll-bar-track", rgba(0x88888822));
                scrollbar.push(fill(
                    Bounds::from_corners(
                        point(x0, bounds.top()),
                        point(x1, bounds.top() + px(viewport_h)),
                    ),
                    track_color,
                ));
                let thumb_h = (viewport_h / content_h * viewport_h)
                    .max(SCROLLBAR_MIN_THUMB)
                    .min(viewport_h);
                let max_scroll = (content_h - viewport_h).max(1.0);
                let t = (scroll / max_scroll).clamp(0.0, 1.0);
                let thumb_top = bounds.top() + px(t * (viewport_h - thumb_h));
                let thumb_color = theme.color("scroll-bar", rgba(0x888888aa));
                scrollbar.push(fill(
                    Bounds::from_corners(point(x0, thumb_top), point(x1, thumb_top + px(thumb_h))),
                    thumb_color,
                ));
            }
        }

        PrepaintState {
            lines: acc.lines,
            cursor: acc.cursor,
            hl_line: acc.hl_line,
            selections: acc.selections,
            search: acc.search,
            paren: acc.paren,
            scrollbar,
            icons: acc.icons,
            images: acc.images,
            diagnostics: acc.diagnostics,
            git_gutter: acc.git_gutter,
            popup,
            text_left,
            line_x: text_x,
            char_width: em,
            gutter: acc.gutter,
            fold_chevrons: acc.fold_chevrons,
            fold_hitboxes: acc.fold_hitboxes,
            fold_cursors: acc.fold_cursors,
            virtual_text: acc.virtual_text,
            avy: acc.avy,
            extra_cursors: acc.extra_cursors,
            cursor_rel_y: acc.cursor_px.map(|p| p.y - bounds.top()),
        }
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
        let selected = self.editor.read(cx).pane_is_selected(self.pane);
        let focus_handle = self.editor.read(cx).focus_handle.clone();

        // Route IME / typed text to the editor for this region — only the
        // selected pane is live, so only it claims the input handler.
        if selected {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.editor.clone()),
                cx,
            );
        }

        let line_height = window.line_height();
        // Captured before the paint closure consumes the popup, so we can report
        // its clickable geometry back to the editor afterwards.
        let popup_hit = prepaint.popup.as_ref().map(|p| p.hit);

        // Clip painting to the viewport so partially-scrolled top/bottom lines
        // don't bleed past the editor (e.g. into the status line).
        let text_left = prepaint.text_left;
        let line_x = prepaint.line_x;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // hl-line band sits behind everything else on its row.
            if let Some(quad) = prepaint.hl_line.take() {
                window.paint_quad(quad);
            }
            // Face backgrounds (`set-face-background` overlay faces) sit just
            // above the hl-line band, under search/paren/selection highlights.
            // `paint` doesn't draw run backgrounds — they need this pass.
            for line in &prepaint.lines {
                let origin = point(
                    line_x + line.indent,
                    bounds.top() + line.top + line.content_dy,
                );
                line.wrapped
                    .paint_background(origin, line_height, TextAlign::Left, None, window, cx)
                    .ok();
            }
            // Search highlights sit behind the selection and the text.
            for quad in prepaint.search.drain(..) {
                window.paint_quad(quad);
            }
            // Matching-delimiter highlights, likewise behind selection and text.
            for quad in prepaint.paren.drain(..) {
                window.paint_quad(quad);
            }
            for quad in prepaint.selections.drain(..) {
                window.paint_quad(quad);
            }
            // Line-number gutter labels.
            for (label, origin) in prepaint.gutter.drain(..) {
                label
                    .paint(origin, line_height, TextAlign::Left, None, window, cx)
                    .ok();
            }
            for line in &prepaint.lines {
                let origin = point(
                    line_x + line.indent,
                    bounds.top() + line.top + line.content_dy,
                );
                line.wrapped
                    .paint(origin, line_height, TextAlign::Left, None, window, cx)
                    .ok();
            }
            // Virtual suffix text (inlay/blame), after the line content.
            for (label, origin) in prepaint.virtual_text.drain(..) {
                label
                    .paint(origin, line_height, TextAlign::Left, None, window, cx)
                    .ok();
            }
            for quad in prepaint.diagnostics.drain(..) {
                window.paint_quad(quad);
            }
            // git-gutter change bars + deletion marks, in the gutter's left edge.
            for quad in prepaint.git_gutter.drain(..) {
                window.paint_quad(quad);
            }
            // Scroll bar sits above the text on the right edge.
            for quad in prepaint.scrollbar.drain(..) {
                window.paint_quad(quad);
            }
            // File-tree icons (monochrome SVGs tinted by the line color).
            for (icon_bounds, path, color) in prepaint.icons.drain(..) {
                window
                    .paint_svg(
                        icon_bounds,
                        path,
                        None,
                        TransformationMatrix::unit(),
                        color,
                        cx,
                    )
                    .ok();
            }
            // Fold chevrons in the gutter (monochrome SVGs, tinted like numbers).
            for (chevron_bounds, path, color) in prepaint.fold_chevrons.drain(..) {
                window
                    .paint_svg(
                        chevron_bounds,
                        path,
                        None,
                        TransformationMatrix::unit(),
                        color,
                        cx,
                    )
                    .ok();
            }
            // A hand pointer over each chevron (overrides the editor's IBeam).
            for hitbox in &prepaint.fold_cursors {
                window.set_cursor_style(CursorStyle::PointingHand, hitbox);
            }
            // Inline raster images (Org inline images / LaTeX previews), painted
            // in the rows reserved below their line.
            for (img_bounds, data) in prepaint.images.drain(..) {
                window
                    .paint_image(img_bounds, img_bounds, Corners::default(), data, 0, false)
                    .ok();
            }
            if focus_handle.is_focused(window)
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
            if let Some(popup) = prepaint.popup.take() {
                for quad in popup.quads {
                    window.paint_quad(quad);
                }
                for (line, origin) in popup.labels {
                    line.paint(origin, line_height, TextAlign::Left, None, window, cx)
                        .ok();
                }
            }
            // Secondary cursors (multiple-cursors), drawn whenever focused.
            if focus_handle.is_focused(window) {
                for caret in prepaint.extra_cursors.drain(..) {
                    window.paint_quad(caret);
                }
            }
            // avy hints sit on top of everything (text, cursor, popup).
            for (bg, label, origin) in prepaint.avy.drain(..) {
                window.paint_quad(bg);
                label
                    .paint(origin, line_height, TextAlign::Left, None, window, cx)
                    .ok();
            }
        });

        // Report this pane's bounds for click-to-focus, and (for the selected
        // pane only) hand back the visible layout for hit-testing / IME / cursor.
        let lines = if selected {
            std::mem::take(&mut prepaint.lines)
        } else {
            Vec::new()
        };
        let pane = self.pane;
        let char_width = prepaint.char_width;
        let major_mode = self
            .editor
            .read(cx)
            .pane_major_mode(pane)
            .map(str::to_string);
        let word_wrap = cx
            .global::<crate::settings::Settings>()
            .word_wrap_for(major_mode.as_deref());
        // The text region starts past the gutter; mouse hit-testing uses it so
        // clicks map to the right column when line numbers are on.
        let text_bounds = gpui::Bounds {
            origin: point(text_left, bounds.top()),
            size: gpui::size(
                bounds.size.width - (text_left - bounds.left()),
                bounds.size.height,
            ),
        };
        let cursor_rel_y = prepaint.cursor_rel_y;
        let fold_hitboxes = std::mem::take(&mut prepaint.fold_hitboxes);
        self.editor.update(cx, |editor, cx| {
            editor.report_pane_bounds(pane, bounds);
            if selected {
                editor.set_layout(lines, text_bounds, line_height, char_width, word_wrap);
                editor.report_completion_popup(popup_hit);
                editor.report_fold_hitboxes(fold_hitboxes);
                // Follow the cursor against the real (wrapped) geometry; corrects
                // the buffer-line estimate `ensure_cursor_visible` works from.
                editor.reconcile_cursor_scroll(cursor_rel_y, cx);
            }
        });
    }
}

/// Byte offset within `text` of character column `col`, clamped to the text's
/// length (a column past the end lands at the end). Maps a block-Visual
/// rectangle's columns to per-line byte ranges for highlighting.
fn char_col_byte_in(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Emit one background-fill quad per visual row that the display-text byte
/// range `[disp_start, disp_end)` covers within a (possibly wrapped) line.
/// Indices are relative to the line's display text (holes already spliced
/// out — see [`LineCtx::disp`]). `origin` is the line's top-left in window
/// space (text x already shifted for horizontal scroll). `min_width` widens a
/// zero/near-zero span so it stays visible (used for diagnostics).
#[allow(clippy::too_many_arguments)] // a focused per-row highlight helper
fn push_range_quads(
    out: &mut Vec<PaintQuad>,
    wrapped: &WrappedLine,
    disp_start: usize,
    disp_end: usize,
    origin: Point<Pixels>,
    line_height: Pixels,
    color: Hsla,
    min_width: Pixels,
) {
    if disp_end <= disp_start {
        return;
    }
    let local_s = disp_start;
    let local_e = disp_end;
    let len = wrapped.len();
    // Byte index where each visual row (after the first) begins.
    let boundaries: Vec<usize> = wrapped
        .wrap_boundaries()
        .iter()
        .map(|wb| wrapped.unwrapped_layout.runs[wb.run_ix].glyphs[wb.glyph_ix].index)
        .collect();
    let mut row_start = 0usize;
    for row in 0..=boundaries.len() {
        let row_end = boundaries.get(row).copied().unwrap_or(len);
        let a = local_s.max(row_start);
        let b = local_e.min(row_end);
        if b > a {
            let row_x0 = wrapped.unwrapped_layout.x_for_index(row_start);
            let x0 = wrapped.unwrapped_layout.x_for_index(a) - row_x0;
            let mut x1 = wrapped.unwrapped_layout.x_for_index(b) - row_x0;
            if x1 - x0 < min_width {
                x1 = x0 + min_width;
            }
            let y = origin.y + px(row as f32 * f32::from(line_height));
            out.push(fill(
                Bounds::from_corners(
                    point(origin.x + x0, y),
                    point(origin.x + x1, y + line_height),
                ),
                color,
            ));
        }
        row_start = row_end;
    }
}

/// One styled stretch of a line, before it becomes a `TextRun`.
#[derive(Clone)]
struct Seg {
    start: usize,
    end: usize,
    color: Hsla,
    /// `None`, or `Some((style, color))` — the underline carries its own
    /// color so a squiggle can be tinted without recoloring the text under it.
    underline: Option<(Underline, Hsla)>,
    bold: bool,
    /// Background painted behind the run (`set-face-background`).
    background: Option<Hsla>,
    /// Font family override (`set-face-font`); `None` = the editor font.
    family: Option<SharedString>,
}

impl Seg {
    fn plain(start: usize, end: usize, color: Hsla, bold: bool) -> Seg {
        Seg {
            start,
            end,
            color,
            underline: None,
            bold,
            background: None,
            family: None,
        }
    }

    fn with_range(&self, start: usize, end: usize) -> Seg {
        Seg {
            start,
            end,
            ..self.clone()
        }
    }
}

/// Build the `TextRun`s for one line: tile it with the buffer's tree-sitter
/// highlight spans, then let sub-line overlay faces override on top (the Emacs
/// overlay model). With no overlays this is exactly the old syntax-only path.
#[allow(clippy::too_many_arguments)]
fn line_runs(
    spans: &[HlSpan],
    overlays: &[(std::ops::Range<usize>, SharedString)],
    line: std::ops::Range<usize>,
    font: &Font,
    default: Hsla,
    default_bold: bool,
    theme: &Theme,
    text: Option<&str>,
) -> Vec<TextRun> {
    // 1. Base segments from syntax spans. `default_bold` styles the face-less
    //    base — a whole-line face is resolved to `default` by the caller, so
    //    its bold flag arrives here.
    let mut segs: Vec<Seg> = Vec::new();
    let mut covered = line.start;
    for span in spans {
        if span.range.end <= line.start {
            continue;
        }
        if span.range.start >= line.end {
            break;
        }
        let start = span.range.start.max(line.start);
        let end = span.range.end.min(line.end);
        if start > covered {
            segs.push(Seg::plain(covered, start, default, default_bold));
        }
        let mut seg = Seg::plain(start, end, default, default_bold);
        if let Some(i) = span.face {
            let name = face_name(i);
            seg.color = theme.color(name, default);
            seg.underline = theme.underline(name).map(|style| (style, seg.color));
            seg.bold = theme.is_bold(name);
        }
        segs.push(seg);
        covered = end;
    }
    if covered < line.end {
        segs.push(Seg::plain(covered, line.end, default, default_bold));
    }
    if segs.is_empty() {
        segs.push(Seg::plain(line.start, line.end, default, default_bold));
    }

    // 2. Overlay faces override their sub-range, splitting base segments. An
    //    overlay face may also carry underline (e.g. web-reader link markers)
    //    or bold. A WAVY overlay face colors only its squiggle (spellcheck
    //    style: the word keeps the color it already had); a straight-underline
    //    or plain face recolors the text as before. A face with a background
    //    (`set-face-background`) paints it; one with ONLY a background keeps
    //    everything else it lands on, so it layers under a foreground face
    //    (text-plot half-blocks: `plot-heat-3` + `plot-heat-5-bg`). A face
    //    font (`set-face-font`) switches the run's family.
    //    Face ranges are byte offsets set from Scheme and need not fall on
    //    char boundaries; a run edge inside a multibyte char makes gpui's
    //    shaper panic once the face switches font. Widen each edge to the
    //    enclosing char boundaries of the line's `text` (when given).
    let snap = |b: usize, up: bool| -> usize {
        let Some(t) = text.filter(|t| t.len() == line.len()) else {
            return b;
        };
        let mut rel = b - line.start;
        while !t.is_char_boundary(rel) {
            if up { rel += 1 } else { rel -= 1 }
        }
        line.start + rel
    };
    for (range, face) in overlays {
        // Skip overlays that miss this line before snapping: `snap` assumes
        // its offset lies within `line`.
        let (os, oe) = (range.start.max(line.start), range.end.min(line.end));
        if os >= oe {
            continue;
        }
        let (os, oe) = (snap(os, false), snap(oe, true));
        let face_bg = theme.background_of(face);
        let bg_only = face_bg.is_some() && !theme.has_color(face);
        let family = theme
            .font_of(face)
            .map(|f| SharedString::from(f.to_string()));
        let fcolor = theme.color(face, default);
        let ul = theme.underline(face).map(|style| (style, fcolor));
        let wavy_only = matches!(ul, Some((Underline::Wavy, _)));
        let bold = theme.is_bold(face);
        let mut next = Vec::with_capacity(segs.len() + 2);
        for seg in segs.drain(..) {
            if seg.end <= os || seg.start >= oe {
                next.push(seg);
                continue;
            }
            if seg.start < os {
                next.push(seg.with_range(seg.start, os));
            }
            let mut mid = seg.with_range(seg.start.max(os), seg.end.min(oe));
            if bg_only {
                mid.background = face_bg;
            } else {
                if !wavy_only {
                    mid.color = fcolor;
                }
                mid.underline = ul;
                mid.bold = bold;
                mid.background = face_bg.or(mid.background);
            }
            if family.is_some() {
                mid.family = family.clone();
            }
            next.push(mid);
            if seg.end > oe {
                next.push(seg.with_range(oe, seg.end));
            }
        }
        segs = next;
    }

    // 3. Emit runs (skip any empty segment), styled as the segment asked.
    segs.into_iter()
        .filter(|seg| seg.end > seg.start)
        .map(|seg| {
            let mut run = plain_run(seg.end - seg.start, font.clone(), seg.color);
            if let Some((style, ul_color)) = seg.underline {
                run.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(ul_color),
                    wavy: style == Underline::Wavy,
                });
            }
            if seg.bold {
                run.font.weight = FontWeight::BOLD;
            }
            if let Some(family) = seg.family {
                run.font.family = family;
            }
            run.background_color = seg.background;
            run
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_face_overrides_subrange() {
        let mut theme = Theme::default();
        let red: Hsla = rgb(0xff0000);
        theme.set("hl", red);
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        // Line [0,10), no syntax spans, one overlay [2,5) with face "hl".
        let overlays = vec![(2usize..5usize, SharedString::from("hl"))];
        let runs = line_runs(&[], &overlays, 0..10, &font, default, false, &theme, None);
        assert_eq!(
            runs.iter().map(|r| r.len).collect::<Vec<_>>(),
            vec![2, 3, 5]
        );
        assert_eq!(runs[1].color, red, "overlay colors its sub-range");
        assert_eq!(runs[0].color, default);
        assert_eq!(runs[2].color, default);
    }

    #[test]
    fn overlay_outside_line_is_skipped() {
        let mut theme = Theme::default();
        theme.set("hl", rgb(0xff0000));
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        // Line [10,20) with its text; overlays wholly before and after it
        // must be skipped, not snapped (snapping one underflowed).
        let overlays = vec![
            (0usize..5usize, SharedString::from("hl")),
            (25usize..30usize, SharedString::from("hl")),
        ];
        let text = "0123456789";
        let runs = line_runs(
            &[],
            &overlays,
            10..20,
            &font,
            default,
            false,
            &theme,
            Some(text),
        );
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![10]);
        assert_eq!(runs[0].color, default);
    }

    #[test]
    fn background_face_layers_under_foreground_face() {
        let red: Hsla = rgb(0xff0000);
        let blue: Hsla = rgb(0x0000ff);
        let mut theme = Theme::default();
        theme.set("heat-7", red);
        theme.set_background("heat-2-bg", blue);
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        // Text-plot half-block: a fg face over [0,6), a bg-only face over
        // [3,6) — the overlap keeps red text AND gets the blue background.
        let overlays = vec![
            (0usize..6usize, SharedString::from("heat-7")),
            (3usize..6usize, SharedString::from("heat-2-bg")),
        ];
        let runs = line_runs(&[], &overlays, 0..9, &font, default, false, &theme, None);
        assert_eq!(
            runs.iter().map(|r| r.len).collect::<Vec<_>>(),
            vec![3, 3, 3]
        );
        assert_eq!((runs[0].color, runs[0].background_color), (red, None));
        assert_eq!((runs[1].color, runs[1].background_color), (red, Some(blue)));
        assert_eq!((runs[2].color, runs[2].background_color), (default, None));

        // A bg-only face alone leaves the text color untouched.
        let overlays = vec![(0usize..3usize, SharedString::from("heat-2-bg"))];
        let runs = line_runs(&[], &overlays, 0..3, &font, default, false, &theme, None);
        assert_eq!(
            (runs[0].color, runs[0].background_color),
            (default, Some(blue))
        );
    }

    #[test]
    fn face_font_switches_the_run_family() {
        let mut theme = Theme::default();
        theme.set("plot-series-0", rgb(0x87c3ff));
        theme.set_font("plot-series-0", "Macros Plot");
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        let overlays = vec![(3usize..9usize, SharedString::from("plot-series-0"))];
        let runs = line_runs(&[], &overlays, 0..12, &font, default, false, &theme, None);
        let families: Vec<&str> = runs.iter().map(|r| r.font.family.as_ref()).collect();
        assert_eq!(families, vec!["Lilex", "Macros Plot", "Lilex"]);
    }

    #[test]
    fn overlay_face_underlines_when_set() {
        let mut theme = Theme::default();
        theme.set("link", rgb(0x4ea1f3));
        theme.set_underline("link", true);
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        // One overlay [2,5) with an underlined face; the rest of the line is not.
        let overlays = vec![(2usize..5usize, SharedString::from("link"))];
        let runs = line_runs(&[], &overlays, 0..10, &font, default, false, &theme, None);
        assert!(
            runs[1].underline.is_some(),
            "underlined face underlines its run"
        );
        assert!(runs[0].underline.is_none());
        assert!(runs[2].underline.is_none());
    }

    #[test]
    fn wavy_face_colors_only_the_squiggle_not_the_text() {
        let red: Hsla = rgb(0xff0000);
        let mut theme = Theme::default();
        theme.set("spell-error", red);
        theme.set_wavy("spell-error", true);
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        let overlays = vec![(2usize..5usize, SharedString::from("spell-error"))];
        let runs = line_runs(&[], &overlays, 0..10, &font, default, false, &theme, None);
        let ul = runs[1].underline.as_ref().expect("wavy implies underline");
        assert!(ul.wavy, "wavy face squiggles its run");
        assert_eq!(ul.color, Some(red), "the squiggle takes the face color");
        assert_eq!(
            runs[1].color, default,
            "the text keeps its color — only the squiggle is red"
        );
        assert!(runs[0].underline.is_none());
        assert!(runs[2].underline.is_none());
    }

    #[test]
    fn no_overlays_tiles_whole_line() {
        let theme = Theme::default();
        let font = font("Lilex");
        let default: Hsla = rgb(0x111111);
        let runs = line_runs(&[], &[], 0..7, &font, default, false, &theme, None);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), 7);
    }

    #[test]
    fn ghost_display_shift_and_inverse() {
        let ghost = Some((4usize, 3usize)); // 3 ghost bytes injected at index 4
        // Point itself (== insertion index) does NOT shift: the cursor renders
        // before the ghost.
        assert_eq!(ghost_to_display(4, ghost), 4);
        // Everything after point shifts right by the ghost length.
        assert_eq!(ghost_to_display(5, ghost), 8);
        assert_eq!(ghost_to_display(3, ghost), 3);
        assert_eq!(ghost_to_display(9, None), 9);
        // Inverse: real positions round-trip…
        for rel in [0, 3, 4, 5, 9] {
            assert_eq!(ghost_from_display(ghost_to_display(rel, ghost), ghost), rel);
        }
        // …and a display index inside the ghost (a click on the suggestion)
        // collapses to the insertion point.
        assert_eq!(ghost_from_display(5, ghost), 4);
        assert_eq!(ghost_from_display(7, ghost), 4);
    }

    #[test]
    fn inject_ghost_run_splits_covering_run() {
        let font = font("Lilex");
        let c: Hsla = rgb(0x111111);
        let g: Hsla = rgb(0x888888);
        let ghost = plain_run(3, font.clone(), g);
        // Mid-run: [6] at index 4 → [4, 3(ghost), 2].
        let runs = inject_ghost_run(vec![plain_run(6, font.clone(), c)], 4, ghost.clone());
        assert_eq!(
            runs.iter().map(|r| r.len).collect::<Vec<_>>(),
            vec![4, 3, 2]
        );
        assert_eq!(runs[1].color, g);
        // On a run boundary: [2, 4] at index 2 → the ghost slots between them.
        let runs = inject_ghost_run(
            vec![plain_run(2, font.clone(), c), plain_run(4, font.clone(), c)],
            2,
            ghost.clone(),
        );
        assert_eq!(
            runs.iter().map(|r| r.len).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(runs[1].color, g);
        // At the start and past the end (EOL), and on an empty line.
        let runs = inject_ghost_run(vec![plain_run(5, font.clone(), c)], 0, ghost.clone());
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![3, 5]);
        let runs = inject_ghost_run(vec![plain_run(5, font.clone(), c)], 5, ghost.clone());
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![5, 3]);
        let runs = inject_ghost_run(Vec::new(), 0, ghost);
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn bold_face_bolds_runs_without_needing_a_color() {
        let mut theme = Theme::default();
        theme.set_bold("feeds-unread", true); // NO color entry on purpose
        let font = font("Lilex");
        let default: Hsla = rgb(0x222222);
        // Whole-line-face path: caller resolves the face to `default` (fallback
        // — the face has no color) and passes its bold flag as the base.
        let runs = line_runs(
            &[],
            &[],
            0..7,
            &font,
            theme.color("feeds-unread", default),
            theme.is_bold("feeds-unread"),
            &theme,
            None,
        );
        assert_eq!(runs[0].font.weight, FontWeight::BOLD);
        assert_eq!(runs[0].color, default, "colorless bold face keeps default");
        // Overlay path: a bold overlay bolds only its sub-range.
        let overlays = vec![(2usize..5usize, SharedString::from("feeds-unread"))];
        let runs = line_runs(&[], &overlays, 0..10, &font, default, false, &theme, None);
        assert_eq!(runs[0].font.weight, FontWeight::NORMAL);
        assert_eq!(runs[1].font.weight, FontWeight::BOLD);
        assert_eq!(runs[2].font.weight, FontWeight::NORMAL);
    }

    #[test]
    fn overlay_face_edges_snap_to_char_boundaries() {
        let mut theme = Theme::default();
        let red: Hsla = rgb(0xff0000);
        theme.set("hl", red);
        let font = font("Lilex");
        let default: Hsla = rgb(0x000000);
        // "aé b": "é" is bytes 1..3; the face [2,4) starts inside it.
        let text = "aé b";
        let overlays = vec![(2usize..4usize, SharedString::from("hl"))];
        let runs = line_runs(
            &[],
            &overlays,
            0..5,
            &font,
            default,
            false,
            &theme,
            Some(text),
        );
        let lens: Vec<usize> = runs.iter().map(|r| r.len).collect();
        assert_eq!(lens, vec![1, 3, 1], "face widened to cover all of é");
        let mut at = 0;
        for len in lens {
            at += len;
            assert!(text.is_char_boundary(at));
        }
        // An end inside a multibyte char widens up to the char's end.
        let overlays = vec![(0usize..2usize, SharedString::from("hl"))];
        let runs = line_runs(
            &[],
            &overlays,
            0..5,
            &font,
            default,
            false,
            &theme,
            Some(text),
        );
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![3, 2]);
    }

    #[test]
    fn suffix_goes_after_ghost_text_anchored_at_eol() {
        // Line text ends at display index 5; ghost of 7 bytes at EOL.
        assert_eq!(display_end(5, Some((5, 7))), 12);
        // A ghost mid-line also pushes the end right.
        assert_eq!(display_end(5, Some((2, 7))), 12);
        assert_eq!(display_end(5, None), 5);
    }

    #[test]
    fn hidden_range_maps_are_inverse() {
        // "*** Title": collapse the first two stars (bytes [0,2)).
        let holes = [(0usize, 2usize)];
        // Buffer offset -> display index: hidden bytes map to the hole start (0),
        // the surviving star (offset 2) lands at display 0, then 1:1.
        assert_eq!(to_display(0, &holes), 0);
        assert_eq!(to_display(1, &holes), 0);
        assert_eq!(to_display(2, &holes), 0); // surviving '*'
        assert_eq!(to_display(3, &holes), 1); // ' '
        assert_eq!(to_display(4, &holes), 2); // 'T'
        // Inverse lands just past the hole, and round-trips for visible offsets.
        assert_eq!(from_display(0, &holes), 2);
        assert_eq!(from_display(1, &holes), 3);
        assert_eq!(from_display(2, &holes), 4);
        for rel in 2..=8 {
            assert_eq!(from_display(to_display(rel, &holes), &holes), rel);
        }
        // Identity when there are no holes.
        assert_eq!(to_display(5, &[]), 5);
        assert_eq!(from_display(5, &[]), 5);
    }

    #[test]
    fn hidden_range_splices_text_and_runs() {
        assert_eq!(remove_holes("*** Title", &[(0, 2)]), "* Title");
        assert_eq!(remove_holes("no holes", &[]), "no holes");
        // Two disjoint holes.
        assert_eq!(remove_holes("abcdef", &[(1, 2), (4, 5)]), "acdf");

        let font = font("Lilex");
        let c: Hsla = rgb(0);
        // One run of len 9 ("*** Title"), collapse [0,2) -> run shrinks to 7.
        let runs = punch_runs(vec![plain_run(9, font.clone(), c)], &[(0, 2)]);
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![7]);
        // A run fully inside a hole is dropped.
        let runs = punch_runs(
            vec![plain_run(2, font.clone(), c), plain_run(5, font.clone(), c)],
            &[(0, 2)],
        );
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![5]);
    }
}
