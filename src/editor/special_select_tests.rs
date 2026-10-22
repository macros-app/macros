//! Selecting text in special (read-only, program-generated) buffers. With the
//! mouse, a drag selects and a `mouse-1` binding runs only on a click, as in
//! Emacs; real mouse events go through `on_mouse_down` / `on_mouse_move` /
//! `on_mouse_up` in a painted test window. From the keyboard, evil and helix
//! get `v` / `V` … `y` (see `special_select.rs`) and Emacs keeps `C-SPC` …
//! `M-w`.

use gpui::{
    Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, TestAppContext,
    VisualTestContext,
};

use super::*;

const TEXT: &str = "alpha beta\\ngamma delta\\nepsilon\\n";

/// A special buffer in a window, painted so clicks map to text. `bind` is an
/// optional `mouse-1` command for the buffer's mode.
fn windowed(cx: &mut TestAppContext, bind: bool) -> (Entity<Editor>, &mut VisualTestContext) {
    windowed_with(cx, bind, "")
}

/// [`windowed`], running `setup` (e.g. `(evil-mode)`) after startup.
fn windowed_with<'a>(
    cx: &'a mut TestAppContext,
    bind: bool,
    setup: &str,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    test_context(cx);
    let (ed, vcx) = cx.add_window_view(|_w, cx| Editor::new(cx));
    ed.update(vcx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        if !setup.is_empty() {
            e.run_script_blocking(setup, cx);
        }
        e.mark_config_loaded(cx);
        e.run_script_blocking("(define clicks 0)", cx);
        if bind {
            e.run_script_blocking("(define-command (mt-click) (set! clicks (+ clicks 1)))", cx);
            e.run_script_blocking("(define-key \"mt-mode\" \"mouse-1\" 'mt-click)", cx);
            // A buffer key on `y`, like a dashboard's copy-id.
            e.run_script_blocking("(define-key \"mt-mode\" \"y\" 'mt-click)", cx);
            e.run_script_blocking("(define-key \"mt-mode\" \"n\" 'next-line)", cx);
        }
        e.run_script_blocking(
            &format!("(render-buffer \"*mt*\" \"mt-mode\" \"{TEXT}\" '())"),
            cx,
        );
    });
    vcx.run_until_parked();
    paint(&ed, vcx);
    (ed, vcx)
}

fn paint(ed: &Entity<Editor>, vcx: &mut VisualTestContext) {
    for _ in 0..2 {
        ed.update(vcx, |_, cx| cx.notify());
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
    }
}

/// Window position of `col` on buffer line `line`, mid-row.
fn at(ed: &Entity<Editor>, vcx: &mut VisualTestContext, line: usize, col: usize) -> Point<Pixels> {
    ed.read_with(vcx, |e, _| {
        assert_eq!(e.active.name, "*mt*");
        let bounds = e.last_bounds.expect("painted");
        let l = &e.lines[line];
        point(
            bounds.left() + l.indent + e.char_width * col as f32 + e.char_width * 0.25,
            bounds.top() + l.top + e.line_height * 0.5,
        )
    })
}

fn down(ed: &Entity<Editor>, vcx: &mut VisualTestContext, pos: Point<Pixels>, clicks: usize) {
    let ev = MouseDownEvent {
        button: MouseButton::Left,
        position: pos,
        modifiers: Modifiers::default(),
        click_count: clicks,
        first_mouse: false,
    };
    ed.update_in(vcx, |e, window, cx| e.on_mouse_down(&ev, window, cx));
}

fn drag(ed: &Entity<Editor>, vcx: &mut VisualTestContext, pos: Point<Pixels>) {
    let ev = MouseMoveEvent {
        position: pos,
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    };
    ed.update_in(vcx, |e, window, cx| e.on_mouse_move(&ev, window, cx));
}

fn up(ed: &Entity<Editor>, vcx: &mut VisualTestContext, pos: Point<Pixels>) {
    let ev = MouseUpEvent {
        button: MouseButton::Left,
        position: pos,
        modifiers: Modifiers::default(),
        click_count: 1,
    };
    ed.update_in(vcx, |e, window, cx| e.on_mouse_up(&ev, window, cx));
    vcx.run_until_parked();
}

