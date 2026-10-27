//! Evil (modal editing) + search/ex-command methods for [`Editor`].

use regex::Regex;

use super::ex::{ExCmd, parse_ex_line};
use super::*;

/// A block-wise Visual rectangle: rows `top..=bottom` (0-based lines) and
/// character columns `left..=right` (inclusive, vim semantics).
#[derive(Clone, Copy, Debug)]
struct BlockRect {
    top: usize,
    bottom: usize,
    left: usize,
    right: usize,
}

impl BlockRect {
    fn rows(self) -> std::ops::RangeInclusive<usize> {
        self.top..=self.bottom
    }
}

impl Editor {
    /// Whether Vim (evil) modal editing is on — not plain Emacs keys and not
    /// Helix, which reuses the state machine but has no Vim registers or marks.
    pub(super) fn vim_active(&self) -> bool {
        self.evil != EvilState::Disabled && !self.modal_helix
    }

    /// Leave Helix behind when switching modal styles: `modal_helix` steers
    /// EVERY keymap lookup (see `evil_keymap`), and Helix's extra selections
    /// don't survive the switch.
    fn leave_helix(&mut self, cx: &mut Context<Self>) {
        if self.modal_helix {
            self.modal_helix = false;
            if self.has_extra_cursors() {
                self.mc_clear(cx);
            }
        }
    }

    /// True in Normal/Visual, where plain keys are commands (not self-insert).
    pub(super) fn evil_intercepts(&self) -> bool {
        matches!(
            self.evil,
            EvilState::Normal | EvilState::Visual | EvilState::VisualLine | EvilState::VisualBlock
        )
    }

