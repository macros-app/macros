//! Audit probes.
use super::grid::SHEET;
use super::*;
use gpui::{Entity, Keystroke, TestAppContext, VisualTestContext};
use std::path::PathBuf;

fn globals(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(crate::theme::Theme::default());
        cx.set_global(crate::settings::Settings::default());
    });
}

fn tmp(csv: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-grid-audit-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.csv");
    std::fs::write(&path, csv).unwrap();
    path
}

fn boot(e: &mut Editor, path: &PathBuf, evil: bool, cx: &mut Context<Editor>) {
    for src in crate::runtime::startup_scripts() {
        e.run_script_blocking(src, cx);
    }
    if evil {
        e.run_script_blocking("(evil-mode)", cx);
    }
    e.mark_config_loaded(cx);
    if evil {
        e.evil = EvilState::Normal;
    }
    e.open_grid(
        "data.csv".into(),
        path.clone(),
        std::fs::read(path).unwrap(),
        cx,
    );
}

fn setup(cx: &mut TestAppContext, csv: &str, evil: bool) -> (Entity<Editor>, PathBuf) {
    globals(cx);
    let path = tmp(csv);
    let editor = cx.new(Editor::new);
    let p = path.clone();
    editor.update(cx, |e, cx| boot(e, &p, evil, cx));
    settle(&editor, cx);
    (editor, path)
}

/// A windowed editor (so prepaint/paint run).
fn setup_window<'a>(
    cx: &'a mut TestAppContext,
    csv: &str,
) -> (Entity<Editor>, &'a mut VisualTestContext) {
    globals(cx);
    let path = tmp(csv);
    let (editor, vcx) = cx.add_window_view(|_w, cx| Editor::new(cx));
    editor.update(vcx, |e, cx| boot(e, &path, false, cx));
    vcx.run_until_parked();
    (editor, vcx)
}

fn settle(editor: &Entity<Editor>, cx: &mut TestAppContext) {
    for _ in 0..6 {
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let _ = e.script.eval("0");
        });
        cx.run_until_parked();
    }
}

fn keys(editor: &Entity<Editor>, cx: &mut TestAppContext, seq: &str) {
    for k in seq.split(' ').filter(|k| !k.is_empty()) {
        let action = matches!(
            k,
            "enter"
                | "tab"
                | "up"
                | "down"
                | "left"
                | "right"
                | "home"
                | "end"
                | "backspace"
                | "delete"
        );
        editor.update(cx, |e, cx| {
            if action {
                if !e.grid_action_key(k, cx) {
                    panic!("grid did not take action key {k}");
                }
            } else {
                let ks: Keystroke = if k.chars().count() == 1 {
                    super::ex::token_keystroke(k).expect("printable key")
                } else {
                    Keystroke::parse(k).expect("keystroke")
                };
                e.dispatch_keystroke(&ks, cx);
            }
        });
        settle(editor, cx);
    }
}

