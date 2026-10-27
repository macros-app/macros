//! Emacs core input commands for [`Editor`]: the mark ring (`C-u C-SPC`,
//! `C-x C-SPC`), text scaling (`C-x C-+`, `cmd-=`), `overwrite-mode`,
//! `quoted-insert` (`C-q`), `scroll-other-window` (`C-M-v`), `repeat`
//! (`C-x z`) and the regexp / symbol isearch entry points. These methods
//! extend the same `Editor` type via an additional `impl` block; the key
//! bindings live in `scheme/emacs-input.scm`.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use gpui::{Context, Keystroke, SharedString};

use super::input::Handled;
use super::isearch::IsearchKind;
use super::*;

/// The commands this module owns, as one [`EditorCommand::EmacsInput`]
/// variant so the shared command enum gains a single line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmacsInputCmd {
    /// `C-u C-SPC`: jump to the mark and rotate the buffer's mark ring.
    PopToMark,
    /// `C-x C-SPC`: jump to the newest global-mark-ring entry (any buffer).
    PopGlobalMark,
    /// `C-x C-+` / `C-x C--` / `C-x C-0`: this buffer's text scale.
    TextScale(ScaleStep),
    /// `cmd-=` / `cmd--` / `cmd-0`: every buffer's (and the UI's) text scale.
    GlobalTextScale(ScaleStep),
    OverwriteMode,
    QuotedInsert,
    /// `C-M-v` (`true`) / `C-M-S-v` (`false`).
    ScrollOtherWindow(bool),
    Repeat,
    IsearchRegexp(Direction),
    IsearchSymbolAtPoint,
}

/// One text-scale adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleStep {
    Increase,
    Decrease,
    Reset,
}

/// Steel names for [`EmacsInputCmd`]; registered as nullary commands.
pub const EMACS_INPUT_COMMANDS: &[(&str, EmacsInputCmd)] = {
    use EmacsInputCmd::*;
    use ScaleStep::*;
    &[
        ("pop-to-mark-command", PopToMark),
        ("pop-global-mark", PopGlobalMark),
        ("text-scale-increase", TextScale(Increase)),
        ("text-scale-decrease", TextScale(Decrease)),
        ("text-scale-reset", TextScale(Reset)),
        ("global-text-scale-increase", GlobalTextScale(Increase)),
        ("global-text-scale-decrease", GlobalTextScale(Decrease)),
        ("global-text-scale-reset", GlobalTextScale(Reset)),
        ("overwrite-mode", OverwriteMode),
        ("quoted-insert", QuotedInsert),
        ("scroll-other-window", ScrollOtherWindow(true)),
        ("scroll-other-window-down", ScrollOtherWindow(false)),
        ("repeat", Repeat),
        ("isearch-forward-regexp", IsearchRegexp(Direction::Forward)),
        (
            "isearch-backward-regexp",
            IsearchRegexp(Direction::Backward),
        ),
        ("isearch-forward-symbol-at-point", IsearchSymbolAtPoint),
    ]
};

/// Emacs `mark-ring-max` / `global-mark-ring-max`.
const MARK_RING_MAX: usize = 16;
/// Emacs `text-scale-mode-step`: each step scales the font by this factor.
const TEXT_SCALE_STEP: f32 = 1.2;
/// The global scale moves in smaller steps, like an app's zoom.
const GLOBAL_SCALE_STEP: f32 = 1.1;
const SCALE_MIN: i32 = -6;
const SCALE_MAX: i32 = 12;

/// The transient keymap a command leaves armed (Emacs `set-transient-map`):
/// the next plain key in it runs again; any other key exits and runs normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transient {
    /// After `C-x C-+`: `+`/`=`, `-`, `0` adjust the buffer's scale again.
    TextScale,
    /// After `C-x z`: `z` repeats again.
    Repeat,
}

/// A `C-q` waiting for its key: octal digits typed so far, and the count
/// (`C-u 3 C-q x` inserts `xxx`).
pub(super) struct QuotedInsert {
    digits: String,
    count: usize,
}

