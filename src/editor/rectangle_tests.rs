//! Rectangles (`C-x r k/d/y/o/c/t/N`, `C-x SPC`) and registers (`C-x r
//! s/i/SPC/j/n/+/w`) end to end: real keystrokes through a window, with the
//! bundled scheme/rectangles.scm and scheme/registers.scm loaded.

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::mcp::parse_keys;
use super::*;

/// A windowed editor with Emacs keys and a buffer holding `text`, point at 0.
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

const GRID: &str = "abcdef\nghijkl\nmnopqr\n";

// --- rectangles ---------------------------------------------------------------

#[gpui::test]
fn kill_and_yank_rectangle(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    // Corners (row 0, col 1) and (row 2, col 3): columns 1..3 of every row.
    press(&ed, vcx, "C-f C-SPC C-n C-n C-f C-f C-x r k");
    assert_eq!(text(&ed, vcx), "adef\ngjkl\nmpqr\n");
    assert_eq!(point(&ed, vcx), 1, "point at the top-left corner");
    // Yank it back at the end of the first line.
    press(&ed, vcx, "C-e C-x r y");
    assert_eq!(text(&ed, vcx), "adefbc\ngjklhi\nmpqrno\n");
    assert_eq!(point(&ed, vcx), 20, "point after the last yanked line");
    // One undo step each.
    press(&ed, vcx, "C-/");
    assert_eq!(text(&ed, vcx), "adef\ngjkl\nmpqr\n");
}

#[gpui::test]
fn yank_rectangle_pads_short_lines_and_extends_the_buffer(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    press(&ed, vcx, "C-SPC C-n C-f C-f C-x r M-w");
    assert_eq!(text(&ed, vcx), GRID, "copy leaves the text alone");
    // Row 2 col 3 → rows 2 and 3 (a new row) at column 3.
    press(&ed, vcx, "M-> C-p C-f C-f C-f C-x r y");
    // Row 3 is the (empty) line after the final newline.
    assert_eq!(text(&ed, vcx), "abcdef\nghijkl\nmnoabpqr\n   gh");
}

#[gpui::test]
fn delete_open_and_clear_rectangle(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    press(&ed, vcx, "C-f C-SPC C-n C-f C-f C-x r d");
    assert_eq!(text(&ed, vcx), "adef\ngjkl\nmnopqr\n");
    press(&ed, vcx, "C-/ M-< C-f C-SPC C-n C-f C-f C-x r o");
    assert_eq!(text(&ed, vcx), "a  bcdef\ng  hijkl\nmnopqr\n");
    press(&ed, vcx, "C-/ M-< C-f C-SPC C-n C-f C-f C-x r c");
    assert_eq!(text(&ed, vcx), "a  def\ng  jkl\nmnopqr\n");
}

#[gpui::test]
fn string_rectangle_and_number_lines(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    press(&ed, vcx, "C-f C-SPC C-n C-n C-f C-x r t \"XY\" RET");
    assert_eq!(text(&ed, vcx), "aXYcdef\ngXYijkl\nmXYopqr\n");
    // A zero-width rectangle (one column) numbers each line at that column.
    press(&ed, vcx, "M-< C-SPC C-n C-n C-x r N");
    assert_eq!(text(&ed, vcx), "1 aXYcdef\n2 gXYijkl\n3 mXYopqr\n");
}

#[gpui::test]
fn rectangle_columns_are_display_columns(cx: &mut TestAppContext) {
    // Tab width 4: the tab covers columns 0..4. Corners (row 1, col 2) and
    // (row 0, byte 2 = `b`, col 5) give columns 2..5, cutting the tab in half.
    let (ed, vcx) = setup(cx, "\tab\nxyzwvu\n");
    press(&ed, vcx, "C-n C-f C-f C-SPC M-< C-f C-f C-x r k");
    assert_eq!(
        text(&ed, vcx),
        "  b\nxyu\n",
        "the cut tab leaves two spaces"
    );
    // The killed rectangle is padded to its display width.
    press(&ed, vcx, "M-> C-x r y");
    assert_eq!(text(&ed, vcx), "  b\nxyu\n  a\nzwv");
}