fn cell(editor: &Entity<Editor>, cx: &mut TestAppContext, r: usize, c: usize) -> String {
    editor.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().cell(r, c).to_string()
    })
}
fn stored(editor: &Entity<Editor>, cx: &mut TestAppContext, r: usize, c: usize) -> String {
    editor.read_with(cx, |e, _| {
        e.active
            .grid
            .as_ref()
            .unwrap()
            .wb
            .cell(SHEET, r, c)
            .to_string()
    })
}
fn is_formula(editor: &Entity<Editor>, cx: &mut TestAppContext, r: usize, c: usize) -> bool {
    editor.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().is_formula_at(r, c)
    })
}
fn scheme(editor: &Entity<Editor>, cx: &mut TestAppContext, src: &str) {
    editor.update(cx, |e, _| {
        e.publish_point();
        e.script.dispatch(src, false);
    });
    settle(editor, cx);
}
fn eval(editor: &Entity<Editor>, cx: &mut TestAppContext, src: &str) -> String {
    editor.update(cx, |e, _| e.publish_point());
    editor.read_with(cx, |e, _| match e.script.eval(src) {
        Ok(v) => v.value.unwrap_or_default(),
        Err(err) => format!("ERR: {err:?}"),
    })
}
fn status(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
    editor.read_with(cx, |e, _| e.messages.join(" | "))
}
fn clear_msgs(editor: &Entity<Editor>, cx: &mut TestAppContext) {
    editor.update(cx, |e, _| e.messages.clear());
}
fn prompt(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Option<String> {
    editor.read_with(cx, |e, _| e.minibuffer.as_ref().map(|m| m.prompt.clone()))
}

#[gpui::test]
fn probe_window_draws(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b\n1,2\n");
    vcx.update(|w, _| w.refresh());
    vcx.run_until_parked();
    let geom = ed.read_with(vcx, |e, _| {
        format!("{:?}", e.active.grid.as_ref().map(|g| g.view.viewport))
    });
    eprintln!("viewport after draw: {geom}");
}

// ---- literal "=…" text turns into formulas ---------------------------------

const LIT: &str = "k,v\nx,=foo\ny,=1+1\nz,\n";

#[gpui::test]
fn literal_equals_text_from_csv_is_text(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, LIT, false);
    assert!(!is_formula(&ed, cx, 1, 1));
    assert_eq!(cell(&ed, cx, 2, 1), "=1+1");
}

#[gpui::test]
fn regression_copy_paste_turns_literal_into_formula(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, LIT, false);
    // Copy B3 (literal "=1+1"), paste into B4.
    scheme(&ed, cx, "(grid-move-to 2 1)");
    keys(&ed, cx, "alt-w");
    scheme(&ed, cx, "(grid-move-to 3 1)");
    keys(&ed, cx, "ctrl-y");
    eprintln!(
        "B4 stored={:?} shown={:?}",
        stored(&ed, cx, 3, 1),
        cell(&ed, cx, 3, 1)
    );
    assert!(
        !is_formula(&ed, cx, 3, 1),
        "pasted literal text became a formula: shows {:?}",
        cell(&ed, cx, 3, 1)
    );
}

#[gpui::test]
fn regression_map_cells_turns_literal_into_formula_and_blocks_csv_save(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, LIT, false);
    scheme(&ed, cx, "(grid-select-range 1 1 2 1)");
    scheme(&ed, cx, "(grid-trim-cells)");
    eprintln!(
        "B2 stored={:?} shown={:?}",
        stored(&ed, cx, 1, 1),
        cell(&ed, cx, 1, 1)
    );
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    settle(&ed, cx);
    eprintln!(
        "status after save: {:?} prompt {:?}",
        status(&ed, cx),
        prompt(&ed, cx)
    );
    eprintln!("file: {:?}", std::fs::read_to_string(&path).unwrap());
    assert!(
        !is_formula(&ed, cx, 1, 1),
        "trim turned literal '=foo' into a formula: {:?}",
        cell(&ed, cx, 1, 1)
    );
}

#[gpui::test]
fn regression_upcase_turns_literal_into_formula(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, LIT, false);
    scheme(&ed, cx, "(grid-move-to 1 1)");
    keys(&ed, cx, "alt-u");
    eprintln!(
        "B2 stored={:?} shown={:?}",
        stored(&ed, cx, 1, 1),
        cell(&ed, cx, 1, 1)
    );
    assert!(
        !is_formula(&ed, cx, 1, 1),
        "upcase turned literal '=foo' into formula: {:?}",
        cell(&ed, cx, 1, 1)
    );
}

#[gpui::test]
fn regression_editing_literal_turns_into_formula(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, LIT, false);
    scheme(&ed, cx, "(grid-move-to 2 1)");
    keys(&ed, cx, "f2 0 enter");
    eprintln!(
        "B3 stored={:?} shown={:?}",
        stored(&ed, cx, 2, 1),
        cell(&ed, cx, 2, 1)
    );
    assert!(
        !is_formula(&ed, cx, 2, 1),
        "editing literal text made it a formula: {:?}",
        cell(&ed, cx, 2, 1)
    );
}

// ---- grid-set-cells silently drops non-string values ----------------------

