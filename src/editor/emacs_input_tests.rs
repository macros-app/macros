//! Tests for the Emacs core input commands (`emacs_input.rs`): mark rings,
//! text scale, overwrite-mode, quoted-insert, scroll-other-window, repeat and
//! the isearch variants — keys dispatched through a real window, as typed.

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::mcp::parse_keys;
use super::*;

/// A windowed editor with the bundled Scheme loaded, Emacs keys (or evil when
/// `evil`), and a plain-text buffer `test.txt` holding `text`, point at 0.
fn setup_with<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
    evil: bool,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    test_context(cx);
    cx.update(|cx| cx.bind_keys(super::key_bindings()));
    let (editor, vcx) = cx.add_window_view(|_, cx| Editor::new(cx));
    editor.update(vcx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        if evil {
            e.run_script_blocking("(evil-mode)", cx);
        }
        e.mark_config_loaded(cx);
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("test.txt".into(), None, text));
        if evil {
            e.evil = EvilState::Normal;
        }
    });
    let focus = editor.read_with(vcx, |e, _| e.focus_handle.clone());
    vcx.update(|window, cx| window.focus(&focus, cx));
    settle(&editor, vcx);
    (editor, vcx)
}

fn setup<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    setup_with(cx, text, false)
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

fn set_point(editor: &Entity<Editor>, vcx: &mut VisualTestContext, at: usize) {
    editor.update(vcx, |e, _| e.collapse_to(at));
}

// --- mark ring -------------------------------------------------------------------

#[gpui::test]
fn c_u_c_spc_pops_back_through_the_mark_ring(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "aaa bbb ccc ddd\n");
    press(&ed, vcx, "C-SPC C-g");
    set_point(&ed, vcx, 4);
    press(&ed, vcx, "C-SPC C-g");
    set_point(&ed, vcx, 8);
    press(&ed, vcx, "C-SPC C-g");
    set_point(&ed, vcx, 12);
    // The newest mark first, then each older one; plain C-SPC keeps popping.
    press(&ed, vcx, "C-u C-SPC");
    assert_eq!(point(&ed, vcx), 8);
    assert!(ed.read_with(vcx, |e, _| !e.active.mark_active));
    press(&ed, vcx, "C-SPC");
    assert_eq!(point(&ed, vcx), 4);
    press(&ed, vcx, "C-SPC");
    assert_eq!(point(&ed, vcx), 0);
    // The ring rotates: past the oldest mark comes the newest again.
    press(&ed, vcx, "C-SPC");
    assert_eq!(point(&ed, vcx), 8);
    // Any other command ends the popping: C-SPC sets a mark again.
    press(&ed, vcx, "C-f C-SPC");
    assert!(ed.read_with(vcx, |e, _| e.active.mark_active));
    assert_eq!(status(&ed, vcx), "Mark set");
}

#[gpui::test]
fn pop_mark_without_a_mark_says_so(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    press(&ed, vcx, "C-u C-SPC");
    assert_eq!(status(&ed, vcx), "No mark set in this buffer");
    assert_eq!(point(&ed, vcx), 0);
}

#[gpui::test]
fn marks_follow_edits_and_buffer_jumps_push_them(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one two three\n");
    set_point(&ed, vcx, 4);
    press(&ed, vcx, "C-SPC C-g");
    set_point(&ed, vcx, 0);
    // Text typed before the mark shifts it.
    press(&ed, vcx, "\"XX\"");
    press(&ed, vcx, "C-u C-SPC");
    assert_eq!(point(&ed, vcx), 6);
    // M-> leaves a mark where it started.
    set_point(&ed, vcx, 9);
    press(&ed, vcx, "M->");
    press(&ed, vcx, "C-u C-SPC");
    assert_eq!(point(&ed, vcx), 9);
}

#[gpui::test]
fn c_x_c_spc_pops_the_global_mark_ring_across_buffers(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "first buffer\n");
    set_point(&ed, vcx, 3);
    press(&ed, vcx, "C-SPC C-g");
    ed.update(vcx, |e, _| {
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("other.txt".into(), None, "second\n"));
    });
    set_point(&ed, vcx, 2);
    press(&ed, vcx, "C-SPC C-g");
    set_point(&ed, vcx, 5);
    // Newest first: this buffer's mark, then the one in test.txt.
    press(&ed, vcx, "C-x C-SPC");
    assert_eq!(
        ed.read_with(vcx, |e, _| (e.active.name.clone(), e.point())),
        ("other.txt".to_string(), 2)
    );
    press(&ed, vcx, "C-x C-SPC");
    assert_eq!(
        ed.read_with(vcx, |e, _| (e.active.name.clone(), e.point())),
        ("test.txt".to_string(), 3)
    );
}

// --- text scale ------------------------------------------------------------------

