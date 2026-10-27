//! Evil / Helix primitives: registers, macros, marks, ex commands, counts,
//! jumps and the search query.

use super::*;
use steel::rvals::SteelVal;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared {
        queue, snapshot, ..
    } = ctx;
    reg.section(Section::Modal);
    {
        // `(set-count-aware! 'name)` — the modal count loop dispatches `name`
        // ONCE with `(prefix-count)` published instead of running it N times.
        // Accepts a symbol or a string (symbol-name references, like keymaps).
        register_unary(reg, queue, "set-count-aware!", |Name(name)| {
            EditorCommand::SetCountAware(name)
        });
    }
    register_unary(
        reg,
        queue,
        "evil-block-replace",
        EditorCommand::EvilBlockReplace,
    );
    register_unary(
        reg,
        queue,
        "evil-use-register",
        EditorCommand::EvilUseRegister,
    );
    register_unary(
        reg,
        queue,
        "evil-insert-register",
        EditorCommand::EvilInsertRegister,
    );
    register_unary(
        reg,
        queue,
        "evil-macro-record",
        EditorCommand::EvilMacroRecord,
    );
    register_unary(reg, queue, "evil-macro-play", EditorCommand::EvilMacroPlay);
    register_unary(reg, queue, "evil-set-mark", EditorCommand::EvilSetMark);
    register_unary(reg, queue, "evil-ex", EditorCommand::EvilEx);
    register_unary(
        reg,
        queue,
        "evil-replace-with",
        EditorCommand::EvilReplaceWith,
    );
    register_unary(
        reg,
        queue,
        "evil-ex-goto-line",
        EditorCommand::EvilExGotoLine,
    );
    register_unary(reg, queue, "evil-goto-mark", |m: String| {
        EditorCommand::EvilGotoMark(m, true)
    });
    register_unary(reg, queue, "evil-goto-mark-line", |m: String| {
        EditorCommand::EvilGotoMark(m, false)
    });
    {
        // `(set-jump-command! 'name)` — running `name` from a key is a jump:
        // the jump list and `''` record where it started (G, /, %, gd, …).
        register_unary(reg, queue, "set-jump-command!", |Name(name)| {
            EditorCommand::SetJumpCommand(name)
        });
    }
    {
        // `(mark-position "a")` → byte offset of mark a in this buffer, or -1.
        let shared = snapshot.clone();
        reg.register_fn("mark-position", move |m: String| -> isize {
            shared
                .load()
                .marks
                .iter()
                .find(|(c, _)| *c == m)
                .map_or(-1, |(_, p)| *p as isize)
        });
    }
    // `(select-to pos)` — extend the selection to byte `pos` (goto-char
    // collapses; this is its extend-flavored twin for visual-mode motions).
    register_unary(reg, queue, "select-to", |n: isize| {
        EditorCommand::SelectTo(scheme_pos(n))
    });
    // `(set-search-query "regex" forward?)` — store the query + direction the
    // way `/` and `?` do, without prompting, so evil-search-next/-previous
    // repeat it (evil `*`/`#`).
    register_binary(reg, queue, "set-search-query", |query, forward| {
        EditorCommand::SetSearchQuery { query, forward }
    });
    // `(evil-enabled?)` → #t when modal (evil) editing is active.
    register_snapshot_read(reg, snapshot, "evil-enabled?", |s| s.evil_enabled);
    // `(evil-state)` → "normal" / "insert" / "visual" / "visual-line" /
    // "visual-block", or "" when modal editing is off.
    register_snapshot_read(reg, snapshot, "evil-state", |s| s.evil_state.clone());
    // `(search-query)` → the active `/` regex ("" if none); `(search-forward?)`
    // → whether it was a forward (`/`) search. For the `n`/`N` motions.
    register_snapshot_read(reg, snapshot, "search-query", |s| s.search_query.clone());
    register_snapshot_read(reg, snapshot, "search-forward?", |s| s.search_forward);
    // `(major-mode)` → the buffer's major mode name ("rust", "org-mode", …).
    register_snapshot_read(reg, snapshot, "major-mode", |s| s.major_mode.clone());
    // `(helix-enabled?)` → #t when Helix (selection-first) modal editing is on.
    register_snapshot_read(reg, snapshot, "helix-enabled?", |s| s.helix_enabled);
    // `(prefix-count)` → the count typed before a count-aware modal command, 0
    // when none (so a command can tell `G` from `1G`).
    register_snapshot_read(reg, snapshot, "prefix-count", |s| s.prefix_count);
    // `(current-prefix-arg)` → the universal argument the command was invoked
    // with (Emacs's numeric value: `C-u` → 4, `C-u C-u` → 16, `C-u 7` → 7), or
    // #f without one. Only the command's FIRST queued effect consumes it, so
    // a command reading it should start with a non-repeating one.
    register_snapshot_read(reg, snapshot, "current-prefix-arg", |s| {
        s.prefix_arg
            .map_or(SteelVal::BoolV(false), |n| SteelVal::IntV(n as isize))
    });
    // `(rectangle-mark-mode?)` → #t while `rectangle-mark-mode` (`C-x SPC`) is
    // on: the region is a rectangle.
    register_snapshot_read(reg, snapshot, "rectangle-mark-mode?", |s| s.rect_mark);
    // `(kbd-macro-recording?)` → #t while a keyboard macro is being recorded.
    register_snapshot_read(reg, snapshot, "kbd-macro-recording?", |s| {
        s.kbd_macro_recording
    });
}