#[gpui::test]
fn scheme_writes_numbers_and_reports_bad_values(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n1,2\n3,4\n", false);
    clear_msgs(&ed, cx);
    scheme(&ed, cx, "(grid-set-cells (list (list 1 0 42)))");
    eprintln!("A2={:?} status={:?}", cell(&ed, cx, 1, 0), status(&ed, cx));
    scheme(&ed, cx, "(grid-select-range 1 0 2 1)");
    scheme(
        &ed,
        cx,
        "(grid-map-cells (lambda (v) (* 2 (string->number v))))",
    );
    eprintln!(
        "after map: A2={:?} B3={:?} status={:?}",
        cell(&ed, cx, 1, 0),
        cell(&ed, cx, 2, 1),
        status(&ed, cx)
    );
    // Numbers are written as their text: 42 doubled, 4 doubled.
    assert_eq!(cell(&ed, cx, 1, 0), "84");
    assert_eq!(cell(&ed, cx, 2, 1), "8");
    scheme(&ed, cx, "(grid-set-cells (list (list 1 0 (list 1 2))))");
    assert!(status(&ed, cx).contains("skipped"), "{}", status(&ed, cx));
}

// ---- rendering ---------------------------------------------------------------

use super::grid_element::GridElement;
use super::grid_element::probe::{Probe, Seen};
use gpui::{point, px, size};
use std::{cell::RefCell, rc::Rc};

fn draw(ed: &Entity<Editor>, vcx: &mut VisualTestContext) -> Seen {
    let seen = Rc::new(RefCell::new(Seen::default()));
    for _ in 0..2 {
        let s2 = seen.clone();
        let e2 = ed.clone();
        vcx.draw(
            point(px(0.), px(0.)),
            size(px(900.), px(600.)),
            move |_, _| Probe {
                inner: GridElement {
                    editor: e2,
                    pane: 0,
                },
                seen: s2,
            },
        );
        vcx.run_until_parked();
    }
    seen.take()
}

fn vscheme(ed: &Entity<Editor>, vcx: &mut VisualTestContext, src: &str) {
    ed.update(vcx, |e, _| {
        e.publish_point();
        e.script.dispatch(src, false);
    });
    for _ in 0..4 {
        vcx.run_until_parked();
        ed.read_with(vcx, |e, _| {
            let _ = e.script.eval("0");
        });
        vcx.run_until_parked();
    }
}

#[gpui::test]
fn regression_render_right_aligned_text_shows_hashes(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "name,b\nA rather long piece of text here,x\n");
    vscheme(&ed, vcx, "(grid-column-align \"A\" 'right)");
    let s = draw(&ed, vcx);
    let texts: Vec<&String> = s.cells.iter().map(|c| &c.0).collect();
    eprintln!("cells: {texts:?}");
    assert!(
        !texts.iter().any(|t| t.starts_with('#')),
        "right-aligned text drawn as hashes: {texts:?}"
    );
}

#[gpui::test]
fn regression_render_long_text_in_number_column_shows_hashes(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "qty,b\n1,x\n2,x\n3,x\n");
    // Type a note into the int column after the file loaded.
    vscheme(&ed, vcx, "(grid-set-cell 4 0 \"pending, see notes\")");
    let s = draw(&ed, vcx);
    let texts: Vec<&String> = s.cells.iter().map(|c| &c.0).collect();
    eprintln!("cells: {texts:?}");
    assert!(
        !texts.iter().any(|t| t.starts_with('#')),
        "text in a number column drawn as hashes: {texts:?}"
    );
}

#[gpui::test]
fn regression_render_show_formulas_overflow_hides_formula(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(
        cx,
        "a,b\nsome long text that spills over into the next cells,\n",
    );
    vscheme(&ed, vcx, "(grid-set-cell 1 1 \"=\\\"\\\"\")");
    vscheme(&ed, vcx, "(grid-show-formulas)");
    let shows = ed.read_with(vcx, |e, _| {
        e.active.grid.as_ref().unwrap().shown_text(1, 1).to_string()
    });
    let s = draw(&ed, vcx);
    let texts: Vec<&String> = s.cells.iter().map(|c| &c.0).collect();
    eprintln!("B2 shown_text={shows:?}; cells: {texts:?}");
    assert!(
        texts.iter().any(|t| t.as_str() == shows),
        "B2's formula text {shows:?} is not drawn: A2's overflow covers it"
    );
}