fn scale(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> i32 {
    editor.read_with(vcx, |e, _| e.text_scale_steps("test.txt"))
}

#[gpui::test]
fn text_scale_adjust_keeps_adjusting_with_plain_keys(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    press(&ed, vcx, "C-x C-+");
    assert_eq!(scale(&ed, vcx), 1);
    press(&ed, vcx, "+ = +");
    assert_eq!(scale(&ed, vcx), 4);
    press(&ed, vcx, "-");
    assert_eq!(scale(&ed, vcx), 3);
    press(&ed, vcx, "0");
    assert_eq!(scale(&ed, vcx), 0);
    assert_eq!(text(&ed, vcx), "abc\n", "the adjusting keys insert nothing");
    // Another key ends it and runs as itself.
    press(&ed, vcx, "x +");
    assert_eq!(text(&ed, vcx), "x+abc\n");
    assert_eq!(scale(&ed, vcx), 0);
    press(&ed, vcx, "C-x C-- C-x C--");
    assert_eq!(scale(&ed, vcx), -2);
    press(&ed, vcx, "C-x C-0 C-x C-=");
    assert_eq!(scale(&ed, vcx), 1);
    // A scaled buffer renders larger than the window's default text.
    ed.update(vcx, |e, cx| {
        let base = cx.global::<crate::settings::Settings>().font_size();
        let (size, _) = e.buffer_text_style("test.txt", cx).expect("scaled");
        assert!(size > base);
        assert!(e.buffer_text_style("*scratch*", cx).is_none());
    });
    // C-u N scales by N steps.
    press(&ed, vcx, "C-u 3 C-x C-+");
    assert_eq!(scale(&ed, vcx), 4);
}

#[gpui::test]
fn global_text_scale_zooms_without_a_transient_map(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc\n");
    press(&ed, vcx, "s-=");
    assert!(ed.read_with(vcx, |e, _| e.global_text_scale_factor() > 1.0));
    // cmd-= is an app zoom: a following `+` is just text.
    press(&ed, vcx, "+");
    assert_eq!(text(&ed, vcx), "+abc\n");
    press(&ed, vcx, "s-- s-- s--");
    assert!(ed.read_with(vcx, |e, _| e.global_text_scale_factor() < 1.0));
    press(&ed, vcx, "s-0");
    assert_eq!(ed.read_with(vcx, |e, _| e.global_text_scale_factor()), 1.0);
    assert_eq!(scale(&ed, vcx), 0, "the buffer scale is separate");
}

// --- overwrite-mode --------------------------------------------------------------

#[gpui::test]
fn overwrite_mode_types_over_text_up_to_the_line_end(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abcd\nnext\n");
    press(&ed, vcx, "<insert>");
    assert_eq!(status(&ed, vcx), "Overwrite mode enabled");
    let modes = ed.read_with(vcx, |e, _| {
        e.mode_line_segments()
            .iter()
            .map(|(t, _)| t.to_string())
            .collect::<String>()
    });
    assert!(modes.contains("Ovwrt"), "mode line: {modes}");
    press(&ed, vcx, "\"XY\"");
    assert_eq!(text(&ed, vcx), "XYcd\nnext\n");
    // At the end of the line it inserts instead of eating the newline.
    press(&ed, vcx, "\"123\"");
    assert_eq!(text(&ed, vcx), "XY123\nnext\n");
    // C-q still inserts.
    set_point(&ed, vcx, 0);
    press(&ed, vcx, "C-q z");
    assert_eq!(text(&ed, vcx), "zXY123\nnext\n");
    press(&ed, vcx, "M-x \"overwrite-mode\" RET");
    assert_eq!(status(&ed, vcx), "Overwrite mode disabled");
    press(&ed, vcx, "q");
    assert_eq!(text(&ed, vcx), "zqXY123\nnext\n");
}

#[gpui::test]
fn overwrite_mode_is_per_buffer_and_off_under_evil(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_with(cx, "abc\n", true);
    assert!(ed.read_with(vcx, |e, _| !e.overwrite_mode_active()));
    // evil Insert types normally.
    press(&ed, vcx, "i \"xy\"");
    assert_eq!(text(&ed, vcx), "xyabc\n");
    ed.update(vcx, |e, cx| {
        e.run_script_blocking("(overwrite-mode)", cx);
        assert!(e.overwrite_mode_active());
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("other.txt".into(), None, ""));
        assert!(
            !e.overwrite_mode_active(),
            "only the buffer it was enabled in"
        );
    });
}

// --- quoted-insert ---------------------------------------------------------------

