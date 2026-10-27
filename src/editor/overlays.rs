//! Overlays, folds, faces, inline images, and line scales for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, RenderImage, SharedString};

use super::*;

/// Overlays / text properties on a buffer (the Emacs overlay + text-property
/// model). Sub-line face ranges layer over tree-sitter highlighting; folded line
/// ranges are hidden from display; text properties carry arbitrary metadata
/// Scheme can read back. All are byte/line-range based and reset only when the
/// buffer itself is replaced.
#[derive(Default)]
pub(super) struct Overlays {
    /// `(byte_range, face)` — overrides syntax highlighting in that range.
    pub(super) faces: Vec<(Range<usize>, SharedString)>,
    /// Byte ranges hidden from display, without changing the buffer text.
    pub(super) hidden: Vec<Range<usize>>,
    /// Folded line ranges (`start..end`, end-exclusive) — hidden from display.
    pub(super) folds: Vec<Range<usize>>,
    /// `(byte_range, key, value)` text properties (Emacs `put-text-property`).
    pub(super) props: Vec<(Range<usize>, String, String)>,
    /// `line -> (suffix_text, face)` virtual text appended after a line (inlay
    /// hints, blame, inline diagnostics).
    pub(super) suffixes: HashMap<usize, (SharedString, SharedString)>,
    /// `line -> columns` of virtual left indentation (display-only), the Org
    /// `org-indent-mode` look. The buffer text is untouched.
    pub(super) indents: HashMap<usize, usize>,
    /// `line -> font-size multiplier` for whole-line scaling (markdown/org
    /// headings render larger). A scaled line reserves extra display rows the
    /// same way an inline image does, so cursor/scroll math stays correct.
    pub(super) scales: HashMap<usize, f32>,
    /// `line -> is_open` fold chevrons painted in the gutter. Scheme declares a
    /// marker on each foldable header line (`set-fold-marker`); the value is the
    /// glyph state (open = expanded). A click on the chevron is hit-tested and
    /// routed to Scheme's `fold-toggle-at-line`, which mutates `folds` to match.
    pub(super) fold_markers: HashMap<usize, bool>,
}

impl Overlays {
    /// Shift the overlays to track an edit that replaced `[start, old_end)`
    /// with `new_len` bytes (`line_delta` net newlines, the edit beginning on
    /// `start_line`; `carry`: newlines were inserted at column 0 of
    /// `start_line`, so its line entries move down with its text). Byte-range
    /// overlays (faces, hidden, props) move with the text; whole-line overlays
    /// (indents, suffixes, scales, chevrons, folds) shift by the line delta
    /// when lines were added/removed. Without this they'd point at stale
    /// positions until the debounced re-fontify, making content below an edit
    /// flicker.
    pub(super) fn adjust_for_edit(
        &mut self,
        start: usize,
        old_end: usize,
        new_len: usize,
        line_delta: isize,
        start_line: usize,
        carry: bool,
    ) {
        let delta = new_len as isize - (old_end - start) as isize;
        let insertion = old_end == start;
        // A byte offset, mapped past the edit: before it is fixed, inside it
        // collapses to its start, after it shifts by the size delta.
        let shift = |pos: usize| -> usize {
            if pos <= start {
                pos
            } else if pos >= old_end {
                (pos as isize + delta).max(start as isize) as usize
            } else {
                start
            }
        };
        if delta != 0 {
            self.faces.retain_mut(|(r, _)| {
                *r = shift(r.start)..shift(r.end);
                r.end > r.start
            });
            // A pure insertion exactly at a hide's start lands BEFORE it: the
            // hide is concealment of the text that was there (a heading's `## `),
            // not of whatever is typed in front of it. Anchoring the start would
            // stretch the hide over the new text — typing at column 0 of a
            // heading, or pressing Enter there, made the whole line vanish.
            self.hidden.retain_mut(|r| {
                let s = if insertion && r.start == start {
                    start + new_len
                } else {
                    shift(r.start)
                };
                *r = s..shift(r.end);
                r.end > r.start
            });
            self.props.retain_mut(|(r, _, _)| {
                *r = shift(r.start)..shift(r.end);
                r.end > r.start
            });
        }
        if line_delta != 0 {
            shift_line_map(&mut self.indents, start_line, line_delta, carry);
            shift_line_map(&mut self.scales, start_line, line_delta, carry);
            shift_line_map(&mut self.suffixes, start_line, line_delta, carry);
            shift_line_map(&mut self.fold_markers, start_line, line_delta, carry);
            // Folded line ranges shift too, else editing above a folded subtree
            // (e.g. pressing RET to push an Org heading down) leaves the fold
            // hiding stale line numbers — the content lands on lines the fold no
            // longer covers while old lines stay hidden. Nothing reapplies folds
            // on edit, so without this the mis-hidden content persists until the
            // next fold cycle.
            shift_fold_ranges(&mut self.folds, start_line, line_delta);
        }
    }