#[gpui::test]
fn regression_render_huge_column_width_panics(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b\nhello,x\n");
    vscheme(&ed, vcx, "(grid-set-column-width 0 1e20)");
    let w = ed.read_with(vcx, |e, _| {
        e.active.grid.as_ref().unwrap().view.cols.size(0)
    });
    eprintln!("col A width = {w}");
    let s = draw(&ed, vcx);
    eprintln!("cells: {:?}", s.cells);
}

#[gpui::test]
fn regression_render_huge_freeze_is_slow(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b\nhello,x\n");
    vscheme(&ed, vcx, "(grid-freeze-rows 20000000)");
    let t = std::time::Instant::now();
    let _ = draw(&ed, vcx);
    let el = t.elapsed();
    eprintln!("two frames with 20M frozen rows took {el:?}");
    assert!(
        el < std::time::Duration::from_millis(200),
        "frame took {el:?}"
    );
}

#[gpui::test]
fn regression_render_completion_popup_rows_at_low_zoom(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b\n1,2\n");
    vscheme(&ed, vcx, "(grid-set-zoom 10)");
    vscheme(&ed, vcx, "(grid-move-to 3 3)");
    ed.update(vcx, |e, cx| {
        for k in ["=", "s", "u"] {
            let ks = super::ex::token_keystroke(k).unwrap();
            e.dispatch_keystroke(&ks, cx);
        }
    });
    vcx.run_until_parked();
    let open = ed.read_with(vcx, |e, _| e.grid_completion_open());
    let s = draw(&ed, vcx);
    eprintln!(
        "completion open={open}, popup row height={:?}",
        s.popup_row_height
    );
    assert!(
        s.popup_row_height.unwrap_or(99.0) >= 10.0,
        "popup rows {:?} px tall",
        s.popup_row_height
    );
}

#[gpui::test]
fn regression_render_selection_outline_over_frozen_rows(cx: &mut TestAppContext) {
    let csv: String = (0..200).map(|i| format!("r{i},{i}\n")).collect();
    let (ed, vcx) = setup_window(cx, &csv);
    vscheme(&ed, vcx, "(grid-freeze-rows 3)");
    vscheme(&ed, vcx, "(grid-move-to 150 0)");
    let _ = draw(&ed, vcx);
    vscheme(&ed, vcx, "(grid-select-range 10 0 160 1)");
    let s = draw(&ed, vcx);
    let top = s.outlines.iter().map(|q| q.1).fold(f32::INFINITY, f32::min);
    eprintln!(
        "frozen band {}..{}, outline top {top}",
        s.cells_top, s.frozen_bottom
    );
    assert!(
        top >= s.frozen_bottom - 2.0,
        "selection outline drawn over the frozen rows (top {top} < {})",
        s.frozen_bottom
    );
}

#[gpui::test]
fn regression_render_enormous_column_width_overflows(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b\nhello,x\n");
    vscheme(&ed, vcx, "(grid-set-column-width 0 1e30)");
    let s = draw(&ed, vcx);
    eprintln!("cells: {:?}", s.cells);
}

