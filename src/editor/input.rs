//! Keyboard, mouse, scroll, and OS action handlers for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use std::time::Duration;

use gpui::{
    Bounds, Context, KeyDownEvent, Keystroke, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, ScrollWheelEvent, SharedString, Window, px,
};

use crate::mode::{LEADER_KEYMAP, LeaderMatch};
use crate::script::EditorCommand;

use super::*;

/// Emacs universal-argument (`C-u` / `M-<digit>`) reading state. While
/// `reading`, digit keys accumulate into `digits` and repeated `C-u` raises
/// `cu` (giving the 4/16/64 default). When a non-argument key follows, the
/// computed count lands in `pending` for the next command to consume as a
/// repeat count. The three reading fields reset together via
/// [`Editor::reset_uarg`].
#[derive(Default)]
pub(super) struct UargState {
    pub(super) reading: bool,
    pub(super) digits: Option<i64>,
    pub(super) cu: u32,
    pub(super) pending: Option<i64>,
}

/// Whether a modal key layer consumed the keystroke. `Yes` stops the router
/// (the layer already called `cx.stop_propagation()` if the event must not
/// reach the IME / OS); `No` hands the key to the next layer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Handled {
    Yes,
    No,
}

impl Editor {
    /// One ASCII digit `0`–`9` if `key` is exactly that, else `None`.
    pub(super) fn ascii_digit(key: &str) -> Option<i64> {
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_digit() => Some((c as u8 - b'0') as i64),
            _ => None,
        }
    }

    /// Handle a keystroke while reading (or about to start) a universal-argument.
    /// Returns `true` if the key was consumed as part of the argument (the caller
    /// stops here); `false` if the key should proceed to normal dispatch — in
    /// which case `self.uarg.pending` has been set so the command repeats.
    pub(super) fn handle_prefix_arg(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> bool {
        let only_ctrl = ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
        let only_alt = ks.modifiers.alt && !ks.modifiers.control && !ks.modifiers.platform;
        let plain = !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
        let digit = Self::ascii_digit(&ks.key);

        // `C-u`: begin reading, or multiply the running default by 4 (4, 16, …).
        if only_ctrl && ks.key == "u" {
            self.uarg.reading = true;
            self.uarg.digits = None;
            self.uarg.cu = self.uarg.cu.saturating_add(1);
            self.echo_prefix_arg(cx);
            return true;
        }

        if self.uarg.reading {
            // A digit (plain, or `M-<digit>`) extends the explicit number.
            if (plain || only_alt)
                && let Some(d) = digit
            {
                let acc = self
                    .uarg
                    .digits
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(d);
                self.uarg.digits = Some(acc);
                self.echo_prefix_arg(cx);
                return true;
            }
            // Any other key ends the argument: stash the count and fall through
            // so this key runs as the command.
            self.uarg.pending = Some(prefix_arg_value(self.uarg.digits, self.uarg.cu));
            self.reset_uarg();
            return false;
        }

        // Not yet reading: `M-<digit>` starts a digit-argument (Emacs).
        if only_alt && let Some(d) = digit {
            self.uarg.reading = true;
            self.uarg.digits = Some(d);
            self.uarg.cu = 0;
            self.echo_prefix_arg(cx);
            return true;
        }

        false
    }

    pub(super) fn reset_uarg(&mut self) {
        self.uarg.reading = false;
        self.uarg.digits = None;
        self.uarg.cu = 0;
    }

    /// Echo the argument being built in the status line, Emacs-style.
    pub(super) fn echo_prefix_arg(&mut self, cx: &mut Context<Self>) {
        let v = prefix_arg_value(self.uarg.digits, self.uarg.cu);
        self.status = format!("C-u {v}").into();
        cx.notify();
    }

    /// Mode-driven dispatch for `Ctrl-`/`Alt-` chords and prefix sequences.
    /// Plain printable keys are left to the IME (self-insert); OS-special keys
    /// (arrows, Backspace, …) are handled by GPUI actions in `main`.
    ///
    /// This is a pure router: each modal layer lives in its own `key_*` method,
    /// tried in precedence order. A layer returns [`Handled::Yes`] when it
    /// consumed the keystroke (having stopped propagation itself if the event
    /// must not reach the IME / OS) and [`Handled::No`] to pass it on. The final
    /// Emacs dispatch (`key_emacs`) is the catch-all.
    pub(super) fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;
        if ks.key.is_empty() {
            return; // modifier-only press
        }

        // Any keystroke is cursor activity: keep the cursor solid and restart the
        // blink cycle (no-op timer when `cursor-blink` is off).
        self.reset_blink(cx);
        self.dispatch_keystroke(ks, cx);
        self.anvil_eldoc_tick();
    }

    /// Route one keystroke through the modal layers (see [`Self::on_key_down`]);
    /// also how `:normal` feeds its keys.
    pub(super) fn dispatch_keystroke(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        // Precedence: startup gate → transient → helm → minibuffer → completion
        // popup → terminal → evil replace-pending → evil Normal/Visual → modal
        // leader → evil Insert → special-buffer chord overlay → special buffer →
        // universal argument → Emacs dispatch.
        let layers = [
            Self::key_config_pending,
            Self::key_sub_confirm,
            Self::key_titlebar_dropdown,
            Self::key_transient,
            Self::key_helm,
            Self::key_minibuffer,
            Self::key_completion_popup,
            Self::key_terminal,
            Self::key_evil_replace,
            Self::key_read_char,
            Self::key_read_key,
            Self::key_emacs_input,
            Self::key_evil_capture,
            Self::key_evil_modal,
            Self::key_grid,
            Self::key_leader,
            Self::key_evil_insert,
            Self::key_special_select,
            Self::key_special_chord_overlay,
            Self::key_special_buffer,
            Self::key_universal_argument,
            Self::key_named_binding,
        ];
        for layer in layers {
            if layer(self, ks, cx) == Handled::Yes {
                return;
            }
        }
        self.key_emacs(ks, cx);
    }

    /// Startup config hasn't been applied yet (the window may have been
    /// revealed by the load watchdog before `init.scm` finished). Swallow
    /// plain keys so they can't self-insert before `(evil-mode)` and the
    /// keymaps take effect; Ctrl/Alt/Cmd chords still pass (C-g, C-x C-c, …).
    fn key_config_pending(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.config_pending
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform
        {
            cx.stop_propagation();
            return Handled::Yes;
        }
        Handled::No
    }

    /// The title-bar perspectives menu is modal while unfolded: Escape / C-g
    /// fold it, and the Emacs chords steer the search field (C-n/C-p move the
    /// selection, C-b/C-f/C-a/C-e the caret, C-d forward-deletes). Plain
    /// printable keys fall through to the IME, which routes them into the
    /// filter (like Helm); any other chord is swallowed so it can't reach the
    /// buffer under the menu. The OS-special keys (Enter, Backspace, arrows,
    /// Home/End, Tab) never reach this layer — they're bound as gpui actions,
    /// which dispatch before raw key-down listeners — so their dropdown
    /// routing lives in the `act_*` handlers below.
    fn key_titlebar_dropdown(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if !self.titlebar_dropdown_open() {
            return Handled::No;
        }
        let ctrl = ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
        if ks.key == "escape" || (ctrl && ks.key == "g") {
            self.titlebar_dropdown_close(cx);
        } else if ctrl && ks.key == "p" {
            self.titlebar_dropdown_move(-1, cx);
        } else if ctrl && ks.key == "n" {
            self.titlebar_dropdown_move(1, cx);
        } else if ctrl && ks.key == "b" {
            self.titlebar_dropdown_cursor_move(-1, cx);
        } else if ctrl && ks.key == "f" {
            self.titlebar_dropdown_cursor_move(1, cx);
        } else if ctrl && ks.key == "a" {
            self.titlebar_dropdown_cursor_set(0, cx);
        } else if ctrl && ks.key == "e" {
            self.titlebar_dropdown_cursor_set(usize::MAX, cx);
        } else if ctrl && ks.key == "d" {
            self.titlebar_dropdown_delete(cx);
        } else if !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform {
            return Handled::Yes; // plain text: let the IME route it to the filter
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// A transient popup is modal: C-g / Escape close it; a row's key runs its
    /// command (the popup closes first); everything else is swallowed.
    fn key_transient(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.transient.is_none() {
            return Handled::No;
        }
        if (ks.modifiers.control && ks.key == "g") || ks.key == "escape" {
            self.transient_quit(cx);
        } else if let Some(command) = self
            .transient_command(&chord_string(ks))
            // Rows are declared with the glyph users see ("F"), but a shifted
            // letter arrives as "shift-f" — try the literal character too.
            .or_else(|| shifted_letter(ks).and_then(|k| self.transient_command(&k)))
        {
            // A sticky popup stays open after a row runs (the command re-opens
            // it to refresh, or calls `transient-quit`); a normal one closes
            // first so its command runs without the menu in the way.
            if !self.transient_is_sticky() {
                self.transient = None;
            }
            cx.notify();
            // Transient rows store a bare command name (e.g. `magit-do-commit`);
            // wrap it in parens so it's *called*, not just evaluated to the
            // closure — the same convention `DefineKey` uses for keymaps.
            self.run_script(&format!("({command})"), cx);
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// Helm is modal: C-g aborts, chords resolve against the Scheme `helm`
    /// keymap, any other Ctrl/Alt chord is swallowed, and plain text falls
    /// through to the IME (→ `helm_input`).
    fn key_helm(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.helm.is_none() {
            return Handled::No;
        }
        if ks.modifiers.control && ks.key == "g" {
            self.helm_abort(cx);
            cx.stop_propagation();
            return Handled::Yes;
        }
        let chord = chord_string(ks);
        if let Some(expr) = self.keymap.resolve_mode("helm", &chord).map(str::to_string) {
            self.run_script(&expr, cx);
            cx.stop_propagation();
            return Handled::Yes;
        }
        if ks.modifiers.control || ks.modifiers.alt {
            cx.stop_propagation();
        }
        Handled::Yes
    }

    /// While the minibuffer is open, C-g aborts it (Emacs); other chords are
    /// swallowed so they can't reach the main buffer. Plain and OS-special keys
    /// fall through to the IME / GPUI actions, which route to the minibuffer.
    /// Under evil ONLY, Escape also aborts, so the `:` ex prompt (and any other
    /// read) is dismissed with a single Esc (vim). In pure Emacs mode Escape is
    /// left alone — there it is the Meta prefix, and C-g is the canceller.
    fn key_minibuffer(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.minibuffer.is_none() {
            return Handled::No;
        }
        let evil_escape = ks.key == "escape" && self.evil != EvilState::Disabled;
        if (ks.modifiers.control && ks.key == "g") || evil_escape {
            self.mb_abort(cx);
            cx.stop_propagation();
        } else if self.is_isearch() && ks.modifiers.control && ks.key == "s" {
            // C-s within isearch: repeat forward (Emacs).
            self.isearch_repeat(Direction::Forward, cx);
            cx.stop_propagation();
        } else if self.is_isearch() && ks.modifiers.control && ks.key == "r" {
            // C-r within isearch: repeat backward.
            self.isearch_repeat(Direction::Backward, cx);
            cx.stop_propagation();
        } else if self.is_isearch() && ks.modifiers.alt && !ks.modifiers.control && ks.key == "r" {
            // M-r within isearch: toggle regexp matching.
            self.isearch_toggle_regexp(cx);
            cx.stop_propagation();
        } else if ks.modifiers.platform
            && ks.key == "v"
            && !ks.modifiers.control
            && !ks.modifiers.alt
        {
            self.mb_paste(false, cx); // cmd-v: the system clipboard
            cx.stop_propagation();
        } else if ks.modifiers.control && ks.key == "y" && !ks.modifiers.alt {
            self.mb_paste(true, cx); // C-y: yank (Emacs minibuffer)
            cx.stop_propagation();
        } else if ks.modifiers.alt && ks.key == "p" {
            self.mb_history(-1, cx); // M-p: older history
            cx.stop_propagation();
        } else if ks.modifiers.alt && ks.key == "n" {
            self.mb_history(1, cx); // M-n: newer history
            cx.stop_propagation();
        } else if ks.modifiers.control || ks.modifiers.alt {
            cx.stop_propagation();
        }
        Handled::Yes
    }

    /// While the completion popup is open, `Ctrl-`/`Alt-` chords resolve
    /// against the Scheme-defined `completion` keymap first (C-n/C-p/C-g, …).
    /// An unbound chord dismisses the popup and is then handled normally; plain
    /// text keys fall through to self-insert, which re-filters the popup.
    fn key_completion_popup(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.completion_popup.is_none() || !(ks.modifiers.control || ks.modifiers.alt) {
            return Handled::No;
        }
        let chord = chord_string(ks);
        if let Some(expr) = self
            .keymap
            .resolve_mode("completion", &chord)
            .map(str::to_string)
        {
            self.pending_keys.clear();
            self.run_script(&expr, cx);
            cx.stop_propagation();
            return Handled::Yes;
        }
        self.completion_popup = None; // dismiss; fall through to normal dispatch
        Handled::No
    }

    /// An embedded terminal grabs the keyboard: keys are translated to PTY
    /// bytes rather than editor commands. `C-x` stays the editor prefix (so
    /// window/buffer management still works), as does any in-progress prefix
    /// sequence, any chord listed in the `terminal-passthrough-keys` option, and
    /// a Ctrl/Alt chord that starts a modal leader sequence (the alt leader,
    /// `M-SPC` by default) — everything else (including C-c, arrows, C-r…) goes
    /// to the shell. Once the shell has exited the terminal is dead: it releases
    /// the keyboard so normal editor dispatch (M-x, C-x k, …) resumes in the
    /// leftover buffer.
    fn key_terminal(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let Some(tid) = self.active_running_terminal_id() else {
            return Handled::No;
        };
        let passthrough = cx
            .global::<crate::settings::Settings>()
            .terminal_passthrough_keys()
            .to_string();
        if terminal_key_goes_to_editor(
            ks,
            !self.pending_keys.is_empty(),
            &passthrough,
            &self.keymap,
        ) {
            return Handled::No;
        }
        if let Some(bytes) = keystroke_to_bytes(ks) {
            self.terminal_input(tid, bytes, cx);
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// Vim `r`: while replace-pending is armed (by `evil-replace-char`), the
    /// very next keystroke is consumed as the literal replacement character —
    /// it must NOT reach the evil keymap, or `r` followed by `x`/`d`/… would
    /// run that command instead of overwriting with the letter. A printable key
    /// overwrites the char under the cursor; any other chord (Escape, a
    /// Ctrl/Alt combo, …) cancels with no edit, matching vim.
    fn key_evil_replace(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if !self.evil_replace_pending {
            return Handled::No;
        }
        self.evil_replace_pending = false;
        self.status = SharedString::default();
        let is_text = ks.key_char.is_some()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform;
        if is_text && let Some(ch) = ks.key_char.clone() {
            let n = self.evil_replace_count;
            let expr = format!("(evil-replace-with {})", super::vim::steel_quote(&ch));
            self.record_event(if n > 1 {
                MacroEvent::Counted(expr, n)
            } else {
                MacroEvent::Command(expr)
            });
            self.evil_replace_char(&ch, cx);
        }
        cx.notify();
        cx.stop_propagation();
        Handled::Yes
    }

    /// While a `read-char` capture is armed (vim `f`/`t`/`F`/`T` and friends,
    /// via the Scheme `read-char` primitive), the very next printable keystroke
    /// is passed to the armed callback instead of the keymap — same contract as
    /// `r` above. Any other chord (Escape, a Ctrl/Alt combo, …) cancels the
    /// capture with no call, matching vim.
    fn key_read_char(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let Some(callback) = self.read_char_pending.take() else {
            return Handled::No;
        };
        self.status = SharedString::default();
        let is_text = ks.key_char.is_some()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform;
        if is_text && let Some(ch) = ks.key_char.clone() {
            // A recording macro gets the RESOLVED call — the arming expr was
            // dropped when ReadChar applied (see the ReadChar dispatch), so
            // replay runs `(callback "<char>")` directly instead of re-arming
            // a capture that playback could never satisfy.
            let quoted = super::vim::steel_quote(&ch);
            self.record_event(MacroEvent::Command(format!("({callback} {quoted})")));
            // The engine passes `ch` as the callback's argument verbatim (no
            // source-string interpolation), so quotes/backslashes are safe.
            self.script.dispatch_action(&callback, Some(&ch));
        }
        cx.notify();
        cx.stop_propagation();
        Handled::Yes
    }

    /// While a sticky `read-key-start` capture is armed (query-replace), EVERY
    /// keystroke goes to its Scheme callback: printable keys as the character
    /// itself, anything else as its chord name (`"escape"`, `"ctrl-g"`, …).
    /// The OS-special keys (RET, DEL, TAB, arrows) arrive via the `act_*`
    /// handlers instead — see [`read_key_action`](Self::read_key_action).
    fn key_read_key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let Some(callback) = self.read_key_pending.clone() else {
            return Handled::No;
        };
        let is_text = ks.key_char.is_some()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform;
        let key = match ks.key_char.clone() {
            Some(ch) if is_text => ch,
            _ => chord_string(ks),
        };
        self.deliver_read_key(&callback, &key, cx);
        cx.stop_propagation();
        Handled::Yes
    }

    /// The `act_*` (gpui action key) side of [`key_read_key`](Self::key_read_key):
    /// when a capture is armed and no modal surface (minibuffer, Helm, …) owns
    /// the key, hand `key` to the callback and report it consumed.
    pub(super) fn read_key_action(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if self.minibuffer.is_some()
            || self.helm.is_some()
            || self.transient.is_some()
            || self.titlebar_dropdown_open()
        {
            return false;
        }
        if self.emacs_input_action_key(key, cx) {
            return true; // C-q TAB / C-q RET
        }
        let Some(callback) = self.read_key_pending.clone() else {
            return false;
        };
        self.deliver_read_key(&callback, key, cx);
        true
    }

    /// Pass `key` to the read-key callback against a fresh snapshot. Escape and
    /// C-g also disarm the capture (after delivery), so a Scheme error mid-
    /// dialog can't leave every key swallowed.
    fn deliver_read_key(&mut self, callback: &str, key: &str, cx: &mut Context<Self>) {
        if key == "escape" || key == "ctrl-g" {
            self.read_key_pending = None;
        }
        self.status = SharedString::default();
        self.publish_point();
        self.script.dispatch_action(callback, Some(key));
        cx.notify();
    }

    /// Evil Normal/Visual: the evil keymap is consulted FIRST for every chord,
    /// so it can override ANY Emacs default — plain keys, Ctrl/Alt chords, and
    /// multi-key sequences alike. An evil binding wins; an evil prefix pends
    /// (`g g`, leader `space …`, or a user-defined `C-x …`). Otherwise a plain
    /// key is evil-owned, so an unbound one is a no-op (vim doesn't self-insert
    /// in normal/visual) — UNLESS it continues an Emacs prefix already in
    /// progress (so `C-x b`, `C-x k`, … still work); a Ctrl/Alt chord evil
    /// doesn't bind falls through to the Emacs dispatch, so unshadowed
    /// `C-x C-s`, `C-g`, `M-x`, … keep working. Platform (cmd) chords always
    /// fall through to the OS / Emacs.
    ///
    /// Special read-only buffers (magit, *Help*, …) are skipped: they are
    /// already fully modal via their own keymap, which binds plain keys like
    /// `s` (stage) that evil-normal doesn't know about. Without this guard the
    /// evil intercept would swallow those keys before the special-buffer layer
    /// ever runs. Their keymap takes precedence over evil.
    fn key_evil_modal(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if !self.evil_intercepts() || self.active.special.is_some() || ks.modifiers.platform {
            return Handled::No;
        }
        if self.vim.op.is_some() {
            return self.key_operator_pending(ks, cx);
        }
        let chord = chord_string(ks);
        // Vim/Helix count prefix: at the start of a command, a digit is a
        // repeat count, not a command — `3j`, `2dd`, `5w`. `0` keeps its
        // binding (start of line) unless a count is already being typed, which
        // is exactly vim's rule. The count is consumed by the next binding that
        // resolves; Escape and an unbound key clear it (see below).
        if self.pending_keys.is_empty()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && let Some(d) = Self::ascii_digit(&chord)
            && (d != 0 || self.modal_count.is_some())
        {
            let n = self.modal_count.unwrap_or(0).saturating_mul(10) + d as usize;
            // Cap it: a pasted digit run shouldn't turn into a multi-million
            // iteration loop that wedges the UI.
            self.modal_count = Some(n.min(10_000));
            self.status = format!("{n}").into();
            cx.notify();
            cx.stop_propagation();
            return Handled::Yes;
        }
        let candidate = if self.pending_keys.is_empty() {
            chord.clone()
        } else {
            format!("{} {}", self.pending_keys.join(" "), chord)
        };
        // The active major mode (top of the stack) scopes bindings, so an
        // `evil-normal:rust` keymap can shadow the global `evil-normal`.
        let state = self.evil_keymap();
        let major: Option<String> = self.active.modes.names().last().map(|s| s.to_string());
        if let Some(expr) = self
            .keymap
            .resolve_evil(state, major.as_deref(), &candidate)
            .map(str::to_string)
        {
            self.pending_keys.clear();
            self.status = SharedString::default();
            // Consume any count prefix (`3dd`, `5G`); see `dispatch_counted`.
            let count = self.modal_count.take();
            // A new Normal-state command: the previous one is done (and, if
            // it changed the buffer, becomes what `.` repeats).
            if self.evil == EvilState::Normal {
                self.dot_boundary(&expr);
                self.register_boundary(&expr);
            }
            // Visual keys keep `'<` / `'>` (and `gv`) current before they act.
            if matches!(
                self.evil,
                EvilState::Visual | EvilState::VisualLine | EvilState::VisualBlock
            ) {
                self.note_visual_exit();
            }
            // Jump commands (G, %, n, …) remember where they started.
            if self.evil_intercepts() && self.is_jump_expr(&expr) {
                self.record_jump();
            }
            // An operator (`d`) or a `%c` binding (`m`, `"`) waits for more
            // input instead of running now.
            if !self.evil_route_binding(&expr, &candidate, count, cx) {
                self.dispatch_counted(&expr, count, cx);
            }
            cx.stop_propagation();
            return Handled::Yes;
        }
        if self
            .keymap
            .is_prefix_evil(state, major.as_deref(), &candidate)
        {
            self.pending_keys.push(chord);
            // Keep the count visible while the chord completes (`3d-` → `3dd`).
            let count = self.modal_count.map(|n| n.to_string()).unwrap_or_default();
            self.status = format!("{count}{}-", pretty_sequence(&candidate)).into();
            cx.notify();
            cx.stop_propagation();
            return Handled::Yes;
        }
        // No evil binding/prefix for this sequence. A plain (non-Ctrl/Alt) key
        // is evil-owned: swallow it as a no-op — unless it's completing an
        // Emacs prefix already pending (then let the Emacs dispatch finish it).
        // Ctrl/Alt chords always fall through so unshadowed Emacs keys work.
        if !ks.modifiers.control && !ks.modifiers.alt {
            let continues_emacs = !self.pending_keys.is_empty()
                && (self
                    .keymap
                    .resolve(&self.active.modes, &candidate)
                    .is_some()
                    || self.keymap.is_prefix(&self.active.modes, &candidate));
            if !continues_emacs {
                self.pending_keys.clear();
                // An unbound key aborts the whole command, count included (vim
                // beeps and forgets); leaving it set would silently multiply the
                // NEXT command the user types.
                self.modal_count = None;
                self.status = SharedString::default();
                cx.notify();
                cx.stop_propagation();
                return Handled::Yes;
            }
        }
        // Fall through to the Emacs dispatch with `pending_keys` intact.
        Handled::No
    }

    /// Modal leader chords where the `evil-normal` keymap never reaches:
    /// special buffers (magit, dashboards, terminals — skipped by
    /// [`key_evil_modal`](Self::key_evil_modal) so their own keymaps win) and
    /// Insert state. Resolves against the `<style>-normal:<major>` overlay
    /// first (so per-mode leader bindings shadow global ones), then the
    /// [`LEADER_KEYMAP`](crate::mode::LEADER_KEYMAP) that `evil-leader-set`
    /// fills. A plain key never STARTS a sequence in Insert state or a live
    /// terminal — it types — so there only the Ctrl/Alt alt-leader (`M-SPC`)
    /// opens one. Anything neither map knows falls through untouched, except
    /// the dead end of a leader sequence, which is swallowed (not typed).
    fn key_leader(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let special = self.active.special.is_some();
        let insert = self.evil == EvilState::Insert;
        if self.evil == EvilState::Disabled || ks.modifiers.platform || !(special || insert) {
            return Handled::No;
        }
        let chord = chord_string(ks);
        let plain = !ks.modifiers.control && !ks.modifiers.alt;
        let typing = insert || self.active_running_terminal_id().is_some() || self.grid_editing();
        if self.pending_keys.is_empty() && plain && typing {
            return Handled::No;
        }
        let pending = self.pending_keys.join(" ");
        let sequence = if pending.is_empty() {
            chord.clone()
        } else {
            format!("{pending} {chord}")
        };
        let overlay: Option<String> = special
            .then(|| {
                self.active
                    .modes
                    .names()
                    .last()
                    .map(|m| format!("{}:{m}", self.modal_collection_prefix()))
            })
            .flatten();
        match self.keymap.resolve_leader(overlay.as_deref(), &sequence) {
            LeaderMatch::Bound(expr) => {
                let expr = expr.to_string();
                self.pending_keys.clear();
                self.status = SharedString::default();
                self.dispatch_recorded(&expr, cx);
            }
            LeaderMatch::Prefix => {
                self.pending_keys.push(chord);
                self.status = format!("{}-", pretty_sequence(&sequence)).into();
                cx.notify();
            }
            LeaderMatch::Miss => {
                // Only claim the dead end of a sequence the leader map started;
                // anything else (an Emacs `C-x …` in progress, a plain key the
                // buffer's own keymap binds) belongs to the later layers.
                if pending.is_empty() || !self.keymap.is_prefix_mode(LEADER_KEYMAP, &pending) {
                    return Handled::No;
                }
                self.pending_keys.clear();
                self.status = format!("{} is undefined", pretty_sequence(&sequence)).into();
                cx.notify();
            }
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// Evil Insert: a chord bound in `evil-insert` (e.g. Escape → normal) wins;
    /// otherwise fall through (plain keys self-insert, C-x etc. still work).
    fn key_evil_insert(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.evil == EvilState::Insert
            && self.pending_keys.is_empty()
            && let Some(expr) = self
                .keymap
                .resolve_mode(self.evil_insert_keymap(), &chord_string(ks))
                .map(str::to_string)
        {
            // `C-r a` (insert register a): wait for the register key.
            if !self.evil_route_binding(&expr, &chord_string(ks), None, cx) {
                self.dispatch_recorded(&expr, cx);
            }
            cx.stop_propagation();
            return Handled::Yes;
        }
        Handled::No
    }

    /// Under evil, a special buffer may bind a Ctrl/Alt chord in its
    /// `evil-normal:<major>` overlay (e.g. `C-d`/`C-u` half-page scroll in a
    /// *compilation* window). Consult that overlay FIRST for a lone chord; an
    /// unbound one falls through to the normal dispatch, so C-x C-c, C-g, and
    /// any ctrl-bound mode keys still work. Additive: nothing binds ctrl chords
    /// here unless the evil layer opted in.
    fn key_special_chord_overlay(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.active.special.is_some()
            && self.evil_intercepts()
            && (ks.modifiers.control || ks.modifiers.alt)
            && !ks.modifiers.platform
            && self.pending_keys.is_empty()
            && let Some(km) = self
                .active
                .modes
                .names()
                .last()
                .map(|m| format!("{}:{m}", self.modal_collection_prefix()))
            && let Some(expr) = self
                .keymap
                .resolve_mode(&km, &chord_string(ks))
                .map(str::to_string)
        {
            self.dispatch_recorded(&expr, cx);
            cx.stop_propagation();
            return Handled::Yes;
        }
        Handled::No
    }

    /// Special read-only buffers (magit-style): every plain key is a command
    /// resolved against the buffer's keymap — never self-insert. Ctrl/Alt
    /// chords fall through to the normal dispatch (so C-x C-c, C-g, and any
    /// ctrl-bound mode keys still work). Mirrors Evil Normal.
    fn key_special_buffer(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.active.special.is_none()
            || ks.modifiers.control
            || ks.modifiers.alt
            || ks.modifiers.platform
        {
            return Handled::No;
        }
        self.pending_keys.push(chord_string(ks));
        let sequence = self.pending_keys.join(" ");
        // Emacs keybindings are the default: plain (non-modal) editing
        // resolves straight against the buffer's own keymap, untouched. Under
        // evil-mode or helix-mode ONLY, a `<style>-normal:<major>` keymap is
        // consulted FIRST (see `modal_collection_prefix`), so the Scheme modal
        // layer (`evil-collection-init` / `helix-collection-init`) can overlay
        // vim/helix keys on these read-only buffers — magit, dired, … —
        // without disturbing their Emacs-default keymaps. The overlay is pure
        // policy in Scheme; this is only the mechanism. Inert when modal off.
        let scoped: Option<String> = self
            .evil_intercepts()
            .then(|| {
                self.active
                    .modes
                    .names()
                    .last()
                    .map(|m| format!("{}:{m}", self.modal_collection_prefix()))
            })
            .flatten();
        if let Some(expr) = scoped
            .as_deref()
            .and_then(|m| self.keymap.resolve_mode(m, &sequence))
            .map(str::to_string)
        {
            self.pending_keys.clear();
            self.status = SharedString::default();
            self.dispatch_recorded(&expr, cx);
        } else if scoped
            .as_deref()
            .is_some_and(|m| self.keymap.is_prefix_mode(m, &sequence))
        {
            // The evil overlay has a LONGER binding for this sequence (e.g.
            // `g g` / `g r` in magit). Pend for the next key even though the
            // base keymap may bind this exact key on its own (single `g` =
            // refresh). This is how evil-collection turns `g` into a prefix
            // in magit without disturbing the Emacs-default keymap beneath —
            // checked before the base exact-match below so the prefix wins.
            self.status = format!("{}-", pretty_sequence(&sequence)).into();
            cx.notify();
        } else if let Some(expr) = self
            .keymap
            .resolve(&self.active.modes, &sequence)
            .map(str::to_string)
        {
            self.pending_keys.clear();
            self.status = SharedString::default();
            self.dispatch_recorded(&expr, cx);
        } else if self.keymap.is_prefix(&self.active.modes, &sequence) {
            self.status = format!("{}-", pretty_sequence(&sequence)).into();
            cx.notify();
        } else {
            self.pending_keys.clear();
            cx.notify();
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// Emacs universal-argument: `C-u` and `M-<digit>` begin a numeric prefix
    /// that the next command consumes as a repeat count. While the argument
    /// is being read, digits (and further `C-u`) are swallowed here; any
    /// other key ends the argument and falls through to run as the command.
    fn key_universal_argument(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.handle_prefix_arg(ks, cx) {
            cx.stop_propagation();
            return Handled::Yes;
        }
        Handled::No
    }

    /// The final catch-all: platform (cmd) chords, then ordinary Emacs prefix
    /// sequences resolved against the mode keymaps. Plain printable keys are
    /// left to the IME (self-insert) and OS-special keys to the GPUI actions.
    fn key_emacs(&mut self, ks: &Keystroke, cx: &mut Context<Self>) {
        let is_text = ks.key_char.is_some()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform;

        // Platform (cmd) chords: dispatch when bound (e.g. cmd-c/x/v), otherwise
        // fall through to the OS WITHOUT consuming the event, so system shortcuts
        // (cmd-q, cmd-w, cmd-m, cmd-h, …) still work. No multi-key cmd sequences.
        if self.pending_keys.is_empty() && ks.modifiers.platform {
            let chord = chord_string(ks);
            if let Some(expr) = self
                .keymap
                .resolve(&self.active.modes, &chord)
                .map(str::to_string)
            {
                self.dispatch_recorded(&expr, cx);
                cx.stop_propagation();
            }
            return;
        }

        if self.pending_keys.is_empty() {
            if is_text {
                return; // self-insert via the IME
            }
            if !(ks.modifiers.control || ks.modifiers.alt) {
                return; // OS-special key; handled as a GPUI action
            }
        }

        self.pending_keys.push(chord_string(ks));
        let sequence = self.pending_keys.join(" ");

        if let Some(expr) = self
            .keymap
            .resolve(&self.active.modes, &sequence)
            .map(str::to_string)
        {
            self.pending_keys.clear();
            self.status = SharedString::default(); // clear any prefix echo
            self.dispatch_recorded(&expr, cx);
        } else if self.keymap.is_prefix(&self.active.modes, &sequence) {
            self.status = format!("{}-", pretty_sequence(&sequence)).into();
            cx.notify();
        } else {
            self.status = format!("{} is undefined", pretty_sequence(&sequence)).into();
            self.pending_keys.clear();
            cx.notify();
        }
        cx.stop_propagation();
    }

    // --- Mouse ------------------------------------------------------------

    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A click inside the completion popup picks that candidate (rather than
        // dismissing the popup and moving the cursor).
        if let Some(hit) = self.completion_popup_hit
            && hit.bounds.contains(&event.position)
        {
            let row = ((event.position.y - hit.bounds.origin.y) / hit.row_height) as usize;
            if row < hit.rows {
                self.completion_click(hit.start + row, cx);
                return;
            }
        }
        self.completion_popup = None;
        self.completion_popup_hit = None;
        // A click on a gutter fold chevron toggles that line's fold. Policy lives
        // in Scheme: we just route the line to `fold-toggle-at-line`. Checked
        // before cursor positioning so the click doesn't also move point.
        if let Some(line) = self
            .fold_hitboxes
            .iter()
            .find(|h| h.bounds.contains(&event.position))
            .map(|h| h.line)
        {
            self.run_script(&format!("(fold-toggle-at-line {line})"), cx);
            return;
        }
        // A click in a grid pane: cell / header / border / formula bar.
        if self.grid_mouse_down(
            event.position,
            event.button,
            event.click_count,
            event.modifiers.shift,
            cx,
        ) {
            return;
        }
        // A click in a terminal pane starts a grid text selection (and focuses it).
        if self.terminal_selection_begin(event.position, cx) {
            return;
        }
        // A click in another pane focuses it (Emacs: mouse selects the window);
        // for a normal buffer the cursor is positioned by a *subsequent* click.
        // Special buffers (dired-sidebar/dired/…) act on the first click, so fall
        // through to position the cursor and run their `mouse-1` binding below.
        // A plain click drops any Visual selection back to Normal, as in Vim.
        // Done before the pane-switch return below: the evil state is shared
        // across panes, so leaving it Visual would make the next `d` act on
        // whatever the newly focused pane has selected.
        if !event.modifiers.shift
            && matches!(
                self.evil,
                EvilState::Visual | EvilState::VisualLine | EvilState::VisualBlock
            )
        {
            self.evil = EvilState::Normal;
        }
        if let Some(pane) = self.pane_at_position(event.position)
            && pane != self.selected_pane
        {
            self.select_pane(pane, cx);
            if self.active.special.is_none() {
                return;
            }
        }
        self.active.mark_active = false;
        self.special_click = None;
        self.special_select_drop();
        // In a special (read-only) buffer, a plain left press positions the
        // cursor on the clicked line and starts a drag selection, as in Emacs.
        // When the mode binds `mouse-1` (dired-sidebar-visit, etc.), the
        // binding runs on the release if the pointer stayed put (a click); a
        // press that moves is a drag and selects text instead (see
        // `SpecialClick`). The line is mapped from pane geometry rather than
        // `self.lines`, so a click that *also* focuses the pane (cursor was in
        // another buffer) still lands on the right line this frame instead of
        // needing a second click.
        if self.active.special.is_some() && !event.modifiers.shift {
            // Normally map the click through the painted line layout (handles
            // wrapped lines exactly). On the frame a click *also* focuses this
            // pane, `self.lines` was just cleared and isn't repainted yet, so fall
            // back to mapping straight from pane geometry.
            let offset = if self.lines.is_empty() {
                // `self.line_height` is the real height from the last paint;
                // `window.line_height()` in an event handler is the unstyled
                // default and would scale every row off (clicks land one line up).
                self.line_start_for_click_y(event.position.y, self.line_height)
            } else {
                Some(self.offset_for_position(event.position))
            };
            if let Some(offset) = offset {
                self.move_to(offset, cx);
            }
            match self
                .keymap
                .resolve(&self.active.modes, "mouse-1")
                .map(str::to_string)
            {
                // The binding waits for the release; until then the press may
                // still turn into a drag.
                Some(expr) => {
                    self.special_click = Some(SpecialClick {
                        origin: event.position,
                        expr,
                    });
                }
                None => {
                    if let Some(offset) = offset
                        && event.click_count >= 2
                    {
                        self.select_by_click(offset, event.click_count >= 3, cx);
                        return;
                    }
                }
            }
            self.is_selecting = true;
            return;
        }
        let offset = self.offset_for_position(event.position);
        if !event.modifiers.shift && event.click_count >= 2 {
            // Double-click selects the word, triple-click the line. No drag
            // after these: pointer jitter would otherwise shrink the selection.
            self.select_by_click(offset, event.click_count >= 3, cx);
            return;
        }
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(offset, cx);
            self.evil_sync_mouse_selection();
        } else {
            // Helix: a click replaces ALL selections with one cursor.
            if self.modal_helix && self.has_extra_cursors() {
                self.mc_clear(cx);
            }
            self.move_to(offset, cx);
            self.evil_clamp_cursor();
        }
    }

    /// Double-click (`line` false) / triple-click (`line` true) selection. In
    /// evil Normal this enters Visual / Visual Line like Vim; elsewhere it's a
    /// plain (exclusive) selection of the word or the whole line + newline.
    fn select_by_click(&mut self, offset: usize, line: bool, cx: &mut Context<Self>) {
        // Not in a special buffer: see `evil_sync_mouse_selection`.
        let evil_visual = !self.modal_helix
            && matches!(self.evil, EvilState::Normal)
            && self.active.special.is_none();
        if self.modal_helix && self.has_extra_cursors() {
            self.mc_clear(cx);
        }
        let buf = &self.active.buffer;
        let range = if line {
            let lr = buf.line_range_at(offset);
            if evil_visual {
                // Visual Line rounds out to whole lines itself.
                lr.start..lr.start
            } else if lr.end < buf.len() {
                lr.start..buf.next_grapheme(lr.end)
            } else {
                lr
            }
        } else {
            let wr = buf.word_range_at(offset);
            if wr.is_empty() {
                // Past the end of the line: nothing to select, just a click.
                self.move_to(offset, cx);
                return;
            }
            if evil_visual {
                // Visual is inclusive of the char under the cursor: park the
                // cursor ON the word's last char, not past it.
                wr.start..buf.prev_grapheme(wr.end).max(wr.start)
            } else {
                wr
            }
        };
        self.move_to(range.start, cx);
        self.select_to(range.end, cx);
        if evil_visual {
            self.evil = if line {
                EvilState::VisualLine
            } else {
                EvilState::Visual
            };
        }
        // Visual needs the mark active; so do Helix verbs (`all_selections`).
        if evil_visual || self.modal_helix || self.active.special.is_some() {
            self.active.mark_active = true;
        }
        cx.notify();
    }

    /// Vim (`mouse=a`): a mouse-made selection in Normal state enters charwise
    /// Visual, so `d`/`y`/`c`/… act on the highlighted text instead of starting
    /// a Normal-state operator (`dd` would otherwise delete the cursor's line).
    /// Helix keeps selections in Normal, so it's left alone there.
    fn evil_sync_mouse_selection(&mut self) {
        if !matches!(self.evil, EvilState::Normal) || self.active.selected_range.is_empty() {
            return;
        }
        // Special buffers bypass the modal layers (their own keymap gets every
        // key), so Visual would do nothing there and, being shared across
        // panes, would follow the user into the next buffer. Keep a plain
        // region the copy commands can read.
        if self.active.special.is_some() {
            self.active.mark_active = true;
            return;
        }
        // Helix has no Visual state, but its verbs only see a selection whose
        // mark is active (`all_selections`), so a drag must activate it.
        if !self.modal_helix {
            self.evil = EvilState::Visual;
            self.dot_unrepeatable();
        }
        self.active.mark_active = true;
    }

    pub(super) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Dragging a split divider: set the split's ratio from the pointer's
        // position within the split's bounds (union of the panes under it).
        if let Some(drag) = self.resizing.clone() {
            let leaves = self
                .layout
                .node_at(&drag.path)
                .map(WindowLayout::leaves)
                .unwrap_or_default();
            let rects: Vec<Bounds<Pixels>> = leaves
                .iter()
                .filter_map(|id| {
                    self.pane_bounds
                        .iter()
                        .find(|(p, _)| p == id)
                        .map(|(_, b)| *b)
                })
                .collect();
            if let Some(area) = union_bounds(&rects) {
                let pos = event.position;
                let ratio = if drag.axis == SplitAxis::Vertical {
                    (f32::from(pos.y) - f32::from(area.top()))
                        / f32::from(area.size.height).max(1.0)
                } else {
                    (f32::from(pos.x) - f32::from(area.left()))
                        / f32::from(area.size.width).max(1.0)
                };
                if self.layout.set_ratio_at(&drag.path, ratio) {
                    cx.notify();
                }
            }
            return;
        }
        if self.terminal_drag.is_some() {
            self.terminal_selection_drag(event.position, cx);
            return;
        }
        if self.grid_mouse_move(event.position, cx) {
            return;
        }
        if self.is_selecting {
            // A special-buffer press stays a click until the pointer leaves
            // the slop; only then does it start selecting.
            if let Some(click) = &self.special_click {
                if !past_click_slop(click.origin, event.position) {
                    return;
                }
                self.special_click = None;
            }
            let offset = self.offset_for_position(event.position);
            self.select_to(offset, cx);
            self.evil_sync_mouse_selection();
            // If the pointer is near/past the top or bottom edge, keep scrolling
            // (and extending the selection) until it comes back inside or the
            // drag ends — driven by a self-rescheduling timer so it continues
            // even while the pointer is held still.
            self.autoscroll.pos = event.position;
            self.autoscroll.dy = self.drag_autoscroll_velocity(event.position);
            if self.autoscroll.dy != 0.0 && !self.autoscroll.active {
                self.autoscroll.active = true;
                self.schedule_drag_autoscroll(cx);
            }
            return;
        }
        // Hover: echo the diagnostic under the pointer (only over the selected
        // pane, so an adjacent pane's text doesn't show the wrong message).
        let over_selected = self
            .pane_at_position(event.position)
            .is_none_or(|p| p == self.selected_pane);
        let message = if over_selected {
            let offset = self.offset_for_position(event.position);
            self.diagnostic_at_offset(offset)
        } else {
            None
        };
        match message {
            Some(m) => {
                // Only re-render when the hovered message actually changes.
                let changed = self
                    .hover_message
                    .as_ref()
                    .is_none_or(|s| s.as_ref() != m.as_str());
                if changed {
                    self.status = m.clone().into();
                    self.hover_message = Some(m.into());
                    cx.notify();
                }
            }
            None => {
                if let Some(prev) = self.hover_message.take() {
                    if self.status == prev {
                        self.status = SharedString::default();
                    }
                    cx.notify();
                }
            }
        }
    }

    pub(super) fn on_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.grid_mouse_up(cx);
        self.is_selecting = false;
        // A special-buffer press that never became a drag is a click: run its
        // `mouse-1` binding now. Selecting has already stopped, so a binding
        // that switches panes cannot drag a selection along with it.
        if let Some(click) = self.special_click.take()
            && !past_click_slop(click.origin, event.position)
        {
            self.dispatch_recorded(&click.expr, cx);
        }
        // The autoscroll timer chain checks `is_selecting` and stops itself.
        self.autoscroll.dy = 0.0;
        self.resizing = None;
        // End any terminal drag; the selection stays so it can be copied.
        self.terminal_drag = None;
    }

    /// Per-tick vertical autoscroll velocity (signed pixels) for a drag-select,
    /// given the pointer position. Zero when the pointer is comfortably inside
    /// the text area; otherwise it ramps up the further past the edge the
    /// pointer is, so flicking well below the view scrolls faster than hugging
    /// the edge. Negative scrolls up, positive scrolls down.
    pub(super) fn drag_autoscroll_velocity(&self, pos: Point<Pixels>) -> f32 {
        let Some(bounds) = self.last_bounds else {
            return 0.0;
        };
        let lh = f32::from(self.line_height).max(1.0);
        // Start scrolling once the pointer is within a line of the edge (so you
        // don't have to push fully past the window), ramping over ~4 lines.
        let margin = lh;
        let ramp = lh * 4.0;
        let min_speed = lh * 0.4;
        let max_speed = lh * 2.5;
        let speed = |over: f32| (min_speed + (over / ramp) * max_speed).min(max_speed);
        let y = f32::from(pos.y);
        let top = f32::from(bounds.top());
        let bottom = f32::from(bounds.bottom());
        if y > bottom - margin {
            speed(y - (bottom - margin))
        } else if y < top + margin {
            -speed((top + margin) - y)
        } else {
            0.0
        }
    }

    /// Schedule the next drag-autoscroll tick. Each tick re-checks that a drag
    /// is still active and the pointer is still off the edge; otherwise it stops
    /// the chain. Mirrors [`schedule_blink`]'s self-rescheduling timer.
    pub(super) fn schedule_drag_autoscroll(&mut self, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(Duration::from_millis(16));
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |editor, cx| {
                if !editor.is_selecting || editor.autoscroll.dy == 0.0 {
                    editor.autoscroll.active = false;
                    return;
                }
                if editor.apply_drag_autoscroll(cx) {
                    editor.schedule_drag_autoscroll(cx);
                } else {
                    // Hit the top/bottom of the buffer — nothing left to reveal.
                    editor.autoscroll.active = false;
                }
            })
            .ok();
        })
        .detach();
    }

    /// Apply one autoscroll step: scroll by the current velocity, then re-extend
    /// the selection to the (now newly revealed) line under the last pointer
    /// position. Returns `false` when the view can't scroll any further in that
    /// direction, so the caller can stop the timer chain.
    pub(super) fn apply_drag_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        let vh = f32::from(self.viewport_height);
        let max = self.max_scroll_offset(vh);
        let cur = f32::from(self.active.scroll_offset);
        let next = (cur + self.autoscroll.dy).clamp(0.0, max);
        if (next - cur).abs() < 0.01 {
            return false;
        }
        self.active.scroll_offset = px(next);
        // The pointer sits past the edge, so this hit-tests to the first/last
        // visible line; after the scroll + repaint that line has moved, so the
        // selection grows by roughly the scrolled distance each tick.
        let offset = self.offset_for_position(self.autoscroll.pos);
        self.select_to(offset, cx);
        cx.notify();
        true
    }

    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let pixel_delta = event.delta.pixel_delta(line_height);
        // Over a grid pane, the wheel scrolls the grid (Cmd/Ctrl zooms).
        if self.grid_scroll(
            event.position,
            f32::from(pixel_delta.x),
            f32::from(pixel_delta.y),
            event.modifiers.platform || event.modifiers.control,
            event.modifiers.shift,
            cx,
        ) {
            return;
        }
        // Over a terminal pane, the wheel scrolls the terminal's scrollback.
        if let Some(pane) = self.pane_at_position(event.position)
            && let Some(tid) = self.pane_terminal_id(pane)
        {
            let lines = (f32::from(pixel_delta.y) / f32::from(line_height)).round() as i32;
            if lines != 0 {
                self.terminal_scroll(tid, lines, cx);
            }
            return;
        }
        // `window.line_height()` above is only for the wheel's line→pixel delta
        // conversion; the scroll CLAMP uses the shared display-row helper (real
        // painted line height, folds collapsed) so the wheel can't over-scroll
        // into blank space below a folded buffer.
        let vh = f32::from(self.viewport_height);
        let max = self.max_scroll_offset(vh);
        // A downward wheel delta is negative, which scrolls content up.
        let scroll =
            (f32::from(self.active.scroll_offset) - f32::from(pixel_delta.y)).clamp(0.0, max);
        self.active.scroll_offset = px(scroll);
        // Horizontal scrolling only when word-wrap is off (otherwise lines never
        // extend past the right edge). The clamp upper bound is generous — the
        // true line width isn't known here — and h-scroll is re-clamped to the
        // widest visible line during paint.
        if !self.word_wrap {
            let h = (f32::from(self.h_scroll_offset) - f32::from(pixel_delta.x)).max(0.0);
            self.h_scroll_offset = px(h);
        }
        cx.notify();
    }

    // --- GPUI action handlers (OS-special keys -> commands) ----------------
    //
    // Backspace/Delete/Enter/Tab/arrows/Home/End are bound as gpui actions,
    // which dispatch before `on_key_down` — so none of them reach
    // `dispatch_keystroke`'s layers. [`Self::route_action_key`] is their
    // equivalent: ONE precedence order shared by every such key (mirroring
    // `dispatch_keystroke`: modal surfaces, then grid, terminal, special
    // buffer, ghost, modal keymap). Each `act_*` handler only describes how its
    // key behaves in each surface ([`ActionKey`]) and supplies the plain-buffer
    // fallback for when no layer claimed it.

    /// Route an OS-special key through the shared layers. True when a layer
    /// handled (or deliberately swallowed) it; false means "run the fallback".
    pub(super) fn route_action_key(&mut self, k: &ActionKey, cx: &mut Context<Self>) -> bool {
        if self.read_key_action(k.name, cx) {
            return true;
        }
        if self.titlebar_dropdown_open() {
            (k.titlebar)(self, cx);
            return true;
        }
        if self.transient.is_some() {
            return true; // transient is modal; unbound keys are ignored
        }
        if self.helm.is_some() {
            // Helm is modal: a key with no Helm meaning must still not edit or
            // move in the buffer underneath.
            if let Some(f) = k.helm {
                f(self, cx);
            }
            return true;
        }
        match &k.popup {
            PopupKey::Ignore => {}
            PopupKey::Dismiss => self.completion_popup = None,
            PopupKey::Run(cmd) => {
                if self.popup_special_key(k.name, cmd(), cx) {
                    return true;
                }
            }
        }
        if self.minibuffer.is_some() {
            (k.minibuffer)(self, cx);
            return true;
        }
        // grid: cell editor or grid keymap
        self.grid_action_key(k.name, cx)
            // terminal: sent to the shell, like every plain key in a terminal
            // (before the modal keymap, matching `dispatch_keystroke`)
            || self.terminal_action_key(k.name, cx)
            // read-only buffer: the key is consumed whether or not it's bound
            || (k.special && self.special_key(k.name, cx))
            // a visible ghost suggestion's `ghost`-keymap binding (after the
            // popup and the modal surfaces, so a suggestion left showing behind
            // an open minibuffer can't steal its key)
            || (k.ghost && self.ghost_special_key(k.name, cx))
            // an evil/helix keymap binding for this key wins over the default
            || self.modal_action_key(k.name, cx)
    }

    pub(super) fn act_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.route_action_key(&KEY_BACKSPACE, cx) {
            return;
        }
        if self.evil_intercepts() {
            self.apply_command(EditorCommand::BackwardChar, cx); // vim: move left
        } else {
            self.record_event(MacroEvent::Command("(delete-backward-char)".into()));
            self.apply_command(EditorCommand::DeleteBackwardChar, cx);
            if self.completion_popup.is_some() {
                self.popup_refilter(cx);
            }
        }
    }
    pub(super) fn act_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.route_action_key(&KEY_DELETE, cx) {
            return;
        }
        // No `evil_intercepts` branch on purpose: in vim, Del in Normal mode
        // deletes the char under the cursor (like `x`), unlike Backspace which
        // only moves left — see act_backspace.
        if !self.evil_intercepts() {
            self.record_event(MacroEvent::Command("(delete-char)".into()));
        }
        self.apply_command(EditorCommand::DeleteChar, cx);
    }
    pub(super) fn act_left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if !self.route_action_key(&KEY_LEFT, cx) {
            self.apply_command(EditorCommand::BackwardChar, cx);
        }
    }
    pub(super) fn act_right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if !self.route_action_key(&KEY_RIGHT, cx) {
            self.apply_command(EditorCommand::ForwardChar, cx);
        }
    }
    pub(super) fn act_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if !self.route_action_key(&KEY_UP, cx) {
            self.apply_command(EditorCommand::PreviousLine, cx);
        }
    }
    pub(super) fn act_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if !self.route_action_key(&KEY_DOWN, cx) {
            self.apply_command(EditorCommand::NextLine, cx);
        }
    }
    pub(super) fn act_home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        if !self.route_action_key(&KEY_HOME, cx) {
            self.apply_command(EditorCommand::BeginningOfLine, cx);
        }
    }
    pub(super) fn act_end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        if !self.route_action_key(&KEY_END, cx) {
            self.apply_command(EditorCommand::EndOfLine, cx);
        }
    }
    pub(super) fn act_newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.route_action_key(&KEY_ENTER, cx) {
            return;
        }
        if !self.evil_intercepts() && !self.mode_key("enter", cx) {
            // a mode may rebind RET (comint send-input); else insert a newline
            self.record_event(MacroEvent::Command("(newline)".into()));
            self.apply_command(EditorCommand::Newline, cx);
        }
    }
    pub(super) fn act_tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        // A modal user's explicit TAB binding is consulted (last) by the
        // router, so it isn't shadowed by a mode map like org-tab.
        if self.route_action_key(&KEY_TAB, cx) {
            return;
        }
        if !self.mode_key("tab", cx) {
            // A mode binding (e.g. org-mode's `org-tab` heading cycle) wins in
            // every state, including evil Normal/Visual — TAB is a gpui action
            // key that never reaches the evil keymap, so consult the mode map
            // here or modal users lose it. The plain indent fallback still only
            // runs when evil isn't intercepting; in evil Normal, TAB is Vim's
            // C-i (jump forward) — the same key on a terminal.
            if !self.evil_intercepts() {
                self.record_event(MacroEvent::Command("(indent-for-tab-command)".into()));
                self.apply_command(EditorCommand::IndentForTab, cx);
            } else if self.evil == EvilState::Normal
                && !self.modal_helix
                && self.active.special.is_none()
            {
                self.evil_jump(true, cx);
            }
        }
    }
    pub(super) fn act_select_left(
        &mut self,
        _: &SelectLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.minibuffer.is_none() && !self.titlebar_dropdown_open() {
            if self.grid_action_key("shift-left", cx) {
                return;
            }
            self.apply_command(EditorCommand::ExtendBackwardChar, cx);
        }
    }
    pub(super) fn act_select_right(
        &mut self,
        _: &SelectRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.minibuffer.is_none() && !self.titlebar_dropdown_open() {
            if self.grid_action_key("shift-right", cx) {
                return;
            }
            self.apply_command(EditorCommand::ExtendForwardChar, cx);
        }
    }
    pub(super) fn act_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.minibuffer.is_none() && !self.titlebar_dropdown_open() {
            self.apply_command(EditorCommand::MarkWholeBuffer, cx);
        }
    }
}