/// State for this module, in [`Editor::emacs`]. Per-buffer parts are keyed by
/// buffer name (like the Vim marks), so `Document` stays untouched.
#[derive(Default)]
pub(super) struct EmacsInputState {
    /// Each buffer's mark ring, oldest first; the last entry is the current mark.
    mark_rings: HashMap<String, Vec<usize>>,
    /// `(buffer, position)`, oldest first: one entry per buffer change.
    global_ring: Vec<(String, usize)>,
    /// The universal argument of the command being applied (`C-u` → 4).
    pub(super) prefix_arg: Option<i64>,
    /// Counts recorded commands, so a pop can tell whether it came right after
    /// the previous one (`C-u C-SPC C-SPC` keeps popping).
    events: u64,
    pop_event: Option<u64>,
    /// The last recorded command, with its prefix argument, for `C-x z`.
    last_command: Option<(MacroEvent, Option<i64>)>,
    pub(super) transient: Option<Transient>,
    quoted: Option<QuotedInsert>,
    overwrite: HashSet<String>,
    text_scale: HashMap<String, i32>,
    global_scale: i32,
}

impl EmacsInputState {
    /// Push `pos` as `buffer`'s new mark, and note a buffer change on the
    /// global ring (Emacs `push-mark`).
    fn push_mark(&mut self, buffer: &str, pos: usize) {
        let ring = self.mark_rings.entry(buffer.to_string()).or_default();
        if ring.last() != Some(&pos) {
            ring.push(pos);
            if ring.len() > MARK_RING_MAX {
                ring.remove(0);
            }
        }
        match self.global_ring.last_mut() {
            Some((b, p)) if b == buffer => *p = pos,
            _ => {
                self.global_ring.push((buffer.to_string(), pos));
                if self.global_ring.len() > MARK_RING_MAX {
                    self.global_ring.remove(0);
                }
            }
        }
    }

    /// Keep the rings on the same text through an edit of `buffer`.
    fn shift(&mut self, buffer: &str, start: usize, old_end: usize, new_len: usize) {
        if let Some(ring) = self.mark_rings.get_mut(buffer) {
            for p in ring.iter_mut() {
                *p = vim::shift_pos(*p, start, old_end, new_len);
            }
        }
        for (b, p) in self.global_ring.iter_mut() {
            if b == buffer {
                *p = vim::shift_pos(*p, start, old_end, new_len);
            }
        }
    }
}

/// The character a quoted key stands for: `C-l` is a form feed, `TAB` a
/// tab, `RET` a carriage return (Emacs `^M`; `C-j` gives a newline).
fn quoted_char(ks: &Keystroke) -> Option<String> {
    let m = &ks.modifiers;
    if m.platform {
        return None;
    }
    if m.control {
        let c = match ks.key.as_str() {
            "space" | "@" | "2" => '\0',
            "[" => '\x1b',
            "\\" => '\x1c',
            "]" => '\x1d',
            "/" | "_" => '\x1f',
            k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphabetic() => {
                (k.as_bytes()[0].to_ascii_lowercase() - b'a' + 1) as char
            }
            _ => return None,
        };
        return Some(c.to_string());
    }
    match ks.key.as_str() {
        "escape" => Some("\x1b".into()),
        "space" => Some(" ".into()),
        _ => ks
            .key_char
            .clone()
            .or_else(|| (ks.key.chars().count() == 1).then(|| ks.key.clone())),
    }
}

/// The character for an OS-special (gpui action) key, by its keymap name.
fn quoted_action_char(key: &str) -> Option<&'static str> {
    match key {
        "tab" => Some("\t"),
        "enter" => Some("\r"),
        "backspace" => Some("\x7f"),
        _ => None,
    }
}

/// An octal code as the character it names (`C-q 1 4 RET` → `\x0c`).
fn octal_char(digits: &str) -> Option<String> {
    let code = u32::from_str_radix(digits, 8).ok()?;
    char::from_u32(code).map(String::from)
}