#[gpui::test]
fn regression_formula_bar_click_caret(cx: &mut TestAppContext) {
    let text: String = "abcdefghij".repeat(6);
    let (ed, vcx) = setup_window(cx, &format!("{text},b\n1,2\n"));
    let _ = draw(&ed, vcx);
    let chrome_w = vcx.update(|window, cx| {
        let s = cx.global::<crate::settings::Settings>();
        let size = px(s.font_size().min(14.0) * 0.92);
        let f = gpui::font(s.font_family().to_string());
        let run = gpui::TextRun {
            len: 1,
            font: f,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        f32::from(
            window
                .text_system()
                .shape_line("0".into(), size, &[run], None)
                .x_for_index(1),
        )
    });
    let geo = ed.read_with(vcx, |e, _| *e.grid_ui.geom.get(&0).unwrap());
    eprintln!("geo {geo:?}");
    let (ox, oy) = (f32::from(geo.origin.x), f32::from(geo.origin.y));
    let x = ox + 200.0 + 34.0 + 40.0 * chrome_w + 0.3 * chrome_w;
    ed.update(vcx, |e, cx| {
        assert!(e.grid_mouse_down(
            point(px(x), px(oy + 12.)),
            gpui::MouseButton::Left,
            1,
            false,
            cx
        ));
    });
    let caret = ed.read_with(vcx, |e, _| {
        e.active
            .grid
            .as_ref()
            .unwrap()
            .view
            .edit
            .as_ref()
            .map(|e| e.caret)
    });
    eprintln!("chrome char w {chrome_w}; clicked over char 40; caret {caret:?}");
    assert_eq!(caret, Some(40));
}

#[gpui::test]
fn regression_header_click_left_edge_after_hidden_col_collapses(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b,c\n1,2,3\n");
    vscheme(&ed, vcx, "(grid-set-column-hidden 0 #t)");
    let _ = draw(&ed, vcx);
    let geo = ed.read_with(vcx, |e, _| *e.grid_ui.geom.get(&0).unwrap());
    eprintln!("geo {geo:?}");
    let (ox, oy) = (f32::from(geo.origin.x), f32::from(geo.origin.y));
    let (x0, y) = (ox + geo.header_w + 1.0, oy + geo.bar_h + geo.header_h / 2.0);
    // Click column B's header 1 px inside its left edge, wobbling 1 px.
    ed.update(vcx, |e, cx| {
        e.grid_mouse_down(point(px(x0), px(y)), gpui::MouseButton::Left, 1, false, cx);
        eprintln!("drag {:?}", e.grid_ui.drag);
        e.grid_mouse_move(point(px(x0 + 1.0), px(y)), cx);
        eprintln!("drag {:?}", e.grid_ui.drag);
        e.grid_mouse_up(cx);
    });
    let (w, sel) = ed.read_with(vcx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        (g.view.cols.size(1), g.view.anchor)
    });
    eprintln!("col B width after click: {w}; anchor {sel:?}");
    assert!(w > 10.0, "clicking column B's header collapsed it to {w}px");
}

const NUMS: &str = "item,n,day\na,1,Mon\nb,5,Tue\nc,3,Wed\nd,8,Thu\n";

#[gpui::test]
fn regression_paste_over_filtered_range_writes_hidden_rows(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    scheme(&ed, cx, "(grid-filter-rows \"B\" \">\" 2)");
    assert_eq!(eval(&ed, cx, "(grid-row-visible? 1)"), "#false");
    scheme(&ed, cx, "(grid-select-range 1 0 4 0)");
    ed.update(cx, |_, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("X".into()))
    });
    keys(&ed, cx, "ctrl-y");
    eprintln!(
        "hidden A2 = {:?}; A3 = {:?}",
        cell(&ed, cx, 1, 0),
        cell(&ed, cx, 2, 0)
    );
    assert_eq!(
        cell(&ed, cx, 1, 0),
        "a",
        "paste overwrote a row the filter hides"
    );
}

#[gpui::test]
fn regression_paste_block_over_filtered_range_writes_hidden_rows(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    scheme(&ed, cx, "(grid-filter-rows \"B\" \">\" 4)");
    assert_eq!(eval(&ed, cx, "(grid-row-visible? 3)"), "#false");
    scheme(&ed, cx, "(grid-move-to 2 2)");
    ed.update(cx, |_, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("P\nQ\nR\n".into()))
    });
    keys(&ed, cx, "ctrl-y");
    eprintln!(
        "C3..C5 = {:?} {:?} {:?}",
        cell(&ed, cx, 2, 2),
        cell(&ed, cx, 3, 2),
        cell(&ed, cx, 4, 2)
    );
}