    /// Hidden ranges intersecting the line spanning bytes `line`, as
    /// `(start, end)` offsets relative to the line start, sorted and merged.
    fn holes_in_line(&self, line: Range<usize>) -> Vec<(usize, usize)> {
        let mut holes: Vec<(usize, usize)> = self
            .hidden
            .iter()
            .filter_map(|r| {
                let s = r.start.max(line.start);
                let e = r.end.min(line.end);
                (s < e).then(|| (s - line.start, e - line.start))
            })
            .collect();
        if holes.len() > 1 {
            holes.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(holes.len());
            for (s, e) in holes {
                match merged.last_mut() {
                    Some(last) if s <= last.1 => last.1 = last.1.max(e),
                    _ => merged.push((s, e)),
                }
            }
            holes = merged;
        }
        holes
    }

    /// Whether buffer line `line` is hidden by a fold.
    pub(super) fn is_line_folded(&self, line: usize) -> bool {
        self.folds.iter().any(|r| r.contains(&line))
    }

    /// Virtual left indentation (in columns) for `line`, 0 if none.
    fn indent(&self, line: usize) -> usize {
        self.indents.get(&line).copied().unwrap_or(0)
    }

    /// The font-size multiplier for `line` (1.0 = unscaled).
    fn scale(&self, line: usize) -> f32 {
        self.scales.get(&line).copied().unwrap_or(1.0)
    }
}

/// Re-key a line-indexed overlay map after `line_delta` lines were inserted
/// (positive) or removed (negative) starting just after `start_line`. Entries on
/// or before `start_line` keep their line; later ones shift; any that would land
/// at/before `start_line` (deleted out) are dropped.
pub(super) fn shift_line_map<V>(
    map: &mut HashMap<usize, V>,
    start_line: usize,
    line_delta: isize,
    carry_start_line: bool,
) {
    if line_delta == 0 {
        return;
    }
    // Lines inserted at column 0 of `start_line`: its entry moves with its text.
    // No entry can sit in the freshly inserted lines, so this can't collide.
    let carried = if carry_start_line && line_delta > 0 {
        map.remove(&start_line)
    } else {
        None
    };
    // Re-key only the entries past the edit, in place. Process in an order that
    // never overwrites an entry we haven't moved yet: highest-first when shifting
    // up, lowest-first when shifting down. Entries shifted to at/before
    // `start_line` (deleted out) are dropped.
    let mut movers: Vec<usize> = map.keys().copied().filter(|&l| l > start_line).collect();
    if line_delta > 0 {
        movers.sort_unstable_by(|a, b| b.cmp(a));
    } else {
        movers.sort_unstable();
    }
    for line in movers {
        let Some(v) = map.remove(&line) else { continue };
        let shifted = line as isize + line_delta;
        if shifted > start_line as isize {
            map.insert(shifted as usize, v);
        }
    }
    if let Some(v) = carried {
        map.insert(start_line + line_delta as usize, v);
    }
}

/// Shift folded line ranges to track an edit that added/removed `line_delta`
/// lines beginning on `start_line`. A line at/before `start_line` is fixed; a
/// later line moves by `line_delta` (clamped to `start_line`) — the line-space
/// twin of [`Overlays::adjust_for_edit`]'s byte shift. Ranges that collapse to empty are
/// dropped. Without this, editing above a folded subtree (e.g. RET to push an
/// Org heading down) leaves the fold hiding stale line numbers.
fn shift_fold_ranges(folds: &mut Vec<Range<usize>>, start_line: usize, line_delta: isize) {
    if line_delta == 0 {
        return;
    }
    let shift = |l: usize| -> usize {
        if l <= start_line {
            l
        } else {
            (l as isize + line_delta).max(start_line as isize) as usize
        }
    };
    folds.retain_mut(|r| {
        *r = shift(r.start)..shift(r.end);
        r.end > r.start
    });
}