#[gpui::test]
fn quoted_insert_inserts_control_characters_literally(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "");
    press(&ed, vcx, "C-q");
    assert_eq!(status(&ed, vcx), "C-q-");
    press(&ed, vcx, "C-l");
    assert_eq!(text(&ed, vcx), "\x0c");
    press(&ed, vcx, "C-q TAB C-q RET C-q C-j C-q ESC");
    assert_eq!(text(&ed, vcx), "\x0c\t\r\n\x1b");
    // A plain key quotes itself; C-u N repeats it.
    press(&ed, vcx, "C-u 3 C-q x");
    assert_eq!(text(&ed, vcx), "\x0c\t\r\n\x1bxxx");
}

#[gpui::test]
fn quoted_insert_reads_octal_codes(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "");
    // Three digits insert at once.
    press(&ed, vcx, "C-q 0 1 4");
    assert_eq!(text(&ed, vcx), "\x0c");
    // RET ends a shorter code and is dropped...
    press(&ed, vcx, "C-q 1 2 RET");
    assert_eq!(text(&ed, vcx), "\x0c\n");
    // ...any other key ends it and then runs as itself.
    press(&ed, vcx, "C-q 7 a");
    assert_eq!(text(&ed, vcx), "\x0c\n\x07a");
    press(&ed, vcx, "C-q 1 0 1");
    assert_eq!(text(&ed, vcx), "\x0c\n\x07aA");
}

#[gpui::test]
fn quoted_insert_replays_in_a_keyboard_macro(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "");
    press(&ed, vcx, "C-x ( C-q TAB C-x ) C-x e");
    assert_eq!(text(&ed, vcx), "\t\t");
}

// --- scroll-other-window -----------------------------------------------------------

#[gpui::test]
fn scroll_other_window_pages_the_next_window_and_stays_here(cx: &mut TestAppContext) {
    let body: String = (0..300).map(|i| format!("line {i}\n")).collect();
    let (ed, vcx) = setup(cx, &body);
    press(&ed, vcx, "C-SPC C-g");
    press(&ed, vcx, "C-M-v");
    assert_eq!(status(&ed, vcx), "There is no other window");
    press(&ed, vcx, "C-x 2");
    let (here, other) = ed.read_with(vcx, |e, _| {
        let leaves = e.layout.leaves();
        (
            e.selected_pane,
            *leaves.iter().find(|&&p| p != e.selected_pane).unwrap(),
        )
    });
    press(&ed, vcx, "C-M-v");
    let other_point = ed.read_with(vcx, |e, _| e.pane_point(other));
    assert!(other_point > 0, "the other window scrolled");
    assert_eq!(ed.read_with(vcx, |e, _| e.selected_pane), here);
    assert_eq!(point(&ed, vcx), 0, "this window's point stays");
    press(&ed, vcx, "C-M-v");
    assert!(ed.read_with(vcx, |e, _| e.pane_point(other)) > other_point);
    press(&ed, vcx, "C-M-S-v C-M-S-v");
    assert_eq!(ed.read_with(vcx, |e, _| e.pane_point(other)), 0);
}

// --- repeat ----------------------------------------------------------------------

#[gpui::test]
fn repeat_runs_the_last_command_and_z_repeats_again(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abcdefghij\n");
    press(&ed, vcx, "C-x z");
    assert_eq!(status(&ed, vcx), "No command to repeat");
    press(&ed, vcx, "C-f C-x z");
    assert_eq!(point(&ed, vcx), 2);
    press(&ed, vcx, "z z");
    assert_eq!(point(&ed, vcx), 4);
    assert_eq!(text(&ed, vcx), "abcdefghij\n");
    // Another key ends it: `z` is text again.
    press(&ed, vcx, "C-b z");
    assert_eq!(text(&ed, vcx), "abczdefghij\n");
    // Typed text repeats too.
    press(&ed, vcx, "C-x z");
    assert_eq!(text(&ed, vcx), "abczzdefghij\n");
}

#[gpui::test]
fn repeat_reuses_the_prefix_argument_and_m_x_commands(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abcdefghijklmnop\n");
    press(&ed, vcx, "C-u 3 C-f C-x z");
    assert_eq!(point(&ed, vcx), 6);
    press(&ed, vcx, "M-x \"forward-char\" RET");
    assert_eq!(point(&ed, vcx), 7);
    press(&ed, vcx, "C-x z z");
    assert_eq!(point(&ed, vcx), 9);
}

// --- isearch -----------------------------------------------------------------------

