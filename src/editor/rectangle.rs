//! Emacs `rectangle-mark-mode` (`C-x SPC`): the region between mark and point
//! is shown and acted on as a rectangle of display columns. The highlight
//! reuses evil's block-wise Visual strip (see [`Editor::pane_visual_block`]);
//! the rectangle commands themselves live in scheme/rectangles.scm.

use super::*;

impl Editor {
    /// Whether `rectangle-mark-mode` is on: set by `C-x SPC`, and only while
    /// the mark it started with is still active.
    pub(super) fn rect_mark_active(&self) -> bool {
        self.rect_mark && self.active.mark_active && self.evil == EvilState::Disabled
    }

    /// `C-x SPC`: toggle `rectangle-mark-mode`, setting the mark at point first
    /// when none is active (like Emacs).
    pub(super) fn rectangle_mark_mode(&mut self, cx: &mut Context<Self>) {
        if self.rect_mark_active() {
            self.deactivate_mark(cx);
            self.echo("Rectangle-Mark mode disabled", cx);
            return;
        }
        if !self.active.mark_active {
            let p = self.point();
            self.active.selected_range = p..p;
            self.active.selection_reversed = false;
            self.active.mark_active = true;
        }
        self.rect_mark = true;
        self.echo("Rectangle-Mark mode enabled", cx);
    }

    /// Emacs `deactivate-mark`: drop the region (and `rectangle-mark-mode`),
    /// leaving point where it is.
    pub(super) fn deactivate_mark(&mut self, cx: &mut Context<Self>) {
        let p = self.point();
        self.collapse_to(p);
        cx.notify();
    }

    /// The rectangle-mark-mode corners for the renderer: `(top_row,
    /// bottom_row, mark, point)` with the corners as byte offsets — the
    /// renderer turns them into display columns with its tab width.
    pub(super) fn rect_mark_corners(&self) -> Option<(usize, usize, usize, usize)> {
        if !self.rect_mark_active() {
            return None;
        }
        let point = self.cursor_offset();
        let r = &self.active.selected_range;
        let mark = if self.active.selection_reversed {
            r.end
        } else {
            r.start
        };
        let buf = &self.active.buffer;
        let (ra, rb) = (buf.line_index_at_byte(mark), buf.line_index_at_byte(point));
        Some((ra.min(rb), ra.max(rb), mark, point))
    }

    /// In `rectangle-mark-mode`, the region commands act on the rectangle
    /// (Emacs's `rectangle-mark-mode-map`): `C-w` kills it, `M-w` copies it
    /// and `C-t` replaces it with a string. Returns `false` (run the command
    /// as usual) otherwise.
    pub(super) fn rect_mark_redirect(
        &mut self,
        command: &EditorCommand,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.rect_mark_active() {
            return false;
        }
        let expr = match command {
            EditorCommand::KillRegion => "(kill-rectangle)",
            EditorCommand::KillRingSave => "(copy-rectangle-as-kill)",
            EditorCommand::TransposeChars => "(string-rectangle)",
            _ => return false,
        };
        self.run_script(expr, cx);
        true
    }
}
