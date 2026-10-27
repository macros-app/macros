//! Narrowing tests (`C-x n n` / `d` / `s` / `w`): a windowed Emacs-keys
//! `Editor` with the bundled Scheme loaded, keys dispatched through the window
//! (see `key_tests.rs`), and every settle repaints — so the renderer's
//! narrowed row walk and boundary-line clipping run too.

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::mcp::parse_keys;
use super::*;

/// Five lines: `zero` 0..4, `one` 5..8, `two` 9..12, `three` 13..18,
/// `four` 19..23; 24 bytes in all.
const TEXT: &str = "zero\none\ntwo\nthree\nfour\n";

/// A windowed editor with Emacs keys visiting a buffer named `name` (its
/// extension picks the language) holding `text`, point at its start.
fn setup<'a>(
    cx: &'a mut TestAppContext,
    name: &str,
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
        e.install_active(make_document(name.into(), Some(name.into()), text));
        e.parse_full();
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

/// Run Scheme `src` against the current buffer (snapshot published first).
fn run(editor: &Entity<Editor>, vcx: &mut VisualTestContext, src: &str) {
    editor.update(vcx, |e, cx| {
        e.publish_point();
        e.run_script_blocking(src, cx);
    });
    settle(editor, vcx);
}

/// Evaluate Scheme `src` against the current buffer, printed.
fn eval(editor: &Entity<Editor>, vcx: &mut VisualTestContext, src: &str) -> String {
    editor.update(vcx, |e, _| {
        e.publish_point();
        e.script.eval(src).unwrap().value.unwrap_or_default()
    })
}

fn goto(editor: &Entity<Editor>, vcx: &mut VisualTestContext, offset: usize) {
    editor.update(vcx, |e, cx| e.move_to(offset, cx));
}

fn narrow(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> Option<Range<usize>> {
    editor.read_with(vcx, |e, _| e.active.narrow.clone())
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

fn mode_line(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> String {
    editor.read_with(vcx, |e, _| {
        e.mode_line_segments()
            .into_iter()
            .map(|(t, _)| t.to_string())
            .collect()
    })
}

#[gpui::test]
fn keys_narrow_to_the_region_and_motion_stays_inside(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "t.txt", TEXT);
    goto(&ed, vcx, 5);
    press(&ed, vcx, "ctrl-space ctrl-n ctrl-n ctrl-x n n");
    assert_eq!(narrow(&ed, vcx), Some(5..13));
    ed.read_with(vcx, |e, _| {
        assert!(!e.active.mark_active, "narrowing drops the mark")
    });
    assert!(mode_line(&ed, vcx).contains("Narrow"));

    press(&ed, vcx, "alt-<");
    assert_eq!(point(&ed, vcx), 5, "beginning-of-buffer is point-min");
    press(&ed, vcx, "ctrl-b ctrl-p");
    assert_eq!(point(&ed, vcx), 5, "nothing before point-min");
    press(&ed, vcx, "alt->");
    assert_eq!(point(&ed, vcx), 13, "end-of-buffer is point-max");
    press(&ed, vcx, "ctrl-f ctrl-n alt-f");
    assert_eq!(point(&ed, vcx), 13, "nothing after point-max");

    press(&ed, vcx, "ctrl-x n w");
    assert_eq!(narrow(&ed, vcx), None);
    assert!(!mode_line(&ed, vcx).contains("Narrow"));
    press(&ed, vcx, "alt->");
    assert_eq!(point(&ed, vcx), TEXT.len());
}

#[gpui::test]
fn point_min_and_max_are_the_bounds(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "t.txt", TEXT);
    assert_eq!(eval(&ed, vcx, "(point-min)"), "0");
    run(&ed, vcx, "(narrow-to-region 13 5)");
    assert_eq!(narrow(&ed, vcx), Some(5..13), "ends normalized");
    assert_eq!(point(&ed, vcx), 5, "point moved inside");
    assert_eq!(eval(&ed, vcx, "(point-min)"), "5");
    assert_eq!(eval(&ed, vcx, "(point-max)"), "13");
    assert_eq!(eval(&ed, vcx, "(buffer-size)"), "24");
    run(&ed, vcx, "(goto-char 100)");
    assert_eq!(point(&ed, vcx), 13);
    run(&ed, vcx, "(goto-char 0)");
    assert_eq!(point(&ed, vcx), 5);
    // Narrowing to the whole buffer is no narrowing.
    run(&ed, vcx, "(narrow-to-region 0 (buffer-size))");
    assert_eq!(narrow(&ed, vcx), None);
}

#[gpui::test]
fn edits_at_the_bounds(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "t.txt", TEXT);
    run(&ed, vcx, "(narrow-to-region 5 13)");

    // Typing at point-max extends the narrowing over the new text.
    press(&ed, vcx, "alt-> x");
    assert_eq!(text(&ed, vcx), "zero\none\ntwo\nxthree\nfour\n");
    assert_eq!(narrow(&ed, vcx), Some(5..14));
    assert_eq!(point(&ed, vcx), 14);
    // Deleting past point-max is refused, as in Emacs.
    press(&ed, vcx, "ctrl-d");
    assert_eq!(status(&ed, vcx), "End of buffer");
    assert_eq!(text(&ed, vcx), "zero\none\ntwo\nxthree\nfour\n");

    // Deleting before point-min is refused; typing there lands inside.
    press(&ed, vcx, "alt-< backspace");
    assert_eq!(status(&ed, vcx), "Beginning of buffer");
    press(&ed, vcx, "y");
    assert_eq!(text(&ed, vcx), "zero\nyone\ntwo\nxthree\nfour\n");
    assert_eq!(narrow(&ed, vcx), Some(5..15));

    // Undo moves the bounds back with the text.
    ed.update(vcx, |e, cx| e.apply_command(EditorCommand::Undo, cx));
    assert_eq!(text(&ed, vcx), "zero\none\ntwo\nxthree\nfour\n");
    assert_eq!(narrow(&ed, vcx), Some(5..14));

    // A program can't reach outside either.
    run(&ed, vcx, "(insert-at 0 \"no\")");
    assert_eq!(status(&ed, vcx), "Beginning of buffer");
    run(&ed, vcx, "(delete-region 10 20)");
    assert_eq!(status(&ed, vcx), "End of buffer");
    assert_eq!(text(&ed, vcx), "zero\none\ntwo\nxthree\nfour\n");
}