#[gpui::test]
fn regression_upcase_over_filtered_range(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    scheme(&ed, cx, "(grid-filter-rows \"B\" \">\" 2)");
    scheme(&ed, cx, "(grid-select-range 1 0 4 0)");
    keys(&ed, cx, "alt-u");
    eprintln!(
        "hidden A2 = {:?}; A3 = {:?}",
        cell(&ed, cx, 1, 0),
        cell(&ed, cx, 2, 0)
    );
}

#[gpui::test]
fn regression_mcp_grid_text(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    scheme(&ed, cx, "(grid-set-cell 4 2 \"x\\ry\")");
    scheme(&ed, cx, "(grid-filter-rows \"B\" \">\" 4)");
    let out = ed.read_with(cx, |e, _| e.mcp_buffer_text(None, None, false).unwrap());
    eprintln!("{out:?}");
    assert!(!out.contains('\r'), "raw CR in grid_text output");
}

#[gpui::test]
fn regression_render_extremes_smoke(cx: &mut TestAppContext) {
    let (ed, vcx) = setup_window(cx, "a,b,c\n1,2,3\n4,5,6\n");
    for src in [
        "(grid-set-zoom 10)",
        "(grid-move-to 5000000000 20000000000)",
        "(grid-set-zoom 400)",
        "(grid-freeze-rows 2)",
        "(grid-set-row-height 0 0)",
        "(grid-set-column-width 2147483647 0)",
        "(grid-set-zoom 10)",
        "(grid-filter-rows \"A\" \">\" 100)",
        "(grid-move-to 0 0)",
        "(grid-set-column-hidden 0 #t)",
        "(grid-set-column-hidden 1 #t)",
        "(grid-set-column-hidden 2 #t)",
        "(grid-set-row-height 1 100000)",
        "(grid-set-zoom 400)",
    ] {
        vscheme(&ed, vcx, src);
        let s = draw(&ed, vcx);
        let cur = ed.read_with(vcx, |e, _| e.active.grid.as_ref().unwrap().view.cursor);
        eprintln!("{src}: cursor {cur:?}, {} texts", s.cells.len());
    }
}

// ---- typing formulas ------------------------------------------------------------

fn edit_text(ed: &Entity<Editor>, cx: &mut TestAppContext) -> Option<String> {
    ed.read_with(cx, |e, _| {
        e.active
            .grid
            .as_ref()
            .unwrap()
            .view
            .edit
            .as_ref()
            .map(|e| e.text.clone())
    })
}

#[gpui::test]
fn regression_enter_after_whole_column_ref_completes_function(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n1,2\n3,4\n", false);
    scheme(&ed, cx, "(grid-move-to 3 0)");
    keys(&ed, cx, "= s u m ( a : a");
    eprintln!("before enter: {:?}", edit_text(&ed, cx));
    keys(&ed, cx, "enter");
    eprintln!(
        "after enter: edit {:?} source {:?}",
        edit_text(&ed, cx),
        stored(&ed, cx, 3, 0)
    );
    assert_eq!(edit_text(&ed, cx), None, "Enter didn't commit =SUM(B:B");
}

#[gpui::test]
fn regression_copy_formula_with_empty_result_cannot_paste(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n1,\n2,\n", false);
    scheme(
        &ed,
        cx,
        "(grid-set-cell 1 1 \"=IF(A2>5,\\\"big\\\",\\\"\\\")\")",
    );
    assert_eq!(cell(&ed, cx, 1, 1), "");
    scheme(&ed, cx, "(grid-move-to 1 1)");
    keys(&ed, cx, "alt-w");
    scheme(&ed, cx, "(grid-move-to 2 1)");
    clear_msgs(&ed, cx);
    keys(&ed, cx, "ctrl-y");
    eprintln!(
        "B3 stored {:?}; messages {:?}",
        stored(&ed, cx, 2, 1),
        status(&ed, cx)
    );
    assert!(is_formula(&ed, cx, 2, 1), "copied formula didn't paste");
}