fn selected(ed: &Entity<Editor>, vcx: &mut VisualTestContext) -> String {
    ed.read_with(vcx, |e, _| {
        let r = e.active.selected_range.clone();
        e.active.buffer.slice_to_string(r)
    })
}

fn clicks(ed: &Entity<Editor>, vcx: &mut VisualTestContext) -> String {
    ed.read_with(vcx, |e, _| {
        e.script.eval("clicks").unwrap().value.unwrap_or_default()
    })
}

#[gpui::test]
fn dragging_in_a_special_buffer_selects_text(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed(cx, false);
    let from = at(&ed, vcx, 0, 6);
    let to = at(&ed, vcx, 1, 5);
    down(&ed, vcx, from, 1);
    drag(&ed, vcx, to);
    up(&ed, vcx, to);
    assert_eq!(selected(&ed, vcx), "beta\ngamma");
}

#[gpui::test]
fn double_click_in_a_special_buffer_selects_the_word(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed(cx, false);
    let pos = at(&ed, vcx, 1, 7);
    down(&ed, vcx, pos, 1);
    up(&ed, vcx, pos);
    down(&ed, vcx, pos, 2);
    up(&ed, vcx, pos);
    assert_eq!(selected(&ed, vcx), "delta");
}

#[gpui::test]
fn a_click_runs_the_mouse_1_binding_on_release(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed(cx, true);
    let pos = at(&ed, vcx, 1, 2);
    down(&ed, vcx, pos, 1);
    assert_eq!(clicks(&ed, vcx), "0", "the binding waits for the release");
    // Pointer jitter within the slop is still a click.
    drag(&ed, vcx, point(pos.x + px(2.), pos.y + px(1.)));
    up(&ed, vcx, point(pos.x + px(2.), pos.y + px(1.)));
    assert_eq!(clicks(&ed, vcx), "1");
    assert_eq!(selected(&ed, vcx), "");
    ed.read_with(vcx, |e, _| {
        assert_eq!(
            e.active.buffer.line_index_at_byte(e.point()),
            1,
            "point on the clicked line"
        );
    });
}

#[gpui::test]
fn a_drag_selects_instead_of_running_the_mouse_1_binding(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed(cx, true);
    let from = at(&ed, vcx, 0, 0);
    let to = at(&ed, vcx, 0, 5);
    down(&ed, vcx, from, 1);
    drag(&ed, vcx, to);
    up(&ed, vcx, to);
    assert_eq!(clicks(&ed, vcx), "0");
    assert_eq!(selected(&ed, vcx), "alpha");
}

/// Under evil a drag in a special buffer stays a plain region: the buffer's
/// keymap gets every key there, so Visual would only leak into the next
/// buffer. Cmd-C copies the region.
#[gpui::test]
fn evil_drag_in_a_special_buffer_copies_without_entering_visual(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, false, "(evil-mode)");
    ed.update(vcx, |e, _| e.evil = EvilState::Normal);
    let from = at(&ed, vcx, 1, 0);
    let to = at(&ed, vcx, 1, 5);
    down(&ed, vcx, from, 1);
    drag(&ed, vcx, to);
    up(&ed, vcx, to);
    assert_eq!(selected(&ed, vcx), "gamma");
    ed.read_with(vcx, |e, _| {
        assert!(e.evil == EvilState::Normal, "still Normal")
    });
    let pos = at(&ed, vcx, 0, 2);
    down(&ed, vcx, pos, 2);
    up(&ed, vcx, pos);
    assert_eq!(selected(&ed, vcx), "alpha");
    ed.read_with(vcx, |e, _| {
        assert!(e.evil == EvilState::Normal, "still Normal")
    });
    let copy = gpui::Keystroke::parse("cmd-c").unwrap();
    ed.update(vcx, |e, cx| e.dispatch_keystroke(&copy, cx));
    vcx.run_until_parked();
    let clip = vcx.read_from_clipboard().and_then(|c| c.text());
    assert_eq!(clip.as_deref(), Some("alpha"));
}