fn matched(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> Option<Range<usize>> {
    editor.read_with(vcx, |e, _| {
        e.isearch.as_ref().and_then(|s| s.matched.clone())
    })
}

#[gpui::test]
fn c_s_is_literal_and_c_m_s_is_a_regexp(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc a.c\n");
    press(&ed, vcx, "C-s \"a.c\"");
    assert_eq!(matched(&ed, vcx), Some(4..7));
    // M-r toggles to a regexp: `.` now matches `b` too.
    press(&ed, vcx, "C-g");
    set_point(&ed, vcx, 0);
    press(&ed, vcx, "C-s \"a.c\" M-r");
    assert_eq!(matched(&ed, vcx), Some(0..3));
    let prompt = ed.read_with(vcx, |e, _| e.minibuffer.as_ref().unwrap().prompt.clone());
    assert_eq!(prompt, "Regexp I-search: ");
    press(&ed, vcx, "C-g");
    set_point(&ed, vcx, 0);
    press(&ed, vcx, "C-M-s \"a.c\"");
    assert_eq!(matched(&ed, vcx), Some(0..3));
    press(&ed, vcx, "C-s");
    assert_eq!(matched(&ed, vcx), Some(4..7));
    press(&ed, vcx, "RET");
    assert_eq!(point(&ed, vcx), 7);
    // Finishing leaves the mark where the search began.
    press(&ed, vcx, "C-u C-SPC");
    assert_eq!(point(&ed, vcx), 0);
    press(&ed, vcx, "M->");
    press(&ed, vcx, "C-M-r \"[ab]\\\\.\"");
    assert_eq!(matched(&ed, vcx), Some(4..6));
}

#[gpui::test]
fn m_s_dot_searches_the_symbol_at_point(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "foo foobar xfoo foo\n");
    set_point(&ed, vcx, 1);
    press(&ed, vcx, "M-s .");
    let prompt = ed.read_with(vcx, |e, _| e.minibuffer.as_ref().unwrap().prompt.clone());
    assert_eq!(prompt, "Symbol I-search: ");
    assert_eq!(matched(&ed, vcx), Some(0..3));
    // Only whole symbols: `foobar` and `xfoo` are skipped.
    press(&ed, vcx, "C-s");
    assert_eq!(matched(&ed, vcx), Some(16..19));
    press(&ed, vcx, "RET");
    assert_eq!(point(&ed, vcx), 19);
}

#[gpui::test]
fn m_s_dot_needs_a_symbol(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "a  b\n");
    set_point(&ed, vcx, 2);
    press(&ed, vcx, "M-s .");
    assert_eq!(status(&ed, vcx), "No symbol at point");
    assert!(ed.read_with(vcx, |e, _| e.minibuffer.is_none()));
}

#[gpui::test]
fn evil_normal_keeps_its_own_bindings(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_with(cx, "abc\n", true);
    // Plain keys stay evil motions: no transient map or quoted key is armed.
    press(&ed, vcx, "l");
    assert_eq!(point(&ed, vcx), 1);
    press(&ed, vcx, "0");
    assert_eq!(point(&ed, vcx), 0);
    assert_eq!(text(&ed, vcx), "abc\n");
    assert!(ed.read_with(vcx, |e, _| e.evil == EvilState::Normal));
}

// --- narrowing ---------------------------------------------------------------------

#[gpui::test]
fn narrowing_bounds_isearch_marks_and_overwrite(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "foo one\nfoo two\nfoo\n");
    // A mark set before narrowing, outside the narrowed line.
    press(&ed, vcx, "C-SPC C-g");
    ed.update(vcx, |e, cx| e.narrow_to(8, 15, cx));
    set_point(&ed, vcx, 8);
    // Isearch only finds matches inside the narrowing, literal or regexp.
    press(&ed, vcx, "C-s \"foo\"");
    assert_eq!(matched(&ed, vcx), Some(8..11));
    press(&ed, vcx, "C-s");
    assert_eq!(matched(&ed, vcx), Some(8..11), "wraps inside the narrowing");
    press(&ed, vcx, "RET");
    set_point(&ed, vcx, 8);
    press(&ed, vcx, "C-M-s \"o+\" C-s C-s");
    assert!(matched(&ed, vcx).is_some_and(|m| 8 <= m.start && m.end <= 15));
    press(&ed, vcx, "C-g");
    // The symbol at the narrowing's edge ends there.
    set_point(&ed, vcx, 14);
    press(&ed, vcx, "M-s .");
    assert_eq!(matched(&ed, vcx), Some(12..15));
    press(&ed, vcx, "C-g");
    // Popping to a mark outside the narrowing stops at its edge.
    set_point(&ed, vcx, 12);
    press(&ed, vcx, "C-u C-SPC");
    assert!(
        (8..=15).contains(&point(&ed, vcx)),
        "point {}",
        point(&ed, vcx)
    );
    // Overwriting stops at the narrowing's end.
    ed.update(vcx, |e, cx| {
        e.run_script_blocking("(overwrite-mode)", cx);
        e.collapse_to(14);
    });
    press(&ed, vcx, "\"XY\"");
    assert_eq!(text(&ed, vcx), "foo one\nfoo twXY\nfoo\n");
}
