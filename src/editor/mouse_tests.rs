//! Mouse selection in special (read-only, program-generated) buffers: a drag
//! selects text, and a `mouse-1` binding runs only on a click, as in Emacs.
//! Real mouse events go through `on_mouse_down` / `on_mouse_move` /
//! `on_mouse_up` in a painted test window.

use gpui::{
    Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, TestAppContext,
    VisualTestContext,
};

use super::*;

const TEXT: &str = "alpha beta\\ngamma delta\\nepsilon\\n";

/// A special buffer in a window, painted so clicks map to text. `bind` is an
/// optional `mouse-1` command for the buffer's mode.
fn windowed(cx: &mut TestAppContext, bind: bool) -> (Entity<Editor>, &mut VisualTestContext) {
    test_context(cx);
    let (ed, vcx) = cx.add_window_view(|_w, cx| Editor::new(cx));
    ed.update(vcx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        e.mark_config_loaded(cx);
        e.run_script_blocking("(define clicks 0)", cx);
        if bind {
            e.run_script_blocking("(define-command (mt-click) (set! clicks (+ clicks 1)))", cx);
            e.run_script_blocking("(define-key \"mt-mode\" \"mouse-1\" 'mt-click)", cx);
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
