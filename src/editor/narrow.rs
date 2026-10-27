//! Narrowing (Emacs `C-x n`) for [`Editor`]: restrict the active buffer to an
//! accessible byte range.
//!
//! The rope keeps the whole text and every offset stays absolute (as in Emacs,
//! where `point-min` is simply > 1 while narrowed). The restriction is one
//! per-buffer range, [`Document::narrow`], that the rest of the editor
//! consults at a few seams:
//!
//! - point: `move_to` / `select_to` clamp into it, so every motion (Rust or
//!   Scheme `goto-char`) stops at the bounds;
//! - edits: `edit` refuses a change reaching outside it (see
//!   [`Editor::narrow_edit`]), and `apply_tracked_replace` moves the bounds
//!   with every change, undo included, like a pair of markers;
//! - display: lines outside it are invisible (the fold row machinery), the
//!   boundary lines are clipped, and line numbers count from its first line;
//! - search, `point-min` / `point-max`, and the mode line read it.
//!
//! Saving goes through `Document::save_bytes`, which reads the rope, so a save
//! always writes the whole buffer.

use std::ops::Range;

use gpui::Context;

use crate::script::EditorCommand;

use super::*;

/// Tree-sitter node kinds `narrow-to-defun` treats as a defun: functions and
/// methods, then type-level items (struct/class/impl/…) for point between
/// methods. The innermost enclosing match wins (Emacs's `nested` tactic).
const DEFUN_KINDS: &[&str] = &[
    "function_item",
    "function_definition",
    "function_declaration",
    "method_declaration",
    "method_definition",
    "generator_function_declaration",
    "constructor_declaration",
    "method",
    "singleton_method",
    "fn_declaration",
    "local_function",
    "test_declaration",
    "function_clause",
    "struct_item",
    "enum_item",
    "impl_item",
    "trait_item",
    "union_item",
    "mod_item",
    "class_definition",
    "class_declaration",
    "class",
    "interface_declaration",
    "enum_declaration",
    "struct_specifier",
    "class_specifier",
    "namespace_definition",
    "type_declaration",
    "module",
];

/// The defun in a tree-sitter ancestor chain (innermost first, the root last,
/// as `Highlighter::node_at` returns it): the innermost node of a
/// [`DEFUN_KINDS`] kind, else the top-level form holding point (the root's
/// child — a Lisp `(define …)`, which is just a `list` to tree-sitter).
pub(super) fn defun_in_chain(chain: &[(String, usize, usize)]) -> Option<(usize, usize)> {
    chain
        .iter()
        .find(|(kind, _, _)| DEFUN_KINDS.contains(&kind.as_str()))
        .or_else(|| chain.len().checked_sub(2).map(|i| &chain[i]))
        .map(|&(_, s, e)| (s, e))
        .filter(|(s, e)| s < e)
}

/// Where a narrowing bound at `p` lands after `start..old_end` was replaced by
/// `new_len` bytes. `advance` is the marker's insertion type: the end bound
/// advances over text inserted at it (typing at `point-max` stays visible),
/// the start bound doesn't (text inserted at `point-min` lands inside).
pub(super) fn shift_bound(
    p: usize,
    start: usize,
    old_end: usize,
    new_len: usize,
    advance: bool,
) -> usize {
    if p < start || (p == start && (!advance || old_end > start)) {
        p
    } else if p >= old_end {
        p - (old_end - start) + new_len
    } else if advance {
        start + new_len
    } else {
        start
    }
}

/// The smallest edit with the same effect as replacing `old` (at `at`) with
/// `new`: their common prefix and suffix trimmed, on char boundaries. Returns
/// the buffer range and the slice of `new` that still has to be written.
pub(super) fn minimal_edit(at: usize, old: &str, new: &str) -> (Range<usize>, Range<usize>) {
    let (o, n) = (old.as_bytes(), new.as_bytes());
    let mut pre = o.iter().zip(n).take_while(|(a, b)| a == b).count();
    while !(old.is_char_boundary(pre) && new.is_char_boundary(pre)) {
        pre -= 1;
    }
    let room = (o.len() - pre).min(n.len() - pre);
    let mut suf = o
        .iter()
        .rev()
        .zip(n.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    while !(old.is_char_boundary(o.len() - suf) && new.is_char_boundary(n.len() - suf)) {
        suf -= 1;
    }
    (at + pre..at + o.len() - suf, pre..n.len() - suf)
}

/// Where old byte `p` lands in `new`, following a line diff of `old` →
/// `new`: unchanged text carries it along. Inside a rewritten hunk it goes to
/// the hunk's start in `new`, or past the hunk's new text when `advance` (the
/// end bound — see [`shift_bound`]); a pure insertion at `p` lands after the
/// start bound and before the end bound, as for an ordinary edit.
pub(super) fn map_through_diff(old: &str, new: &str, p: usize, advance: bool) -> usize {
    use similar::{ChangeTag, TextDiff};
    let (mut o, mut n) = (0, 0);
    // Just past a deletion: an insertion here belongs to the same hunk.
    let mut after_delete = false;
    // (`advance`) `p` was deleted: it lands after the hunk's insertions.
    let mut in_hunk = false;
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Insert if p == o && !advance && !after_delete => return n,
            ChangeTag::Insert => n += len,
            ChangeTag::Equal if in_hunk => return n,
            ChangeTag::Equal if p < o + len => return n + (p - o),
            ChangeTag::Equal => {
                (o, n) = (o + len, n + len);
                after_delete = false;
            }
            ChangeTag::Delete => {
                if p < o + len && !in_hunk {
                    if !advance {
                        return n;
                    }
                    in_hunk = true;
                }
                o += len;
                after_delete = true;
            }
        }
    }
    if in_hunk { n } else { n + p.saturating_sub(o) }
}

