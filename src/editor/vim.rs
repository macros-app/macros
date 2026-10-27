//! Vim-grammar infrastructure for evil mode: operator-pending state, one-key
//! argument capture (`%c` bindings), and the routing that turns `2d3w` into a
//! single Scheme call. The motions/operators themselves live in
//! `scheme/evil.scm`; Rust only owns the parts that must be synchronous with
//! keystrokes (which keymap the next key resolves against, what counts were
//! typed), so the composed command is self-contained — a macro or `.` replays
//! it without any of this state.

use gpui::{Context, Keystroke, SharedString};

use super::input::Handled;
use super::*;

/// The keymap motions and text objects resolve against while an operator is
/// pending (`d`, `c`, `y`, `>`, `gU`, …). Like the other evil keymaps it may be
/// shadowed per major mode (`evil-operator:<major>`).
pub(super) const OPERATOR_KEYMAP: &str = "evil-operator";

/// The placeholder a binding uses for a one-keystroke argument: `m` is bound to
/// `evil-set-mark %c`, so `ma` runs `(evil-set-mark "a")`.
pub(super) const CHAR_ARG: &str = "%c";

/// An operator waiting for its motion (`d` typed, `w` not yet).
#[derive(Clone, Debug)]
pub(super) struct OpPending {
    /// Operator name handed to Scheme's `evil-operate` ("delete", "change", …).
    pub op: String,
    /// The key sequence that started it — typing it again (`dd`, `gUgU`), or
    /// its last key (`gUU`), means "the current line(s)".
    pub keys: String,
    /// The count typed before the operator (`2` in `2d3w`).
    pub count: Option<usize>,
    /// `v` / `V` typed after the operator: force charwise / linewise.
    pub force: Option<Force>,
}

/// An operator's forced motion type (`dv…` / `dV…`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Force {
    Char,
    Line,
}

impl Force {
    /// The key that forces it — also how `evil-operate` receives it.
    pub fn key(self) -> &'static str {
        match self {
            Force::Char => "v",
            Force::Line => "V",
        }
    }
}

/// A binding waiting for its one-key argument (`%c`).
#[derive(Clone, Debug)]
pub(super) struct Capture {
    pub expr: String,
    pub count: Option<usize>,
    /// Set when the capture is a motion of a pending operator (`dfx`, `d'a`).
    pub op: Option<OpPending>,
}

/// All Vim-grammar state hung off the editor.
#[derive(Default)]
pub(super) struct VimState {
    pub op: Option<OpPending>,
    pub capture: Option<Capture>,
    /// The motion count typed before an operator's `/` / `?` search motion,
    /// held while the minibuffer reads the pattern.
    pub search_count: Option<usize>,
    /// Set by `run_script` whenever it hands a command to the engine thread
    /// (its commands apply later) — a replay then waits for the sentinel.
    pub went_async: bool,
    /// The Normal-state command being recorded for `.` (from its first key
    /// through any Insert session it opens).
    pub dot: Option<DotRec>,
    /// The last command that changed the buffer — what `.` replays.
    pub last_change: Vec<MacroEvent>,
    /// A replay in progress (macro / `.`).
    pub replay: Option<Replay>,
    /// Undo state when the current Normal-state command began, so the Insert
    /// session it opens squashes into one undo step on Escape.
    pub cmd_undo_base: Option<UndoBase>,
    /// Vim registers (named, numbered, small-delete, read-only).
    pub registers: super::registers::Registers,
    /// `"x` typed: the register the next yank/delete/paste uses.
    pub pending_register: Option<char>,
    /// The register prefix was just typed — it survives one command boundary
    /// (the command it prefixes).
    pub register_armed: bool,
    /// `qx`: the register the macro being recorded goes to.
    pub macro_register: Option<char>,
    /// Macros by register (`qa` … `q`, `@a`).
    pub macros: std::collections::HashMap<char, Vec<MacroEvent>>,
    /// For `@@`.
    pub last_macro_register: Option<char>,
    /// The last ex command line (`":`, `@:`).
    pub last_ex: String,
    /// Text typed in the current Insert session (`".`).
    pub insert_acc: String,
    /// Buffer-local marks by buffer name: `a`–`z` plus the automatic ones
    /// (`'` previous context, `[` `]` last change/yank, `<` `>` last visual,
    /// `.` last change, `^` last insert). Byte positions, shifted on edits.
    pub marks: std::collections::HashMap<String, std::collections::HashMap<char, usize>>,
    /// File marks `A`–`Z`: buffer (and file, to reopen it) + position.
    pub global_marks: std::collections::HashMap<char, GlobalMark>,
    /// The jump list (oldest first) and the C-o / C-i cursor into it.
    pub jumps: Vec<Jump>,
    pub jump_idx: usize,
    /// Command names whose execution is a jump (`set-jump-command!`).
    pub jump_commands: std::collections::HashSet<String>,
    /// The last Visual state, for `gv`.
    pub last_visual: Option<EvilState>,
    /// The last `:s` (for `:s`, `&`, `:&&`, `g&`).
    pub last_sub: Option<super::ex::LastSub>,
    /// An interactive `:s///c` in progress.
    pub sub_confirm: Option<super::ex::SubConfirm>,
    /// `:g` line anchors: line starts that shift with edits; `None` once the
    /// line was deleted.
    pub ex_anchors: Vec<Option<usize>>,
}

/// A file mark (`mA`).
#[derive(Clone, Debug)]
pub(super) struct GlobalMark {
    pub buffer: String,
    pub path: Option<std::path::PathBuf>,
    pub pos: usize,
}

/// Outcome of showing the buffer a mark / jump points into.
enum Target {
    /// It is the active buffer now.
    Here,
    /// Its file is still opening; the position is applied when it opens.
    Deferred,
    /// No such buffer, and no file to revisit.
    Gone,
}

/// 0-based line and byte column of byte `pos` in `text` (clamped), or of
/// that line's first non-blank when `exact` is false.
fn line_col_at(text: &str, pos: usize, exact: bool) -> (usize, usize) {
    let pos = text.floor_char_boundary(pos);
    let line_start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let line0 = text[..line_start].matches('\n').count();
    let col = if exact {
        pos - line_start
    } else {
        text[line_start..]
            .find(|c: char| !matches!(c, ' ' | '\t'))
            .unwrap_or(0)
    };
    (line0, col)
}

/// Marks / jumps lifted out of a block of text that is being moved (offsets
/// from the block's start; jumps by index into the jump list).
#[derive(Default)]
pub(super) struct MovedPositions {
    local: Vec<(char, usize)>,
    global: Vec<(char, usize)>,
    jumps: Vec<(usize, usize)>,
}

/// One jump-list entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Jump {
    pub buffer: String,
    pub path: Option<std::path::PathBuf>,
    pub pos: usize,
}

/// Where a position lands after the edit replacing `[start, old_end)` with
/// `new_len` bytes: after it shifts by the size change, inside it collapses to
/// `start` (the same rule as Emacs markers).
pub(super) fn shift_pos(pos: usize, start: usize, old_end: usize, new_len: usize) -> usize {
    if pos >= old_end {
        (pos + new_len).saturating_sub(old_end - start)
    } else if pos > start {
        start
    } else {
        pos
    }
}

/// `` ` `` and `'` name the same mark (the previous context).
pub(super) fn normalize_mark(c: char) -> char {
    if c == '`' { '\'' } else { c }
}

/// Why [`Editor::resolve_mark`] has no position to jump to.
enum MarkErr {
    /// Unset, or its buffer is gone (already echoed).
    Missing,
    /// The mark's file is still opening; the jump lands once it is open.
    Deferred,
}

