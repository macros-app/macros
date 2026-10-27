//! Key dispatch and command execution tests: prefix sequences, the universal
//! argument, the startup key gate and `M-x`, through a real window the way a
//! typed key travels (see `key_tests.rs`), plus [`Editor::apply_command`]'s
//! repeat / undo-group / yank bookkeeping called directly.

use gpui::{Entity, Keystroke, TestAppContext, VisualTestContext};

use super::mcp::parse_keys;
use super::*;

/// A windowed editor with Emacs keys and a plain-text buffer holding `text`,
/// point at its start.
fn setup<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    test_context(cx);
    cx.update(|cx| {
        cx.bind_keys(super::key_bindings());
    });
    let (editor, vcx) = cx.add_window_view(|_, cx| Editor::new(cx));
    editor.update(vcx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        e.mark_config_loaded(cx);
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("test.txt".into(), None, text));
    });
    let focus = editor.read_with(vcx, |e, _| e.focus_handle.clone());
    vcx.update(|window, cx| window.focus(&focus, cx));
    settle(&editor, vcx);
    (editor, vcx)
}

fn settle(editor: &Entity<Editor>, vcx: &mut VisualTestContext) {
    for _ in 0..8 {
        vcx.run_until_parked();
        editor.read_with(vcx, |e, _| {
            let _ = e.script.eval("0");
        });
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
    }
}

/// Press `seq` (`press_keys` notation), settling after each key.
fn press(editor: &Entity<Editor>, vcx: &mut VisualTestContext, seq: &str) {
    for ks in parse_keys(seq).expect("valid keys") {
        vcx.update(|window, cx| {
            window.dispatch_keystroke(ks, cx);
        });
        settle(editor, vcx);
    }
}