impl Editor {
    /// Apply one of this module's commands.
    pub(super) fn emacs_input_command(&mut self, cmd: EmacsInputCmd, cx: &mut Context<Self>) {
        use EmacsInputCmd::*;
        match cmd {
            PopToMark => self.pop_to_mark(cx),
            PopGlobalMark => self.pop_global_mark(cx),
            TextScale(step) => self.text_scale_adjust(step, false, cx),
            GlobalTextScale(step) => self.text_scale_adjust(step, true, cx),
            OverwriteMode => self.toggle_overwrite_mode(cx),
            QuotedInsert => self.quoted_insert_arm(cx),
            ScrollOtherWindow(up) => self.scroll_other_window(up, cx),
            Repeat => self.repeat_last_command(cx),
            IsearchRegexp(dir) => self.isearch_start_with(dir, IsearchKind::Regexp, "", cx),
            IsearchSymbolAtPoint => self.isearch_symbol_at_point(cx),
        }
    }

    // --- Mark ring ---------------------------------------------------------

    /// `C-SPC` (`set-mark-command`): set the mark at point and push it onto the
    /// mark ring. With `C-u`, pop instead; right after a pop, pop again
    /// (Emacs `set-mark-command-repeat-pop`).
    pub(super) fn set_mark_command(&mut self, cx: &mut Context<Self>) {
        let after_pop = self.emacs.pop_event.is_some()
            && self.emacs.pop_event == self.emacs.events.checked_sub(1);
        if self.emacs.prefix_arg.is_some() || after_pop {
            self.pop_to_mark(cx);
            return;
        }
        let p = self.point();
        let name = self.active.name.clone();
        self.emacs.push_mark(&name, p);
        self.set_mark(cx);
    }

    /// Note `pos` on the mark rings without activating a region (Emacs
    /// `push-mark` from `M-<`, `M->` and isearch).
    pub(super) fn push_mark_quietly(&mut self, pos: usize) {
        let name = self.active.name.clone();
        self.emacs.push_mark(&name, pos);
    }

    /// Jump to the mark, deactivating the region, and rotate the ring so the
    /// next pop goes one older.
    pub(super) fn pop_to_mark(&mut self, cx: &mut Context<Self>) {
        let name = self.active.name.clone();
        let Some(ring) = self
            .emacs
            .mark_rings
            .get_mut(&name)
            .filter(|r| !r.is_empty())
        else {
            self.echo("No mark set in this buffer", cx);
            return;
        };
        let target = ring.pop().unwrap_or(0);
        ring.insert(0, target);
        self.emacs.pop_event = Some(self.emacs.events);
        let target = self.active.buffer.snap(self.clamp_accessible(target));
        self.collapse_to(target);
        self.move_to(target, cx);
    }

    /// `C-x C-SPC`: go to the newest global-mark-ring entry, switching buffer
    /// if it lies elsewhere; entries whose buffer is gone are dropped.
    pub(super) fn pop_global_mark(&mut self, cx: &mut Context<Self>) {
        while let Some((buffer, pos)) = self.emacs.global_ring.pop() {
            let exists = buffer == self.active.name || self.inactive_index(&buffer).is_some();
            if !exists {
                continue;
            }
            self.emacs.global_ring.insert(0, (buffer.clone(), pos));
            if buffer != self.active.name {
                self.switch_to_buffer(&buffer, cx);
            }
            let pos = self.active.buffer.snap(self.clamp_accessible(pos));
            self.collapse_to(pos);
            self.move_to(pos, cx);
            return;
        }
        self.echo("No global mark set", cx);
    }

    /// Edit hook: keep the mark rings on the same text.
    pub(super) fn emacs_shift_positions(&mut self, start: usize, old_end: usize, new_len: usize) {
        let name = self.active.name.clone();
        self.emacs.shift(&name, start, old_end, new_len);
    }

    /// Record hook (every recorded command or typed text): count events for
    /// the repeat-pop rule and remember the command `C-x z` repeats.
    pub(super) fn emacs_note_event(&mut self, ev: &MacroEvent) {
        self.emacs.events += 1;
        let is_repeat = matches!(ev, MacroEvent::Command(e) if e == "(repeat)");
        if !is_repeat && !matches!(ev, MacroEvent::Key(_)) {
            self.emacs.last_command = Some((ev.clone(), self.uarg.pending));
        }
    }