#[gpui::test]
fn regression_copy_formula_block_with_empty_results(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b,c\n1,,\n2,,\n", false);
    scheme(&ed, cx, "(grid-set-cell 1 1 \"=IF(A2>5,1,\\\"\\\")\")");
    scheme(&ed, cx, "(grid-set-cell 2 1 \"=IF(A3>5,1,\\\"\\\")\")");
    scheme(&ed, cx, "(grid-select-range 1 1 2 1)");
    keys(&ed, cx, "alt-w");
    let kill = ed.read_with(cx, |e, _| e.kill_ring.last().cloned());
    scheme(&ed, cx, "(grid-move-to 1 2)");
    clear_msgs(&ed, cx);
    keys(&ed, cx, "ctrl-y");
    eprintln!(
        "kill {kill:?}; C2 {:?} C3 {:?}; messages {:?}",
        stored(&ed, cx, 1, 2),
        stored(&ed, cx, 2, 2),
        status(&ed, cx)
    );
    assert!(
        is_formula(&ed, cx, 1, 2) && is_formula(&ed, cx, 2, 2),
        "formulas lost on paste"
    );
}

#[gpui::test]
fn regression_point_mode_lands_on_hidden_column(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b,c\n1,2,3\n", false);
    scheme(&ed, cx, "(grid-set-column-hidden 1 #t)");
    scheme(&ed, cx, "(grid-move-to 1 0)");
    keys(&ed, cx, "= right");
    eprintln!("edit text {:?}", edit_text(&ed, cx));
    assert_eq!(
        edit_text(&ed, cx).as_deref(),
        Some("=C2"),
        "pointed at hidden column B"
    );
}

#[test]
fn regression_unclosed_parens_quoted_sheet() {
    use crate::formula::engine::store_formula;
    let got = super::grid_formula::stored_input("=SUM('Q (1)'!A1:A3");
    eprintln!("stored {got:?}");
    assert_eq!(got, store_formula("=SUM('Q (1)'!A1:A3)"));
}

#[test]
fn regression_call_context_quoted_sheet() {
    use crate::formula::parse::call_context;
    let t = "=VLOOKUP(A1,'Q (1)'!A:B,";
    eprintln!("{:?}", call_context(t, t.len()));
    assert_eq!(call_context(t, t.len()), Some(("VLOOKUP".into(), 2)));
    let t = "=SUM('a,b'!A1,";
    eprintln!("{:?}", call_context(t, t.len()));
    assert_eq!(call_context(t, t.len()), Some(("SUM".into(), 1)));
}

// ---- Scheme API -------------------------------------------------------------------

#[gpui::test]
fn regression_grid_fn_shadowing_builtin_is_refused(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n1,2\n", false);
    clear_msgs(&ed, cx);
    scheme(&ed, cx, "(define-grid-function (SUM x) 42)");
    assert!(status(&ed, cx).contains("built-in"), "{}", status(&ed, cx));
    scheme(&ed, cx, "(grid-set-cell 1 2 \"=SUM(5)\")");
    assert_eq!(cell(&ed, cx, 1, 2), "5");
}