/// An edit that the narrowing allows (see [`Editor::narrow_edit`]).
pub(super) struct NarrowedEdit {
    /// The buffer range to replace.
    pub range: Range<usize>,
    /// The part of the new text to write there.
    pub text: Range<usize>,
    /// The narrowing once the edit lands, when a whole-buffer rewrite moves
    /// the bounds more than the edit's own shift would.
    pub bounds: Option<Range<usize>>,
}

impl Editor {
    /// The accessible byte range: the narrowing, else the whole buffer.
    pub(super) fn accessible(&self) -> Range<usize> {
        self.active
            .narrow
            .clone()
            .unwrap_or(0..self.active.buffer.len())
    }

    /// `offset` clamped into the accessible range.
    pub(super) fn clamp_accessible(&self, offset: usize) -> usize {
        match &self.active.narrow {
            Some(r) => offset.clamp(r.start, r.end),
            None => offset,
        }
    }

    /// Whether the active buffer is narrowed (Emacs `buffer-narrowed-p`).
    pub fn is_narrowed(&self) -> bool {
        self.active.narrow.is_some()
    }

    /// The first and last buffer lines of the narrowing, if narrowed.
    pub(super) fn narrow_lines(&self) -> Option<(usize, usize)> {
        let r = self.active.narrow.as_ref()?;
        let b = &self.active.buffer;
        Some((b.line_index_at_byte(r.start), b.line_index_at_byte(r.end)))
    }

    /// Pane `id`'s buffer narrowing, for the renderer (an inactive pane shows
    /// its own buffer, narrowed or not).
    pub fn pane_narrow(&self, id: usize) -> Option<Range<usize>> {
        self.pane_doc(id).narrow.clone()
    }