    /// `M-x NAME` runs outside the key path, so note it for `C-x z` here.
    pub(super) fn emacs_note_mx(&mut self, name: &str) {
        if name != "repeat" {
            let ev = MacroEvent::Command(format!("({name})"));
            self.emacs.last_command = Some((ev, None));
        }
    }

    // --- Text scale ----------------------------------------------------------

    /// `C-x C-+` and friends: scale this buffer's text, then leave `+`/`-`/`0`
    /// armed for further adjustment. `global` scales all text (`cmd-=`), which
    /// is a plain app zoom and arms nothing.
    fn text_scale_adjust(&mut self, step: ScaleStep, global: bool, cx: &mut Context<Self>) {
        let n = self.emacs.prefix_arg.unwrap_or(1).clamp(-64, 64) as i32;
        let name = self.active.name.clone();
        let current = if global {
            self.emacs.global_scale
        } else {
            self.emacs.text_scale.get(&name).copied().unwrap_or(0)
        };
        let next = match step {
            ScaleStep::Increase => current + n,
            ScaleStep::Decrease => current - n,
            ScaleStep::Reset => 0,
        }
        .clamp(SCALE_MIN, SCALE_MAX);
        if global {
            self.emacs.global_scale = next;
        } else if next == 0 {
            self.emacs.text_scale.remove(&name);
        } else {
            self.emacs.text_scale.insert(name, next);
        }
        if global {
            self.echo(format!("Global text scale {next:+}"), cx);
        } else {
            self.emacs.transient = Some(Transient::TextScale);
            self.echo(
                format!("Text scale {next:+}; use +, -, 0 for further adjustment"),
                cx,
            );
        }
    }

    /// The buffer-local text-scale steps of `buffer` (0 when unscaled).
    pub fn text_scale_steps(&self, buffer: &str) -> i32 {
        self.emacs.text_scale.get(buffer).copied().unwrap_or(0)
    }

    /// The factor every font size is multiplied by (`cmd-=`).
    pub(super) fn global_text_scale_factor(&self) -> f32 {
        GLOBAL_SCALE_STEP.powi(self.emacs.global_scale)
    }

    /// The scaled `(font size, line height)` for a pane showing `buffer`, or
    /// `None` when the buffer isn't scaled (the pane inherits the window's).
    pub(super) fn buffer_text_style(&self, buffer: &str, cx: &gpui::App) -> Option<(f32, f32)> {
        let steps = self.text_scale_steps(buffer);
        if steps == 0 {
            return None;
        }
        let settings = cx.global::<crate::settings::Settings>();
        let f = self.global_text_scale_factor() * TEXT_SCALE_STEP.powi(steps);
        Some((settings.font_size() * f, settings.line_height() * f))
    }

    // --- Overwrite mode ------------------------------------------------------

    fn toggle_overwrite_mode(&mut self, cx: &mut Context<Self>) {
        let name = self.active.name.clone();
        let on = if self.emacs.overwrite.remove(&name) {
            false
        } else {
            self.emacs.overwrite.insert(name);
            true
        };
        self.echo(
            format!("Overwrite mode {}", if on { "enabled" } else { "disabled" }),
            cx,
        );
    }

    /// Whether `overwrite-mode` is on in the active buffer.
    pub fn overwrite_mode_active(&self) -> bool {
        self.emacs.overwrite.contains(&self.active.name)
    }

    /// The range typing `text` at the empty `range` replaces in overwrite
    /// mode: as many characters as it types, stopping at the end of the line.
    pub(super) fn overwrite_range(&self, range: Range<usize>, text: &str) -> Range<usize> {
        if !range.is_empty() || text.contains('\n') || !self.overwrite_mode_active() {
            return range;
        }
        let buffer = &self.active.buffer;
        let limit = self.accessible().end;
        let mut end = range.start;
        for _ in 0..text.chars().count() {
            let next = buffer.next_grapheme(end);
            if next == end || next > limit || buffer.slice_to_string(end..next) == "\n" {
                break;
            }
            end = next;
        }
        range.start..end
    }

    // --- quoted-insert -------------------------------------------------------