#[test]
fn regression_grid_fn_char_literal() {
    use crate::grid_fn::{GridFnEngine, GridFnOutcome};
    let (e, mut rx) = GridFnEngine::new();
    let src = format!(
        "(define (TABS) (string {}))",
        crate::grid_fn::write_datum(&steel::rvals::SteelVal::CharV('\t'))
    );
    eprintln!("source {src:?}");
    e.define(src);
    e.call(1, "TABS", &[]);
    let start = std::time::Instant::now();
    loop {
        if let Ok(o) = rx.try_recv() {
            match o {
                GridFnOutcome::DefineFailed { error } => eprintln!("define failed: {error}"),
                GridFnOutcome::Called { result, .. } => {
                    eprintln!("call: {result:?}");
                    assert_eq!(result, Ok(crate::formula::value::Value::text("\t")));
                    break;
                }
            }
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn regression_write_datum_round_trip() {
    use steel::rvals::SteelVal;
    for v in [
        SteelVal::CharV('\t'),
        SteelVal::NumV(1.0),
        SteelVal::NumV(1e21),
        SteelVal::NumV(0.1),
        SteelVal::CharV('('),
    ] {
        eprintln!("{v:?} -> {:?}", crate::grid_fn::write_datum(&v));
    }
}

#[gpui::test]
fn regression_set_cell_far_away_is_silent(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n1,2\n", false);
    clear_msgs(&ed, cx);
    scheme(&ed, cx, "(grid-set-cell 100000000 50000 \"x\")");
    eprintln!("messages {:?}", status(&ed, cx));
    clear_msgs(&ed, cx);
    let t = std::time::Instant::now();
    let n = std::env::var("AUDIT_N").unwrap_or("1000".into());
    scheme(&ed, cx, &format!("(grid-insert-rows 0 {n})"));
    eprintln!(
        "insert huge: {:?} messages {:?} dims {}",
        t.elapsed(),
        status(&ed, cx),
        eval(&ed, cx, "(grid-dimensions)")
    );
}

// ---- range fill ------------------------------------------------------------

#[gpui::test]
fn regression_fill_down_keeps_literal_equals_text_as_text(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, LIT, false);
    // B3 holds the literal text "=1+1": filling it down must not make a
    // formula of it.
    scheme(&ed, cx, "(grid-select-range 2 1 3 1)");
    scheme(&ed, cx, "(grid-fill-down)");
    assert!(
        !is_formula(&ed, cx, 3, 1),
        "shows {:?}",
        cell(&ed, cx, 3, 1)
    );
    assert_eq!(cell(&ed, cx, 3, 1), "=1+1");
    // Nor the fill-selection commit (`'=…` is Excel's escape for it).
    scheme(&ed, cx, "(grid-select-range 1 0 2 0)");
    keys(&ed, cx, "' = 2 ctrl-enter");
    assert!(!is_formula(&ed, cx, 1, 0) && !is_formula(&ed, cx, 2, 0));
    assert_eq!(
        (cell(&ed, cx, 1, 0), cell(&ed, cx, 2, 0)),
        ("=2".into(), "=2".into())
    );
}

#[gpui::test]
fn regression_fill_down_over_whole_columns_spares_the_header(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "k,v\nx,1\ny,2\nz,3\n", false);
    // A whole-column selection starts at the header: fill from row 2.
    scheme(&ed, cx, "(grid-select-range 0 1 3 1)");
    scheme(&ed, cx, "(grid-fill-down)");
    assert_eq!(cell(&ed, cx, 0, 1), "v", "the header stays");
    assert_eq!(
        (cell(&ed, cx, 2, 1), cell(&ed, cx, 3, 1)),
        ("1".into(), "1".into())
    );
    // The fill-selection command leaves it alone too.
    scheme(&ed, cx, "(grid-select-range 0 0 3 0)");
    scheme(&ed, cx, "(grid-fill-selection)");
    assert_eq!(cell(&ed, cx, 0, 0), "k");
    assert_eq!(cell(&ed, cx, 1, 0), "z", "from the cursor cell, A4");
}

#[gpui::test]
fn regression_fill_cancel_and_formula_errors(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n1,2\n3,4\n", false);
    // Typing over a range, then C-g, changes nothing.
    scheme(&ed, cx, "(grid-select-range 1 0 2 1)");
    keys(&ed, cx, "9 ctrl-g");
    assert_eq!(cell(&ed, cx, 1, 0), "1");
    assert_eq!(cell(&ed, cx, 2, 1), "4");
    scheme(&ed, cx, "(grid-set-cell 2 1 \"=A1\")");
    scheme(&ed, cx, "(grid-select-range 2 0 2 1)");
    scheme(&ed, cx, "(grid-fill-right)");
    assert_eq!(cell(&ed, cx, 2, 1), "3", "B3 from A3, a value");
    // `=A1` typed in B3 and committed over A2:B3: A2's copy moves up a row
    // and left a column, off the sheet.
    scheme(&ed, cx, "(grid-select-range 1 0 2 1)");
    keys(&ed, cx, "= a 1 ctrl-enter");
    assert_eq!(cell(&ed, cx, 1, 0), "#REF!", "{:?}", stored(&ed, cx, 1, 0));
    assert_eq!(cell(&ed, cx, 2, 0), "#REF!", "A3 = A1 one column left");
}