/// Whether `c` names a settable / jumpable mark.
pub(super) fn is_mark(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '\'' | '`' | '[' | ']' | '<' | '>' | '.' | '^')
}

/// A Vim-grammar call that must apply synchronously — `(name "arg")` for the
/// register / macro commands. `None` for anything else.
pub(super) fn native_command(expr: &str) -> Option<EditorCommand> {
    let (name, mut args) = parse_string_call(expr)?;
    if args.len() != 1 {
        return None;
    }
    let arg = args.pop()?;
    Some(match name.as_str() {
        "evil-use-register" => EditorCommand::EvilUseRegister(arg),
        "evil-insert-register" => EditorCommand::EvilInsertRegister(arg),
        "evil-macro-record" => EditorCommand::EvilMacroRecord(arg),
        "evil-macro-play" => EditorCommand::EvilMacroPlay(arg),
        "evil-set-mark" => EditorCommand::EvilSetMark(arg),
        "evil-goto-mark" => EditorCommand::EvilGotoMark(arg, true),
        "evil-goto-mark-line" => EditorCommand::EvilGotoMark(arg, false),
        "evil-ex" => EditorCommand::EvilEx(arg),
        "evil-replace-with" => EditorCommand::EvilReplaceWith(arg),
        "evil-ex-goto-line" => EditorCommand::EvilExGotoLine(arg),
        _ => return None,
    })
}

/// The first char of a register argument.
fn reg_char(r: &str) -> Option<char> {
    let mut chars = r.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// The command `.` may repeat, as it is being recorded.
pub(super) struct DotRec {
    pub events: Vec<MacroEvent>,
    /// Buffer + edit tick when it began: it counts as a change only if that
    /// buffer was edited by the time the next command starts.
    pub buffer: String,
    pub tick: u64,
    /// Only a register prefix so far (`"a`): the next key continues it.
    pub prefix_only: bool,
    /// Built from a mouse selection — not reproducible by replaying keys.
    pub unrepeatable: bool,
}

impl DotRec {
    /// An empty recording of a command starting now in the active buffer.
    fn new(ed: &Editor, prefix_only: bool, unrepeatable: bool) -> Self {
        Self {
            events: Vec::new(),
            buffer: ed.active.name.clone(),
            tick: ed.active.buffer.tick(),
            prefix_only,
            unrepeatable,
        }
    }
}

/// Recorded events replayed one at a time: each async (Scheme) command must
/// apply before the next runs, or every command would read the same stale
/// pre-replay snapshot.
pub(super) struct Replay {
    pub queue: std::collections::VecDeque<MacroEvent>,
    /// An async command is in flight; `(replay-step)` resumes.
    pub waiting: bool,
    /// Squash the whole replay into one undo step (`.`).
    pub undo_base: Option<UndoBase>,
}

/// The command name a key-bound expression calls: the head symbol of
/// `(name arg …)` (or a bare `name`).
pub(super) fn expr_head(expr: &str) -> &str {
    expr.trim()
        .trim_start_matches('(')
        .split(|c: char| c.is_whitespace() || c == ')')
        .next()
        .unwrap_or("")
}

/// Whether `expr` is a `"x` register prefix rather than a command proper.
fn is_register_prefix(expr: &str) -> bool {
    expr_head(expr) == "evil-use-register"
}

/// Commands that never count as "the last change", even though they edit.
fn is_repeat_neutral(expr: &str) -> bool {
    matches!(
        expr_head(expr),
        "undo"
            | "redo"
            | "undo-tree-undo"
            | "undo-tree-redo"
            | "evil-repeat"
            | "call-last-kbd-macro"
            | "evil-macro-play"
            // these open a prompt — replaying them would ask again
            | "evil-ex-command"
            | "evil-search-forward"
            | "evil-search-backward"
    )
}

/// `(evil-operate "op" COUNT …)` (as [`compose_operate`] builds it) with
/// COUNT replaced by `n`. `None` when `expr` isn't such a call.
fn with_operate_count(expr: &str, n: usize) -> Option<String> {
    const HEAD: &str = "(evil-operate ";
    let args = expr.strip_prefix(HEAD)?;
    // Past the operator name's closing quote.
    let open = args.find('"')?;
    let close = args[open + 1..].find('"')?;
    let name_end = HEAD.len() + open + 1 + close + 1;
    let rest = expr[name_end..].trim_start();
    let tail = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    Some(format!("{} {n}{tail}", &expr[..name_end]))
}

/// `3.`: give the repeated command the new count. The operator call carries
/// its count inline; anything else becomes a counted command.
pub(super) fn override_count(events: &mut [MacroEvent], n: usize) {
    for ev in events.iter_mut() {
        match ev {
            // `"a` before the change: the register prefix, not the change.
            MacroEvent::Command(expr) if is_register_prefix(expr) => {}
            MacroEvent::Command(expr) | MacroEvent::Counted(expr, _)
                if expr.starts_with("(evil-operate ") =>
            {
                if let Some(recounted) = with_operate_count(expr, n) {
                    *ev = MacroEvent::Command(recounted);
                }
                return;
            }
            MacroEvent::Counted(_, c) => {
                *c = n;
                return;
            }
            MacroEvent::Command(expr) => {
                *ev = MacroEvent::Counted(expr.clone(), n);
                return;
            }
            MacroEvent::Text(_) | MacroEvent::Key(_) => {}
        }
    }
}

/// `(evil-operator "name")` → `name`.
pub(super) fn parse_operator_expr(expr: &str) -> Option<String> {
    let (name, args) = parse_string_call(expr)?;
    (name == "evil-operator" && args.len() == 1).then(|| args[0].clone())
}

/// Parse `(name "a" "b" …)` — a call whose arguments are all string literals.
/// `None` for anything else (nested forms, numbers, symbols).
pub(super) fn parse_string_call(expr: &str) -> Option<(String, Vec<String>)> {
    let inner = expr.trim().strip_prefix('(')?.strip_suffix(')')?;
    let inner = inner.trim_start();
    let name_end = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() || name.contains(['(', ')', '"']) {
        return None;
    }
    let mut rest = inner[name_end..].trim_start();
    let mut args = Vec::new();
    while !rest.is_empty() {
        let body = rest.strip_prefix('"')?;
        let mut out = String::new();
        let mut chars = body.char_indices();
        let mut end = None;
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    let (_, n) = chars.next()?;
                    out.push(n);
                }
                '"' => {
                    end = Some(i);
                    break;
                }
                _ => out.push(c),
            }
        }
        let end = end?;
        args.push(out);
        rest = body[end + 1..].trim_start();
    }
    Some((name.to_string(), args))
}

pub(super) use crate::script::steel_quote;

/// The operator count times the motion count (`2d3w` = 6), or 0 when neither
/// was typed (so a motion can tell `G` from `1G`). Capped like the plain count.
pub(super) fn combined_count(op: Option<usize>, motion: Option<usize>) -> usize {
    match (op, motion) {
        (None, None) => 0,
        (a, b) => a
            .unwrap_or(1)
            .saturating_mul(b.unwrap_or(1))
            .clamp(1, 10_000),
    }
}

/// The self-contained Scheme call an operator + motion resolves to. A macro or
/// `.` replays exactly this string — no pending state is needed to rerun it.
pub(super) fn compose_operate(op: &OpPending, motion_count: Option<usize>, motion: &str) -> String {
    let motion = motion.trim();
    let motion = if motion.starts_with('(') {
        motion.to_string()
    } else {
        format!("({motion})")
    };
    format!(
        "(evil-operate {} {} {} (lambda () {motion}))",
        steel_quote(&op.op),
        combined_count(op.count, motion_count),
        steel_quote(op.force.map_or("", Force::key)),
    )
}