/// How one OS-special (gpui action) key behaves in each modal surface; see
/// [`Editor::route_action_key`].
pub(super) struct ActionKey {
    /// The key's name in keymaps and the PTY encoder ("backspace", "up", …).
    pub(super) name: &'static str,
    /// While the title-bar dropdown is open.
    titlebar: fn(&mut Editor, &mut Context<Editor>),
    /// While Helm is open; `None` swallows the key.
    helm: Option<fn(&mut Editor, &mut Context<Editor>)>,
    /// While the completion popup is open.
    popup: PopupKey,
    /// While the minibuffer is open.
    minibuffer: fn(&mut Editor, &mut Context<Editor>),
    /// Consult (and be consumed by) a special buffer's keymap. Off for motion
    /// keys, which must keep moving point in read-only buffers.
    special: bool,
    /// Consult the `ghost` keymap while a suggestion is visible.
    ghost: bool,
}

/// What an action key does while the completion popup is open.
pub(super) enum PopupKey {
    /// Nothing: fall through to the next layer.
    Ignore,
    /// Close the popup, then fall through.
    Dismiss,
    /// The `completion`-keymap binding, or this default command if unbound.
    Run(fn() -> EditorCommand),
}

pub(super) const KEY_BACKSPACE: ActionKey = ActionKey {
    name: "backspace",
    titlebar: |e, cx| e.titlebar_dropdown_backspace(cx),
    helm: Some(|e, cx| e.helm_backspace(cx)),
    popup: PopupKey::Ignore,
    minibuffer: |e, cx| e.mb_backspace(cx),
    special: true,
    ghost: false,
};
pub(super) const KEY_DELETE: ActionKey = ActionKey {
    name: "delete",
    titlebar: |e, cx| e.titlebar_dropdown_delete(cx),
    helm: None, // Helm's input has no forward-delete
    popup: PopupKey::Ignore,
    minibuffer: |e, cx| e.mb_delete(cx),
    special: true,
    ghost: false,
};
pub(super) const KEY_LEFT: ActionKey = ActionKey {
    name: "left",
    titlebar: |e, cx| e.titlebar_dropdown_cursor_move(-1, cx),
    helm: None,
    popup: PopupKey::Dismiss, // horizontal motion dismisses the popup
    minibuffer: |e, cx| e.mb_left(cx),
    special: false,
    ghost: false,
};
pub(super) const KEY_RIGHT: ActionKey = ActionKey {
    name: "right",
    titlebar: |e, cx| e.titlebar_dropdown_cursor_move(1, cx),
    helm: None,
    popup: PopupKey::Dismiss, // horizontal motion dismisses the popup
    minibuffer: |e, cx| e.mb_right(cx),
    special: false,
    ghost: false,
};
pub(super) const KEY_UP: ActionKey = ActionKey {
    name: "up",
    titlebar: |e, cx| e.titlebar_dropdown_move(-1, cx),
    helm: Some(|e, cx| e.helm_previous(cx)),
    popup: PopupKey::Run(|| EditorCommand::CompletionPrevious),
    minibuffer: |e, cx| e.mb_history(-1, cx), // older
    special: false,
    ghost: false,
};
pub(super) const KEY_DOWN: ActionKey = ActionKey {
    name: "down",
    titlebar: |e, cx| e.titlebar_dropdown_move(1, cx),
    helm: Some(|e, cx| e.helm_next(cx)),
    popup: PopupKey::Run(|| EditorCommand::CompletionNext),
    minibuffer: |e, cx| e.mb_history(1, cx), // newer
    special: false,
    ghost: false,
};
pub(super) const KEY_HOME: ActionKey = ActionKey {
    name: "home",
    titlebar: |e, cx| e.titlebar_dropdown_cursor_set(0, cx),
    helm: None,
    popup: PopupKey::Ignore,
    minibuffer: |e, cx| e.mb_move(0, cx),
    special: false,
    ghost: false,
};
pub(super) const KEY_END: ActionKey = ActionKey {
    name: "end",
    titlebar: |e, cx| e.titlebar_dropdown_cursor_set(usize::MAX, cx),
    helm: None,
    popup: PopupKey::Ignore,
    minibuffer: |e, cx| {
        let len = e.minibuffer.as_ref().map_or(0, |mb| mb.input.len());
        e.mb_move(len, cx);
    },
    special: false,
    ghost: false,
};
pub(super) const KEY_ENTER: ActionKey = ActionKey {
    name: "enter",
    titlebar: |e, cx| e.titlebar_dropdown_select_current(cx),
    helm: Some(|e, cx| e.helm_accept(cx)),
    popup: PopupKey::Run(|| EditorCommand::CompletionAccept),
    minibuffer: |e, cx| e.mb_submit(cx),
    special: true, // RET runs its binding (e.g. visit/expand)
    ghost: true,   // some setups accept a suggestion on RET
};
pub(super) const KEY_TAB: ActionKey = ActionKey {
    name: "tab",
    titlebar: |_, _| {}, // dropdown is modal; TAB has no filter binding
    // complete to selection / descend folders
    helm: Some(|e, cx| e.helm_complete(cx)),
    popup: PopupKey::Run(|| EditorCommand::CompletionNext),
    minibuffer: |e, cx| e.mb_complete(cx),
    special: true, // TAB runs its binding (e.g. toggle section)
    // scheme/ghost.scm binds TAB to ghost-text-accept. Before the
    // modal and mode maps, so e.g. org-tab doesn't shadow an accept.
    ghost: true,
};