    /// The keymap name plain keys resolve against in the current evil state.
    /// Under Helix the selection-first bindings live in a single `helix-normal`
    /// map — Helix keeps selections in Normal instead of a separate Visual state.
    pub(super) fn evil_keymap(&self) -> &'static str {
        if self.modal_helix {
            return "helix-normal";
        }
        match self.evil {
            EvilState::Visual => "evil-visual",
            EvilState::VisualLine => "evil-visual-line",
            EvilState::VisualBlock => "evil-visual-block",
            _ => "evil-normal",
        }
    }

    /// The keymap consulted for chords while in Insert state (e.g. Escape →
    /// Normal). Helix and evil keep separate insert maps so each owns its Escape.
    pub(super) fn evil_insert_keymap(&self) -> &'static str {
        if self.modal_helix {
            "helix-insert"
        } else {
            "evil-insert"
        }
    }

    /// Keymap-name prefix for special-buffer (magit/dired/…) modal overlays: the
    /// active style's Normal map, scoped per major mode as `<prefix>:<major>`.
    /// Lets Helix overlay its own collection bindings on read-only buffers, just
    /// like evil-collection does, instead of inheriting evil's.
    pub(super) fn modal_collection_prefix(&self) -> &'static str {
        if self.modal_helix {
            "helix-normal"
        } else {
            "evil-normal"
        }
    }

    /// Whether the `"auto"` cursor style should be a block: always, except in
    /// evil insert mode (a bar signals insertion, the vim convention). With evil
    /// disabled this is a plain block — matching the default cursor shape.
    pub fn wants_block_cursor(&self) -> bool {
        !matches!(self.evil, EvilState::Insert)
    }

    /// Vim-style `-- MODE --` banner shown in the echo bar when no chord hint
    /// is pending. None in Normal and when disabled — Normal is the resting
    /// state and (like vim) shows nothing. The bar reverts to this whenever
    /// `status` clears.
    pub fn evil_mode_name(&self) -> Option<&'static str> {
        // Helix keeps selections in Normal, so — unlike vim — show a persistent
        // mode indicator there too (the echo row doubles as the modal banner), so
        // it's obvious you're in Helix and not plain Emacs keys.
        if self.modal_helix {
            return match self.evil {
                EvilState::Disabled => None,
                EvilState::Insert => Some("-- INSERT --"),
                _ => Some("-- HELIX --"),
            };
        }
        match self.evil {
            EvilState::Disabled | EvilState::Normal => None,
            EvilState::Insert => Some("-- Insert --"),
            EvilState::Visual => Some("-- Visual --"),
            EvilState::VisualLine => Some("-- Visual Line --"),
            EvilState::VisualBlock => Some("-- Visual Block --"),
        }
    }

    pub(super) fn evil_enable(&mut self, cx: &mut Context<Self>) {
        // Or `(evil-mode)` from Helix silently keeps the Helix keymaps (and
        // the Helix banner).
        self.leave_helix(cx);
        self.evil = EvilState::Normal;
        self.active.mark_active = false;
        cx.notify();
    }

    /// Enter Helix modal editing: reuse the Normal/Insert state machine but route
    /// keys through the `helix-*` keymaps (see [`Self::evil_keymap`]).
    pub(super) fn helix_enable(&mut self, cx: &mut Context<Self>) {
        self.modal_helix = true;
        self.evil = EvilState::Normal;
        self.active.mark_active = false;
        // Normal mode shows no banner, so confirm the switch in the echo area.
        self.echo("Helix mode enabled", cx);
        cx.notify();
    }

    /// Leave Helix modal editing — back to plain Emacs keys.
    pub(super) fn helix_disable(&mut self, cx: &mut Context<Self>) {
        self.modal_helix = false;
        self.evil = EvilState::Disabled;
        if self.has_extra_cursors() {
            self.mc_clear(cx);
        }
        self.echo("Helix mode disabled", cx);
        cx.notify();
    }

    /// Escape from Insert back to Helix Normal. Unlike [`Self::evil_normal_state`]
    /// this keeps any secondary cursors (so `c`/`i`/`a` across several selections
    /// leave you with one cursor per selection, the Helix way) and does not nudge
    /// the cursor left (Helix has no Insert-exit nudge).
    pub(super) fn helix_normal_state(&mut self, cx: &mut Context<Self>) {
        self.evil = EvilState::Normal;
        self.active.mark_active = false;
        cx.notify();
    }

    pub(super) fn evil_normal_state(&mut self, cx: &mut Context<Self>) {
        let was_insert = matches!(self.evil, EvilState::Insert);
        if was_insert {
            let p = self.point();
            self.set_auto_mark('^', p);
        }
        self.evil = EvilState::Normal;
        // One insert session (with the command that opened it: `cw`, `o`, …)
        // undoes as one step, as in Vim.
        if was_insert {
            // Esc closes the completion popup with Insert (vim); a popup left
            // up would still catch Enter/Tab — e.g. swallow a `:w<RET>`.
            self.completion_popup = None;
            self.squash_insert_session();
            // `".` — the text typed in the session.
            let typed = std::mem::take(&mut self.vim.insert_acc);
            self.vim.registers.set('.', &typed, false);
        }
        // Collapse any block-insert cursors (vim leaves a single cursor after a
        // blockwise `I`/`A` once you press Escape). No-op without extras.
        if self.has_extra_cursors() {
            self.mc_clear(cx);
        }
        // Leaving visual/insert clears any selection (vim's Escape behaviour).
        self.active.mark_active = false;
        // Vim nudges the cursor one column left when leaving Insert (the caret
        // sat *after* the last typed char), but not past the line's start.
        let mut point = self.point();
        if was_insert {
            let line_start = self.active.buffer.line_range_at(point).start;
            if point > line_start {
                point = self.active.buffer.prev_grapheme(point);
            }
        }
        self.collapse_to(point);
        cx.notify();
    }

    pub(super) fn evil_insert_state(&mut self, cx: &mut Context<Self>) {
        self.evil = EvilState::Insert;
        self.vim.insert_acc.clear();
        self.active.mark_active = false;
        cx.notify();
    }

    pub(super) fn evil_visual_state(&mut self, cx: &mut Context<Self>) {
        self.evil = EvilState::Visual;
        // Anchor the selection at point; motions extend it (mark is active).
        self.active.mark_active = true;
        cx.notify();
    }

    /// Vim line-wise Visual (`V`). Like [`Self::evil_visual_state`] but the
    /// selection is always rounded out to whole lines by [`Self::visual_region`],
    /// so `d`/`y` operate line-wise and `j`/`k` extend the selection a line at a
    /// time.
    pub(super) fn evil_visual_line_state(&mut self, cx: &mut Context<Self>) {
        self.evil = EvilState::VisualLine;
        self.active.mark_active = true;
        cx.notify();
    }

    /// Block insert (`I`, `at_eol` false) / append (`A`, `at_eol` true) over a
    /// Visual selection: drop a cursor at the start (or end) of every selected
    /// line and switch to Insert. Typed text, Enter, and Backspace replicate to
    /// every cursor via the multiple-cursors machinery, so one edit lands on all
    /// lines — vim's blockwise `I`/`A`. Escape (→ [`Self::evil_normal_state`])
    /// collapses the cursors back to one.
    pub(super) fn evil_visual_insert(&mut self, at_eol: bool, cx: &mut Context<Self>) {
        let region = self.visual_region();
        let first = self.active.buffer.line_index_at_byte(region.start);
        // `region.end` is the start of the line after the last selected line
        // (line-wise); step back one byte to land on the last selected line.
        let last_byte = region.end.saturating_sub(1).max(region.start);
        let last = self.active.buffer.line_index_at_byte(last_byte);

        // Primary cursor on the first line; a secondary cursor on each of the
        // rest.
        let positions: Vec<usize> = (first..=last)
            .map(|line| {
                let lr = self.active.buffer.line_range(line);
                if at_eol { lr.end } else { lr.start }
            })
            .collect();
        self.place_cursors(positions);
        self.evil_insert_state(cx);
    }

    /// Enter block-wise Visual (`C-v`). The two corners ride on
    /// `selected_range.start`/`.end` like the other visual states, so the usual
    /// motions move the cursor corner; the rectangle is derived on demand by
    /// [`Self::visual_block_rect`] and the renderer highlights only that column
    /// strip (not whole lines).
    pub(super) fn evil_visual_block_state(&mut self, cx: &mut Context<Self>) {
        self.evil = EvilState::VisualBlock;
        self.active.mark_active = true;
        cx.notify();
    }

    /// The block-wise Visual rectangle (see [`Self::visual_block_rect`]).
    fn block_rect(&self) -> Option<BlockRect> {
        let (top, bottom, left, right) = self.visual_block_rect()?;
        Some(BlockRect {
            top,
            bottom,
            left,
            right,
        })
    }

    /// The byte offset of character column `col` on each of the rectangle's
    /// rows, top-down (clamped to each row's end).
    fn block_column(&self, rect: BlockRect, col: usize) -> Vec<usize> {
        rect.rows()
            .map(|row| self.active.buffer.byte_at_char_column(row, col))
            .collect()
    }

    /// The byte span of the rectangle's columns on each row, top-down — empty
    /// on a row shorter than its left column.
    fn block_row_spans(&self, rect: BlockRect) -> Vec<Range<usize>> {
        let buf = &self.active.buffer;
        rect.rows()
            .map(|row| {
                let s = buf.byte_at_char_column(row, rect.left);
                let e = buf.byte_at_char_column(row, rect.right + 1);
                s..e.max(s)
            })
            .collect()
    }

    /// Edit every row's span with `replacement(span)`, bottom-up so earlier
    /// edits don't shift later rows' offsets, as one undo step.
    fn block_edit_rows(
        &mut self,
        rect: BlockRect,
        replacement: impl Fn(&Self, &Range<usize>) -> String,
    ) {
        self.begin_undo_group();
        for span in self.block_row_spans(rect).into_iter().rev() {
            if !span.is_empty() {
                let new = replacement(self, &span);
                self.edit(span, &new);
            }
        }
        self.end_undo_group();
    }

    /// Back to Normal with the cursor at the block's top-left.
    fn block_exit_to_top_left(&mut self, rect: BlockRect, cx: &mut Context<Self>) {
        let cursor = self.active.buffer.byte_at_char_column(rect.top, rect.left);
        self.collapse_to(cursor);
        self.evil_normal_state(cx);
    }

    /// The primary cursor at the first of `positions` and a secondary cursor
    /// at each of the rest. `collapse_to` clears the selection so the insert
    /// path replicates.
    fn place_cursors(&mut self, positions: impl IntoIterator<Item = usize>) {
        let mut positions = positions.into_iter();
        if let Some(first) = positions.next() {
            self.collapse_to(first);
            for p in positions {
                self.mc_add_cursor_at(p);
            }
        }
    }

    /// Block `I` (`append` false) / `A` (`append` true): drop a cursor at the
    /// block's left column (or just past its right column) on every row and
    /// switch to Insert, so typed text replicates down the column via the
    /// multiple-cursors machinery — vim's block-wise insert. Escape collapses the
    /// cursors back to one.
    pub(super) fn evil_block_insert(&mut self, append: bool, cx: &mut Context<Self>) {
        let Some(rect) = self.block_rect() else {
            return;
        };
        let col = if append { rect.right + 1 } else { rect.left };
        let positions = self.block_column(rect, col);
        self.place_cursors(positions);
        self.evil_insert_state(cx);
    }

    /// File a block's text like any yank/delete: the pending register (`"a`),
    /// the kill ring and the system clipboard — or nowhere for `"_`.
    fn save_block_kill(
        &mut self,
        kind: super::registers::KillKind,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let reg = self.take_kill_register();
        self.file_kill(kind, reg, text, false, cx);
    }

    /// Block `d`/`x`: delete the rectangle (columns `[left, right]` on every row),
    /// copy it to the kill ring + system clipboard, and return to Normal with the
    /// cursor at the block's top-left.
    pub(super) fn evil_block_delete(&mut self, cx: &mut Context<Self>) {
        let Some(rect) = self.block_rect() else {
            return;
        };
        let killed = self.block_text(rect);
        self.save_block_kill(super::registers::KillKind::Delete, killed, cx);
        self.block_edit_rows(rect, |_, _| String::new());
        self.block_exit_to_top_left(rect, cx);
    }

    /// Block `c`: delete the rectangle (like [`Self::evil_block_delete`]), then
    /// drop a cursor at the block's left column on every row and switch to
    /// Insert, so the replacement text lands on all rows at once — vim's
    /// block-wise change. Escape collapses the cursors back to one.
    pub(super) fn evil_block_change(&mut self, cx: &mut Context<Self>) {
        let Some(rect) = self.block_rect() else {
            return;
        };
        let killed = self.block_text(rect);
        self.save_block_kill(super::registers::KillKind::Delete, killed, cx);
        self.block_edit_rows(rect, |_, _| String::new());
        // Cursors are computed AFTER the deletes, so each row's left column is
        // its post-delete position.
        let positions = self.block_column(rect, rect.left);
        self.place_cursors(positions);
        self.evil_insert_state(cx);
    }

    /// Block `y`: copy the rectangle to the kill ring + system clipboard without
    /// removing it; the cursor returns to the block's top-left.
    pub(super) fn evil_block_yank(&mut self, cx: &mut Context<Self>) {
        let Some(rect) = self.block_rect() else {
            return;
        };
        let text = self.block_text(rect);
        self.save_block_kill(super::registers::KillKind::Yank, text, cx);
        self.block_exit_to_top_left(rect, cx);
    }

    /// Block `r<char>`: overwrite every char inside the rectangle (only the
    /// `[left, right]` columns of each row, rows shorter than `left` untouched),
    /// then return to Normal at the block's top-left.
    pub(super) fn evil_block_replace(&mut self, ch: &str, cx: &mut Context<Self>) {
        let Some(rect) = self.block_rect() else {
            return;
        };
        self.block_edit_rows(rect, |ed, span| {
            let buf = &ed.active.buffer;
            ch.repeat(buf.byte_to_char(span.end) - buf.byte_to_char(span.start))
        });
        self.block_exit_to_top_left(rect, cx);
    }

    /// Helix `p` (after) / `P` (before): paste the clipboard/kill-ring text at
    /// the end (start) of EVERY selection — a bare cursor counts as the char
    /// under it — and select what was pasted. Text ending in a newline pastes
    /// line-wise: on the line below (above) the selection.
    pub(super) fn helix_paste(&mut self, after: bool, cx: &mut Context<Self>) {
        self.sync_clipboard_into_kill_ring(cx);
        let Some(text) = self.kill_ring.last().cloned() else {
            self.echo("Kill ring is empty", cx);
            return;
        };
        let linewise = text.ends_with('\n');
        let buf = &self.active.buffer;
        let mut inserts: Vec<usize> = self
            .all_selections()
            .into_iter()
            .map(|(a, h)| {
                let (lo, hi) = if a == h {
                    let eol = buf.line_range_at(h).end;
                    (h, buf.next_grapheme(h).min(eol.max(h)))
                } else {
                    (a.min(h), a.max(h))
                };
                if linewise {
                    if after {
                        // Start of the line after the selection's last line
                        // (`hi` is exclusive, so a whole-line `x` selection
                        // already ends on that line start).
                        let last = if hi > lo && hi > 0 { hi - 1 } else { hi };
                        let lr = buf.line_range_at(last);
                        if lr.end < buf.len() {
                            // Past the whole line break (`\r\n` is one grapheme).
                            buf.next_grapheme(lr.end)
                        } else {
                            lr.end
                        }
                    } else {
                        buf.line_range_at(lo).start
                    }
                } else if after {
                    hi
                } else {
                    lo
                }
            })
            .collect();
        inserts.sort_unstable();
        inserts.dedup();
        let len = self.active.buffer.len();
        let ends_in_newline = len > 0 && self.active.buffer.slice_to_string(len - 1..len) == "\n";
        // Pasting line-wise after the last line of a file with no trailing
        // newline: add the break before the text instead.
        let piece = |at: usize| -> String {
            if linewise && after && at == len && len > 0 && !ends_in_newline {
                format!("\n{}", text.strip_suffix('\n').unwrap_or(&text))
            } else {
                text.clone()
            }
        };
        let pieces: Vec<(usize, String)> = inserts.iter().map(|&at| (at, piece(at))).collect();
        self.begin_undo_group();
        for (at, t) in pieces.iter().rev() {
            self.edit(*at..*at, t);
        }
        self.end_undo_group();
        // Select each pasted piece, shifting by the text inserted before it.
        let mut shift = 0;
        let mut sels = Vec::new();
        for (at, t) in &pieces {
            let s = at + shift;
            let e = s + t.len();
            let s = if t.starts_with('\n') && linewise {
                s + 1
            } else {
                s
            };
            sels.push((s, e));
            shift += t.len();
        }
        self.set_selections(sels, cx);
    }

    /// The text inside a block rectangle: columns `[left, right]` of rows
    /// `[top, bottom]`, one buffer line per row joined by newlines (rows shorter
    /// than `left` contribute an empty string).
    fn block_text(&self, rect: BlockRect) -> String {
        self.block_row_spans(rect)
            .into_iter()
            .map(|span| self.active.buffer.slice_to_string(span))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn evil_disable(&mut self, cx: &mut Context<Self>) {
        // Same contract as `evil_enable`: drop the Helix flag too, or
        // `(helix-enabled?)` keeps reporting Helix with no modal mode on at all
        // (which is how `C-h t` ended up opening the Helix tutorial in Emacs
        // mode).
        self.leave_helix(cx);
        self.evil = EvilState::Disabled;
        cx.notify();
    }

    // --- evil paste (`p` / `P`) ------------------------------------------

    /// Vim `p` (after) / `P` (before). Pastes the most recent kill, choosing
    /// line-wise or character-wise placement from the flag set by the last
    /// yank/delete (`evil_yank_linewise`).
    pub(super) fn evil_paste(&mut self, after: bool, cx: &mut Context<Self>) {
        // `"ap` / `"+p` / `"0p`: paste a register instead of the kill ring.
        // Peeked, not taken, so `"a3p` pastes it three times; the register
        // prefix expires at the next command boundary.
        let reg = self
            .vim_active()
            .then_some(self.vim.pending_register)
            .flatten();
        if let Some(r) = reg {
            if r == '_' {
                return;
            }
            match self.evil_register_value(r, cx) {
                Some(v) if v.linewise => self.evil_paste_linewise(&v.text, after, cx),
                Some(v) => self.evil_paste_charwise(&v.text, after, cx),
                None => self.echo(format!("Register {r} is empty"), cx),
            }
            return;
        }
        // Reconcile with the system clipboard before pasting, so `p` pastes what
        // was last copied anywhere. External text carries no line-wise marker, so
        // paste it char-wise.
        if self.sync_clipboard_into_kill_ring(cx) {
            self.evil_yank_linewise = false;
        }
        let Some(text) = self.kill_ring.last().cloned() else {
            self.echo("Kill ring is empty", cx);
            return;
        };
        if self.evil_yank_linewise {
            self.evil_paste_linewise(&text, after, cx);
        } else {
            self.evil_paste_charwise(&text, after, cx);
        }
    }

    /// Character-wise paste: insert after (vim `p`) or before (`P`) the grapheme
    /// under the cursor, leaving the cursor on the last grapheme pasted.
    fn evil_paste_charwise(&mut self, text: &str, after: bool, cx: &mut Context<Self>) {
        let p = self.point();
        // After the char under the cursor — but never past the line end: on an
        // empty line `next_grapheme` would step over the break and paste onto
        // the NEXT line.
        let at = if after {
            let eol = self.active.buffer.line_range_at(p).end;
            self.active.buffer.next_grapheme(p).min(eol)
        } else {
            p
        };
        let end = self.edit(at..at, text);
        // Vim leaves the cursor on the last character of the inserted text.
        let cursor = if end > at {
            self.active.buffer.prev_grapheme(end)
        } else {
            end
        };
        self.collapse_to(cursor);
        cx.notify();
    }

    /// Line-wise paste: drop the killed line(s) on a fresh line below (vim `p`)
    /// or above (`P`) the current one, cursor at the first non-blank.
    fn evil_paste_linewise(&mut self, text: &str, after: bool, cx: &mut Context<Self>) {
        // A line yanked from the last line of a file with no trailing newline
        // has none; supply it so the paste still lands on a line of its own.
        let owned;
        let text = if text.ends_with('\n') {
            text
        } else {
            owned = format!("{text}\n");
            &owned
        };
        let line = self.active.buffer.line_range_at(self.point());
        let line_start = if after {
            if line.end < self.active.buffer.len() {
                // Insert at the start of the following line; `text` ends in '\n'.
                let at = self.active.buffer.next_grapheme(line.end);
                self.edit(at..at, text);
                at
            } else {
                // Last line (no trailing newline): add the break ourselves and
                // drop the trailing one from `text` so we don't leave a blank.
                let body = text.strip_suffix('\n').unwrap_or(text);
                let at = line.end;
                self.edit(at..at, &format!("\n{body}"));
                self.active.buffer.next_grapheme(at)
            }
        } else {
            let at = line.start;
            self.edit(at..at, text);
            at
        };
        self.collapse_to(self.first_non_blank(line_start));
        cx.notify();
    }

    /// The first non-whitespace byte on the line containing `offset` (or the
    /// line's end if it's all blank) — where vim parks the cursor after a
    /// line-wise paste or `^`.
    pub(super) fn first_non_blank(&self, offset: usize) -> usize {
        let line = self.active.buffer.line_range_at(offset);
        let text = self.active.buffer.slice_to_string(line.clone());
        match text.find(|c: char| !c.is_whitespace()) {
            Some(i) => line.start + i,
            None => line.start,
        }
    }

    // --- evil search (`/` `?` `n` `N`) and ex commands (`:`) --------------

    pub(super) fn evil_search_forward(&mut self, cx: &mut Context<Self>) {
        self.open_minibuffer("/", MinibufferPurpose::Search(Direction::Forward), "", cx);
    }

    pub(super) fn evil_search_backward(&mut self, cx: &mut Context<Self>) {
        self.open_minibuffer("?", MinibufferPurpose::Search(Direction::Backward), "", cx);
    }

    /// `n` — repeat the last search in its original direction (vim semantics:
    /// after `?`, `n` keeps going backward).
    pub(super) fn evil_search_next(&mut self, cx: &mut Context<Self>) {
        self.do_search(self.evil_search_forward, cx);
    }

    /// `N` — repeat the last search in the opposite direction.
    pub(super) fn evil_search_previous(&mut self, cx: &mut Context<Self>) {
        self.do_search(!self.evil_search_forward, cx);
    }

    /// Move point to the next/previous regex match of the stored query, wrapping
    /// around the buffer. The cursor lands on the start of the match.
    pub(super) fn do_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(query) = self.search_query.clone() else {
            return;
        };
        // Vim atoms (`\c`, `\<`) a `/` query may carry; plain regex passes through.
        let regex = match Regex::new(&super::ex::vim_pattern(&query, false)) {
            Ok(r) => r,
            Err(err) => {
                self.echo(format!("Invalid regex: {err}"), cx);
                return;
            }
        };
        let text = self.active.buffer.full_text();
        let point = self.point();
        // Narrowed: only matches wholly inside the accessible range count.
        let acc = self.accessible();
        let starts: Vec<usize> = regex
            .find_iter(&text)
            .filter(|m| acc.start <= m.start() && m.end() <= acc.end)
            .map(|m| m.start())
            .collect();
        if starts.is_empty() {
            self.echo(format!("No match for /{query}/"), cx);
            return;
        }
        let target = if forward {
            starts
                .iter()
                .find(|&&s| s > point)
                .copied()
                .unwrap_or(starts[0]) // wrap to first
        } else {
            starts
                .iter()
                .rev()
                .find(|&&s| s < point)
                .copied()
                .unwrap_or(*starts.last().unwrap()) // wrap to last
        };
        self.move_to(target, cx);
    }

    /// Vim's quit guard for the non-`!` quit commands: when any buffer has
    /// unsaved changes, echo E37 (this buffer) or E162 (another one) and return
    /// `true` — the caller must not quit.
    fn refuse_quit_if_modified(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(name) = self.modified_buffer_names().into_iter().next() else {
            return false;
        };
        if name == self.active.name {
            self.echo("E37: No write since last change (add ! to override)", cx);
        } else {
            self.echo(
                format!("E162: No write since last change for buffer \"{name}\""),
                cx,
            );
        }
        true
    }

    /// Run an ex command line (the text after `:`): `%s/…/…/`, `s/…/…/`, `w`,
    /// `q`, `wq`/`x`, `q!`.
    pub(super) fn run_ex_command(&mut self, input: &str, cx: &mut Context<Self>) {
        let cmd = input.trim();
        if !cmd.is_empty() {
            self.vim.last_ex = cmd.to_string();
            self.vim.registers.set(':', cmd, false);
        }
        // In a grid, `:12` / `:C7` / `:$` move between cells and line commands
        // don't apply; everything else (`:w`, `:q`, `:e`, …) runs as usual.
        if self.grid_ex(cmd, cx) {
            return;
        }
        // Ranges and the line commands (`:%s`, `:'<,'>d`, `:g/…/`, `:12`, …).
        let parsed = parse_ex_line(cmd);
        if self.ex_dispatch(&parsed, cx) {
            return;
        }
        if parsed.range.is_some() {
            self.echo(format!("E481: No range allowed: {cmd}"), cx);
            return;
        }
        // The verb is the first whitespace-separated word (`:e file`,
        // `:b name`, …). It must be exactly the parsed name plus its `!`: one
        // glued to its argument (`:b2`, `:e!x`) is unknown.
        let (verb, arg) = match cmd.split_once(char::is_whitespace) {
            Some((v, a)) => (v, a.trim()),
            None => (cmd, ""),
        };
        let bang = parsed.bang;
        let spelled = verb.strip_suffix('!').filter(|_| bang).unwrap_or(verb);
        let kind = ExCmd::lookup(&parsed.name).filter(|_| spelled == parsed.name);
        match (kind, bang) {
            // Write / quit.
            (Some(ExCmd::Write), false) if !arg.is_empty() => self.write_buffer_to(arg, cx),
            (Some(ExCmd::Write), false) => self.save_buffer(cx),
            // `:w!` writes even over a file changed on disk since it was read.
            (Some(ExCmd::Write), true) if arg.is_empty() => self.save_buffer_ex(false, cx),
            // Every quit here ends the APP (vim's would close one window),
            // so the non-`!` forms refuse while ANY buffer is unsaved —
            // vim's E37/E162 — and `!` is the explicit discard.
            (Some(ExCmd::Quit), false) => {
                if !self.refuse_quit_if_modified(cx) {
                    self.quit_now(cx);
                }
            }
            (Some(ExCmd::Quit), true) => self.quit_now(cx),
            // Write-then-quit must save *synchronously*: `save_buffer` spawns
            // a background write that `cx.quit()` would cancel mid-flight
            // (see `save_all_modified`'s quit-path contract). A failed write
            // aborts the quit so the error stays visible and no work is lost.
            (Some(ExCmd::WriteQuit), false) => {
                if self.save_active_sync(true, cx) && !self.refuse_quit_if_modified(cx) {
                    self.quit_now(cx);
                }
            }
            // `:wq!` writes this buffer, then quits discarding the others.
            (Some(ExCmd::WriteQuit), true) => {
                if self.save_active_sync(false, cx) {
                    self.quit_now(cx);
                }
            }
            // Still refuses if something couldn't be written (a modified
            // buffer with no file, e.g. an edited *scratch*-like buffer).
            (Some(ExCmd::WriteQuitAll), false) => {
                if self.save_all_modified(true, cx) && !self.refuse_quit_if_modified(cx) {
                    self.quit_now(cx);
                }
            }
            (Some(ExCmd::WriteQuitAll), true) => {
                if self.save_all_modified(false, cx) {
                    self.quit_now(cx);
                }
            }
            // Files.
            (Some(ExCmd::Edit), true) => self.revert_buffer(cx),
            (Some(ExCmd::Edit), false) if !arg.is_empty() => self.find_file(arg, cx),
            (Some(ExCmd::Edit), false) => self.echo("Usage: :e <file>", cx),
            // Clear search highlighting.
            (Some(ExCmd::NoHighlight), false) => {
                self.search_query = None;
                cx.notify();
            }
            // Buffers.
            (Some(ExCmd::Buffer), false) if !arg.is_empty() => self.switch_to_buffer(arg, cx),
            (Some(ExCmd::BufferNext), false) => self.next_buffer(cx),
            (Some(ExCmd::BufferPrev), false) => self.previous_buffer(cx),
            (Some(ExCmd::BufferDelete), false) => self.kill_buffer(cx),
            // Windows (Vim abbreviations): :split / :vsplit and friends.
            (Some(ExCmd::Split), false) => self.split_window(SplitAxis::Vertical, cx),
            (Some(ExCmd::VSplit), false) => self.split_window(SplitAxis::Horizontal, cx),
            (Some(ExCmd::Only), false) => self.delete_other_windows(cx),
            (Some(ExCmd::New), false) => self.new_window_buffer(SplitAxis::Vertical, cx),
            (Some(ExCmd::VNew), false) => self.new_window_buffer(SplitAxis::Horizontal, cx),
            (Some(ExCmd::WinCmd), false) => self.run_wincmd(arg, cx),
            (Some(ExCmd::Close), false) => self.delete_window(cx),
            (Some(ExCmd::Registers), false) => self.show_registers(arg, cx),
            (Some(ExCmd::Marks), false) => self.show_marks(arg, cx),
            (Some(ExCmd::Jumps), false) => self.show_jumps(cx),
            (Some(ExCmd::ClearJumps), false) => {
                self.vim.jumps.clear();
                self.vim.jump_idx = 0;
            }
            (Some(ExCmd::DelMarks), bang) => self.delete_marks(arg, bang, cx),
            _ => self.echo(format!("Unknown ex command: {verb}"), cx),
        }
    }
}