    /// Check an edit of `range` (already clamped to the buffer) against the
    /// narrowing. `Ok` carries the range to edit and the part of the new text
    /// to write: unchanged when unnarrowed or inside the bounds, else trimmed
    /// to the minimal change — so a whole-buffer rewrite (`replace-region 0
    /// (buffer-size)`, a formatter) that only touches the accessible text is
    /// allowed and the bounds stay put around it. A whole-buffer replacement
    /// is always allowed (revert, format-on-save); anything else reaching past
    /// the bounds is refused with Emacs's message.
    pub(super) fn narrow_edit(
        &self,
        range: Range<usize>,
        new_text: &str,
    ) -> Result<NarrowedEdit, &'static str> {
        let unchanged = Ok(NarrowedEdit {
            range: range.clone(),
            text: 0..new_text.len(),
            bounds: None,
        });
        let Some(acc) = self.active.narrow.clone() else {
            return unchanged;
        };
        let inside = |r: &Range<usize>| acc.start <= r.start && r.end <= acc.end;
        if inside(&range) {
            return unchanged;
        }
        let old = self.active.buffer.slice_to_string(range.clone());
        let (r, text) = minimal_edit(range.start, &old, new_text);
        if inside(&r) {
            Ok(NarrowedEdit {
                range: r,
                text,
                bounds: None,
            })
        } else if range == (0..self.active.buffer.len()) {
            // Changes on both sides of a bound (a formatter): one edit
            // spanning the narrowing would swallow it, so carry the bounds
            // over the rewrite by diff instead.
            let s = map_through_diff(&old, new_text, acc.start, false);
            let e = map_through_diff(&old, new_text, acc.end, true);
            Ok(NarrowedEdit {
                range: r,
                text,
                bounds: Some(s..e.max(s)),
            })
        } else if r.start < acc.start {
            Err("Beginning of buffer")
        } else {
            Err("End of buffer")
        }
    }

    /// Move the narrowing bounds over a replacement of `start..old_end` by
    /// `new_len` bytes (see [`shift_bound`]). Called for every change, undo
    /// and redo included.
    pub(super) fn shift_narrow(&mut self, start: usize, old_end: usize, new_len: usize) {
        if let Some(r) = self.active.narrow.as_mut() {
            let s = shift_bound(r.start, start, old_end, new_len, false);
            let e = shift_bound(r.end, start, old_end, new_len, true);
            *r = s..e.max(s);
        }
    }

    /// Restrict the active buffer to `start..end` (normalized). Point moves
    /// inside and the mark is deactivated, as in Emacs. Narrowing to the
    /// whole buffer is the same as widening.
    pub(super) fn narrow_to(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        if self.active.grid.is_some() {
            self.echo("Use grid-narrow-to-selection (C-x n n) in a grid", cx);
            return;
        }
        let r = self.active.buffer.clamp_range(start..end);
        self.active.narrow = (r != (0..self.active.buffer.len())).then_some(r);
        self.active.mark_active = false;
        let p = self.clamp_accessible(self.point());
        self.active.selected_range = p..p;
        self.active.selection_reversed = false;
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Emacs `widen`: make the whole buffer accessible again.
    pub(super) fn widen(&mut self, cx: &mut Context<Self>) {
        self.active.narrow = None;
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Emacs `narrow-to-defun`: narrow to the whole lines of the defun around
    /// point, found from the tree-sitter parse (see [`defun_in_chain`]).
    pub(super) fn narrow_to_defun(&mut self, cx: &mut Context<Self>) {
        let p = self.point();
        let defun = self.active.highlighter.as_ref().and_then(|h| {
            defun_in_chain(&h.node_at(p))
                .or_else(|| p.checked_sub(1).and_then(|q| defun_in_chain(&h.node_at(q))))
        });
        let Some((s, e)) = defun else {
            self.echo("No defun at point", cx);
            return;
        };
        let b = &self.active.buffer;
        let start = b.line_range_at(s).start;
        let last = b.line_index_at_byte(e - 1);
        let end = if last + 1 < b.line_count() {
            b.line_range(last + 1).start
        } else {
            b.len()
        };
        self.narrow_to(start, end, cx);
    }

    /// The narrowing commands.
    pub(super) fn cmd_narrow(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            NarrowToRegion { start, end } => self.narrow_to(start, end, cx),
            NarrowToDefun => self.narrow_to_defun(cx),
            Widen => self.widen(cx),
            other => return Err(other),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{defun_in_chain, map_through_diff, minimal_edit, shift_bound};

    #[test]
    fn bounds_move_like_markers() {
        // Edits before the bound shift it; edits after leave it.
        assert_eq!(shift_bound(10, 2, 4, 5, false), 13);
        assert_eq!(shift_bound(10, 12, 14, 0, true), 10);
        // Insertion AT the start bound lands inside (start stays) …
        assert_eq!(shift_bound(10, 10, 10, 3, false), 10);
        // … and at the end bound extends it (end advances).
        assert_eq!(shift_bound(10, 10, 10, 3, true), 13);
        // A deletion swallowing the bound pulls it to the deletion's start.
        assert_eq!(shift_bound(10, 8, 12, 0, false), 8);
        assert_eq!(shift_bound(10, 8, 12, 1, true), 9);
        // Replacing text just past the end bound doesn't pull it along.
        assert_eq!(shift_bound(10, 10, 12, 4, true), 10);
    }

    #[test]
    fn minimal_edit_trims_common_ends_on_char_boundaries() {
        assert_eq!(minimal_edit(0, "abcXdef", "abcYYdef"), (3..4, 3..5));
        assert_eq!(minimal_edit(5, "same", "same"), (9..9, 4..4));
        // `é` and `è` share their first byte: the trim backs off to the char.
        assert_eq!(minimal_edit(0, "aé", "aè"), (1..3, 1..3));
        // Pure insertion with a repeated char: prefix and suffix can't overlap.
        assert_eq!(minimal_edit(0, "aa", "aaa"), (2..2, 2..3));
    }

    #[test]
    fn bounds_follow_a_rewrite_through_the_diff() {
        let old = "zero\none\ntwo\nthree\n";
        let new = "ZERO\nzero2\none\ntwo\nthree\nfour\n";
        // `one\ntwo\n` was 5..13; it is now 11..19.
        assert_eq!(map_through_diff(old, new, 5, false), 11);
        assert_eq!(map_through_diff(old, new, 13, true), 19);
        // A bound in a rewritten line goes to the hunk's start (start bound)
        // or past its insertion (end bound).
        assert_eq!(map_through_diff(old, new, 2, false), 0);
        assert_eq!(map_through_diff(old, new, 2, true), 11);
        assert_eq!(map_through_diff(old, new, old.len(), true), new.len());
    }

    #[test]
    fn defun_prefers_function_kinds_then_top_level_form() {
        let chain = |kinds: &[(&str, usize, usize)]| -> Vec<(String, usize, usize)> {
            kinds
                .iter()
                .map(|&(k, s, e)| (k.to_string(), s, e))
                .collect()
        };
        let rust = chain(&[
            ("identifier", 20, 21),
            ("block", 15, 40),
            ("function_item", 10, 40),
            ("impl_item", 0, 50),
            ("source_file", 0, 60),
        ]);
        assert_eq!(defun_in_chain(&rust), Some((10, 40)));
        let lisp = chain(&[
            ("symbol", 9, 10),
            ("list", 8, 12),
            ("list", 0, 20),
            ("program", 0, 30),
        ]);
        assert_eq!(defun_in_chain(&lisp), Some((0, 20)));
        assert_eq!(defun_in_chain(&chain(&[("program", 0, 30)])), None);
    }
}