#[gpui::test]
fn whole_buffer_rewrites_keep_the_narrowing(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "t.txt", TEXT);
    run(&ed, vcx, "(narrow-to-region 5 13)");
    // org-style "replace everything" with a change inside the bounds.
    run(
        &ed,
        vcx,
        "(replace-region 0 (buffer-size) \"zero\\nONE!\\ntwo\\nthree\\nfour\\n\")",
    );
    assert_eq!(text(&ed, vcx), "zero\nONE!\ntwo\nthree\nfour\n");
    assert_eq!(narrow(&ed, vcx), Some(5..14));
    // A formatter rewriting outside the bounds still goes through whole.
    run(
        &ed,
        vcx,
        "(replace-region 0 (buffer-size) \"ZERO\\nONE!\\ntwo\\nthree\\nfour\\nfive\\n\")",
    );
    assert_eq!(text(&ed, vcx), "ZERO\nONE!\ntwo\nthree\nfour\nfive\n");
    assert_eq!(narrow(&ed, vcx), Some(5..14));
}

#[gpui::test]
fn searches_stay_inside(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "t.txt", "apple\nbanana apple\ncherry apple\n");
    run(&ed, vcx, "(narrow-to-region 6 19)");
    ed.update(vcx, |e, cx| {
        e.isearch_start(Direction::Forward, cx);
        e.isearch_run("apple", Direction::Forward, 6, cx);
        assert_eq!(e.point(), 18);
        // Repeating wraps to the first match INSIDE, not the hidden ones.
        e.isearch_run("apple", Direction::Forward, 14, cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(13..18));
        e.isearch_run("apple", Direction::Backward, 12, cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(13..18));
        e.isearch_run("cherry", Direction::Forward, 6, cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, None);
        e.keyboard_quit(cx);
    });
    settle(&ed, vcx);
    goto(&ed, vcx, 6);
    assert_eq!(eval(&ed, vcx, "(search-forward \"cherry\")"), "-1");
    assert_eq!(eval(&ed, vcx, "(search-forward \"apple\")"), "13");
    assert_eq!(eval(&ed, vcx, "(re-search-forward \"ch\")"), "()");
    assert_eq!(eval(&ed, vcx, "(re-search-forward \"a.p\")"), "(13 16)");
}

#[gpui::test]
fn only_the_accessible_lines_display(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "t.txt", TEXT);
    run(&ed, vcx, "(narrow-to-region 6 15)");
    ed.read_with(vcx, |e, _| {
        // Lines 1..=3: `one` (from `ne`), `two`, `three` (to `th`).
        assert_eq!(e.display_line_count(), 3);
        assert!(e.is_line_invisible(0) && e.is_line_invisible(4));
        assert!(!e.is_line_invisible(2));
        assert_eq!(e.display_row_of_line(2), 1);
        // The painted lines are the accessible bytes, boundary lines clipped.
        let painted: Vec<_> = e.lines.iter().map(|l| l.range.clone()).collect();
        assert_eq!(painted, vec![6..8, 9..12, 13..15]);
    });
    press(&ed, vcx, "ctrl-x n w");
    ed.read_with(vcx, |e, _| assert_eq!(e.display_line_count(), 6));
}

#[gpui::test]
fn saving_writes_the_whole_buffer(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("macros-narrow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notes.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();
    let (ed, vcx) = setup(cx, "scratch.txt", "");
    ed.update(vcx, |e, cx| e.find_file(&path.to_string_lossy(), cx));
    settle(&ed, vcx);
    run(&ed, vcx, "(narrow-to-region 2 4)");
    press(&ed, vcx, "b");
    ed.update(vcx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    settle(&ed, vcx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nbb\nc\n");
    assert_eq!(narrow(&ed, vcx), Some(2..5), "saving keeps the narrowing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui::test]
fn narrow_to_defun_takes_the_enclosing_form(cx: &mut TestAppContext) {
    let src = "(define (a)\n  1)\n\n(define (b)\n  (+ 1\n     2))\n";
    let (ed, vcx) = setup(cx, "t.scm", src);
    let inner = src.find("(+ 1").unwrap() + 1;
    goto(&ed, vcx, inner);
    press(&ed, vcx, "ctrl-x n d");
    assert_eq!(
        narrow(&ed, vcx),
        Some(src.find("(define (b)").unwrap()..src.len())
    );
    assert_eq!(point(&ed, vcx), inner);
}

#[gpui::test]
fn org_narrow_to_subtree(cx: &mut TestAppContext) {
    let src = "* A\na body\n** A1\nx\n* B\nb\n";
    let (ed, vcx) = setup(cx, "t.org", src);
    goto(&ed, vcx, src.find("x\n").unwrap());
    run(&ed, vcx, "(org-narrow-to-subtree)");
    assert_eq!(narrow(&ed, vcx), Some(11..19));
    run(&ed, vcx, "(widen)");
    goto(&ed, vcx, 0);
    run(&ed, vcx, "(org-narrow-to-subtree)");
    assert_eq!(
        narrow(&ed, vcx),
        Some(0..19),
        "a subtree holds its children"
    );
}