/// Whether a keystroke in a live terminal belongs to the editor instead of the
/// shell: an in-progress editor sequence continues, `C-x` is the editor prefix,
/// platform (cmd) chords stay the editor's (so cmd-c/cmd-v copy/paste and cmd-q
/// work), a chord listed in the whitespace-separated `passthrough` option is
/// the user's escape hatch, and a Ctrl/Alt chord that opens a leader sequence
/// (the alt leader) is handed over. A plain key always types into the shell.
fn terminal_key_goes_to_editor(
    ks: &Keystroke,
    pending: bool,
    passthrough: &str,
    keymap: &crate::mode::Keymap,
) -> bool {
    if pending || ks.modifiers.platform || (ks.modifiers.control && ks.key == "x") {
        return true;
    }
    let chord = chord_string(ks);
    if passthrough.split_whitespace().any(|k| k == chord) {
        return true;
    }
    (ks.modifiers.control || ks.modifiers.alt) && keymap.is_prefix_mode(LEADER_KEYMAP, &chord)
}

#[cfg(test)]
mod tests {
    use super::terminal_key_goes_to_editor;
    use crate::mode::{Keymap, LEADER_KEYMAP};
    use gpui::Keystroke;

    fn key(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn terminal_routes_only_editor_chords_away_from_the_shell() {
        let mut km = Keymap::default();
        km.define(LEADER_KEYMAP, "space n o", "delete-other-windows");
        km.define(LEADER_KEYMAP, "alt-space n o", "delete-other-windows");
        let to_editor = |k: &str, pending: bool, pass: &str| {
            terminal_key_goes_to_editor(&key(k), pending, pass, &km)
        };

        // The shell owns ordinary keys — including the plain leader (SPC types).
        assert!(!to_editor("space", false, ""));
        assert!(!to_editor("ctrl-c", false, ""));
        assert!(!to_editor("alt-o", false, ""));
        // Built-in editor chords.
        assert!(to_editor("ctrl-x", false, ""));
        assert!(to_editor("cmd-v", false, ""));
        // An in-progress sequence keeps going to the editor.
        assert!(to_editor("n", true, ""));
        // The alt leader opens a leader sequence.
        assert!(to_editor("alt-space", false, ""));
        // User pass-through list.
        assert!(to_editor("alt-o", false, "ctrl-] alt-o"));
        assert!(!to_editor("alt-p", false, "ctrl-] alt-o"));
    }
}