    fn quoted_insert_arm(&mut self, cx: &mut Context<Self>) {
        let count = self.emacs.prefix_arg.unwrap_or(1).clamp(1, 10_000) as usize;
        self.emacs.quoted = Some(QuotedInsert {
            digits: String::new(),
            count,
        });
        // Replay inserts the resolved text; the arming command would wait for
        // a key that playback never sends.
        self.unrecord_last_event();
        self.status = "C-q-".into();
        cx.notify();
    }

    /// Insert the quoted `text` (`count` times), recorded as typed text.
    fn quoted_insert_finish(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(q) = self.emacs.quoted.take() else {
            return;
        };
        self.status = SharedString::default();
        let text = text.repeat(q.count);
        self.record_event(MacroEvent::Text(text.clone()));
        self.apply_command(EditorCommand::Insert(text), cx);
    }

    /// Finish a pending octal code; false (and disarmed) if it isn't valid.
    fn quoted_insert_octal(&mut self, cx: &mut Context<Self>) {
        let digits = self
            .emacs
            .quoted
            .as_ref()
            .map(|q| q.digits.clone())
            .unwrap_or_default();
        match octal_char(&digits) {
            Some(c) => self.quoted_insert_finish(&c, cx),
            None => self.emacs.quoted = None,
        }
    }

    // --- scroll-other-window -------------------------------------------------

    /// `C-M-v` / `C-M-S-v`: scroll the next window a page without leaving this one.
    fn scroll_other_window(&mut self, up: bool, cx: &mut Context<Self>) {
        let leaves = self.layout.leaves();
        let Some(pos) = leaves.iter().position(|&id| id == self.selected_pane) else {
            return;
        };
        if leaves.len() < 2 {
            self.echo("There is no other window", cx);
            return;
        }
        let here = self.selected_pane;
        let other = leaves[(pos + 1) % leaves.len()];
        self.select_pane(other, cx);
        self.scroll_page(if up { 1 } else { -1 }, cx);
        self.select_pane(here, cx);
    }

    // --- repeat ----------------------------------------------------------------

    /// `C-x z`: run the last command again with the same argument (`C-u N
    /// C-x z` gives it `N` instead); `z` keeps repeating.
    fn repeat_last_command(&mut self, cx: &mut Context<Self>) {
        let Some((ev, arg)) = self.emacs.last_command.clone() else {
            self.echo("No command to repeat", cx);
            return;
        };
        let arg = self.emacs.prefix_arg.or(arg);
        match &ev {
            MacroEvent::Command(expr) => {
                self.uarg.pending = arg;
                self.run_script(expr, cx);
            }
            MacroEvent::Counted(expr, n) => self.run_counted(expr, *n, cx),
            MacroEvent::Text(t) => {
                let n = arg.unwrap_or(1).clamp(1, 10_000) as usize;
                self.apply_command(EditorCommand::Insert(t.repeat(n)), cx);
            }
            MacroEvent::Key(_) => {}
        }
        self.emacs.transient = Some(Transient::Repeat);
    }

    // --- isearch -------------------------------------------------------------

    /// `M-s .`: isearch for the symbol at point, matching whole symbols only.
    fn isearch_symbol_at_point(&mut self, cx: &mut Context<Self>) {
        let Some(range) = self.symbol_range_at_point() else {
            self.echo("No symbol at point", cx);
            return;
        };
        let symbol = self.active.buffer.slice_to_string(range.clone());
        self.collapse_to(range.start);
        self.isearch_start_with(Direction::Forward, IsearchKind::Symbol, &symbol, cx);
    }

    /// Whether `c` belongs to a symbol here (Lisp symbols take `-`, `?`, …).
    pub(super) fn is_symbol_char(&self, c: char) -> bool {
        c.is_alphanumeric() || c == '_' || (self.is_lisp_buffer() && "-!?*<>=/+:%&$^~".contains(c))
    }

