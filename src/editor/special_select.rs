//! Keyboard selection in special (read-only) buffers under evil or helix.
//! Special buffers bypass the modal layers so their own keymap gets every key,
//! which left no way to select and yank text from the keyboard. `v` / `V`
//! (when the buffer's keymaps leave them unbound) start a charwise / linewise
//! selection; while it is live, the `special-select` keymap supplies the
//! motions, `y` copies it, and `escape` (or `v` / `V` again) ends it. Nothing
//! here edits the buffer. Emacs keys need none of this: `C-SPC`, the motions
//! and `M-w` already work there.

use gpui::{Context, Keystroke};

use super::input::Handled;
use super::*;

/// Scheme keymap of the motions offered while selecting.
pub(super) const SPECIAL_SELECT_KEYMAP: &str = "special-select";

impl Editor {
    /// Whether a keyboard selection is live in the active buffer.
    pub(super) fn special_selecting(&self) -> bool {
        self.special_select.as_deref() == Some(self.active.name.as_str())
    }

    /// The key layer: start, extend, copy or end a selection. Runs before the
    /// special-buffer layer so `y` copies instead of reaching the buffer's own
    /// `y` binding while a selection is live.
    pub(super) fn key_special_select(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.active.special.is_none()
            || !self.evil_intercepts()
            || ks.modifiers.platform
            || ks.modifiers.alt
        {
            return Handled::No;
        }
        let chord = chord_string(ks);
        if !self.special_selecting() {
            if !self.pending_keys.is_empty()
                || ks.modifiers.control
                || !(chord == "v" || chord == "shift-v")
                || self.special_buffer_binds(&chord)
            {
                return Handled::No;
            }
            self.special_select_start(chord == "shift-v", cx);
            cx.stop_propagation();
            return Handled::Yes;
        }
        if self.pending_keys.is_empty() {
            let line = self.evil == EvilState::VisualLine;
            match chord.as_str() {
                "y" => {
                    // As in vim, the cursor goes back to the selection's start.
                    let start = self.visual_region().start;
                    self.clipboard_copy(cx);
                    self.move_to(start, cx);
                    self.special_select_end(cx);
                }
                "escape" | "ctrl-g" => self.special_select_end(cx),
                "v" if !line => self.special_select_end(cx),
                "shift-v" if line => self.special_select_end(cx),
                "v" | "shift-v" => {
                    self.evil = if chord == "v" {
                        EvilState::Visual
                    } else {
                        EvilState::VisualLine
                    };
                    cx.notify();
                }
                _ => return self.special_select_motion(chord, cx),
            }
            cx.stop_propagation();
            return Handled::Yes;
        }
        self.special_select_motion(chord, cx)
    }

    /// A key while selecting: a `special-select` motion (or the next key of
    /// one, like `g g`) runs; anything else goes on to the buffer's keymap.
    fn special_select_motion(&mut self, chord: String, cx: &mut Context<Self>) -> Handled {
        self.pending_keys.push(chord);
        let sequence = self.pending_keys.join(" ");
        if let Some(expr) = self
            .keymap
            .resolve_mode(SPECIAL_SELECT_KEYMAP, &sequence)
            .map(str::to_string)
        {
            self.pending_keys.clear();
            self.status = SharedString::default();
            self.dispatch_recorded(&expr, cx);
        } else if self.keymap.is_prefix_mode(SPECIAL_SELECT_KEYMAP, &sequence) {
            self.status = format!("{}-", pretty_sequence(&sequence)).into();
            cx.notify();
        } else {
            self.pending_keys.pop();
            return Handled::No;
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// Whether the buffer's own keymaps (its modal overlay or its mode stack)
    /// bind `chord`, in which case it keeps its meaning (magit's `v`).
    fn special_buffer_binds(&self, chord: &str) -> bool {
        let overlay = self
            .active
            .modes
            .names()
            .last()
            .map(|m| format!("{}:{m}", self.modal_collection_prefix()));
        overlay
            .as_deref()
            .is_some_and(|o| self.keymap.resolve_mode(o, chord).is_some())
            || self.keymap.resolve(&self.active.modes, chord).is_some()
    }

    fn special_select_start(&mut self, line: bool, cx: &mut Context<Self>) {
        let p = self.point();
        self.active.selected_range = p..p;
        self.active.selection_reversed = false;
        self.active.mark_active = true;
        // Visual renders the selection the vim way (inclusive of the cursor
        // char, whole lines for `V`), also under helix: the state is reset
        // when the selection ends or the buffer changes.
        self.evil = if line {
            EvilState::VisualLine
        } else {
            EvilState::Visual
        };
        self.special_select = Some(self.active.name.clone());
        cx.notify();
    }

    /// End the selection, leaving the cursor where it is.
    pub(super) fn special_select_end(&mut self, cx: &mut Context<Self>) {
        self.special_select_drop();
        self.active.mark_active = false;
        let p = self.point();
        self.active.selected_range = p..p;
        self.active.selection_reversed = false;
        cx.notify();
    }

    /// Forget a selection and its Visual state, leaving the text alone (also
    /// used while switching buffers, when the active document is already the
    /// next one).
    pub(super) fn special_select_drop(&mut self) {
        if self.special_select.take().is_some()
            && matches!(self.evil, EvilState::Visual | EvilState::VisualLine)
        {
            self.evil = EvilState::Normal;
        }
    }
}