fn text(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> String {
    editor.read_with(vcx, |e, _| e.active.buffer.full_text())
}

fn point(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> usize {
    editor.read_with(vcx, |e, _| e.point())
}

fn status(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> String {
    editor.read_with(vcx, |e, _| e.status.to_string())
}

fn pending(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> String {
    editor.read_with(vcx, |e, _| e.key_state().pending().to_string())
}

const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz";

// --- helpers ---------------------------------------------------------------------

#[test]
fn ascii_digit_takes_exactly_one_ascii_digit() {
    assert_eq!(Editor::ascii_digit("0"), Some(0));
    assert_eq!(Editor::ascii_digit("7"), Some(7));
    assert_eq!(Editor::ascii_digit("12"), None);
    assert_eq!(Editor::ascii_digit(""), None);
    assert_eq!(Editor::ascii_digit("a"), None);
    // Other scripts' digits aren't a prefix argument.
    assert_eq!(Editor::ascii_digit("٣"), None);
}

#[test]
fn chord_string_orders_modifiers_for_keymap_lookup() {
    let chord = |s: &str| chord_string(&Keystroke::parse(s).unwrap());
    assert_eq!(chord("ctrl-x"), "ctrl-x");
    // However the keystroke was written, modifiers come out in one order.
    assert_eq!(chord("cmd-shift-alt-ctrl-k"), "ctrl-alt-shift-cmd-k");
    assert_eq!(chord("alt-ctrl-backspace"), "ctrl-alt-backspace");
    assert_eq!(chord("f5"), "f5");
}

// --- prefix sequences --------------------------------------------------------------

#[gpui::test]
fn prefix_waits_echoes_and_resolves(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one two\n");
    ed.update(vcx, |e, cx| {
        e.run_script_blocking(
            "(define-key \"fundamental\" \"ctrl-c ctrl-z ctrl-z\" 'end-of-buffer)",
            cx,
        )
    });
    press(&ed, vcx, "C-c");
    assert_eq!(pending(&ed, vcx), "ctrl-c");
    assert_eq!(status(&ed, vcx), "C-c-");
    press(&ed, vcx, "C-z");
    assert_eq!(pending(&ed, vcx), "ctrl-c ctrl-z");
    assert_eq!(status(&ed, vcx), "C-c C-z-");
    // Typed text mid-sequence isn't inserted.
    assert_eq!(text(&ed, vcx), "one two\n");
    press(&ed, vcx, "C-z");
    assert_eq!(pending(&ed, vcx), "");
    assert_eq!(status(&ed, vcx), "", "the prefix echo is cleared");
    assert_eq!(point(&ed, vcx), 8);
}

#[gpui::test]
fn undefined_sequence_is_reported_and_dropped(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    press(&ed, vcx, "C-x C-z");
    assert_eq!(pending(&ed, vcx), "");
    assert_eq!(status(&ed, vcx), "C-x C-z is undefined");
    // The next key starts afresh.
    press(&ed, vcx, "C-f");
    assert_eq!(point(&ed, vcx), 1);
    assert_eq!(text(&ed, vcx), "abc\n");
}

#[gpui::test]
fn plain_key_mid_sequence_completes_it(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    // `C-x h`-style bindings take a plain second key; a plain key with no
    // binding after the prefix is undefined rather than typed.
    press(&ed, vcx, "C-x q");
    assert_eq!(status(&ed, vcx), "C-x q is undefined");
    assert_eq!(text(&ed, vcx), "abc\n");
    press(&ed, vcx, "\"z\"");
    assert_eq!(text(&ed, vcx), "zabc\n", "plain keys type again afterwards");
}

#[gpui::test]
fn unbound_platform_chord_is_left_to_the_os(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    let before = status(&ed, vcx);
    press(&ed, vcx, "cmd-j");
    // Neither a pending sequence nor an "undefined" complaint.
    assert_eq!(pending(&ed, vcx), "");
    assert_eq!(status(&ed, vcx), before);
    assert_eq!(text(&ed, vcx), "abc\n");
}

#[gpui::test]
fn config_pending_swallows_plain_keys_not_chords(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    ed.update(vcx, |e, _| e.config_pending = true);
    press(&ed, vcx, "\"xy\"");
    assert_eq!(text(&ed, vcx), "abc\n");
    press(&ed, vcx, "C-f");
    assert_eq!(point(&ed, vcx), 1);
    ed.update(vcx, |e, _| e.config_pending = false);
    press(&ed, vcx, "\"x\"");
    assert_eq!(text(&ed, vcx), "axbc\n");
}

// --- universal argument ------------------------------------------------------------

#[gpui::test]
fn universal_argument_echoes_while_reading(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, ALPHABET);
    press(&ed, vcx, "C-u");
    assert_eq!(status(&ed, vcx), "C-u 4");
    press(&ed, vcx, "C-u");
    assert_eq!(status(&ed, vcx), "C-u 16");
    // A typed number replaces the C-u default.
    press(&ed, vcx, "1 2");
    assert_eq!(status(&ed, vcx), "C-u 12");
    // The digits were read, not typed.
    assert_eq!(text(&ed, vcx), ALPHABET);
    press(&ed, vcx, "C-f");
    assert_eq!(point(&ed, vcx), 12);
    assert!(ed.read_with(vcx, |e, _| !e.uarg.reading && e.uarg.pending.is_none()));
}

#[gpui::test]
fn universal_argument_repeats_motions(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, ALPHABET);
    press(&ed, vcx, "C-u 3 C-f");
    assert_eq!(point(&ed, vcx), 3);
    press(&ed, vcx, "C-u C-f");
    assert_eq!(point(&ed, vcx), 7);
    // M-<digit> starts an argument; further M-digits extend it.
    press(&ed, vcx, "M-2 C-f");
    assert_eq!(point(&ed, vcx), 9);
    press(&ed, vcx, "M-1 M-0 C-f");
    assert_eq!(point(&ed, vcx), 19);
    press(&ed, vcx, "C-u 2 C-b");
    assert_eq!(point(&ed, vcx), 17);
    // The argument only applies to the next command.
    press(&ed, vcx, "C-f");
    assert_eq!(point(&ed, vcx), 18);
    // More repeats than room stops at the buffer end.
    press(&ed, vcx, "C-u C-u C-f");
    assert_eq!(point(&ed, vcx), 26);
}

#[gpui::test]
fn universal_argument_repeats_typed_text_and_edits(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abcdef\n");
    press(&ed, vcx, "C-u 3 \"x\"");
    assert_eq!(text(&ed, vcx), "xxxabcdef\n");
    press(&ed, vcx, "C-u 4 C-d");
    assert_eq!(text(&ed, vcx), "xxxef\n");
    // The repeated edit is one undo step.
    press(&ed, vcx, "C-/");
    assert_eq!(text(&ed, vcx), "xxxabcdef\n");
}

#[gpui::test]
fn universal_argument_survives_a_prefix_and_is_spent_by_a_meta_command(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, ALPHABET);
    // The argument ends at C-x, waits through the prefix, and is consumed by
    // `exchange-point-and-mark`, which runs once (it isn't repeatable).
    press(&ed, vcx, "C-u 5 C-x C-x");
    assert_eq!(status(&ed, vcx), "No mark set");
    assert!(ed.read_with(vcx, |e, _| e.uarg.pending.is_none()));
    press(&ed, vcx, "C-f");
    assert_eq!(point(&ed, vcx), 1);
}

/// A Scheme command sees `C-u` as `(prefix-count)` (Emacs's
/// `current-prefix-arg`): `C-u M-!` asks to insert the output.
#[gpui::test]
fn universal_argument_reaches_a_scheme_command(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, ALPHABET);
    let prompt = |ed: &Entity<Editor>, vcx: &mut VisualTestContext| {
        ed.read_with(vcx, |e, _| e.minibuffer.as_ref().map(|m| m.prompt.clone()))
    };
    press(&ed, vcx, "M-!");
    assert_eq!(prompt(&ed, vcx).as_deref(), Some("Shell command: "));
    press(&ed, vcx, "C-g C-u M-!");
    assert_eq!(
        prompt(&ed, vcx).as_deref(),
        Some("Shell command (insert output): ")
    );
    assert!(ed.read_with(vcx, |e, _| e.uarg.pending.is_none() && e.prefix_count == 0));
}