impl Editor {
    /// Sub-line face ranges, for the element to layer over syntax highlighting.
    pub fn range_faces(&self) -> &[(Range<usize>, SharedString)] {
        &self.active.overlays.faces
    }

    /// Hidden ranges (`overlay-hide`) intersecting buffer line `line`, expressed
    /// as `(start, end)` byte offsets relative to the line start, sorted and
    /// merged. The renderer splices these out so the bytes collapse from display.
    pub fn line_holes(&self, line: usize) -> Vec<(usize, usize)> {
        // Out of range for the active buffer → no holes. Defensive: the caller
        // only asks for selected-pane rows (which index this buffer), but a stray
        // line index must never panic the renderer via `line_range`.
        if self.active.overlays.hidden.is_empty() || line >= self.active.buffer.line_count() {
            return Vec::new();
        }
        let lr = self.active.buffer.line_range(line);
        self.active.overlays.holes_in_line(lr)
    }

    /// Whether any line is folded (the element's fast path skips fold handling).
    pub fn has_folds(&self) -> bool {
        !self.active.overlays.folds.is_empty() || self.active.narrow.is_some()
    }

    /// Whether buffer line `line` is hidden by a fold or lies outside the
    /// narrowing.
    pub fn is_line_invisible(&self, line: usize) -> bool {
        self.narrow_lines()
            .is_some_and(|(first, last)| line < first || line > last)
            || self.active.overlays.is_line_folded(line)
    }

    /// The visible text lines before `line` (all lines when `None`): those
    /// inside the narrowing, less any folded.
    fn visible_lines_before(&self, line: Option<usize>) -> usize {
        let count = self.active.buffer.line_count();
        let (first, last) = self.narrow_lines().unwrap_or((0, count.saturating_sub(1)));
        let end = line.unwrap_or(count).min(last + 1).max(first);
        if self.active.overlays.folds.is_empty() {
            end - first
        } else {
            (first..end)
                .filter(|&i| !self.active.overlays.is_line_folded(i))
                .count()
        }
    }

    /// Whether any gutter fold chevron is set (element's fast path skips them).
    pub fn has_fold_markers(&self) -> bool {
        !self.active.overlays.fold_markers.is_empty()
    }

    /// The fold chevron state for buffer `line`, if one is set: `Some(true)` for
    /// an open (expanded) chevron, `Some(false)` for a collapsed one.
    pub fn fold_marker_at(&self, line: usize) -> Option<bool> {
        self.active.overlays.fold_markers.get(&line).copied()
    }

    /// Number of visible (non-folded) lines — the element's screen-row count.
    pub fn display_line_count(&self) -> usize {
        let text = self.visible_lines_before(None);
        let ghost = self.ghost_rows().map_or(0, |(_, rows)| rows);
        text + self.image_rows_total() + self.scale_rows_total() + ghost
    }

    /// Screen row a buffer line maps to once folds are collapsed and inline
    /// images on earlier lines have reserved their rows.
    pub fn display_row_of_line(&self, line: usize) -> usize {
        let text = self.visible_lines_before(Some(line));
        let ghost = match self.ghost_rows() {
            Some((g, rows)) if g < line => rows,
            _ => 0,
        };
        text + self.image_rows_before(line) + self.scale_rows_before(line) + ghost
    }

    // --- Ghost text (copilot-style inline suggestion) ----------------------
    /// The visible ghost suggestion, if any: `(anchor byte, text)`. Validated
    /// at read time — it only counts as visible while point still sits on the
    /// byte it was shown at, so any cursor motion hides it without needing a
    /// hook on every movement path (edits clear it eagerly in `edit`; undo
    /// and redo, which bypass `edit`, clear it themselves). Hidden while a
    /// region is active, so a suggestion can neither render into a selection
    /// highlight nor be accepted beside text an insert would normally replace.
    pub fn ghost_at(&self) -> Option<(usize, &str)> {
        if !self.active.selected_range.is_empty() {
            return None;
        }
        self.active
            .ghost
            .as_ref()
            .filter(|g| g.at == self.cursor_offset())
            .map(|g| (g.at, g.text.as_str()))
    }