/// Substitute the captured key for the `%c` placeholder.
pub(super) fn fill_char_arg(expr: &str, ch: &str) -> String {
    expr.replacen(CHAR_ARG, &steel_quote(ch), 1)
}

impl Editor {
    /// Append an event to the keyboard macro being recorded (unless it is a
    /// macro-control command) and to the `.` recording. Nothing is recorded
    /// while a replay runs — the replayed command was recorded once already.
    pub(super) fn record_event(&mut self, ev: MacroEvent) {
        if self.vim.replay.is_some() {
            return;
        }
        self.emacs_note_event(&ev);
        let macro_ctl = matches!(&ev, MacroEvent::Command(e) | MacroEvent::Counted(e, _)
            if e.contains("kbd-macro"));
        if !macro_ctl && let Some(events) = self.macro_recording.as_mut() {
            events.push(ev.clone());
        }
        if self.evil == EvilState::Insert
            && let MacroEvent::Text(t) = &ev
        {
            self.vim.insert_acc.push_str(t);
        }
        if let Some(dot) = self.vim.dot.as_mut() {
            dot.events.push(ev);
        }
    }

    // --- registers ---------------------------------------------------------

    /// The register the next kill/paste targets (consumed), under evil only.
    pub(super) fn take_kill_register(&mut self) -> Option<char> {
        if !self.vim_active() {
            return None;
        }
        self.vim.register_armed = false;
        self.vim.pending_register.take()
    }

    /// File a yank/delete into the Vim registers (`0`, `1`–`9`, `-`, named).
    pub(super) fn note_register_kill(
        &mut self,
        kind: super::registers::KillKind,
        reg: Option<char>,
        text: &str,
    ) {
        if !self.vim_active() {
            return;
        }
        if self.kill_append {
            self.vim.registers.append_last(text);
        } else {
            let named = reg.filter(|c| c.is_ascii_alphabetic());
            self.vim.registers.record(kind, named, text);
        }
    }

    /// A Normal-state command boundary: a register prefix lives for exactly
    /// the one command after it.
    pub(super) fn register_boundary(&mut self, expr: &str) {
        if is_register_prefix(expr) {
            return;
        }
        if expr.trim() == "(keyboard-quit)" {
            self.vim.register_armed = false;
        }
        if self.vim.register_armed {
            self.vim.register_armed = false;
        } else {
            self.vim.pending_register = None;
        }
    }

    /// `"x`: select the register for the next yank/delete/paste.
    pub(super) fn evil_use_register(&mut self, r: &str, cx: &mut Context<Self>) {
        match reg_char(r).filter(|c| super::registers::is_register(*c)) {
            Some(c) => {
                self.vim.pending_register = Some(c);
                self.vim.register_armed = true;
                self.status = format!("\"{c}").into();
                cx.notify();
            }
            None => self.echo(format!("Invalid register: {r}"), cx),
        }
    }