#[gpui::test]
fn keyboard_quit_cancels_a_prefix(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, ALPHABET);
    press(&ed, vcx, "C-x C-g");
    assert_eq!(pending(&ed, vcx), "");
    press(&ed, vcx, "C-f");
    assert_eq!(point(&ed, vcx), 1);
}

// --- M-x -------------------------------------------------------------------------

#[gpui::test]
fn m_x_runs_a_command_by_name(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one two three\n");
    press(&ed, vcx, "M-x \"forward-word\" RET");
    assert!(ed.read_with(vcx, |e, _| e.minibuffer.is_none() && e.helm.is_none()));
    assert_eq!(point(&ed, vcx), 3);
    // A universal argument before M-x is spent on M-x itself.
    press(&ed, vcx, "C-u 2 M-x \"forward-word\" RET");
    assert_eq!(point(&ed, vcx), 7);
}

/// The plain `execute-extended-command` minibuffer (M-x before Helm rebinds
/// it) looks the name up among the defined commands.
#[gpui::test]
fn execute_extended_command_looks_names_up(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one two three\n");
    let submit = |ed: &Entity<Editor>, vcx: &mut VisualTestContext, name: &str| {
        ed.update(vcx, |e, cx| {
            e.apply_command(EditorCommand::ExecuteExtendedCommand, cx);
            e.minibuffer.as_mut().unwrap().input = name.into();
            e.mb_submit(cx);
        });
        settle(ed, vcx);
    };
    submit(&ed, vcx, "no-such-command");
    assert_eq!(status(&ed, vcx), "[No match: no-such-command]");
    assert_eq!(point(&ed, vcx), 0);
    submit(&ed, vcx, "  end-of-buffer ");
    assert_eq!(point(&ed, vcx), 14, "the name is trimmed");
    assert!(ed.read_with(vcx, |e, _| e.minibuffer.is_none()));
}