    /// The symbol under (or just before) point.
    fn symbol_range_at_point(&self) -> Option<Range<usize>> {
        let buffer = &self.active.buffer;
        let p = self.point();
        let acc = self.accessible();
        let line = buffer.line_range(buffer.line_index_at_byte(p));
        let line = line.start.max(acc.start)..line.end.min(acc.end);
        let text = buffer.slice_to_string(line.clone());
        let at = p - line.start;
        let is_sym = |c: char| self.is_symbol_char(c);
        let start = text[..at]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_sym(c))
            .last()
            .map_or(at, |(i, _)| i);
        let end = text[at..]
            .char_indices()
            .find(|&(_, c)| !is_sym(c))
            .map_or(text.len(), |(i, _)| at + i);
        (end > start).then(|| line.start + start..line.start + end)
    }

    // --- Key layers ------------------------------------------------------------

    /// A pending `C-q`, or a transient map left by `C-x C-+` / `C-x z`, claims
    /// the next key before the modal layers.
    pub(super) fn key_emacs_input(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if self.emacs.quoted.is_some() {
            return self.key_quoted_insert(ks, cx);
        }
        let Some(transient) = self.emacs.transient.take() else {
            return Handled::No;
        };
        let m = &ks.modifiers;
        if m.control || m.alt || m.platform {
            return Handled::No;
        }
        let key = ks.key_char.as_deref().unwrap_or(ks.key.as_str());
        let expr = match (transient, key) {
            (Transient::TextScale, "+" | "=") => "(text-scale-increase)",
            (Transient::TextScale, "-") => "(text-scale-decrease)",
            (Transient::TextScale, "0") => "(text-scale-reset)",
            (Transient::Repeat, "z") => "(repeat)",
            _ => return Handled::No,
        };
        self.dispatch_recorded(expr, cx);
        cx.stop_propagation();
        Handled::Yes
    }

    fn key_quoted_insert(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let m = &ks.modifiers;
        let plain = !m.control && !m.alt && !m.platform;
        let has_digits = self
            .emacs
            .quoted
            .as_ref()
            .is_some_and(|q| !q.digits.is_empty());
        if plain && ks.key.len() == 1 && matches!(ks.key.as_bytes()[0], b'0'..=b'7') {
            let q = self.emacs.quoted.as_mut().expect("armed");
            q.digits.push_str(&ks.key);
            self.status = format!("C-q {}-", q.digits).into();
            if q.digits.len() == 3 {
                self.quoted_insert_octal(cx);
            }
            cx.notify();
            cx.stop_propagation();
            return Handled::Yes;
        }
        if has_digits {
            // Any other key ends the code and then runs as itself.
            self.quoted_insert_octal(cx);
            return Handled::No;
        }
        match quoted_char(ks) {
            Some(c) => self.quoted_insert_finish(&c, cx),
            None => {
                self.emacs.quoted = None;
                self.status = SharedString::default();
                return Handled::No;
            }
        }
        cx.stop_propagation();
        Handled::Yes
    }

    /// The gpui-action side (TAB, RET, DEL, arrows) of the layers above. True
    /// when the key was consumed.
    pub(super) fn emacs_input_action_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        self.emacs.transient = None;
        let Some(q) = self.emacs.quoted.as_ref() else {
            return false;
        };
        if !q.digits.is_empty() {
            self.quoted_insert_octal(cx);
            // RET just ends the code; any other key also runs.
            return key == "enter";
        }
        match quoted_action_char(key) {
            Some(c) => self.quoted_insert_finish(c, cx),
            None => {
                self.emacs.quoted = None;
                self.status = SharedString::default();
                cx.notify();
            }
        }
        true
    }

    /// A bound named key with no modifiers (`<insert>`, `<f5>`): the Emacs
    /// dispatch leaves these to gpui, so resolve a complete binding here.
    pub(super) fn key_named_binding(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let m = &ks.modifiers;
        if !self.pending_keys.is_empty()
            || m.control
            || m.alt
            || m.platform
            || ks.key_char.is_some()
            || ks.key.chars().count() < 2
            || ks.key == "escape"
        {
            return Handled::No;
        }
        let chord = chord_string(ks);
        let Some(expr) = self
            .keymap
            .resolve(&self.active.modes, &chord)
            .map(str::to_string)
        else {
            return Handled::No;
        };
        self.dispatch_recorded(&expr, cx);
        cx.stop_propagation();
        Handled::Yes
    }
}