    /// The visible ghost's `(anchor line, reserved tail rows)` — one row per
    /// `\n` in the suggestion — when it contributes to row accounting. `None`
    /// for a single-line ghost, or when the anchor line is folded away (the
    /// `is_line_invisible` guard image/scale rows also apply, so rows are
    /// never counted for a line the row loop skips). The single source for
    /// `display_line_count` / `display_row_of_line` / `ensure_cursor_visible`
    /// and the element's row loop, so count and paint can't disagree.
    pub fn ghost_rows(&self) -> Option<(usize, usize)> {
        let (at, text) = self.ghost_at()?;
        let tail = text.matches('\n').count();
        if tail == 0 {
            return None;
        }
        let line = self.active.buffer.line_index_at_byte(at);
        (!self.is_line_invisible(line)).then_some((line, tail))
    }

    /// Virtual left indentation (in columns) for buffer `line`, 0 if none.
    /// Display-only — the Org `org-indent-mode` look.
    pub fn line_indent(&self, line: usize) -> usize {
        self.active.overlays.indent(line)
    }

    /// Whether the active buffer is an Org buffer (set by `org-mode`). The
    /// renderer drops font ligatures here so prose markup like `**` / `***`
    /// doesn't ligate into a single shifted glyph.
    pub fn is_org(&self) -> bool {
        self.active
            .locals
            .get("org-active")
            .is_some_and(|v| v == "t")
    }

    // --- Inline images (Org / LaTeX preview) ------------------------------
    /// Whether the active buffer displays any inline image.
    pub fn has_line_images(&self) -> bool {
        !self.active.line_images.is_empty()
    }

    /// Decoded image + its displayed (width, height) in px for `line`, or `None`
    /// if the line has no image / it hasn't decoded yet. The width is clamped to
    /// the line's own override (set via `set-line-image`'s optional width arg),
    /// or [`INLINE_IMAGE_MAX_WIDTH`] when the override is 0; height keeps aspect.
    pub(super) fn image_display(&self, line: usize) -> Option<(Arc<RenderImage>, f32, f32)> {
        let (path, override_w) = self.active.line_images.get(&line)?;
        let img = self.image_cache.get(path)?;
        if img.width == 0 {
            return None;
        }
        let cap = if *override_w > 0 {
            *override_w as f32
        } else {
            INLINE_IMAGE_MAX_WIDTH
        };
        let (w, h) = (img.width as f32, img.height as f32);
        let disp_w = w.min(cap);
        Some((img.data.clone(), disp_w, h * (disp_w / w)))
    }

    /// How many display rows the image on `line` reserves below it (0 if the
    /// line has no image, the image hasn't decoded yet, or the line is folded
    /// away). Width-independent of the window, so folds/scroll stay consistent.
    pub fn image_rows_at(&self, line: usize) -> usize {
        if self.is_line_invisible(line) {
            return 0;
        }
        let lh = f32::from(self.line_height);
        match self.image_display(line) {
            Some((_, _, disp_h)) if lh > 0.0 => (disp_h / lh).ceil().max(1.0) as usize,
            _ => 0,
        }
    }

    /// The decoded image to paint below `line`, with its displayed pixel size.
    pub fn line_image_at(&self, line: usize) -> Option<(Arc<RenderImage>, f32, f32)> {
        self.image_display(line)
    }

    pub(super) fn image_rows_total(&self) -> usize {
        if self.active.line_images.is_empty() {
            return 0;
        }
        self.active
            .line_images
            .keys()
            .map(|&l| self.image_rows_at(l))
            .sum()
    }

    /// Whether any line is scaled (e.g. headings rendered larger).
    pub fn has_line_scales(&self) -> bool {
        !self.active.overlays.scales.is_empty()
    }

    /// The font-size multiplier for `line` (1.0 = unscaled).
    pub fn line_scale(&self, line: usize) -> f32 {
        self.active.overlays.scale(line)
    }