#[gpui::test]
fn rectangle_mark_mode_highlights_and_redirects_kill(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    press(&ed, vcx, "C-f C-x SPC C-n C-f C-f");
    let block = ed.read_with(vcx, |e, _| e.pane_visual_block(e.selected_pane));
    assert_eq!(
        block,
        Some(BlockSel::Display {
            top: 0,
            bottom: 1,
            corners: (1, 10)
        })
    );
    assert_eq!(
        ed.read_with(vcx, |e, _| e.pane_selection(e.selected_pane)),
        0..0,
        "no contiguous region highlight"
    );
    // C-w kills the rectangle, not the region.
    press(&ed, vcx, "C-w");
    assert_eq!(text(&ed, vcx), "adef\ngjkl\nmnopqr\n");
    assert_eq!(
        ed.read_with(vcx, |e, _| e.pane_visual_block(e.selected_pane)),
        None
    );
    // C-x SPC twice turns it off again; then C-w is the plain region kill.
    press(&ed, vcx, "C-x SPC C-x SPC");
    assert!(!ed.read_with(vcx, |e, _| e.rect_mark_active()));
}

// --- registers -------------------------------------------------------------------

#[gpui::test]
fn copy_and_insert_text_register(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "hello world\n");
    press(&ed, vcx, "C-SPC M-f C-x r s a");
    assert_eq!(text(&ed, vcx), "hello world\n");
    press(&ed, vcx, "C-e C-x r i a");
    assert_eq!(text(&ed, vcx), "hello worldhello\n");
    assert_eq!(point(&ed, vcx), 11, "point before the inserted text");
    // C-u C-x r s deletes the region as it copies.
    press(&ed, vcx, "M-< C-SPC M-f C-u C-x r s b");
    assert_eq!(text(&ed, vcx), " worldhello\n");
    press(&ed, vcx, "C-e C-u C-x r i b");
    assert_eq!(text(&ed, vcx), " worldhellohello\n");
    assert_eq!(point(&ed, vcx), 16, "C-u: point after the inserted text");
}

#[gpui::test]
fn point_register_follows_edits(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one two three\n");
    press(&ed, vcx, "M-f M-f C-x r SPC p");
    // Insert text before the remembered spot, then jump back from elsewhere.
    press(&ed, vcx, "M-< \"XX\" C-e C-x r j p");
    assert_eq!(text(&ed, vcx), "XXone two three\n");
    assert_eq!(point(&ed, vcx), 9, "after `two`, shifted by the insert");
}

#[gpui::test]
fn number_registers(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "x\n");
    press(
        &ed,
        vcx,
        "C-u 5 C-x r n c C-x r + c C-u 3 C-x r + c C-x r i c",
    );
    assert_eq!(text(&ed, vcx), "9x\n");
    // Without a prefix, number-to-register reads the number at point.
    press(&ed, vcx, "C-x r n d C-e C-x r i d");
    assert_eq!(text(&ed, vcx), "9x9\n");
}

#[gpui::test]
fn rectangle_register(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    press(&ed, vcx, "C-SPC C-n C-f C-f C-x r r q M-< C-e C-x r i q");
    assert_eq!(text(&ed, vcx), "abcdefab\nghijklgh\nmnopqr\n");
}

#[gpui::test]
fn window_configuration_register(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    press(&ed, vcx, "C-x 3 C-x r w w C-x 1");
    assert_eq!(ed.read_with(vcx, |e, _| e.layout.leaves().len()), 1);
    press(&ed, vcx, "C-x r j w");
    assert_eq!(ed.read_with(vcx, |e, _| e.layout.leaves().len()), 2);
}

#[gpui::test]
fn bookmark_keys_still_bound(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, GRID);
    let resolve = |e: &Editor, k: &str| e.keymap.resolve_mode("fundamental", k).map(str::to_string);
    ed.read_with(vcx, |e, _| {
        assert_eq!(resolve(e, "ctrl-x r m").as_deref(), Some("(bookmark-set)"));
        assert_eq!(resolve(e, "ctrl-x r b").as_deref(), Some("(bookmark-jump)"));
        assert_eq!(
            resolve(e, "ctrl-x r l").as_deref(),
            Some("(helm-bookmarks)")
        );
    });
}