/// Feed `seq` (space-separated keys) and let the Scheme commands apply.
fn keys(ed: &Entity<Editor>, vcx: &mut VisualTestContext, seq: &str) {
    for k in seq.split(' ').filter(|k| !k.is_empty()) {
        let ks = if k.chars().count() == 1 {
            super::ex::token_keystroke(k).unwrap()
        } else {
            gpui::Keystroke::parse(k).unwrap()
        };
        ed.update(vcx, |e, cx| e.dispatch_keystroke(&ks, cx));
        for _ in 0..4 {
            vcx.run_until_parked();
            ed.read_with(vcx, |e, _| {
                let _ = e.script.eval("0");
            });
        }
        vcx.run_until_parked();
    }
}

fn clipboard(vcx: &mut VisualTestContext) -> Option<String> {
    vcx.read_from_clipboard().and_then(|c| c.text())
}

fn normal(ed: &Entity<Editor>, vcx: &mut VisualTestContext) -> bool {
    ed.read_with(vcx, |e, _| {
        e.evil == EvilState::Normal && !e.active.mark_active
    })
}

#[gpui::test]
fn emacs_mark_and_copy_work_in_a_special_buffer(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, false, "");
    keys(&ed, vcx, "ctrl-space alt-f alt-w");
    assert_eq!(clipboard(vcx).as_deref(), Some("alpha"));
}

#[gpui::test]
fn evil_v_selects_and_y_copies_in_a_special_buffer(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, true, "(evil-mode)");
    keys(&ed, vcx, "n w");
    assert!(normal(&ed, vcx), "w is not a motion outside a selection");
    keys(&ed, vcx, "v e");
    assert!(ed.read_with(vcx, |e, _| e.evil == EvilState::Visual));
    assert_eq!(selected(&ed, vcx), "gamm", "inclusive of the cursor char");
    keys(&ed, vcx, "y");
    assert_eq!(clipboard(vcx).as_deref(), Some("gamma"));
    assert_eq!(
        clicks(&ed, vcx),
        "0",
        "y copied instead of reaching the buffer"
    );
    assert!(normal(&ed, vcx));
    ed.read_with(vcx, |e, _| {
        let start = e.active.buffer.line_range(1).start;
        assert_eq!(e.point(), start, "cursor back at the selection start");
    });
    keys(&ed, vcx, "y");
    assert_eq!(
        clicks(&ed, vcx),
        "1",
        "outside a selection y is the buffer's"
    );
}

#[gpui::test]
fn evil_shift_v_copies_whole_lines(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, false, "(evil-mode)");
    keys(&ed, vcx, "shift-v j y");
    assert_eq!(clipboard(vcx).as_deref(), Some("alpha beta\ngamma delta\n"));
    assert!(normal(&ed, vcx));
}

#[gpui::test]
fn helix_v_selects_and_y_copies_in_a_special_buffer(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, false, "(helix-mode)");
    keys(&ed, vcx, "v w w y");
    assert_eq!(clipboard(vcx).as_deref(), Some("alpha beta\ng"));
    assert!(normal(&ed, vcx));
}

#[gpui::test]
fn escape_ends_a_selection_without_copying(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, false, "(evil-mode)");
    keys(&ed, vcx, "v w");
    ed.update(vcx, |e, cx| assert!(e.special_key("escape", cx)));
    assert!(normal(&ed, vcx));
    assert_eq!(clipboard(vcx), None);
}

#[gpui::test]
fn a_v_the_buffer_binds_keeps_its_meaning(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, true, "(evil-mode)");
    ed.update(vcx, |e, cx| {
        e.run_script_blocking("(define-key \"mt-mode\" \"v\" 'mt-click)", cx)
    });
    keys(&ed, vcx, "v");
    assert_eq!(clicks(&ed, vcx), "1");
    assert!(normal(&ed, vcx));
}

#[gpui::test]
fn leaving_the_buffer_ends_the_selection(cx: &mut TestAppContext) {
    let (ed, vcx) = windowed_with(cx, false, "(evil-mode)");
    keys(&ed, vcx, "v w");
    ed.update(vcx, |e, _| {
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("other.txt".into(), None, "text\n"));
    });
    assert!(ed.read_with(vcx, |e, _| e.evil == EvilState::Normal
        && e.special_select.is_none()));
}