    /// A register's contents (the clipboard for `+`/`*`, the read-only `/`
    /// and `%`, the kill ring top for `"`).
    pub(super) fn evil_register_value(
        &mut self,
        r: char,
        cx: &mut Context<Self>,
    ) -> Option<super::registers::RegValue> {
        use super::registers::RegValue;
        let v = |text: String, linewise: bool| {
            (!text.is_empty()).then_some(RegValue { text, linewise })
        };
        match r {
            '+' | '*' => {
                let text = cx.read_from_clipboard().and_then(|i| i.text())?;
                let lw = text.ends_with('\n');
                v(text, lw)
            }
            '/' => v(self.search_query.clone().unwrap_or_default(), false),
            '%' => v(
                self.active
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.active.name.clone()),
                false,
            ),
            '"' => {
                if self.sync_clipboard_into_kill_ring(cx) {
                    self.evil_yank_linewise = false;
                }
                let text = self.kill_ring.last().cloned().unwrap_or_default();
                v(text, self.evil_yank_linewise)
            }
            _ => self
                .vim
                .registers
                .get(r)
                .cloned()
                .filter(|v| !v.text.is_empty()),
        }
    }

    /// Insert-state `C-r x`: insert register x's text at point.
    pub(super) fn evil_insert_register(&mut self, r: &str, cx: &mut Context<Self>) {
        let Some(c) = reg_char(r) else {
            return;
        };
        match self.evil_register_value(c, cx) {
            Some(v) => self.insert_text(&v.text, cx),
            None => self.echo(format!("Register {c} is empty"), cx),
        }
    }

    /// `:registers` (`:reg abc` limits to those names).
    pub(super) fn show_registers(&mut self, only: &str, cx: &mut Context<Self>) {
        let want = |c: char| only.is_empty() || only.contains(c);
        let mut lines = vec!["Type Name Content".to_string()];
        let mut push = |c: char, v: &super::registers::RegValue| {
            if want(c) {
                let ty = if v.linewise { "  l " } else { "  c " };
                lines.push(format!(
                    "{ty} \"{c}   {}",
                    super::registers::display_value(&v.text)
                ));
            }
        };
        if let Some(top) = self.kill_ring.last() {
            push(
                '"',
                &super::registers::RegValue {
                    text: top.clone(),
                    linewise: self.evil_yank_linewise,
                },
            );
        }
        for (c, v) in self.vim.registers.listing() {
            push(c, &v);
        }
        if let Some(q) = self.search_query.clone() {
            push(
                '/',
                &super::registers::RegValue {
                    text: q,
                    linewise: false,
                },
            );
        }
        let mut macro_regs: Vec<char> = self.vim.macros.keys().copied().collect();
        macro_regs.sort_unstable();
        for c in macro_regs {
            if want(c) {
                let n = self.vim.macros[&c].len();
                lines.push(format!("  m  @{c}   <keyboard macro: {n} commands>"));
            }
        }
        self.show_listing("*registers*", &lines, cx);
    }

    /// Show `lines` in the read-only `help-mode` buffer `name` (the
    /// `:registers` / `:marks` / `:jumps` / `:g…p` listings).
    pub(super) fn show_listing(&mut self, name: &str, lines: &[String], cx: &mut Context<Self>) {
        let text = lines.join("\n") + "\n";
        self.render_buffer(name.into(), "help-mode".into(), text, Vec::new(), cx);
    }

    /// The LSP-style `(line, column)` of `pos` in the active buffer (clamped),
    /// and that line's text, trimmed — a `:marks` / `:jumps` row.
    fn line_text_at(&self, pos: usize) -> (usize, usize, String) {
        let buf = &self.active.buffer;
        let (line, col) = buf.byte_to_position(pos.min(buf.len()));
        let text = buf.slice_to_string(buf.line_range(line));
        (line, col, text.trim().to_string())
    }

    // --- marks & jumps ------------------------------------------------------

    /// Keep marks and jump positions in the active buffer aligned with an
    /// edit of `[start, old_end)` → `new_len` bytes (called from every edit
    /// and undo/redo step).
    pub(super) fn shift_vim_positions(&mut self, start: usize, old_end: usize, new_len: usize) {
        let name = &self.active.name;
        if let Some(m) = self.vim.marks.get_mut(name) {
            for p in m.values_mut() {
                *p = shift_pos(*p, start, old_end, new_len);
            }
        }
        for g in self.vim.global_marks.values_mut() {
            if &g.buffer == name {
                g.pos = shift_pos(g.pos, start, old_end, new_len);
            }
        }
        for j in self.vim.jumps.iter_mut() {
            if &j.buffer == name {
                j.pos = shift_pos(j.pos, start, old_end, new_len);
            }
        }
        for a in self.vim.ex_anchors.iter_mut() {
            if let Some(p) = *a {
                // A removal covering the line start deletes the line.
                *a = if old_end > start && start <= p && p < old_end {
                    None
                } else {
                    Some(shift_pos(p, start, old_end, new_len))
                };
            }
        }
    }

    /// Marks and jump positions in the active buffer inside `range`, as
    /// offsets from its start — so a block that is cut and re-inserted
    /// elsewhere (`:m`) can bring them along via [`Self::restore_vim_positions`].
    pub(super) fn take_vim_positions(&self, range: Range<usize>) -> MovedPositions {
        let name = &self.active.name;
        let inside = |p: usize| range.contains(&p).then(|| p - range.start);
        let mut moved = MovedPositions::default();
        if let Some(m) = self.vim.marks.get(name) {
            for (&c, &p) in m {
                if let Some(off) = inside(p) {
                    moved.local.push((c, off));
                }
            }
        }
        for (&c, g) in &self.vim.global_marks {
            if &g.buffer == name
                && let Some(off) = inside(g.pos)
            {
                moved.global.push((c, off));
            }
        }
        for (i, j) in self.vim.jumps.iter().enumerate() {
            if &j.buffer == name
                && let Some(off) = inside(j.pos)
            {
                moved.jumps.push((i, off));
            }
        }
        moved
    }

    /// Put positions taken by [`Self::take_vim_positions`] back, relative to
    /// the block's new start.
    pub(super) fn restore_vim_positions(&mut self, moved: MovedPositions, start: usize) {
        if let Some(m) = self.vim.marks.get_mut(&self.active.name) {
            for (c, off) in moved.local {
                m.insert(c, start + off);
            }
        }
        for (c, off) in moved.global {
            if let Some(g) = self.vim.global_marks.get_mut(&c) {
                g.pos = start + off;
            }
        }
        for (i, off) in moved.jumps {
            if let Some(j) = self.vim.jumps.get_mut(i) {
                j.pos = start + off;
            }
        }
    }

    /// Set an automatic mark in the active buffer (no-op unless evil is on).
    pub(super) fn set_auto_mark(&mut self, c: char, pos: usize) {
        if !self.vim_active() {
            return;
        }
        // Runs on every edit: avoid cloning the buffer name once its map exists.
        if let Some(m) = self.vim.marks.get_mut(&self.active.name) {
            m.insert(c, pos);
        } else {
            self.vim
                .marks
                .entry(self.active.name.clone())
                .or_default()
                .insert(c, pos);
        }
    }

    /// The active buffer's marks, for the snapshot (`mark-position`): the
    /// local ones plus any file mark that points into this buffer.
    pub(super) fn active_marks(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .vim
            .marks
            .get(&self.active.name)
            .map(|m| m.iter().map(|(c, p)| (c.to_string(), *p)).collect())
            .unwrap_or_default();
        for (c, g) in &self.vim.global_marks {
            if g.buffer == self.active.name {
                out.push((c.to_string(), g.pos));
            }
        }
        out
    }

    /// `m{x}`.
    pub(super) fn evil_set_mark(&mut self, r: &str, cx: &mut Context<Self>) {
        let Some(c) = reg_char(r).filter(|c| is_mark(*c)) else {
            self.echo(format!("Invalid mark: {r}"), cx);
            return;
        };
        let pos = self.point();
        let c = normalize_mark(c);
        if c.is_ascii_uppercase() {
            self.vim.global_marks.insert(
                c,
                GlobalMark {
                    buffer: self.active.name.clone(),
                    path: self.active.path.clone(),
                    pos,
                },
            );
        } else {
            self.vim
                .marks
                .entry(self.active.name.clone())
                .or_default()
                .insert(c, pos);
        }
    }

    /// Show the buffer a jump / file mark points into: switch to it by name,
    /// or revisit its file. A file whose read is still in flight gets
    /// [`Target::Deferred`] — `pos` is applied once it opens; a buffer that
    /// no longer exists (and has no file to revisit) gets [`Target::Gone`].
    fn visit_position_buffer(
        &mut self,
        buffer: &str,
        path: Option<&std::path::PathBuf>,
        pos: usize,
        exact: bool,
        cx: &mut Context<Self>,
    ) -> Target {
        if buffer == self.active.name {
            return Target::Here;
        }
        if self.inactive.iter().any(|d| d.name == buffer) {
            self.switch_to_buffer(buffer, cx);
            return Target::Here;
        }
        // A fileless buffer is identified by its name alone: it is gone.
        let Some(path) = path else {
            return Target::Gone;
        };
        // Already open under another name: find_file switches synchronously.
        self.find_file(&path.to_string_lossy(), cx);
        if self.active.path.as_ref() == Some(path) {
            return Target::Here;
        }
        self.goto_when_visiting(path.clone(), pos, exact, cx);
        Target::Deferred
    }

    /// Land on byte `pos` (`exact` false: that line's first non-blank) of the
    /// file `path` once its buffer — opening asynchronously — is current,
    /// through Scheme's `goto-when-visiting`. That takes a line and column, so
    /// they are read off the file (off the UI thread).
    fn goto_when_visiting(
        &mut self,
        path: std::path::PathBuf,
        pos: usize,
        exact: bool,
        cx: &mut Context<Self>,
    ) {
        let read_path = path.clone();
        let read = cx
            .background_executor()
            .spawn(async move { std::fs::read_to_string(&read_path) });
        cx.spawn(async move |this, cx| {
            let text = read.await.unwrap_or_default();
            let (line0, col) = line_col_at(&text, pos, exact);
            this.update(cx, |editor, _| {
                let expr = format!(
                    "(goto-when-visiting \"\" {} {line0} {col})",
                    steel_quote(&path.to_string_lossy())
                );
                editor.script.dispatch(&expr, false);
            })
            .ok();
        })
        .detach();
    }

    /// Where mark `c` (already normalized) is in the active buffer, without
    /// switching buffers: a local mark, or a file mark pointing here.
    pub(super) fn mark_pos_in_active(&self, c: char) -> Option<usize> {
        if c.is_ascii_uppercase() {
            self.vim
                .global_marks
                .get(&c)
                .filter(|g| g.buffer == self.active.name)
                .map(|g| g.pos)
        } else {
            self.vim
                .marks
                .get(&self.active.name)
                .and_then(|m| m.get(&c))
                .copied()
        }
    }

    /// Where mark `c` is, switching to its buffer first for a file mark in
    /// another buffer. [`MarkErr::Missing`] (with an echo) when unset or
    /// gone; [`MarkErr::Deferred`] when the mark's file is still opening (the
    /// jump then lands once it is open).
    fn resolve_mark(
        &mut self,
        c: char,
        exact: bool,
        cx: &mut Context<Self>,
    ) -> Result<usize, MarkErr> {
        let c = normalize_mark(c);
        if c.is_ascii_uppercase() {
            let Some(g) = self.vim.global_marks.get(&c).cloned() else {
                self.echo(format!("Mark not set: {c}"), cx);
                return Err(MarkErr::Missing);
            };
            if g.buffer != self.active.name {
                match self.visit_position_buffer(&g.buffer, g.path.as_ref(), g.pos, exact, cx) {
                    Target::Here => {}
                    Target::Deferred => return Err(MarkErr::Deferred),
                    Target::Gone => {
                        self.echo(format!("Mark {c}: buffer is gone"), cx);
                        return Err(MarkErr::Missing);
                    }
                }
                // The buffer may have been reopened under a new name.
                if let Some(m) = self.vim.global_marks.get_mut(&c) {
                    m.buffer = self.active.name.clone();
                }
            }
            return Ok(g.pos.min(self.active.buffer.len()));
        }
        match self.mark_pos_in_active(c) {
            Some(p) => Ok(p.min(self.active.buffer.len())),
            None => {
                self.echo(format!("Mark not set: {c}"), cx);
                Err(MarkErr::Missing)
            }
        }
    }

    /// `` `x `` (exact) / `'x` (line, first non-blank): jump to a mark. The
    /// jump is recorded, so `''` comes back.
    pub(super) fn evil_goto_mark(&mut self, r: &str, exact: bool, cx: &mut Context<Self>) {
        let Some(c) = reg_char(r) else {
            return;
        };
        let from_buffer = self.active.name.clone();
        let from = self.point();
        let pos = match self.resolve_mark(c, exact, cx) {
            Ok(pos) => pos,
            Err(MarkErr::Deferred) => {
                // Landing once its file opens: record where we were.
                self.push_jump(Jump {
                    buffer: from_buffer,
                    path: None,
                    pos: from,
                });
                return;
            }
            Err(MarkErr::Missing) => return,
        };
        let target = if exact {
            pos
        } else {
            self.first_non_blank(pos)
        };
        // Record the jump in the buffer we left (it may differ now).
        self.push_jump(Jump {
            buffer: from_buffer.clone(),
            path: None,
            pos: from,
        });
        if from_buffer == self.active.name {
            self.set_auto_mark('\'', from);
        }
        self.goto(target, cx);
        self.evil_clamp_cursor();
    }

    /// Add a jump-list entry (deduplicating the same line) and reset the
    /// C-o cursor to the end, as Vim does.
    pub(super) fn push_jump(&mut self, mut jump: Jump) {
        if jump.path.is_none() {
            jump.path = if jump.buffer == self.active.name {
                self.active.path.clone()
            } else {
                self.inactive
                    .iter()
                    .find(|d| d.name == jump.buffer)
                    .and_then(|d| d.path.clone())
            };
        }
        let line_of = |ed: &Self, j: &Jump| -> Option<usize> {
            (j.buffer == ed.active.name).then(|| ed.active.buffer.line_index_at_byte(j.pos))
        };
        let new_line = line_of(self, &jump);
        let jumps = std::mem::take(&mut self.vim.jumps);
        let mut kept: Vec<Jump> = jumps
            .into_iter()
            .filter(|j| {
                !(j.buffer == jump.buffer && new_line.is_some() && line_of(self, j) == new_line)
            })
            .collect();
        kept.push(jump);
        if kept.len() > 100 {
            kept.remove(0);
        }
        self.vim.jump_idx = kept.len();
        self.vim.jumps = kept;
    }

    /// A jump command is about to run: remember where we were (jump list and
    /// the `''` mark).
    pub(super) fn record_jump(&mut self) {
        if self.evil == EvilState::Disabled {
            return;
        }
        let pos = self.point();
        self.set_auto_mark('\'', pos);
        self.push_jump(Jump {
            buffer: self.active.name.clone(),
            path: None,
            pos,
        });
    }

    /// Whether `expr` runs a registered jump command.
    pub(super) fn is_jump_expr(&self, expr: &str) -> bool {
        self.vim.jump_commands.contains(expr_head(expr))
    }

    /// C-o (`forward` false) / C-i, Tab (`forward` true): walk the jump list.
    /// The first C-o from the newest end records where you are, so C-i can
    /// come back to it.
    pub(super) fn evil_jump(&mut self, forward: bool, cx: &mut Context<Self>) {
        let target = if forward {
            let t = self.vim.jump_idx + 1;
            if t >= self.vim.jumps.len() {
                self.echo("At the newest jump", cx);
                return;
            }
            t
        } else {
            if self.vim.jump_idx >= self.vim.jumps.len() {
                let pos = self.point();
                self.push_jump(Jump {
                    buffer: self.active.name.clone(),
                    path: None,
                    pos,
                });
                // Now the newest entry is where we stand; step past it.
                self.vim.jump_idx = self.vim.jumps.len() - 1;
            }
            if self.vim.jump_idx == 0 {
                self.echo("At the oldest jump", cx);
                return;
            }
            self.vim.jump_idx - 1
        };
        let jump = self.vim.jumps[target].clone();
        match self.visit_position_buffer(&jump.buffer, jump.path.as_ref(), jump.pos, true, cx) {
            Target::Here => {}
            Target::Deferred => {
                // Its file is opening; the position applies once it is open.
                self.vim.jump_idx = target;
                return;
            }
            Target::Gone => {
                // The buffer is gone: drop the entry and keep walking.
                self.vim.jumps.remove(target);
                if !forward {
                    self.vim.jump_idx = target;
                }
                self.echo("Jump target buffer is gone", cx);
                return;
            }
        }
        self.vim.jump_idx = target;
        let pos = jump.pos.min(self.active.buffer.len());
        self.move_to(pos, cx);
        self.evil_clamp_cursor();
    }

    /// `:jumps`.
    pub(super) fn show_jumps(&mut self, cx: &mut Context<Self>) {
        let mut lines = vec![" jump line  col file/text".to_string()];
        let cur = self.vim.jump_idx;
        for (i, j) in self.vim.jumps.iter().enumerate() {
            let dist = i.abs_diff(cur);
            let (line, col, what) = if j.buffer == self.active.name {
                let (l, c, text) = self.line_text_at(j.pos);
                (l + 1, c, text)
            } else {
                (0, 0, j.buffer.clone())
            };
            lines.push(format!("{dist:>5} {line:>5} {col:>4} {what}"));
        }
        if cur >= self.vim.jumps.len() {
            lines.push(">".into());
        }
        self.show_listing("*jumps*", &lines, cx);
    }

    /// `:marks [names]`.
    pub(super) fn show_marks(&mut self, only: &str, cx: &mut Context<Self>) {
        let want = |c: char| only.is_empty() || only.contains(c);
        let mut rows: Vec<(char, String, usize, usize, String)> = Vec::new();
        if let Some(m) = self.vim.marks.get(&self.active.name) {
            for (&c, &p) in m {
                let (line, col, text) = self.line_text_at(p);
                rows.push((c, String::new(), line + 1, col, text));
            }
        }
        for (&c, g) in &self.vim.global_marks {
            let file = g
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| g.buffer.clone());
            let (line, col) = if g.buffer == self.active.name {
                self.active
                    .buffer
                    .byte_to_position(g.pos.min(self.active.buffer.len()))
            } else {
                self.inactive
                    .iter()
                    .find(|d| d.name == g.buffer)
                    .map(|d| d.buffer.byte_to_position(g.pos.min(d.buffer.len())))
                    .unwrap_or((0, 0))
            };
            rows.push((c, file, line + 1, col, String::new()));
        }
        rows.retain(|r| want(r.0));
        rows.sort_by_key(|r| (r.0.is_ascii_alphabetic(), r.0.is_ascii_uppercase(), r.0));
        let mut lines = vec!["mark line  col file/text".to_string()];
        for (c, file, line, col, text) in rows {
            let what = if file.is_empty() { text } else { file };
            lines.push(format!(" {c} {line:>6} {col:>4} {what}"));
        }
        self.show_listing("*marks*", &lines, cx);
    }

    /// `:delmarks a b` / `:delm!` (all lowercase marks of this buffer).
    pub(super) fn delete_marks(&mut self, arg: &str, bang: bool, cx: &mut Context<Self>) {
        if bang {
            if let Some(m) = self.vim.marks.get_mut(&self.active.name) {
                m.retain(|c, _| !c.is_ascii_lowercase());
            }
            return;
        }
        let mut names: Vec<char> = Vec::new();
        let chars: Vec<char> = arg.chars().filter(|c| !c.is_whitespace()).collect();
        let mut i = 0;
        while i < chars.len() {
            if i + 2 < chars.len() && chars[i + 1] == '-' {
                for c in chars[i]..=chars[i + 2] {
                    names.push(c);
                }
                i += 3;
            } else {
                names.push(chars[i]);
                i += 1;
            }
        }
        if names.is_empty() {
            self.echo("Argument required", cx);
            return;
        }
        for c in names {
            if c.is_ascii_uppercase() {
                self.vim.global_marks.remove(&c);
            } else if let Some(m) = self.vim.marks.get_mut(&self.active.name) {
                m.remove(&c);
            }
        }
    }

    /// `gv`: reselect the last Visual area (from the `<` `>` marks).
    pub(super) fn evil_reselect_visual(&mut self, cx: &mut Context<Self>) {
        let marks = self.vim.marks.get(&self.active.name);
        let (Some(&s), Some(&e)) = (
            marks.and_then(|m| m.get(&'<')),
            marks.and_then(|m| m.get(&'>')),
        ) else {
            self.echo("No previous visual selection", cx);
            return;
        };
        let len = self.active.buffer.len();
        let (s, e) = (s.min(len), e.min(len));
        self.evil = self.vim.last_visual.unwrap_or(EvilState::Visual);
        self.active.mark_active = true;
        self.active.selected_range = s..e;
        self.active.selection_reversed = false;
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// In a Visual state: remember the area for `'<` `'>` and `gv`.
    pub(super) fn note_visual_exit(&mut self) {
        if !matches!(
            self.evil,
            EvilState::Visual | EvilState::VisualLine | EvilState::VisualBlock
        ) || self.modal_helix
        {
            return;
        }
        let r = self.active.selected_range.clone();
        self.vim.last_visual = Some(self.evil);
        self.set_auto_mark('<', r.start);
        self.set_auto_mark('>', r.end);
    }

    // --- macros ------------------------------------------------------------

    /// `qx`: record a keyboard macro into register x (`qA` appends to a).
    pub(super) fn evil_macro_record(&mut self, r: &str, cx: &mut Context<Self>) {
        let Some(c) = reg_char(r).filter(|c| c.is_ascii_alphanumeric() || *c == '"') else {
            self.echo(format!("Invalid macro register: {r}"), cx);
            return;
        };
        self.start_kbd_macro(cx);
        self.vim.macro_register = Some(c);
        self.echo(format!("recording @{c}"), cx);
    }

    /// `@x` (count-aware): replay the macro in register x; `@@` repeats the
    /// last one played, `@:` the last ex command.
    pub(super) fn evil_macro_play(&mut self, r: &str, cx: &mut Context<Self>) {
        let n = self.prefix_count.max(1);
        let Some(mut c) = reg_char(r) else {
            return;
        };
        if c == ':' {
            let ex = self.vim.last_ex.clone();
            if ex.is_empty() {
                self.echo("No previous command line", cx);
            }
            for _ in 0..n {
                if ex.is_empty() {
                    break;
                }
                self.run_ex_command(&ex, cx);
            }
            return;
        }
        if c == '@' {
            match self.vim.last_macro_register {
                Some(last) => c = last,
                None => {
                    // No register macro yet: the anonymous `C-x (` one.
                    let events = self.last_macro.clone();
                    let all: Vec<MacroEvent> = (0..n).flat_map(|_| events.clone()).collect();
                    self.replay_events(all, false, cx);
                    return;
                }
            }
        }
        let lower = c.to_ascii_lowercase();
        let Some(events) = self.vim.macros.get(&lower).cloned() else {
            self.echo(format!("Register {lower} holds no macro"), cx);
            return;
        };
        self.vim.last_macro_register = Some(lower);
        let all: Vec<MacroEvent> = (0..n).flat_map(|_| events.clone()).collect();
        self.replay_events(all, false, cx);
    }

    /// Drop the event just recorded (a `read-char` arming command — the
    /// resolved callback is recorded in its place).
    pub(super) fn unrecord_last_event(&mut self) {
        if self.vim.replay.is_some() {
            return;
        }
        if let Some(events) = self.macro_recording.as_mut() {
            events.pop();
        }
        if let Some(dot) = self.vim.dot.as_mut() {
            dot.events.pop();
        }
    }

    /// The edit tick of the buffer named `name` (active or not).
    fn buffer_tick_of(&self, name: &str) -> Option<u64> {
        if self.active.name == name {
            return Some(self.active.buffer.tick());
        }
        self.inactive
            .iter()
            .find(|d| d.name == name)
            .map(|d| d.buffer.tick())
    }

    /// Close the current `.` recording: keep it as the last change when its
    /// buffer was edited.
    fn dot_finalize(&mut self) {
        let Some(rec) = self.vim.dot.take() else {
            return;
        };
        if rec.unrepeatable || rec.events.is_empty() {
            return;
        }
        let changed = self
            .buffer_tick_of(&rec.buffer)
            .is_some_and(|t| t != rec.tick);
        let neutral = rec.events.iter().any(|e| match e {
            MacroEvent::Command(x) | MacroEvent::Counted(x, _) => is_repeat_neutral(x),
            MacroEvent::Text(_) | MacroEvent::Key(_) => false,
        });
        if changed && !neutral {
            self.vim.last_change = rec.events;
        }
    }

    /// A Normal-state command is starting (its key just resolved to `expr`):
    /// finish recording the previous one and start recording this one.
    pub(super) fn dot_boundary(&mut self, expr: &str) {
        if self.vim.replay.is_some() {
            return;
        }
        let register_prefix = is_register_prefix(expr);
        if let Some(rec) = self.vim.dot.as_mut()
            && rec.prefix_only
        {
            // `"a` then `dd`: one command.
            rec.prefix_only = register_prefix;
            return;
        }
        self.dot_finalize();
        self.vim.cmd_undo_base = Some(self.undo_base());
        if expr.trim() == "(evil-repeat)" {
            return; // `.` itself is never recorded
        }
        self.vim.dot = Some(DotRec::new(self, register_prefix, false));
    }

    /// The current command can't be repeated by replaying its keys (a mouse
    /// selection made it).
    pub(super) fn dot_unrepeatable(&mut self) {
        self.dot_finalize();
        self.vim.dot = Some(DotRec::new(self, false, true));
    }

    /// Escape out of Insert: the whole session (and the command that opened
    /// it) becomes one undo step.
    pub(super) fn squash_insert_session(&mut self) {
        if self.vim.replay.is_some() {
            return;
        }
        if let Some(base) = self.vim.cmd_undo_base.take() {
            self.squash_undo_since(&base);
        }
    }

    /// Vim `.`: replay the last change, with `3.` overriding its count. The
    /// replay is one undo step.
    pub(super) fn evil_repeat(&mut self, cx: &mut Context<Self>) {
        // A `.` typed before the next command boundary closes the change
        // being recorded first, so it repeats what was just done.
        self.dot_finalize();
        let mut events = self.vim.last_change.clone();
        if events.is_empty() {
            self.echo("No previous change to repeat", cx);
            return;
        }
        if self.prefix_count > 0 {
            override_count(&mut events, self.prefix_count);
            self.vim.last_change = events.clone();
        }
        self.replay_events(events, true, cx);
    }

    /// Replay recorded events (a macro, `.`). Nested replays (`.` inside a
    /// macro, `@b` inside `@a`) splice their events in front of the rest.
    pub(super) fn replay_events(
        &mut self,
        events: Vec<MacroEvent>,
        squash: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(r) = self.vim.replay.as_mut() {
            for ev in events.into_iter().rev() {
                r.queue.push_front(ev);
            }
            return;
        }
        self.vim.replay = Some(Replay {
            queue: events.into(),
            waiting: false,
            undo_base: squash.then(|| self.undo_base()),
        });
        self.replay_pump(cx);
    }

    /// `(replay-step)`: the async command the replay was waiting on applied.
    pub(super) fn replay_step(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.vim.replay.as_mut() {
            r.waiting = false;
        }
        self.replay_pump(cx);
    }

    fn replay_pump(&mut self, cx: &mut Context<Self>) {
        loop {
            let Some(r) = self.vim.replay.as_mut() else {
                return;
            };
            if r.waiting {
                return;
            }
            let Some(ev) = r.queue.pop_front() else {
                let r = self.vim.replay.take().expect("replay");
                if let Some(base) = r.undo_base {
                    self.squash_undo_since(&base);
                }
                return;
            };
            self.vim.went_async = false;
            match ev {
                MacroEvent::Text(text) => self.insert_text(&text, cx),
                MacroEvent::Key(tok) => self.replay_key(&tok, cx),
                MacroEvent::Command(expr) => self.run_script(&expr, cx),
                MacroEvent::Counted(expr, n) => {
                    if self.count_aware.contains(expr_head(&expr))
                        || self.script.builtins_for(&expr).is_some()
                    {
                        self.run_counted(&expr, n, cx);
                    } else {
                        // A chained repeat would finish asynchronously in
                        // several steps; replay it as N single runs instead.
                        if let Some(r) = self.vim.replay.as_mut() {
                            for _ in 0..n.min(10_000) {
                                r.queue.push_front(MacroEvent::Command(expr.clone()));
                            }
                        }
                        continue;
                    }
                }
            }
            if std::mem::take(&mut self.vim.went_async)
                && let Some(r) = self.vim.replay.as_mut()
            {
                r.waiting = true;
                // FIFO engine: this runs after the command just dispatched,
                // and its outcome applies after that command's.
                self.script.dispatch("(replay-step)", false);
            }
        }
    }

    /// Route a key-resolved evil binding that needs more input before it can
    /// run: a `%c` argument capture, or an operator (`evil-operator`) that now
    /// waits for its motion. Returns true when it took the binding (nothing
    /// should be dispatched yet).
    pub(super) fn evil_route_binding(
        &mut self,
        expr: &str,
        keys: &str,
        count: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        if expr.contains(CHAR_ARG) {
            self.vim.capture = Some(Capture {
                expr: expr.to_string(),
                count,
                op: None,
            });
            let n = count.map(|n| n.to_string()).unwrap_or_default();
            self.status = format!("{n}{}-", pretty_sequence(keys)).into();
            cx.notify();
            return true;
        }
        let Some(op) = parse_operator_expr(expr) else {
            return false;
        };
        if self.evil == EvilState::Normal {
            self.vim.op = Some(OpPending {
                op,
                keys: keys.to_string(),
                count,
                force: None,
            });
            self.echo_operator_pending(cx);
        } else {
            // Visual states: the operator applies to the selection right away.
            let expr = format!("(evil-visual-operator {})", steel_quote(&op));
            self.dispatch_counted(&expr, count, cx);
        }
        true
    }

    fn echo_operator_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(op) = &self.vim.op {
            let n = op.count.map(|n| n.to_string()).unwrap_or_default();
            let m = self.modal_count.map(|n| n.to_string()).unwrap_or_default();
            let force = op.force.map_or("", Force::key);
            let pend = if self.pending_keys.is_empty() {
                String::new()
            } else {
                format!("{} ", pretty_sequence(&self.pending_keys.join(" ")))
            };
            self.status = format!("{n}{}{force}{m}{pend}-", pretty_sequence(&op.keys)).into();
        }
        cx.notify();
    }

    /// Abandon a pending operator / capture (Escape, an unbound key).
    pub(super) fn evil_cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.vim.op = None;
        self.vim.capture = None;
        self.pending_keys.clear();
        self.modal_count = None;
        self.status = SharedString::default();
        cx.notify();
    }

    /// Run an operator over a resolved motion expression.
    pub(super) fn evil_run_operator(
        &mut self,
        op: OpPending,
        motion_count: Option<usize>,
        motion: &str,
        cx: &mut Context<Self>,
    ) {
        let expr = compose_operate(&op, motion_count, motion);
        self.status = SharedString::default();
        self.dispatch_recorded(&expr, cx);
    }

    /// Operator-pending keys: counts, `v`/`V` forcing, the doubled operator
    /// (`dd`), and the motion / text-object keymap. Anything unbound cancels
    /// the operator, as in Vim.
    pub(super) fn key_operator_pending(
        &mut self,
        ks: &Keystroke,
        cx: &mut Context<Self>,
    ) -> Handled {
        let chord = chord_string(ks);
        cx.stop_propagation();
        if chord == "escape" || chord == "ctrl-g" || chord == "ctrl-c" {
            self.evil_cancel_pending(cx);
            return Handled::Yes;
        }
        let plain = !ks.modifiers.control && !ks.modifiers.alt;
        if self.pending_keys.is_empty()
            && plain
            && let Some(d) = Self::ascii_digit(&chord)
            && (d != 0 || self.modal_count.is_some())
        {
            let n = self.modal_count.unwrap_or(0).saturating_mul(10) + d as usize;
            self.modal_count = Some(n.min(10_000));
            self.echo_operator_pending(cx);
            return Handled::Yes;
        }
        let Some(mut op) = self.vim.op.take() else {
            return Handled::No;
        };
        if self.pending_keys.is_empty() && (chord == "v" || chord == "shift-v") {
            op.force = Some(if chord == "v" {
                Force::Char
            } else {
                Force::Line
            });
            self.vim.op = Some(op);
            self.echo_operator_pending(cx);
            return Handled::Yes;
        }
        let candidate = if self.pending_keys.is_empty() {
            chord.clone()
        } else {
            format!("{} {}", self.pending_keys.join(" "), chord)
        };
        // The doubled operator: `dd`, `>>`, `gUgU`, and `gUU` (its last key).
        let last_key = op.keys.rsplit(' ').next().unwrap_or("");
        if candidate == op.keys || (self.pending_keys.is_empty() && chord == last_key) {
            self.pending_keys.clear();
            let count = self.modal_count.take();
            self.evil_run_operator(op, count, "(evil-motion \"line\")", cx);
            return Handled::Yes;
        }
        let major: Option<String> = self.active.modes.names().last().map(|s| s.to_string());
        if let Some(expr) = self
            .keymap
            .resolve_evil(OPERATOR_KEYMAP, major.as_deref(), &candidate)
            .map(str::to_string)
        {
            self.pending_keys.clear();
            let count = self.modal_count.take();
            if expr.contains(CHAR_ARG) {
                self.vim.capture = Some(Capture {
                    expr,
                    count,
                    op: Some(op),
                });
                self.status = format!("{}-", pretty_sequence(&candidate)).into();
                cx.notify();
            } else if let Some(forward) = search_prompt_expr(&expr) {
                // `d/foo<RET>`: read the pattern, then run the operator.
                self.vim.op = Some(op);
                self.vim.search_count = count;
                let prompt = if forward { "/" } else { "?" };
                self.open_minibuffer(
                    prompt,
                    MinibufferPurpose::OperatorSearch(Direction::from_forward(forward)),
                    "",
                    cx,
                );
            } else {
                self.evil_run_operator(op, count, &expr, cx);
            }
            return Handled::Yes;
        }
        if self
            .keymap
            .is_prefix_evil(OPERATOR_KEYMAP, major.as_deref(), &candidate)
        {
            self.pending_keys.push(chord);
            self.vim.op = Some(op);
            self.echo_operator_pending(cx);
            return Handled::Yes;
        }
        // Not a motion: the whole command is aborted.
        self.evil_cancel_pending(cx);
        Handled::Yes
    }

    /// The one-keystroke argument of a `%c` binding (`ma`, `"a`, `dfx`). A
    /// printable key fills it in; anything else cancels.
    pub(super) fn key_evil_capture(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        let Some(capture) = self.vim.capture.take() else {
            return Handled::No;
        };
        cx.stop_propagation();
        self.status = SharedString::default();
        let is_text = ks.key_char.is_some()
            && !ks.modifiers.control
            && !ks.modifiers.alt
            && !ks.modifiers.platform;
        let ch = match ks.key_char.clone() {
            Some(ch) if is_text => ch,
            _ => {
                self.evil_cancel_pending(cx);
                return Handled::Yes;
            }
        };
        let expr = fill_char_arg(&capture.expr, &ch);
        match capture.op {
            Some(op) => self.evil_run_operator(op, capture.count, &expr, cx),
            None => {
                if !self.evil_route_binding(&expr, "", capture.count, cx) {
                    self.dispatch_counted(&expr, capture.count, cx);
                }
            }
        }
        cx.notify();
        Handled::Yes
    }

    /// `d/pat<RET>` / `d?pat<RET>`: the minibuffer submitted the pattern.
    pub(super) fn evil_operator_search_submit(
        &mut self,
        forward: bool,
        input: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(op) = self.vim.op.take() else {
            return;
        };
        let count = self.vim.search_count.take();
        let pattern = if input.is_empty() {
            self.search_query.clone().unwrap_or_default()
        } else {
            input.to_string()
        };
        if pattern.is_empty() {
            return;
        }
        let motion = format!(
            "(evil-motion-search {} {})",
            steel_quote(&pattern),
            if forward { "#t" } else { "#f" }
        );
        self.evil_run_operator(op, count, &motion, cx);
    }
}