// --- apply_command ---------------------------------------------------------------

#[gpui::test]
fn apply_command_repeats_only_repeatable_commands(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abcdef\n");
    ed.update(vcx, |e, cx| {
        e.uarg.pending = Some(3);
        e.apply_command(EditorCommand::DeleteChar, cx);
        assert_eq!(e.active.buffer.full_text(), "def\n");
        assert_eq!(e.uarg.pending, None);
        // A non-repeatable command runs once and spends the argument.
        e.uarg.pending = Some(3);
        e.apply_command(EditorCommand::EndOfLine, cx);
        assert_eq!(e.point(), 3);
        assert_eq!(e.uarg.pending, None);
        // An argument of 1 (or less) is a plain single run.
        e.uarg.pending = Some(0);
        e.apply_command(EditorCommand::BackwardChar, cx);
        assert_eq!(e.point(), 2);
    });
}

#[gpui::test]
fn apply_command_tracks_whether_the_last_command_was_a_yank(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "ab\n");
    ed.update(vcx, |e, cx| {
        e.push_kill("k".into());
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("k".into()));
        e.apply_command(EditorCommand::Yank, cx);
        assert!(e.last_was_yank);
        e.apply_command(EditorCommand::YankPop, cx);
        assert!(e.last_was_yank, "yank-pop keeps the chain going");
        e.apply_command(EditorCommand::ForwardChar, cx);
        assert!(!e.last_was_yank);
        // A command that fails still breaks the chain.
        e.apply_command(EditorCommand::KillRegion, cx);
        assert!(!e.last_was_yank);
    });
}

#[gpui::test]
fn motion_commands_clamp_and_snap(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one\nété\nthree\n");
    ed.update(vcx, |e, cx| {
        // A line past the end goes to the last line.
        e.apply_command(EditorCommand::GotoLine(99), cx);
        assert_eq!(e.point(), e.active.buffer.len());
        e.apply_command(EditorCommand::GotoLine(1), cx);
        assert_eq!(e.point(), 4);
        // A Scheme offset inside a multibyte char floors to its boundary, and
        // one past the end clamps.
        e.apply_command(EditorCommand::GotoChar(5), cx);
        assert_eq!(e.point(), 4);
        e.apply_command(EditorCommand::GotoChar(10_000), cx);
        assert_eq!(e.point(), e.active.buffer.len());
        // Inverted region ends make a backwards region with point at `end`.
        e.apply_command(EditorCommand::SetRegion { start: 3, end: 0 }, cx);
        assert!(e.active.mark_active);
        assert_eq!(e.active.selected_range, 0..3);
        assert!(e.active.selection_reversed);
        assert_eq!(e.point(), 0);
        e.apply_command(EditorCommand::SetRegion { start: 0, end: 3 }, cx);
        assert!(!e.active.selection_reversed);
        assert_eq!(e.point(), 3);
    });
}

#[gpui::test]
fn read_only_buffers_refuse_interactive_edits(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "keep\n");
    ed.update(vcx, |e, cx| {
        e.active.special = Some("test-special".into());
        e.apply_command(EditorCommand::KillLine, cx);
        assert_eq!(e.status.as_ref(), "Buffer is read-only");
        e.apply_command(EditorCommand::DeleteChar, cx);
        e.apply_command(EditorCommand::Newline, cx);
        assert_eq!(e.active.buffer.full_text(), "keep\n");
        // Scheme's addressed primitives still write to it.
        e.apply_command(
            EditorCommand::InsertAt {
                pos: 0,
                text: ">".into(),
            },
            cx,
        );
        assert_eq!(e.active.buffer.full_text(), ">keep\n");
    });
}