    /// Extra display rows a scaled line reserves beyond its own — so a heading
    /// taller than one row claims that space, exactly like an inline image. A
    /// scale of S occupies `ceil(S)` rows total, hence `ceil(S) - 1` extra.
    pub fn scale_rows_at(&self, line: usize) -> usize {
        if self.is_line_invisible(line) {
            return 0;
        }
        let s = self.line_scale(line);
        if s > 1.0 {
            (s.ceil() as usize).saturating_sub(1)
        } else {
            0
        }
    }

    pub(super) fn scale_rows_total(&self) -> usize {
        self.active
            .overlays
            .scales
            .keys()
            .map(|&l| self.scale_rows_at(l))
            .sum()
    }

    pub(super) fn scale_rows_before(&self, line: usize) -> usize {
        self.active
            .overlays
            .scales
            .keys()
            .filter(|&&l| l < line)
            .map(|&l| self.scale_rows_at(l))
            .sum()
    }

    pub(super) fn image_rows_before(&self, line: usize) -> usize {
        if self.active.line_images.is_empty() {
            return 0;
        }
        self.active
            .line_images
            .keys()
            .filter(|&&l| l < line)
            .map(|&l| self.image_rows_at(l))
            .sum()
    }

    /// Record an inline image on `line` and ensure its bitmap is decoded (async,
    /// off the UI thread). Re-uses the cache so a re-scan never re-decodes. The
    /// line→path record follows [`Self::redirect_target`] so a redirected batch
    /// decorates the buffer it re-rendered; the bitmap cache is process-wide.
    pub(super) fn set_line_image(
        &mut self,
        line: usize,
        path: String,
        max_width: usize,
        cx: &mut Context<Self>,
    ) {
        let path = PathBuf::from(expand_path(&path));
        let redirected = self.redirect_index().is_some();
        self.redirect_target()
            .line_images
            .insert(line, (path.clone(), max_width));
        if self.image_cache.contains_key(&path) {
            // Only the active buffer's rows moved under the cursor.
            if !redirected {
                self.ensure_cursor_visible();
            }
            cx.notify();
            return;
        }
        // Decode off the UI thread, then install on the foreground and repaint.
        let decode_path = path.clone();
        let decode = cx
            .background_executor()
            .spawn(async move { load_inline_image(&decode_path) });
        cx.spawn(async move |this, cx| {
            let decoded = decode.await;
            this.update(cx, |editor, cx| {
                if let Some(img) = decoded {
                    editor.image_cache.insert(path, img);
                    editor.ensure_cursor_visible();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn overlay_fold_lines(
        &mut self,
        start_line: usize,
        end_line: usize,
        cx: &mut Context<Self>,
    ) {
        let last = self.active.buffer.line_count();
        let (start, end) = (start_line.min(last), end_line.min(last));
        if start < end {
            self.active.overlays.folds.push(start..end);
            self.ensure_cursor_visible();
            cx.notify();
        }
    }

    pub(super) fn overlay_unfold_all(&mut self, cx: &mut Context<Self>) {
        if !self.active.overlays.folds.is_empty() {
            self.active.overlays.folds.clear();
            self.ensure_cursor_visible();
            cx.notify();
        }
    }

    pub(super) fn overlay_set_fold_marker(
        &mut self,
        line: usize,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        self.active.overlays.fold_markers.insert(line, open);
        cx.notify();
    }

    pub(super) fn overlay_clear_fold_markers(&mut self, cx: &mut Context<Self>) {
        if !self.active.overlays.fold_markers.is_empty() {
            self.active.overlays.fold_markers.clear();
            cx.notify();
        }
    }

    pub(super) fn put_text_property(
        &mut self,
        start: usize,
        end: usize,
        key: String,
        value: String,
    ) {
        let len = self.active.buffer.len();
        let (start, end) = (start.min(len), end.min(len));
        if start < end {
            self.active.overlays.props.push((start..end, key, value));
        }
    }

    // --- Async (subprocess streaming, timers) -----------------------------
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{shift_fold_ranges, shift_line_map};

    #[test]
    fn shift_line_map_tracks_inserted_and_deleted_lines() {
        let base: HashMap<usize, usize> = [(0, 10), (3, 20), (5, 30)].into_iter().collect();
        // Insert 2 lines after line 3: keys <=3 stay, keys >3 shift by +2.
        let mut ins = base.clone();
        shift_line_map(&mut ins, 3, 2, false);
        assert_eq!(ins.get(&0), Some(&10));
        assert_eq!(ins.get(&3), Some(&20));
        assert_eq!(ins.get(&7), Some(&30)); // 5 -> 7
        assert_eq!(ins.get(&5), None);
        // Delete 2 lines after line 0: key 3 -> 1, key 5 -> 3.
        let mut del = base.clone();
        shift_line_map(&mut del, 0, -2, false);
        assert_eq!(del.get(&0), Some(&10));
        assert_eq!(del.get(&1), Some(&20));
        assert_eq!(del.get(&3), Some(&30));
        // A key that would collapse into the deleted region is dropped.
        let mut collapse = base.clone();
        shift_line_map(&mut collapse, 0, -4, false);
        assert_eq!(collapse.get(&0), Some(&10)); // 0 stays
        assert!(!collapse.values().any(|&v| v == 20)); // 3 -> -1 -> dropped
    }

    #[test]
    // These fixtures really are one-element vectors OF ranges (fold spans),
    // which the lint would reject as a probable typo for `(a..b).collect()`.
    #[allow(clippy::single_range_in_vec_init)]
    fn shift_fold_ranges_tracks_edits_above_and_within_a_fold() {
        use std::ops::Range;
        // A folded Org subtree hiding body lines 1..4 (heading on line 0).
        let base: Vec<Range<usize>> = vec![1..4];
        // Press RET above the heading: +1 line at line 0 pushes the fold down.
        // This is the reported bug — content must stay hidden at its NEW lines.
        let mut ins = base.clone();
        shift_fold_ranges(&mut ins, 0, 1);
        assert_eq!(ins, vec![2..5]);
        // Edit on the heading line (== fold start - 1) still counts as "before"
        // the hidden range, so the whole range shifts by the delta.
        let mut on_heading = base.clone();
        shift_fold_ranges(&mut on_heading, 0, 2);
        assert_eq!(on_heading, vec![3..6]);
        // Delete a line above: the fold moves up to keep covering the body.
        let mut del = vec![3..6];
        shift_fold_ranges(&mut del, 0, -1);
        assert_eq!(del, vec![2..5]);
        // An edit inside the folded region grows only the end (start is fixed).
        let mut within = vec![1..4];
        shift_fold_ranges(&mut within, 2, 1);
        assert_eq!(within, vec![1..5]);
        // Deleting through the whole folded body collapses the range (both ends
        // clamp back to the edit line) so it is dropped rather than left hiding
        // the wrong lines.
        let mut collapse = vec![1..4];
        shift_fold_ranges(&mut collapse, 0, -4);
        assert!(collapse.is_empty());
        // A fold entirely below the edit still shifts; delta 0 is a no-op.
        let mut noop = vec![1..4];
        shift_fold_ranges(&mut noop, 0, 0);
        assert_eq!(noop, vec![1..4]);
    }
}

/// Line-keyed decorations that live on the [`Document`] rather than in
/// [`Overlays`] follow edits too.
#[cfg(test)]
mod editor_tests {
    use gpui::{SharedString, TestAppContext};

    use super::*;

    #[gpui::test]
    fn gutter_dots_and_line_images_follow_line_edits(cx: &mut TestAppContext) {
        test_context(cx);
        let editor = cx.new(Editor::new);
        editor.update(cx, |e, _| {
            e.switch_to_doc(make_document("t".into(), None, "a\nb\nc\n"));
            e.active
                .gutter_dots
                .insert(1, SharedString::from("dap-breakpoint"));
            e.active
                .line_images
                .insert(2, (PathBuf::from("/tmp/x.png"), 0));
            // A line added above both: they move down with their text.
            e.edit(0..0, "new\n");
            assert_eq!(
                e.active.gutter_dots.keys().copied().collect::<Vec<_>>(),
                [2]
            );
            assert_eq!(
                e.active.line_images.keys().copied().collect::<Vec<_>>(),
                [3]
            );
            // Deleting that line moves them back up.
            e.edit(0..4, "");
            assert!(e.active.gutter_dots.contains_key(&1));
            assert!(e.active.line_images.contains_key(&2));
            // An edit within a line leaves them put.
            e.edit(0..1, "xyz");
            assert!(e.active.gutter_dots.contains_key(&1));
            assert!(e.active.line_images.contains_key(&2));
        });
    }
}