/// `(evil-op-search "/")` / `(evil-op-search "?")` — the search motions, which
/// need the minibuffer before the operator can run.
fn search_prompt_expr(expr: &str) -> Option<bool> {
    let (name, args) = parse_string_call(expr)?;
    if name != "evil-op-search" || args.len() != 1 {
        return None;
    }
    Some(args[0] != "?")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_calls() {
        assert_eq!(
            parse_string_call(r#"(evil-operator "delete")"#),
            Some(("evil-operator".into(), vec!["delete".into()]))
        );
        assert_eq!(
            parse_string_call(r#"(f "a\"b" "c")"#),
            Some(("f".into(), vec!["a\"b".into(), "c".into()]))
        );
        assert_eq!(parse_string_call("(f 1)"), None);
        assert_eq!(parse_string_call("(f (g))"), None);
        assert_eq!(
            parse_string_call("(evil-repeat)"),
            Some(("evil-repeat".into(), vec![]))
        );
        assert_eq!(
            parse_operator_expr(r#"(evil-operator "upcase")"#).as_deref(),
            Some("upcase")
        );
        assert_eq!(parse_operator_expr("(evil-delete-line)"), None);
    }

    #[test]
    fn composes_operator_calls() {
        let op = OpPending {
            op: "delete".into(),
            keys: "d".into(),
            count: Some(2),
            force: None,
        };
        assert_eq!(
            compose_operate(&op, Some(3), r#"(evil-motion "w")"#),
            r#"(evil-operate "delete" 6 "" (lambda () (evil-motion "w")))"#
        );
        let op = OpPending {
            count: None,
            force: Some(Force::Line),
            ..op
        };
        assert_eq!(
            compose_operate(&op, None, "evil-motion \"j\""),
            r#"(evil-operate "delete" 0 "V" (lambda () (evil-motion "j")))"#
        );
        assert_eq!(combined_count(None, Some(4)), 4);
        assert_eq!(
            fill_char_arg("(evil-motion-find \"f\" %c)", "\""),
            r#"(evil-motion-find "f" "\"")"#
        );
        assert_eq!(search_prompt_expr(r#"(evil-op-search "?")"#), Some(false));
    }

    #[test]
    fn dot_count_override_and_neutral_commands() {
        let mut ev = vec![
            MacroEvent::Command(
                r#"(evil-operate "delete" 6 "" (lambda () (evil-motion "w")))"#.into(),
            ),
            MacroEvent::Text("x".into()),
        ];
        override_count(&mut ev, 3);
        assert!(matches!(&ev[0], MacroEvent::Command(e)
            if e == r#"(evil-operate "delete" 3 "" (lambda () (evil-motion "w")))"#));
        let mut ev = vec![MacroEvent::Command("(evil-delete-char)".into())];
        override_count(&mut ev, 4);
        assert!(matches!(&ev[0], MacroEvent::Counted(e, 4) if e == "(evil-delete-char)"));
        let mut ev = vec![MacroEvent::Counted("(evil-join-line)".into(), 2)];
        override_count(&mut ev, 5);
        assert!(matches!(&ev[0], MacroEvent::Counted(_, 5)));
        // The register prefix keeps its place; the operator after it is recounted.
        let mut ev = vec![
            MacroEvent::Command(r#"(evil-use-register "a")"#.into()),
            MacroEvent::Counted(r#"(evil-operate "yank" 12 "V" (lambda () 1))"#.into(), 1),
        ];
        override_count(&mut ev, 7);
        assert!(matches!(&ev[1], MacroEvent::Command(e)
            if e == r#"(evil-operate "yank" 7 "V" (lambda () 1))"#));
        assert_eq!(expr_head(r#"(evil-goto-mark "a")"#), "evil-goto-mark");
        assert_eq!(expr_head("(undo)"), "undo");
        assert_eq!(expr_head(" redo "), "redo");
        assert!(is_repeat_neutral("(undo)"));
        assert!(is_repeat_neutral("(evil-repeat)"));
        assert!(!is_repeat_neutral("(evil-delete-char)"));
    }

    #[test]
    fn native_register_and_macro_commands() {
        assert!(matches!(
            native_command(r#"(evil-use-register "a")"#),
            Some(EditorCommand::EvilUseRegister(r)) if r == "a"
        ));
        assert!(matches!(
            native_command(r#"(evil-macro-play "@")"#),
            Some(EditorCommand::EvilMacroPlay(r)) if r == "@"
        ));
        assert!(native_command("(evil-repeat)").is_none());
        assert!(native_command(r#"(evil-operate "delete")"#).is_none());
        assert_eq!(reg_char("a"), Some('a'));
        assert_eq!(reg_char("ab"), None);
    }

    #[test]
    fn mark_positions_shift_with_edits() {
        // insert 3 bytes at 5: later marks shift, earlier stay.
        assert_eq!(shift_pos(10, 5, 5, 3), 13);
        assert_eq!(shift_pos(2, 5, 5, 3), 2);
        assert_eq!(shift_pos(5, 5, 5, 3), 8);
        // delete [4, 8): a mark inside collapses to 4; after shifts back.
        assert_eq!(shift_pos(6, 4, 8, 0), 4);
        assert_eq!(shift_pos(9, 4, 8, 0), 5);
        assert!(is_mark('a') && is_mark('Z') && is_mark('\'') && is_mark('<'));
        assert!(!is_mark('1'));
        assert!(matches!(
            native_command(r#"(evil-goto-mark-line "a")"#),
            Some(EditorCommand::EvilGotoMark(m, false)) if m == "a"
        ));
    }
}
