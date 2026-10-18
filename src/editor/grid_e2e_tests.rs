//! End-to-end grid-buffer tests: a real `Editor` on gpui's headless test
//! platform with the bundled Scheme (so `scheme/grid.scm`'s keymaps are the
//! ones exercised), a CSV opened from a temp file, keys fed through the same
//! layers as typing, and saves checked byte for byte on disk.

use std::path::PathBuf;

use gpui::{Entity, Keystroke, TestAppContext};

use super::grid::SHEET;
use super::*;

fn setup(cx: &mut TestAppContext, csv: &str, evil: bool) -> (Entity<Editor>, PathBuf) {
    cx.update(|cx| {
        cx.set_global(crate::theme::Theme::default());
        cx.set_global(crate::settings::Settings::default());
    });
    // A counter as well as the clock: parallel tests can read the same
    // timestamp, and a shared directory means one test saves over another's file.
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-grid-e2e-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.csv");
    std::fs::write(&path, csv).unwrap();
    let editor = cx.new(Editor::new);
    let p = path.clone();
    editor.update(cx, |e, cx| {
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
        e.open_grid("data.csv".into(), p.clone(), std::fs::read(&p).unwrap(), cx);
    });
    settle(&editor, cx);
    (editor, path)
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

/// Feed keys: space-separated tokens. Action keys (`enter`, `tab`, arrows,
/// `backspace`) go through the gpui-action path the way the platform routes
/// them; everything else through `dispatch_keystroke`.
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

fn cursor(editor: &Entity<Editor>, cx: &mut TestAppContext) -> (usize, usize) {
    editor.read_with(cx, |e, _| e.active.grid.as_ref().unwrap().view.cursor)
}

fn save(editor: &Entity<Editor>, cx: &mut TestAppContext) {
    editor.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    settle(editor, cx);
    // The write is on the background executor.
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

const CSV: &str = "name,qty,note\napple,3,\"red, crisp\"\npear,007,\nfig,12,ok\n";

#[gpui::test]
fn opens_csv_as_grid_with_header(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().expect("a grid buffer");
        assert_eq!(g.used(), (4, 3));
        assert!(g.wb.header(SHEET), "row 1 detected as header");
        assert_eq!(g.view.frozen_rows, 0, "nothing frozen by default");
        assert_eq!(e.active.special.as_deref(), Some("grid-mode"));
    });
    assert_eq!(cell(&ed, cx, 1, 2), "red, crisp");
}

#[gpui::test]
fn evil_motion_counts_and_edges(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j l");
    assert_eq!(cursor(&ed, cx), (1, 1));
    keys(&ed, cx, "shift-g");
    assert_eq!(cursor(&ed, cx), (3, 1), "G = last used row");
    keys(&ed, cx, "2 shift-g");
    assert_eq!(cursor(&ed, cx), (1, 1), "2G = row 2");
    keys(&ed, cx, "$");
    assert_eq!(cursor(&ed, cx), (1, 2));
    keys(&ed, cx, "0 w w w");
    assert_eq!(cursor(&ed, cx), (2, 0), "w wraps to the next row");
    keys(&ed, cx, "}");
    assert_eq!(cursor(&ed, cx), (3, 0), "edge jump stops at the data's end");
    // Infinite scroll: the cursor goes past the data freely.
    keys(&ed, cx, "5 0 0 0 shift-g");
    assert_eq!(cursor(&ed, cx), (4999, 0));
}

#[gpui::test]
fn edit_commit_and_save_changes_one_record(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    // Row 3 (pear), column B: replace 007 with 8.
    keys(&ed, cx, "j j l c 8 escape");
    assert_eq!(cell(&ed, cx, 2, 1), "8");
    ed.read_with(cx, |e, _| assert!(e.active.modified, "edit marks modified"));
    save(&ed, cx);
    let disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        disk,
        "name,qty,note\napple,3,\"red, crisp\"\npear,8,\nfig,12,ok\n"
    );
    ed.read_with(cx, |e, _| {
        assert!(!e.active.modified, "saved clears modified")
    });
}

#[gpui::test]
fn save_while_editing_commits_the_pending_edit(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    keys(&ed, cx, "j a");
    keys(&ed, cx, "s");
    save(&ed, cx);
    let disk = std::fs::read_to_string(&path).unwrap();
    assert!(disk.contains("\napples,3,"), "pending edit saved: {disk}");
    ed.read_with(cx, |e, _| assert!(!e.grid_editing()));
}

#[gpui::test]
fn untouched_and_visited_empty_space_saves_identically(cx: &mut TestAppContext) {
    let quirky = "a;b\r\n\"x\"\"y\";2\r\n;\r\nlast;row";
    let (ed, path) = setup(cx, quirky, true);
    // Wander far past the data, type a value, then clear it again.
    keys(&ed, cx, "5 0 0 0 shift-g c z escape x");
    save(&ed, cx);
    assert_eq!(std::fs::read(&path).unwrap(), quirky.as_bytes());
}

#[gpui::test]
fn values_are_written_exactly_as_typed(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "a\n1\n", true);
    keys(&ed, cx, "j c 0 0 7 escape o c 1 e 5 escape");
    keys(
        &ed,
        cx,
        "o c 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 escape",
    );
    save(&ed, cx);
    let disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(disk, "a\n007\n1e5\n12345678901234567890\n");
}

#[gpui::test]
fn enter_and_tab_commit_and_move(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j c X tab");
    assert_eq!(cell(&ed, cx, 1, 0), "X");
    assert_eq!(cursor(&ed, cx), (1, 1));
    ed.read_with(cx, |e, _| assert!(!e.grid_editing()));
    keys(&ed, cx, "c Y enter");
    assert_eq!(cell(&ed, cx, 1, 1), "Y");
    assert_eq!(cursor(&ed, cx), (2, 1));
    // C-g cancels.
    keys(&ed, cx, "c Z ctrl-g");
    assert_eq!(cell(&ed, cx, 2, 1), "007");
}

#[gpui::test]
fn structure_ops_and_undo(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j d d");
    assert_eq!(cell(&ed, cx, 1, 0), "pear");
    keys(&ed, cx, "u");
    assert_eq!(cell(&ed, cx, 1, 0), "apple");
    keys(&ed, cx, "ctrl-r");
    assert_eq!(cell(&ed, cx, 1, 0), "pear");
    keys(&ed, cx, "u o");
    assert_eq!(cell(&ed, cx, 2, 0), "");
    assert_eq!(cell(&ed, cx, 3, 0), "pear");
    keys(&ed, cx, "u");
    // Sort by qty descending, header stays put.
    keys(&ed, cx, "l g shift-s");
    assert_eq!(cell(&ed, cx, 0, 0), "name");
    assert_eq!(cell(&ed, cx, 1, 0), "fig");
    // Undo back to the saved point clears modified.
    keys(&ed, cx, "u");
    ed.read_with(cx, |e, _| assert!(!e.active.modified));
}

#[gpui::test]
fn resize_widen_autofit_and_undo(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    let width = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| e.active.grid.as_ref().unwrap().view.cols.size(2))
    };
    assert_eq!(width(&ed, cx), crate::grid::view::DEFAULT_COL_WIDTH);
    keys(&ed, cx, "l l >");
    assert!(width(&ed, cx) > crate::grid::view::DEFAULT_COL_WIDTH);
    keys(&ed, cx, "u");
    assert_eq!(width(&ed, cx), crate::grid::view::DEFAULT_COL_WIDTH);
}

#[gpui::test]
fn emacs_typing_replaces_the_cell(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    keys(&ed, cx, "ctrl-n ctrl-f");
    assert_eq!(cursor(&ed, cx), (1, 1));
    keys(&ed, cx, "4 2 enter");
    assert_eq!(cell(&ed, cx, 1, 1), "42");
    assert_eq!(cursor(&ed, cx), (2, 1));
    // C-x C-s still reaches the global map from a grid.
    keys(&ed, cx, "ctrl-x");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.pending_keys, vec!["ctrl-x".to_string()])
    });
}

#[gpui::test]
fn copy_paste_tsv_block(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j v l y");
    let clip = cx.read_from_clipboard().and_then(|i| i.text()).unwrap();
    assert_eq!(clip, "apple\t3\n");
    keys(&ed, cx, "0 j j j p");
    assert_eq!(cell(&ed, cx, 4, 0), "apple");
    assert_eq!(cell(&ed, cx, 4, 1), "3");
}

#[gpui::test]
fn formula_bar_text_and_range_summary(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j l");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.bar_text(), ("B2 · qty".to_string(), "3".to_string()));
    });
    keys(&ed, cx, "v j j");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.bar_text().0, "B2:B4");
        assert_eq!(g.summary, "3 cells · Sum 22 · Avg 7.33 · Min 3 · Max 12");
    });
}

#[gpui::test]
fn goto_address_and_header_toggle(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    ed.update(cx, |e, cx| e.grid_goto_address("c4", cx));
    assert_eq!(cursor(&ed, cx), (3, 2));
    keys(&ed, cx, "g h");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert!(!g.wb.header(SHEET));
    });
}

#[gpui::test]
fn freeze_is_opt_in_and_toggles(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    let frozen = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| e.active.grid.as_ref().unwrap().view.frozen_rows)
    };
    assert_eq!(frozen(&ed, cx), 0);
    keys(&ed, cx, "g f");
    assert_eq!(frozen(&ed, cx), 1, "cursor on row 1 freezes row 1");
    keys(&ed, cx, "g f j j g f");
    assert_eq!(frozen(&ed, cx), 2, "freezes the rows above the cursor");
    keys(&ed, cx, "g f");
    assert_eq!(frozen(&ed, cx), 0);
}

#[gpui::test]
fn toggle_between_grid_and_raw_text(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    keys(&ed, cx, "j c X escape");
    // Grid → raw text: same buffer and file, the unsaved edit carried over.
    keys(&ed, cx, "g r");
    ed.read_with(cx, |e, _| {
        assert!(e.active.grid.is_none());
        assert_eq!(e.active.name, "data.csv");
        assert_eq!(e.active.path.as_deref(), Some(path.as_path()));
        assert!(e.active.modified);
        assert!(
            e.active
                .buffer
                .full_text()
                .starts_with("name,qty,note\nX,3,")
        );
    });
    // Raw text → grid with the same key, still modified, still edited.
    keys(&ed, cx, "ctrl-c ctrl-t");
    assert_eq!(cell(&ed, cx, 1, 0), "X");
    ed.read_with(cx, |e, _| assert!(e.active.modified));
    save(&ed, cx);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("name,qty,note\nX,3,")
    );
}

#[gpui::test]
fn leader_sequences_pass_through_the_grid(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    let leader_bound = ed.read_with(cx, |e, _| {
        !matches!(
            e.keymap.resolve_leader(None, "space"),
            crate::mode::LeaderMatch::Miss
        )
    });
    if leader_bound {
        keys(&ed, cx, "space");
        ed.read_with(cx, |e, _| {
            assert_eq!(e.pending_keys, vec!["space".to_string()])
        });
    }
}

/// Run Scheme as a dispatched command batch (the path key bindings and
/// `M-x` take), so its commands apply as one batch / undo group.
fn scheme(editor: &Entity<Editor>, cx: &mut TestAppContext, src: &str) {
    editor.update(cx, |e, _| {
        e.publish_point();
        e.script.dispatch(src, false);
    });
    settle(editor, cx);
}

fn eval(editor: &Entity<Editor>, cx: &mut TestAppContext, src: &str) -> String {
    editor.update(cx, |e, _| e.publish_point());
    editor.read_with(cx, |e, _| {
        e.script.eval(src).expect("eval").value.unwrap_or_default()
    })
}

#[gpui::test]
fn scheme_reads_grid_state(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j l");
    assert_eq!(eval(&ed, cx, "(grid-buffer?)"), "#true");
    assert_eq!(eval(&ed, cx, "(grid-cursor)"), "(1 1)");
    assert_eq!(eval(&ed, cx, "(grid-dimensions)"), "(4 3)");
    assert_eq!(eval(&ed, cx, "(grid-current-value)"), "\"3\"");
    assert_eq!(
        eval(&ed, cx, "(grid-header-names)"),
        "(\"name\" \"qty\" \"note\")"
    );
    assert_eq!(eval(&ed, cx, "(grid-column-type \"B\")"), "\"int\"");
    keys(&ed, cx, "v j");
    assert_eq!(eval(&ed, cx, "(grid-selection)"), "(1 1 2 1)");
}

#[gpui::test]
fn scheme_edits_undo_as_one_step(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cell 1 \"A\" \"APPLE\") (grid-set-cells (list (list 2 0 \"PEAR\") (list 3 0 \"FIG\"))) (grid-insert-rows 4 1) (grid-set-cell 4 0 \"kiwi\"))",
    );
    assert_eq!(cell(&ed, cx, 1, 0), "APPLE");
    assert_eq!(cell(&ed, cx, 3, 0), "FIG");
    assert_eq!(cell(&ed, cx, 4, 0), "kiwi");
    keys(&ed, cx, "u");
    assert_eq!(cell(&ed, cx, 1, 0), "apple", "one u undoes the whole batch");
    assert_eq!(cell(&ed, cx, 3, 0), "fig");
    assert_eq!(cell(&ed, cx, 4, 0), "");
    ed.read_with(cx, |e, _| assert!(!e.active.modified));
    // Sort, view and structure calls.
    scheme(
        &ed,
        cx,
        "(begin (grid-sort 1 #t) (grid-move-to 3 \"C\") (grid-freeze-rows 1) (grid-set-zoom 150) (grid-set-column-width 0 120))",
    );
    assert_eq!(cell(&ed, cx, 1, 0), "fig");
    assert_eq!(cursor(&ed, cx), (3, 2));
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.view.frozen_rows, 1);
        assert!((g.view.zoom - 1.5).abs() < 1e-6);
        assert_eq!(g.view.cols.size(0), 120.0);
    });
}

#[gpui::test]
fn cell_changed_and_load_hooks(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    // The hook records what it saw in a global, read back below.
    scheme(
        &ed,
        cx,
        "(begin (define *seen* '()) (define (on-change cs) (set! *seen* cs)) (add-hook \"grid-cell-changed-hook\" \"on-change\"))",
    );
    keys(&ed, cx, "j c Z escape");
    assert_eq!(eval(&ed, cx, "*seen*"), "((1 0 \"apple\" \"Z\"))");
    // Scheme's own writes don't fire it (no feedback loops).
    scheme(
        &ed,
        cx,
        "(begin (set! *seen* 'none) (grid-set-cell 2 0 \"x\"))",
    );
    assert_eq!(eval(&ed, cx, "*seen*"), "none");
}

#[gpui::test]
fn read_range_calls_back_with_rows(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    scheme(
        &ed,
        cx,
        "(begin (define *got* #f) (define (got rows) (set! *got* rows)) (grid-read-range \"A2:B3\" 'got))",
    );
    assert_eq!(
        eval(&ed, cx, "*got*"),
        "((\"apple\" \"3\") (\"pear\" \"007\"))"
    );
}

#[gpui::test]
fn styling_rules_formats_and_errors(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    scheme(
        &ed,
        cx,
        "(begin (grid-format-column \"B\" \"#,##0.00\") (grid-highlight-rule \"B\" \">\" 10 'error) (grid-column-face 0 'keyword))",
    );
    ed.read_with(cx, |e, _| {
        let s = &e.active.grid.as_ref().unwrap().style;
        assert_eq!(s.display(1, "12").as_deref(), Some("12.00"));
        assert_eq!(s.face(0, 1, "12", |_| ""), Some("error"));
        assert_eq!(s.face(0, 1, "3", |_| ""), None);
        assert_eq!(s.face(0, 0, "apple", |_| ""), Some("keyword"));
    });
    // Display-only: the file is unchanged on save.
    save(&ed, cx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), CSV);
    scheme(&ed, cx, "(grid-format-column \"B\" \"nope\")");
    ed.read_with(cx, |e, _| {
        assert!(e.status.contains("not a number format"), "{}", e.status)
    });
    scheme(&ed, cx, "(grid-clear-styles)");
    ed.read_with(cx, |e, _| {
        assert!(!e.active.grid.as_ref().unwrap().style.touches(1));
    });
}

#[gpui::test]
fn load_hook_styles_matching_files(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(crate::theme::Theme::default());
        cx.set_global(crate::settings::Settings::default());
    });
    let dir = std::env::temp_dir().join(format!("macros-grid-hook-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sales.csv");
    std::fs::write(&path, CSV).unwrap();
    let ed = cx.new(Editor::new);
    ed.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        // The documented example from scheme/grid.scm.
        e.run_script_blocking(
            "(define (my-sales-grid name)
               (when (string-contains? name \"sales\")
                 (grid-format-column \"B\" \"$#,##0.00\")
                 (grid-highlight-rule \"B\" \">\" 10 'warning)))
             (add-hook \"grid-load-hook\" \"my-sales-grid\")",
            cx,
        );
        e.mark_config_loaded(cx);
        e.open_grid(
            "sales.csv".into(),
            path.clone(),
            std::fs::read(&path).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let s = &e.active.grid.as_ref().unwrap().style;
        assert_eq!(s.display(1, "12").as_deref(), Some("$12.00"));
        assert_eq!(s.face(0, 1, "12", |_| ""), Some("warning"));
    });
}

// --- Emacs-style commands, filtering, and the extended Scheme API -----------

fn clipboard(editor: &Entity<Editor>, cx: &mut TestAppContext, text: &str) {
    editor.update(cx, |_, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()))
    });
}

/// Type `input` into the open minibuffer and press RET.
fn submit(editor: &Entity<Editor>, cx: &mut TestAppContext, input: &str) {
    editor.update(cx, |e, cx| {
        let mb = e.minibuffer.as_mut().expect("a minibuffer read");
        mb.input = input.to_string();
        mb.cursor = input.len();
        e.mb_submit(cx);
    });
    settle(editor, cx);
}

const NUMS: &str = "item,n,day\na,1,Mon\nb,5,Tue\nc,3,Wed\nd,8,Thu\n";

#[gpui::test]
fn kill_ring_paste_yank_pop_and_quoted_round_trip(cx: &mut TestAppContext) {
    let tricky = "a,b\n\"say \"\"hi\"\"\",\"two\nlines\"\nx,y\n";
    let (ed, path) = setup(cx, tricky, false);
    // Copy the two tricky cells, paste them at B3, and they survive.
    keys(&ed, cx, "down shift-right alt-w down");
    keys(&ed, cx, "ctrl-y");
    assert_eq!(cell(&ed, cx, 2, 1), "say \"hi\"");
    assert_eq!(cell(&ed, cx, 2, 2), "two\nlines");
    // An outside copy joins the kill ring; M-y swaps the paste for the older kill.
    keys(&ed, cx, "escape ctrl-home");
    clipboard(&ed, cx, "outside");
    keys(&ed, cx, "ctrl-y");
    assert_eq!(cell(&ed, cx, 0, 0), "outside");
    keys(&ed, cx, "alt-y");
    assert_eq!(cell(&ed, cx, 0, 0), "say \"hi\"", "M-y: the previous kill");
    assert_eq!(cell(&ed, cx, 0, 1), "two\nlines");
    keys(&ed, cx, "ctrl-/");
    assert_eq!(
        cell(&ed, cx, 0, 0),
        "a",
        "one undo removes the yank-popped paste"
    );
    keys(&ed, cx, "alt-y");
    ed.read_with(cx, |e, _| {
        assert!(e.status.contains("not a yank"), "{}", e.status)
    });
    // CSV text from a text buffer pastes into columns.
    clipboard(&ed, cx, "p,q\n1,2\n");
    keys(&ed, cx, "ctrl-y");
    assert_eq!(
        (cell(&ed, cx, 0, 1), cell(&ed, cx, 1, 1)),
        ("q".into(), "2".into())
    );
    keys(&ed, cx, "ctrl-/ ctrl-/");
    save(&ed, cx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), tricky);
}

#[gpui::test]
fn org_style_row_and_column_moves_stay_byte_exact(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    keys(&ed, cx, "down alt-down");
    assert_eq!(cell(&ed, cx, 2, 0), "apple");
    assert_eq!(cursor(&ed, cx), (2, 0), "the cursor follows the row");
    keys(&ed, cx, "alt-up alt-up");
    assert_eq!(cell(&ed, cx, 1, 0), "apple", "the header row stays put");
    save(&ed, cx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), CSV);
    // Columns, with undo.
    keys(&ed, cx, "alt-right");
    assert_eq!(cell(&ed, cx, 0, 1), "name");
    assert_eq!(cursor(&ed, cx).1, 1);
    keys(&ed, cx, "ctrl-/");
    save(&ed, cx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), CSV);
}

#[gpui::test]
fn copy_down_series_fill_and_prefix_flag(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    // S-RET on 5 (above it 1): the next value steps by 4, cursor follows.
    keys(&ed, cx, "down down right shift-enter");
    assert_eq!(cell(&ed, cx, 3, 1), "9");
    assert_eq!(cursor(&ed, cx), (3, 1));
    // C-u S-RET copies plainly.
    keys(&ed, cx, "ctrl-u shift-enter");
    assert_eq!(cell(&ed, cx, 4, 1), "9");
    // Fill a selection from its first two values: Mon, Tue → Wed, Thu, Fri.
    scheme(&ed, cx, "(grid-select-range 1 2 5 2)");
    scheme(
        &ed,
        cx,
        "(grid-set-cells (list (list 3 2 \"\") (list 4 2 \"\") (list 5 2 \"\")))",
    );
    scheme(&ed, cx, "(grid-select-range 1 2 5 2)");
    keys(&ed, cx, "shift-enter");
    assert_eq!(cell(&ed, cx, 5, 2), "Fri");
    // C-u C-d clears the whole row.
    keys(&ed, cx, "escape ctrl-home down ctrl-u ctrl-d");
    assert_eq!(
        (cell(&ed, cx, 1, 0), cell(&ed, cx, 1, 2)),
        (String::new(), String::new())
    );
}

#[gpui::test]
fn emacs_habits_case_kill_exchange_and_sort_prompt(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    keys(&ed, cx, "down alt-u");
    assert_eq!(cell(&ed, cx, 1, 0), "APPLE");
    keys(&ed, cx, "alt-c");
    assert_eq!(cell(&ed, cx, 1, 0), "Apple");
    // C-k kills to the end of the row, onto the kill ring.
    keys(&ed, cx, "right ctrl-k");
    assert_eq!(
        (cell(&ed, cx, 1, 1), cell(&ed, cx, 1, 2)),
        (String::new(), String::new())
    );
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.kill_ring.last().map(String::as_str),
            Some("3\tred, crisp\n")
        )
    });
    // C-x C-x swaps the cursor and the anchor.
    keys(&ed, cx, "ctrl-space down down ctrl-x ctrl-x");
    assert_eq!(cursor(&ed, cx), (1, 1));
    keys(&ed, cx, "ctrl-g");
    // `C-u C-c C-s` asks how to sort; N = numeric, descending.
    keys(&ed, cx, "ctrl-u ctrl-c ctrl-s");
    keys(&ed, cx, "N");
    assert_eq!(cell(&ed, cx, 1, 0), "fig", "12 sorts first, descending");
    // C-x h selects everything.
    keys(&ed, cx, "ctrl-x h");
    assert_eq!(eval(&ed, cx, "(grid-selection)"), "(0 0 3 2)");
}

#[gpui::test]
fn filters_narrowing_and_motion_over_hidden_rows(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, true);
    keys(&ed, cx, "l g /");
    submit(&ed, cx, ">2");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert!(
            !g.row_shown(1) && g.row_shown(2) && g.row_shown(0),
            "header pinned"
        );
        assert_eq!(g.filter_status(), "B>2 · 3/4 rows");
    });
    // j steps over hidden rows; the cursor never sits on one.
    keys(&ed, cx, "g g j");
    assert_eq!(cursor(&ed, cx), (2, 1));
    // Stacking a second filter, then copying copies only what's shown.
    scheme(&ed, cx, "(grid-filter-rows \"C\" \"matches\" \"^T\")");
    assert_eq!(eval(&ed, cx, "(grid-visible-row-count)"), "3");
    assert_eq!(eval(&ed, cx, "(grid-row-visible? 3)"), "#false");
    keys(&ed, cx, "shift-v shift-g y");
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.kill_ring.last().map(String::as_str),
            Some("b\t5\tTue\nd\t8\tThu\n")
        )
    });
    // Deleting rows under a filter removes only the shown ones.
    keys(&ed, cx, "escape g g j shift-v j d d");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.grid.as_ref().unwrap().used(), (3, 3))
    });
    assert_eq!(cell(&ed, cx, 1, 0), "a");
    assert_eq!(cell(&ed, cx, 2, 0), "c");
    keys(&ed, cx, "u");
    assert_eq!(cell(&ed, cx, 4, 0), "d", "one undo restores both rows");
    // Widen, then narrow to a selection.
    keys(&ed, cx, "z w");
    ed.read_with(cx, |e, _| {
        assert!(e.active.grid.as_ref().unwrap().filters.is_empty())
    });
    keys(&ed, cx, "g g j j shift-v j z n");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.filter_status(), "rows 3–4 · 2/4 rows");
    });
    // Filter by the cursor's value.
    keys(&ed, cx, "z w g g j l g *");
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.active.grid.as_ref().unwrap().filter_status(),
            "B=1 · 1/4 rows"
        )
    });
}

#[gpui::test]
fn synchronous_reads_and_map_cells(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, true);
    assert_eq!(eval(&ed, cx, "(grid-cell 2 \"C\")"), "\"Tue\"");
    assert_eq!(
        eval(&ed, cx, "(grid-column-values 'B)"),
        "(\"1\" \"5\" \"3\" \"8\")"
    );
    assert_eq!(
        eval(&ed, cx, "(grid-row-values 1)"),
        "(\"a\" \"1\" \"Mon\")"
    );
    // grid-map-cells over a selection, one undo step.
    scheme(&ed, cx, "(grid-select-range 1 2 3 2)");
    scheme(
        &ed,
        cx,
        "(grid-map-cells (lambda (v) (string-append v \"!\")))",
    );
    assert_eq!(cell(&ed, cx, 3, 2), "Wed!");
    keys(&ed, cx, "escape u");
    assert_eq!(cell(&ed, cx, 1, 2), "Mon");
    // Reads see edits made since.
    scheme(&ed, cx, "(grid-set-cell 1 0 \"z\")");
    assert_eq!(eval(&ed, cx, "(grid-cell 1 0)"), "\"z\"");
}

#[gpui::test]
fn extended_styling_api(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, true);
    scheme(
        &ed,
        cx,
        "(begin (grid-row-highlight-rule \"C\" \"=\" \"Wed\" 'error)
                (grid-highlight-rule \"B\" \"top\" 2 'success)
                (grid-set-cell-face 1 0 'warning)
                (grid-stripe-rows #t) (grid-header-face 'keyword)
                (grid-column-align \"A\" 'center)
                (grid-set-column-hidden \"C\" #t))",
    );
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        let s = &g.style;
        let row = |r: usize| move |c: usize| g.cell(r, c);
        assert_eq!(s.face(3, 0, "c", row(3)), Some("error"), "row rule");
        assert_eq!(s.face(4, 1, "8", row(4)), Some("success"), "top 2");
        assert_eq!(s.face(2, 1, "5", row(2)), Some("success"));
        assert_eq!(s.face(1, 1, "1", row(1)), None);
        assert_eq!(s.face(1, 0, "a", row(1)), Some("warning"), "cell face");
        assert_eq!(s.stripe.as_deref(), Some("grid-stripe"));
        assert_eq!(s.align(0), Some(crate::grid::style::Align::Center));
        assert!(!g.view.cols.shown(2));
    });
    // Columnar rules follow edits.
    scheme(&ed, cx, "(grid-set-cell 1 \"B\" \"100\")");
    keys(&ed, cx, "j");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.style.face(1, 1, "100", |_| ""), Some("success"));
        assert_eq!(g.style.face(2, 1, "5", |_| ""), None, "no longer top 2");
    });
    scheme(&ed, cx, "(grid-highlight-rule #f \"top\" 3 'x)");
    ed.read_with(cx, |e, _| {
        assert!(e.status.contains("needs a column"), "{}", e.status)
    });
    // l skips the hidden column; display-only, so the file is unchanged.
    keys(&ed, cx, "g g l l");
    assert_eq!(cursor(&ed, cx).1, 3, "l skips the hidden column C");
    scheme(&ed, cx, "(grid-set-cell 1 \"B\" \"1\")");
    save(&ed, cx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), NUMS);
}

#[gpui::test]
fn completion_column_jump_export_and_mode_line(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "k,status\na,open\nb,done\nc,open\nd,\n", false);
    // Completion offers the column's values, most common first.
    keys(&ed, cx, "ctrl-end ctrl-alt-i");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.completing_candidates, ["open", "done"])
    });
    submit(&ed, cx, "done");
    assert_eq!(cell(&ed, cx, 4, 1), "done");
    // Declared choices win.
    scheme(
        &ed,
        cx,
        "(grid-column-choices \"B\" '(\"open\" \"closed\"))",
    );
    keys(&ed, cx, "ctrl-alt-i");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.completing_candidates, ["open", "closed"])
    });
    keys(&ed, cx, "ctrl-g");
    // Jump to a column by header name.
    keys(&ed, cx, "ctrl-home ctrl-c j");
    submit(&ed, cx, "stat");
    assert_eq!(cursor(&ed, cx), (0, 1));
    // The mode line shows the cursor cell and its column type.
    ed.read_with(cx, |e, _| {
        let line: String = e
            .mode_line_segments()
            .iter()
            .map(|(t, _)| t.to_string())
            .collect();
        assert!(line.contains("B1 · text"), "{line}");
    });
    // Export: the whole sheet as an org table, onto the kill ring.
    keys(&ed, cx, "ctrl-c ctrl-e o");
    ed.read_with(cx, |e, _| {
        let org = e.kill_ring.last().unwrap();
        assert!(org.starts_with("| k | status |\n|---+--------|\n"), "{org}");
    });
}

#[gpui::test]
fn org_table_to_grid_and_mode_hooks(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(
        &ed,
        cx,
        "(begin (define *hooked* 0) (define (on-grid) (set! *hooked* (+ *hooked* 1)))
                (add-hook \"grid-mode-hook\" \"on-grid\")
                (define *moves* 0) (define (on-move) (set! *moves* (+ *moves* 1)))
                (add-hook \"grid-selection-changed-hook\" \"on-move\"))",
    );
    keys(&ed, cx, "down down");
    assert_eq!(eval(&ed, cx, "*moves*"), "2");
    ed.update(cx, |e, cx| {
        let doc = make_document(
            "notes.org".into(),
            None,
            "intro\n| x | y |\n|---+---|\n| 1 | a\\vert{}b |\n",
        );
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(doc);
        e.move_to(10, cx);
        e.apply_command(
            EditorCommand::Grid(crate::grid::cmd::GridCmd::OrgTableToGrid.into()),
            cx,
        );
    });
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().expect("a grid from the table");
        assert_eq!(e.active.name, "*table: notes.org*");
        assert!(g.wb.header(SHEET));
        assert_eq!(g.cell(1, 1), "a|b");
        assert!(e.active.modified, "unsaved until written");
    });
    assert_eq!(
        eval(&ed, cx, "*hooked*"),
        "1",
        "grid-mode-hook ran for the new grid"
    );
}

#[gpui::test]
fn grid_options_apply_to_new_grids(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    scheme(
        &ed,
        cx,
        "(begin (grid-default-column-width 90) (grid-auto-header #f) (grid-max-autofit-width 30))",
    );
    let second = path.with_file_name("second.csv");
    std::fs::write(&second, CSV).unwrap();
    ed.update(cx, |e, cx| {
        let bytes = std::fs::read(&second).unwrap();
        e.open_grid("second.csv".into(), second.clone(), bytes, cx);
    });
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.view.cols.size(5), 90.0);
        assert!(!g.wb.header(SHEET), "auto-header #f forces row 1 to data");
    });
    keys(&ed, cx, "right right ctrl-c =");
    ed.read_with(cx, |e, _| {
        assert!(e.active.grid.as_ref().unwrap().view.cols.size(2) <= 30.0)
    });
}

#[gpui::test]
fn dispatch_menu_opens_from_question_mark(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "?");
    ed.read_with(cx, |e, _| {
        assert!(e.transient.is_some(), "the grid menu is up")
    });
    keys(&ed, cx, "j");
    assert!(
        ed.read_with(cx, |e, _| e.minibuffer.is_some()),
        "j: jump to column"
    );
}

#[gpui::test]
fn colon_opens_ex_and_ex_commands_understand_the_grid(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    keys(&ed, cx, ":");
    ed.read_with(cx, |e, _| {
        assert!(
            matches!(
                e.minibuffer.as_ref().map(|m| &m.purpose),
                Some(MinibufferPurpose::ExCommand)
            ),
            "`:` opens the ex prompt in a grid"
        );
    });
    ed.update(cx, |e, _| e.minibuffer = None);
    let ex = |ed: &Entity<Editor>, cx: &mut TestAppContext, line: &str| {
        ed.update(cx, |e, cx| e.run_ex_command(line, cx));
        settle(ed, cx);
    };
    ex(&ed, cx, "3");
    assert_eq!(cursor(&ed, cx), (2, 0), ":3 = row 3");
    ex(&ed, cx, "C2");
    assert_eq!(cursor(&ed, cx), (1, 2), ":C2 jumps to the cell");
    ex(&ed, cx, "$");
    assert_eq!(cursor(&ed, cx), (3, 2), ":$ = last row");
    ex(&ed, cx, "A2:B3");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.grid.as_ref().unwrap().bar_text().0, "A2:B3");
    });
    ex(&ed, cx, "%s/apple/x/");
    ed.read_with(cx, |e, _| {
        assert!(e.status.contains("not grid cells"), "{}", e.status)
    });
    assert_eq!(
        cell(&ed, cx, 1, 0),
        "apple",
        "line commands don't touch the grid"
    );
    // :w still saves through the grid.
    keys(&ed, cx, "escape c Q escape");
    ex(&ed, cx, "w");
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(std::fs::read_to_string(&path).unwrap().contains("Q"));
}

// ---- formulas ---------------------------------------------------------------

fn source(editor: &Entity<Editor>, cx: &mut TestAppContext, r: usize, c: usize) -> String {
    editor.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().source(r, c).to_string()
    })
}

#[gpui::test]
fn typed_formulas_show_results_and_recalculate(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    // D2 (row 1, col 3) = B2*2 with B2 = 3.
    keys(&ed, cx, "j l l l c = b 2 * 2 escape");
    assert_eq!(cell(&ed, cx, 1, 3), "6");
    assert_eq!(
        source(&ed, cx, 1, 3),
        "=B2*2",
        "stored canonically, upper-cased"
    );
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.bar_text().1, "=B2*2", "the formula bar shows the formula");
    });
    // Editing the input recalculates.
    keys(&ed, cx, "h h c 1 0 escape");
    assert_eq!(cell(&ed, cx, 1, 3), "20");
    // A range over the column.
    keys(&ed, cx, "j j j l l c = s u m ( b : b ) escape");
    assert_eq!(
        cell(&ed, cx, 4, 3),
        "29",
        "10 + 007 + 12; qty header text skipped"
    );
    // Undo the input edit: everything downstream follows.
    keys(&ed, cx, "u u");
    assert_eq!(cell(&ed, cx, 1, 3), "6");
    // Editing a formula cell edits its formula text.
    ed.update(cx, |e, cx| e.grid_goto_address("D2", cx));
    keys(&ed, cx, "a + 1 escape");
    assert_eq!(source(&ed, cx, 1, 3), "=B2*2+1");
    assert_eq!(cell(&ed, cx, 1, 3), "7");
}

#[gpui::test]
fn structural_edits_rewrite_references(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j l l l c = b 2 * 2 escape");
    // Insert a row above row 2: the formula moves to D3 and reads B3.
    keys(&ed, cx, "shift-o");
    assert_eq!(source(&ed, cx, 2, 3), "=B3*2");
    assert_eq!(cell(&ed, cx, 2, 3), "6");
    // One undo restores both the row and the reference.
    keys(&ed, cx, "u");
    assert_eq!(source(&ed, cx, 1, 3), "=B2*2");
    // Deleting the referenced row leaves #REF! (D3 = B2; delete row 2).
    ed.update(cx, |e, cx| e.grid_goto_address("D3", cx));
    keys(&ed, cx, "c = b 2 escape");
    ed.update(cx, |e, cx| e.grid_goto_address("A2", cx));
    keys(&ed, cx, "d d");
    assert_eq!(source(&ed, cx, 1, 3), "=#REF!");
    assert_eq!(cell(&ed, cx, 1, 3), "#REF!");
}

#[gpui::test]
fn formula_errors_and_cycles(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    keys(&ed, cx, "j l l l c = d 2 + 1 escape");
    assert_eq!(cell(&ed, cx, 1, 3), "#CIRC!");
    keys(&ed, cx, "c = s u m ( escape");
    assert_eq!(cell(&ed, cx, 1, 3), "#ERROR!");
    keys(&ed, cx, "c = 1 / 0 escape");
    assert_eq!(cell(&ed, cx, 1, 3), "#DIV/0!");
    keys(&ed, cx, "c ' = 1 + 1 escape");
    assert_eq!(cell(&ed, cx, 1, 3), "=1+1", "an apostrophe makes it text");
}

#[gpui::test]
fn loaded_csv_equals_text_is_not_a_formula(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "a,b\n=1+1,2\n", true);
    assert_eq!(cell(&ed, cx, 1, 0), "=1+1");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.grid.as_ref().unwrap().formulas.formula_count(), 0);
    });
}

#[gpui::test]
fn csv_with_formulas_must_be_saved_as_xlsx(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    keys(&ed, cx, "j l l l c = b 2 * 2 escape");
    // C-x C-s offers Save As .xlsx instead of writing the CSV.
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        assert!(matches!(
            e.minibuffer.as_ref().map(|m| &m.purpose),
            Some(MinibufferPurpose::GridSaveAsXlsx)
        ));
    });
    ed.update(cx, |e, _| e.minibuffer = None);
    // Paths that can't prompt refuse rather than write formula text.
    let ok = ed.update(cx, |e, cx| e.save_all_modified(cx));
    assert!(!ok, "quit-time save refuses a CSV with formulas");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        CSV,
        "the CSV is untouched"
    );
    // Save As .xlsx: the buffer now visits it, the CSV stays as it was.
    let xlsx = path.with_extension("xlsx");
    ed.update(cx, |e, cx| e.grid_save_as_xlsx(&xlsx.to_string_lossy(), cx));
    settle(&ed, cx);
    assert!(xlsx.exists());
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.path.as_deref(), Some(xlsx.as_path()));
        assert_eq!(e.active.name, "data.xlsx");
        assert!(!e.active.modified);
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), CSV);
    // The .xlsx reads back with the formula and its value.
    let book = crate::xlsx::read(&std::fs::read(&xlsx).unwrap()).unwrap();
    let f = book.sheets[0]
        .cells
        .iter()
        .find(|c| (c.row, c.col) == (1, 3))
        .unwrap();
    assert_eq!(
        f.formula,
        Some(crate::xlsx::XFormula::Normal("B2*2".into()))
    );
    assert_eq!(f.value, crate::xlsx::XValue::Number(6.0));
    let text = book.sheets[0]
        .cells
        .iter()
        .find(|c| (c.row, c.col) == (2, 1))
        .unwrap();
    assert_eq!(
        text.value,
        crate::xlsx::XValue::Text("007".into()),
        "kept as typed"
    );
    // Further saves write the .xlsx in place.
    keys(&ed, cx, "c 5 escape");
    save(&ed, cx);
    ed.read_with(cx, |e, _| assert!(!e.active.modified));
}

#[gpui::test]
fn xlsx_files_open_with_formulas(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "a\n1\n", true);
    // Write an .xlsx beside the CSV and open it.
    let x = path.with_extension("xlsx");
    let cells = [
        (0, 0, crate::xlsx::XValue::Text("n".into()), None),
        (1, 0, crate::xlsx::XValue::Number(4.0), None),
        (2, 0, crate::xlsx::XValue::Number(0.0), Some("A2*A2")),
    ];
    let bytes = crate::xlsx::write(
        "Data",
        cells.iter().map(|(r, c, v, f)| crate::xlsx::WriteCell {
            row: *r,
            col: *c,
            value: v,
            formula: *f,
            style: None,
        }),
        false,
    );
    std::fs::write(&x, bytes).unwrap();
    let b = std::fs::read(&x).unwrap();
    ed.update(cx, |e, cx| {
        e.open_grid("data.xlsx".into(), x.clone(), b, cx)
    });
    settle(&ed, cx);
    assert_eq!(cell(&ed, cx, 2, 0), "16", "recalculated on open");
    assert_eq!(source(&ed, cx, 2, 0), "=A2*A2");
}

#[gpui::test]
fn scheme_functions_in_formulas(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    scheme(
        &ed,
        cx,
        "(begin (define-grid-helper (twice x) (* 2 x)) (define-grid-function (QUAD x) (twice (twice x))))",
    );
    keys(&ed, cx, "j l l l c = q u a d ( b 2 ) + 1 escape");
    // The call runs on the grid-function interpreter; wait for its result.
    let mut shown = String::new();
    for _ in 0..400 {
        settle(&ed, cx);
        shown = cell(&ed, cx, 1, 3);
        if shown != "#CALC!" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let status = ed.read_with(cx, |e, _| e.status.to_string());
    let msgs = ed.read_with(cx, |e, _| e.messages.join(" | "));
    assert_eq!(
        shown, "13",
        "QUAD(3)+1 via a helper; status: {status}; messages: {msgs}"
    );
    // The cursor cell's formula is readable from Scheme.
    assert_eq!(eval(&ed, cx, "(grid-formula)"), "\"=QUAD(B2)+1\"");
}

#[gpui::test]
fn point_mode_completion_and_hints(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    ed.update(cx, |e, cx| e.grid_goto_address("D2", cx));
    // `=` then arrows point at cells; shift extends to a range.
    keys(&ed, cx, "c = left left");
    assert_eq!(
        ed.read_with(cx, |e, _| e
            .active
            .grid
            .as_ref()
            .unwrap()
            .view
            .edit
            .as_ref()
            .unwrap()
            .text
            .clone()),
        "=B2"
    );
    keys(&ed, cx, "escape");
    assert_eq!(cell(&ed, cx, 1, 3), "3");
    keys(&ed, cx, "c = s u m i");
    // An LSP-style dropdown lists the matching functions.
    let names = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            let g = e.active.grid.as_ref().unwrap();
            let c = g.view.edit.as_ref().unwrap().completion.clone();
            c.map(|c| (c.names, c.selected))
        })
    };
    let (list, sel) = names(&ed, cx).expect("dropdown open");
    assert_eq!(list, ["SUMIF", "SUMIFS"]);
    assert_eq!(sel, 0);
    // Down moves the highlight; Tab inserts it and opens the call.
    keys(&ed, cx, "down tab");
    let text = ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        g.view.edit.as_ref().unwrap().text.clone()
    });
    assert_eq!(text, "=SUMIFS(");
    assert!(names(&ed, cx).is_none(), "closed after accepting");
    // Escape closes it without leaving the edit; typing reopens it.
    keys(&ed, cx, "ctrl-u = s u escape");
    assert!(names(&ed, cx).is_none());
    ed.read_with(cx, |e, _| assert!(e.grid_editing(), "still editing"));
    keys(&ed, cx, "m");
    assert!(names(&ed, cx).is_some());
    keys(&ed, cx, "ctrl-u = s u m i f ( left");
    let hint = ed.read_with(cx, |e, _| e.status.to_string());
    assert_eq!(hint, "SUMIF(«range», criteria, [sum_range])");
    keys(&ed, cx, "shift-up shift-up , \" > 5 \" ) escape");
    assert_eq!(source(&ed, cx, 1, 3), "=SUMIF(C1:C2,\">5\")");
}

#[gpui::test]
fn show_formulas_and_trace(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    ed.update(cx, |e, cx| e.grid_goto_address("D2", cx));
    keys(&ed, cx, "c = b 2 + b 3 escape");
    keys(&ed, cx, "g `");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(g.shown_text(1, 3), "=B2+B3");
    });
    keys(&ed, cx, "g `");
    ed.update(cx, |e, cx| e.grid_goto_address("D2", cx));
    keys(&ed, cx, "g [");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        let cell = crate::grid::view::CellRange::cell;
        assert_eq!(g.view.trace, vec![cell((1, 1)), cell((2, 1))]);
    });
    ed.update(cx, |e, cx| e.grid_goto_address("B3", cx));
    keys(&ed, cx, "g ]");
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.active.grid.as_ref().unwrap().view.trace,
            vec![crate::grid::view::CellRange::cell((1, 3))]
        );
    });
}

#[gpui::test]
fn enter_closes_open_parentheses(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    ed.update(cx, |e, cx| e.grid_goto_address("D2", cx));
    keys(&ed, cx, "c = s u m ( b 3 , b 4 enter");
    assert_eq!(source(&ed, cx, 1, 3), "=SUM(B3,B4)");
    assert_eq!(cell(&ed, cx, 1, 3), "19", "007 + 12");
    // The same with point mode: arrows insert the references.
    ed.update(cx, |e, cx| e.grid_goto_address("D3", cx));
    keys(&ed, cx, "c = s u m ( left left , left left down enter");
    assert_eq!(source(&ed, cx, 2, 3), "=SUM(B3,B4)");
}

fn fmt(
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
    r: usize,
    c: usize,
) -> crate::grid::cellfmt::CellFmt {
    editor.read_with(cx, |e, _| e.active.grid.as_ref().unwrap().formats.get(r, c))
}

#[gpui::test]
fn formatting_by_hand_cycles_undoes_and_follows_sorts(cx: &mut TestAppContext) {
    use crate::grid::cellfmt::color_index;
    let (ed, path) = setup(cx, NUMS, false);
    let red = color_index("red");
    let orange = color_index("orange");
    // B2 (n = 1): bold, then two steps of text color, then a fill.
    keys(&ed, cx, "ctrl-n ctrl-f alt-o b alt-o c alt-o c alt-o f");
    let f = fmt(&ed, cx, 1, 1);
    assert!(f.bold);
    assert_eq!(f.text, orange);
    assert_eq!(f.fill, red);
    assert!(ed.read_with(cx, |e, _| e.status.contains("Fill: red")));
    // Each command is one undo step.
    keys(&ed, cx, "ctrl-/");
    assert_eq!(fmt(&ed, cx, 1, 1).fill, None);
    keys(&ed, cx, "ctrl-/");
    assert_eq!(fmt(&ed, cx, 1, 1).text, red);
    keys(&ed, cx, "ctrl-shift-/");
    assert_eq!(fmt(&ed, cx, 1, 1).text, orange);
    // A selection takes the cursor cell's next state: B2 is bold, so the
    // column goes not-bold.
    keys(&ed, cx, "shift-down shift-down alt-o b");
    assert!(!fmt(&ed, cx, 1, 1).bold && !fmt(&ed, cx, 3, 1).bold);
    keys(&ed, cx, "ctrl-/");
    assert!(fmt(&ed, cx, 1, 1).bold && !fmt(&ed, cx, 2, 1).bold);
    // Sorting n descending moves B2's row (n = 1) to the bottom, formatting
    // and all; undoing the sort brings it back.
    keys(&ed, cx, "escape ctrl-c ctrl-r");
    assert_eq!(cell(&ed, cx, 4, 1), "1");
    assert!(fmt(&ed, cx, 4, 1).bold);
    assert!(!fmt(&ed, cx, 1, 1).bold);
    keys(&ed, cx, "ctrl-/");
    assert!(fmt(&ed, cx, 1, 1).bold);
    // Deleting the row takes its formatting with it.
    keys(&ed, cx, "ctrl-c ctrl-k");
    assert_eq!(cell(&ed, cx, 1, 1), "5");
    assert!(fmt(&ed, cx, 1, 1).is_plain());
    // From Scheme, as one undo step per batch; bad colors are reported.
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cell-fill-color 2 \"A\" 'yellow)
                (grid-set-cell-text-color 2 \"A\" \"blue\"))",
    );
    assert_eq!(fmt(&ed, cx, 2, 0).fill, color_index("yellow"));
    assert_eq!(fmt(&ed, cx, 2, 0).text, color_index("blue"));
    keys(&ed, cx, "ctrl-/");
    assert!(fmt(&ed, cx, 2, 0).is_plain());
    scheme(&ed, cx, "(grid-set-cell-text-color 0 0 'teal)");
    assert!(ed.read_with(cx, |e, _| e.status.contains("unknown color")));
    // Formatting never reaches the file.
    keys(&ed, cx, "ctrl-x ctrl-s");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "item,n,day\nb,5,Tue\nc,3,Wed\nd,8,Thu\n"
    );
}

#[gpui::test]
fn evil_formatting_keys_and_repeat(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, true);
    keys(&ed, cx, "j g b alt-o f . .");
    let f = fmt(&ed, cx, 1, 0);
    assert!(f.bold);
    assert_eq!(
        f.fill,
        crate::grid::cellfmt::color_index("yellow"),
        "three steps"
    );
    keys(&ed, cx, "alt-o d");
    assert!(fmt(&ed, cx, 1, 0).is_plain());
}

#[gpui::test]
fn xlsx_sheets_list_and_open_as_buffers(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, true);
    let book = path.with_file_name("multi.xlsx");
    std::fs::write(
        &book,
        include_bytes!("../xlsx/testdata/pkg/multi.xlsx").as_slice(),
    )
    .unwrap();
    let b = book.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "multi.xlsx".into(),
            b.clone(),
            std::fs::read(&b).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    let name = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| e.active.name.clone())
    };
    assert_eq!(
        eval(&ed, cx, "(grid-sheets)"),
        "(\"First\" \"Second\" \"Third\")"
    );
    assert_eq!(eval(&ed, cx, "(grid-current-sheet)"), "\"First\"");
    // Another sheet opens as its own buffer, matched without case.
    scheme(&ed, cx, "(grid-open-sheet \"second\")");
    assert_eq!(name(&ed, cx), "multi.xlsx<Second>");
    assert_eq!(cell(&ed, cx, 0, 0), "two");
    assert_eq!(eval(&ed, cx, "(grid-current-sheet)"), "\"Second\"");
    scheme(&ed, cx, "(grid-open-sheet \"Third\")");
    assert_eq!(cell(&ed, cx, 0, 0), "three");
    // From a sheet buffer: the first sheet is the file's own buffer, and an
    // open sheet buffer is reused.
    scheme(&ed, cx, "(grid-open-sheet \"First\")");
    assert_eq!(name(&ed, cx), "multi.xlsx");
    let buffers = ed.read_with(cx, |e, _| e.inactive.len());
    scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    assert_eq!(name(&ed, cx), "multi.xlsx<Second>");
    assert_eq!(ed.read_with(cx, |e, _| e.inactive.len()), buffers);
    // `g t` picks a sheet with Helm (typed text and RET reach Helm through
    // the platform's input path, so they're fed to it directly here).
    keys(&ed, cx, "g t");
    ed.update(cx, |e, cx| {
        assert_eq!(e.helm_match_count(), 3);
        e.helm_input("thi", cx);
        e.helm_accept(cx);
    });
    settle(&ed, cx);
    assert_eq!(name(&ed, cx), "multi.xlsx<Third>");
    scheme(&ed, cx, "(grid-open-sheet \"Nope\")");
    assert!(ed.read_with(cx, |e, _| e.status.contains("No sheet named")));
    // A CSV has no sheets.
    scheme(&ed, cx, "(switch-to-buffer \"data.csv\")");
    assert_eq!(eval(&ed, cx, "(grid-sheets)"), "()");
}

#[gpui::test]
fn formulas_read_other_sheets_live(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, false);
    let book = path.with_file_name("multi.xlsx");
    std::fs::write(
        &book,
        include_bytes!("../xlsx/testdata/pkg/multi.xlsx").as_slice(),
    )
    .unwrap();
    let b = book.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "multi.xlsx".into(),
            b.clone(),
            std::fs::read(&b).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    // From the file: the other sheets' saved values.
    scheme(
        &ed,
        cx,
        "(grid-set-cells (list (list 1 0 \"=Second!A1&\\\"+\\\"&'Third'!A1\")
                               (list 2 0 \"=Nope!A1\")))",
    );
    assert_eq!(cell(&ed, cx, 1, 0), "two+three");
    assert_eq!(cell(&ed, cx, 2, 0), "#REF!");
    // Edit Second in its own buffer: First follows, unsaved, and a formula
    // on Second can read First back.
    scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    scheme(
        &ed,
        cx,
        "(grid-set-cells (list (list 0 0 \"2\") (list 0 1 \"=First!A1\")))",
    );
    assert_eq!(cell(&ed, cx, 0, 1), "one");
    let first = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            let d = e.inactive.iter().find(|d| d.name == "multi.xlsx").unwrap();
            d.grid.as_ref().unwrap().cell(1, 0).to_string()
        })
    };
    assert_eq!(first(&ed, cx), "2+three");
    // And back the other way: editing First updates Second's B1.
    scheme(&ed, cx, "(grid-open-sheet \"First\")");
    scheme(&ed, cx, "(grid-set-cell 0 0 \"uno\")");
    let second_b1 = ed.read_with(cx, |e, _| {
        let d = e
            .inactive
            .iter()
            .find(|d| d.name == "multi.xlsx<Second>")
            .unwrap();
        d.grid.as_ref().unwrap().cell(0, 1).to_string()
    });
    assert_eq!(second_b1, "uno");
}

#[gpui::test]
fn mcp_buffer_text_shows_a_grid(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "item,n\na,1\nb,2\n", false);
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cells (list (list 3 0 \"total\") (list 3 1 \"=SUM(B2:B3)\")
                                      (list 1 0 \"x\ty\")))
                (grid-set-cell-bold 3 1 #t) (grid-set-cell-fill-color 3 1 'yellow))",
    );
    keys(&ed, cx, "ctrl-n shift-right");
    let text = ed.read_with(cx, |e, _| e.mcp_buffer_text(None, None, true).unwrap());
    for want in [
        "grid: 4 rows x 2 columns, header row: yes",
        "cursor: B2 (row 1, column 1, 0-based)  selection: A2:B2",
        "---\n\tA\tB\n1\titem\tn\n2\tx\\ty\t1\n3\tb\t2\n4\ttotal\t3\n",
        "--- formulas (cell: formula)\nB4: =SUM(B2:B3)\n",
        "--- formatting (cell: bold, text color, fill, number format, wrap)\nB4: bold, none, yellow, General, no wrap\n",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    // A row range picks rows.
    let part = ed.read_with(cx, |e, _| {
        e.mcp_buffer_text(None, Some((1, 2)), false).unwrap()
    });
    assert!(part.contains("showing rows 1..2 of 4"), "{part}");
    assert!(
        part.contains("\n2\tx\\ty\t1\n") && !part.contains("total"),
        "{part}"
    );
}
#[gpui::test]
fn write_during_background_load_is_refused(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    let big = path.with_file_name("big.csv");
    let mut s = String::from("id,text\n");
    let mut i = 0;
    while s.len() < 3 * 1024 * 1024 {
        s.push_str(&format!("{i},some longish filler text for row {i}\n"));
        i += 1;
    }
    std::fs::write(&big, &s).unwrap();
    let target = big.to_string_lossy().into_owned();
    ed.update(cx, |e, cx| {
        e.open_grid("big.csv".into(), big.clone(), s.clone().into_bytes(), cx);
        assert!(e.active.grid.as_ref().unwrap().loading.is_some());
        e.write_buffer_to(&target, cx);
    });
    settle(&ed, cx);
    let after = std::fs::read_to_string(&big).unwrap();
    assert_eq!(
        after.lines().count(),
        s.lines().count(),
        "file truncated to {} lines",
        after.lines().count()
    );
}

#[gpui::test]
fn kill_to_row_end_marks_the_buffer_modified(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    keys(&ed, cx, "down ctrl-k");
    assert_eq!(cell(&ed, cx, 1, 0), "");
    ed.read_with(cx, |e, _| {
        assert!(e.active.modified, "buffer not marked modified after C-k")
    });
}

#[gpui::test]
fn undoing_a_column_move_restores_widths(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(&ed, cx, "(grid-set-column-width 0 200)");
    keys(&ed, cx, "alt-right ctrl-/");
    assert_eq!(cell(&ed, cx, 0, 0), "name");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert_eq!(
            (g.view.cols.size(0), g.view.cols.size(1)),
            (200.0, crate::grid::view::DEFAULT_COL_WIDTH)
        );
    });
}

#[gpui::test]
fn column_moves_carry_hidden_flags(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(&ed, cx, "(grid-set-column-hidden 1 #t)");
    keys(&ed, cx, "alt-right");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        let (_, c) = g.view.cursor;
        let hidden: Vec<usize> = g.view.cols.hidden().collect();
        assert_eq!(
            g.cell(0, hidden[0]),
            "qty",
            "hidden flag now on {:?}",
            g.cell(0, hidden[0])
        );
        assert!(g.view.cols.shown(c), "cursor sits on hidden column {c}");
    });
}

#[gpui::test]
fn deleting_a_column_keeps_hidden_flags_on_their_data(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(&ed, cx, "(grid-set-column-hidden 1 #t)");
    keys(&ed, cx, "ctrl-c alt-k");
    assert_eq!(cell(&ed, cx, 0, 1), "note");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert!(
            g.view.cols.shown(1),
            "'note' is now hidden though the user hid 'qty'"
        );
    });
}

#[gpui::test]
fn scheme_batch_with_formatting_is_one_undo_step(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cell 1 0 \"X\") (grid-set-cell-bold 1 0 #t) (grid-set-cell 2 0 \"Y\"))",
    );
    keys(&ed, cx, "ctrl-/");
    assert_eq!(cell(&ed, cx, 2, 0), "pear");
    assert_eq!(
        cell(&ed, cx, 1, 0),
        "apple",
        "one undo should undo the whole batch"
    );
}

#[gpui::test]
fn scheme_batch_with_a_resize_is_one_undo_step(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cell 1 0 \"X\") (grid-set-column-width 0 150))",
    );
    keys(&ed, cx, "ctrl-/");
    assert_eq!(
        cell(&ed, cx, 1, 0),
        "apple",
        "one undo should undo the whole batch"
    );
}

#[gpui::test]
fn typed_edit_after_a_scheme_batch_is_its_own_undo_step(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    scheme(&ed, cx, "(grid-set-cell 3 0 \"S\")");
    keys(&ed, cx, "Z enter");
    assert_eq!(cell(&ed, cx, 0, 0), "Z");
    keys(&ed, cx, "ctrl-/");
    assert_eq!(cell(&ed, cx, 0, 0), "name");
    assert_eq!(
        cell(&ed, cx, 3, 0),
        "S",
        "undoing the typed edit also undid the Scheme edit"
    );
}

#[gpui::test]
fn revert_drops_positional_narrowing(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    keys(&ed, cx, "j j z n");
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert!(g.row_shown(2) && !g.row_shown(1));
    });
    std::fs::write(
        &path,
        "name,qty,note\nkiwi,1,\napple,3,x\npear,007,\nfig,12,ok\n",
    )
    .unwrap();
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::RevertBuffer, cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        let shown: Vec<String> = (1..5)
            .filter(|&r| g.row_shown(r))
            .map(|r| g.cell(r, 0).to_string())
            .collect();
        assert!(
            g.filters.is_empty(),
            "narrowing kept across revert; shows {shown:?} (was pear)"
        );
    });
}

#[gpui::test]
fn same_named_workbooks_get_their_own_sheet_buffers(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, false);
    let dir_a = path.parent().unwrap().join("a");
    let dir_b = path.parent().unwrap().join("b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let a = dir_a.join("multi.xlsx");
    let b = dir_b.join("multi.xlsx");
    for p in [&a, &b] {
        std::fs::write(
            p,
            include_bytes!("../xlsx/testdata/pkg/multi.xlsx").as_slice(),
        )
        .unwrap();
    }
    for p in [&a, &b] {
        let p = p.clone();
        ed.update(cx, |e, cx| {
            e.open_grid(
                "multi.xlsx".into(),
                p.clone(),
                std::fs::read(&p).unwrap(),
                cx,
            );
        });
        settle(&ed, cx);
        scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    }
    let src = ed.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().xlsx.source.clone()
    });
    assert_eq!(
        src.as_deref(),
        Some(b.as_path()),
        "b's sheet shows a's buffer"
    );
}

#[gpui::test]
fn a_far_away_write_is_refused_not_allocated(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    scheme(&ed, cx, "(grid-set-cell 1099511627776 0 \"x\")");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.grid.as_ref().unwrap().used(), (5, 3));
        assert!(e.status.contains("Not written"), "{}", e.status);
    });
}

#[gpui::test]
fn stepping_past_the_first_row_or_column_selects_it(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    let sel = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            let g = e.active.grid.as_ref().unwrap();
            g.view.anchor.map(|_| {
                let r = g.view.selection(g.used());
                (r.r0, r.c0, r.r1, r.c1)
            })
        })
    };
    keys(&ed, cx, "l k");
    assert_eq!(cursor(&ed, cx), (0, 1));
    assert_eq!(
        sel(&ed, cx),
        Some((0, 1, 3, 1)),
        "k on row 1 selects the column"
    );
    keys(&ed, cx, "j");
    assert_eq!(
        (cursor(&ed, cx), sel(&ed, cx)),
        ((0, 1), None),
        "j steps back off it"
    );
    keys(&ed, cx, "j h h");
    assert_eq!(cursor(&ed, cx), (1, 0));
    assert_eq!(
        sel(&ed, cx),
        Some((1, 0, 1, 2)),
        "h on column A selects the row"
    );
    keys(&ed, cx, "l");
    assert_eq!((cursor(&ed, cx), sel(&ed, cx)), ((1, 0), None));
    keys(&ed, cx, "up up");
    assert_eq!(sel(&ed, cx), Some((0, 0, 3, 0)), "arrow keys too");
    keys(&ed, cx, "down down");
    assert_eq!((cursor(&ed, cx), sel(&ed, cx)), ((1, 0), None));
    // Across a selected column, h/l take in the neighbouring columns.
    keys(&ed, cx, "k k l");
    assert_eq!(sel(&ed, cx), Some((0, 0, 3, 1)), "l adds column B");
    keys(&ed, cx, "l h");
    assert_eq!(sel(&ed, cx), Some((0, 0, 3, 1)), "h gives it back");
    keys(&ed, cx, "h h");
    assert_eq!(sel(&ed, cx), Some((0, 0, 3, 0)), "h stops at column A");
    keys(&ed, cx, "j");
    assert_eq!((cursor(&ed, cx), sel(&ed, cx)), ((0, 0), None));
    // Across a selected row, j/k take in the neighbouring rows.
    keys(&ed, cx, "j h j j");
    assert_eq!(sel(&ed, cx), Some((1, 0, 3, 2)), "j adds rows 3 and 4");
    keys(&ed, cx, "k k k k");
    assert_eq!(sel(&ed, cx), Some((0, 0, 1, 2)), "k grows the other way");
    keys(&ed, cx, "l");
    assert_eq!((cursor(&ed, cx), sel(&ed, cx)), ((0, 0), None));
}

#[gpui::test]
fn hiding_columns_is_an_undo_step_in_order_with_inserts(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, false);
    let hidden = |ed: &Entity<Editor>, cx: &mut TestAppContext| -> Vec<usize> {
        ed.read_with(cx, |e, _| {
            e.active.grid.as_ref().unwrap().view.cols.hidden().collect()
        })
    };
    // A width, so the insert records the layout.
    scheme(&ed, cx, "(grid-set-column-width 0 150)");
    scheme(&ed, cx, "(grid-insert-columns 0 1)");
    scheme(&ed, cx, "(grid-set-column-hidden 2 #t)");
    // Undo goes back one step at a time: the hide, then the insert.
    keys(&ed, cx, "ctrl-/");
    assert_eq!(hidden(&ed, cx), Vec::<usize>::new());
    assert_eq!(cell(&ed, cx, 0, 0), "");
    keys(&ed, cx, "ctrl-/");
    assert_eq!(cell(&ed, cx, 0, 0), "name");
    assert_eq!(hidden(&ed, cx), Vec::<usize>::new());
    // And redo replays both.
    keys(&ed, cx, "ctrl-shift-/ ctrl-shift-/");
    assert_eq!(cell(&ed, cx, 0, 0), "");
    assert_eq!(hidden(&ed, cx), vec![2]);
}

#[gpui::test]
fn regression_sheet_buffer_write_over_its_workbook(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, true);
    let book = path.with_file_name("multi.xlsx");
    let orig = include_bytes!("../xlsx/testdata/pkg/multi.xlsx").to_vec();
    std::fs::write(&book, &orig).unwrap();
    let b = book.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "multi.xlsx".into(),
            b.clone(),
            std::fs::read(&b).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    let lossy = ed.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().xlsx.lossy.clone()
    });
    eprintln!("lossy: {lossy:?}");
    let target = book.to_string_lossy().to_string();
    ed.update(cx, |e, cx| e.write_buffer_to(&target, cx));
    settle(&ed, cx);
    let after = crate::xlsx::read(&std::fs::read(&book).unwrap()).unwrap();
    let names: Vec<String> = after.sheets.iter().map(|s| s.name.clone()).collect();
    assert_eq!(
        names,
        ["First", "Second", "Third"],
        "workbook overwritten by one sheet"
    );
}

#[gpui::test]
fn regression_structural_edit_persists_moved_layout(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    scheme(&ed, cx, "(grid-set-column-hidden 2 #t)");
    scheme(&ed, cx, "(grid-delete-columns 0 1)");
    let mem: Vec<usize> = ed.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().view.cols.hidden().collect()
    });
    assert_eq!(mem, vec![1]);
    save(&ed, cx);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap().lines().next(),
        Some("qty,note")
    );
    // Visit the saved file afresh: its layout comes from the sidecar.
    let ed2 = cx.new(Editor::new);
    let p = path.clone();
    ed2.update(cx, |e, cx| {
        e.mark_config_loaded(cx);
        e.open_grid("data.csv".into(), p.clone(), std::fs::read(&p).unwrap(), cx);
    });
    settle(&ed2, cx);
    let reopened: Vec<usize> = ed2.read_with(cx, |e, _| {
        e.active.grid.as_ref().unwrap().view.cols.hidden().collect()
    });
    assert_eq!(reopened, vec![1], "sidecar still has the pre-delete layout");
}
// ---------------------------------------------------------------------------
// Audit: file lifecycle
// ---------------------------------------------------------------------------

fn regression_big(path: &std::path::Path) -> (PathBuf, String) {
    let big = path.with_file_name("big.csv");
    let mut s = String::from("id,text\n");
    let mut i = 0;
    while s.len() < 3 * 1024 * 1024 {
        s.push_str(&format!("{i},some longish filler text for row {i}\n"));
        i += 1;
    }
    std::fs::write(&big, &s).unwrap();
    (big, s)
}

#[gpui::test]
fn regression_wq_during_background_load_truncates(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    let (big, s) = regression_big(&path);
    ed.update(cx, |e, cx| {
        e.open_grid("big.csv".into(), big.clone(), s.clone().into_bytes(), cx);
        assert!(e.active.grid.as_ref().unwrap().loading.is_some());
        // `:wq` / `:x` take this synchronous path.
        e.save_active_sync(cx);
    });
    settle(&ed, cx);
    let after = std::fs::read_to_string(&big).unwrap();
    assert_eq!(
        after.lines().count(),
        s.lines().count(),
        ":wq mid-load truncated the file to {} lines",
        after.lines().count()
    );
}

#[gpui::test]
fn editing_waits_for_the_background_load(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    let (big, s) = regression_big(&path);
    ed.update(cx, |e, cx| {
        e.open_grid("big.csv".into(), big.clone(), s.clone().into_bytes(), cx);
        assert!(e.active.grid.as_ref().unwrap().loading.is_some());
        // RET / F2 is refused like typing: the prefix is replaced when the
        // load lands, and an edit made to it would be lost.
        e.grid_goto_address("B2", cx);
        e.grid_command(crate::grid::cmd::GridCmd::EditEnd, cx);
        assert!(e.active.grid.as_ref().unwrap().view.edit.is_none());
        assert!(e.status.contains("Still loading"), "{}", e.status);
        assert!(!e.active.modified);
    });
}

#[gpui::test]
fn regression_quit_save_after_edit_during_load_truncates(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    let (big, s) = regression_big(&path);
    ed.update(cx, |e, cx| {
        e.open_grid("big.csv".into(), big.clone(), s.clone().into_bytes(), cx);
        e.grid_command(crate::grid::cmd::GridCmd::EditEnd, cx);
        e.grid_edit_input("X", cx);
        e.grid_commit_edit(0, 0, cx);
        // C-x C-c → "yes" (and `:wqa`).
        e.save_all_modified(cx);
    });
    settle(&ed, cx);
    let after = std::fs::read_to_string(&big).unwrap();
    assert_eq!(
        after.lines().count(),
        s.lines().count(),
        "quit-save mid-load truncated the file to {} lines",
        after.lines().count()
    );
}

fn regression_wait_for(
    ed: &Entity<Editor>,
    cx: &mut TestAppContext,
    mut f: impl FnMut(&Editor) -> bool,
    secs: u64,
) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < until {
        settle(ed, cx);
        if ed.read_with(cx, |e, _| f(e)) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[gpui::test]
fn regression_save_as_xlsx_strands_an_in_flight_scheme_call(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, true);
    scheme(
        &ed,
        cx,
        "(begin (define-grid-helper (spin n) (if (= n 0) 42 (spin (- n 1))))
                (define-grid-function (SLOW x) (spin 3000000)))",
    );
    scheme(&ed, cx, "(grid-set-cells (list (list 1 3 \"=SLOW(1)\")))");
    // The call is still running on the grid-function thread.
    assert_ne!(cell(&ed, cx, 1, 3), "42", "call finished too fast to test");
    let xlsx = path.with_extension("xlsx");
    ed.update(cx, |e, cx| e.grid_save_as_xlsx(&xlsx.to_string_lossy(), cx));
    let done = regression_wait_for(
        &ed,
        cx,
        |e| e.active.grid.as_ref().unwrap().cell(1, 3) == "42",
        20,
    );
    let shown = cell(&ed, cx, 1, 3);
    assert!(
        done,
        "SLOW(1) never resolved after Save As renamed the buffer: D2 = {shown:?}"
    );
}

#[gpui::test]
fn regression_control_slow_call_resolves_without_save_as(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, CSV, true);
    scheme(
        &ed,
        cx,
        "(begin (define-grid-helper (spin n) (if (= n 0) 42 (spin (- n 1))))
                (define-grid-function (SLOW x) (spin 3000000)))",
    );
    scheme(&ed, cx, "(grid-set-cells (list (list 1 3 \"=SLOW(1)\")))");
    assert_ne!(cell(&ed, cx, 1, 3), "42");
    let done = regression_wait_for(
        &ed,
        cx,
        |e| e.active.grid.as_ref().unwrap().cell(1, 3) == "42",
        20,
    );
    assert!(done, "control: {:?}", cell(&ed, cx, 1, 3));
}

#[gpui::test]
fn regression_sheet_buffer_goes_stale_when_the_workbook_changes_on_disk(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, false);
    let book = path.with_file_name("multi.xlsx");
    std::fs::write(
        &book,
        include_bytes!("../xlsx/testdata/pkg/multi.xlsx").as_slice(),
    )
    .unwrap();
    let b = book.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "multi.xlsx".into(),
            b.clone(),
            std::fs::read(&b).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    assert_eq!(cell(&ed, cx, 0, 0), "two");
    // Another app rewrites the workbook: "one" → "ONE", "two" → "TWO".
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/xlsx/testdata/pkg/multi_changed.xlsx"
        ),
        &book,
    )
    .unwrap();
    ed.update(cx, |e, cx| e.auto_revert_file(&b, cx));
    settle(&ed, cx);
    let first = ed.read_with(cx, |e, _| {
        let d = e.inactive.iter().find(|d| d.name == "multi.xlsx").unwrap();
        d.grid.as_ref().unwrap().cell(0, 0).to_string()
    });
    assert_eq!(first, "ONE", "the file's own buffer reverted");
    // Neither buffer is modified, yet the open sheet still shows the old file.
    assert_eq!(
        cell(&ed, cx, 0, 0),
        "TWO",
        "sheet buffer multi.xlsx<Second> kept the old file's contents"
    );
}

#[gpui::test]
fn regression_quit_save_yes_discards_edited_sheet_buffer(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, false);
    let book = path.with_file_name("multi.xlsx");
    std::fs::write(
        &book,
        include_bytes!("../xlsx/testdata/pkg/multi.xlsx").as_slice(),
    )
    .unwrap();
    let b = book.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "multi.xlsx".into(),
            b.clone(),
            std::fs::read(&b).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    scheme(&ed, cx, "(grid-set-cell 1 0 \"important\")");
    assert!(ed.read_with(cx, |e, _| e.active.modified));
    assert!(ed.read_with(cx, |e, _| e.any_modified()), "quit prompts");
    // C-x C-c → "yes": quit proceeds when this returns true.
    let ok = ed.update(cx, |e, cx| e.save_all_modified(cx));
    let still = ed.read_with(cx, |e, _| e.active.modified);
    assert!(
        !(ok && still),
        "quit-save reported success with the edited sheet buffer unsaved (would be discarded)"
    );
}

#[gpui::test]
fn regression_before_save_hook_switching_buffers_writes_wrong_grid(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    let other = path.with_file_name("other.csv");
    std::fs::write(&other, "x,y\n1,2\n").unwrap();
    let o = other.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "other.csv".into(),
            o.clone(),
            std::fs::read(&o).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    scheme(&ed, cx, "(grid-set-cell 1 1 \"3\")");
    scheme(
        &ed,
        cx,
        "(begin (define (audit-peek) (switch-to-buffer \"data.csv\"))
                (add-hook \"before-save-hook\" \"audit-peek\"))",
    );
    save(&ed, cx);
    let on_disk = std::fs::read_to_string(&other).unwrap();
    assert_eq!(
        on_disk, "x,y\n1,3\n",
        "other.csv got another buffer's contents"
    );
}

fn regression_formula_book(n: usize) -> Vec<u8> {
    let mut cells = Vec::new();
    for i in 0..n {
        cells.push((i, 0, crate::xlsx::XValue::Number(i as f64), None));
        cells.push((
            i,
            1,
            crate::xlsx::XValue::Number(0.0),
            Some(format!("A{}*2", i + 1)),
        ));
    }
    crate::xlsx::write(
        "Data",
        cells.iter().map(|(r, c, v, f)| crate::xlsx::WriteCell {
            row: *r,
            col: *c,
            value: v,
            formula: f.as_deref(),
            style: None,
        }),
        false,
    )
}

#[gpui::test]
fn two_big_workbooks_opened_together_both_finish_recalculating(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    let n = 150_000;
    let bytes = regression_formula_book(n);
    let a = path.with_file_name("a.xlsx");
    let b = path.with_file_name("b.xlsx");
    std::fs::write(&a, &bytes).unwrap();
    std::fs::write(&b, &bytes).unwrap();
    // Two files dropped at once: both open before the first finishes.
    ed.update(cx, |e, cx| {
        e.open_grid("a.xlsx".into(), a.clone(), bytes.clone(), cx);
        e.open_grid("b.xlsx".into(), b.clone(), bytes.clone(), cx);
        assert!(
            e.active.grid.as_ref().unwrap().formulas.busy(),
            "recalc fit in one slice; raise n"
        );
    });
    // The recalculation loop runs on timers: advance the test clock.
    for _ in 0..400 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(5));
        cx.run_until_parked();
    }
    ed.read_with(cx, |e, _| {
        let a = e.inactive.iter().find(|d| d.name == "a.xlsx").unwrap();
        assert!(
            !a.grid.as_ref().unwrap().formulas.busy(),
            "a.xlsx unfinished"
        );
    });
    let (busy, last) = ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        let wrong = (0..n)
            .filter(|&i| g.cell(i, 1) != (2 * i).to_string())
            .count();
        (
            g.formulas.busy(),
            format!("{} wrong cells; last {}", wrong, g.cell(n - 1, 1)),
        )
    });
    assert!(
        !busy,
        "b.xlsx still mid-recalc with no loop driving it; B{n} shows {last:?}"
    );
}

#[gpui::test]
fn regression_sheet_opened_after_an_edit_reads_the_file_not_the_edit(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, NUMS, false);
    let book = path.with_file_name("multi.xlsx");
    std::fs::write(
        &book,
        include_bytes!("../xlsx/testdata/pkg/multi.xlsx").as_slice(),
    )
    .unwrap();
    let b = book.clone();
    ed.update(cx, |e, cx| {
        e.open_grid(
            "multi.xlsx".into(),
            b.clone(),
            std::fs::read(&b).unwrap(),
            cx,
        );
    });
    settle(&ed, cx);
    // Edit First before any other sheet is open.
    scheme(&ed, cx, "(grid-set-cell 0 0 \"uno\")");
    scheme(&ed, cx, "(grid-open-sheet \"Second\")");
    scheme(&ed, cx, "(grid-set-cells (list (list 0 1 \"=First!A1\")))");
    assert_eq!(
        cell(&ed, cx, 0, 1),
        "uno",
        "Second's =First!A1 reads the file's value, not First's unsaved edit"
    );
}

#[gpui::test]
fn regression_revert_to_unreadable_xlsx_claims_success(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "a\n1\n", false);
    let x = path.with_extension("xlsx");
    let cells = [(0, 0, crate::xlsx::XValue::Text("n".into()), None::<&str>)];
    let bytes = crate::xlsx::write(
        "Data",
        cells.iter().map(|(r, c, v, f)| crate::xlsx::WriteCell {
            row: *r,
            col: *c,
            value: v,
            formula: *f,
            style: None,
        }),
        false,
    );
    std::fs::write(&x, &bytes).unwrap();
    ed.update(cx, |e, cx| {
        e.open_grid("data.xlsx".into(), x.clone(), bytes, cx)
    });
    settle(&ed, cx);
    scheme(&ed, cx, "(grid-set-cell 0 0 \"mine\")");
    // The file is replaced by something unreadable (half-written, …).
    std::fs::write(&x, b"PK\x03\x04garbage").unwrap();
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::RevertBuffer, cx));
    settle(&ed, cx);
    let (status, shown, hist) = ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        (e.status.to_string(), g.cell(0, 0).to_string(), g.hist_len())
    });
    assert!(
        !status.starts_with("Reverted"),
        "revert said {status:?} but the buffer still shows {shown:?} (undo history: {hist} entries)"
    );
}

#[gpui::test]
fn regression_toggle_view_round_trip_drops_formatting(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, false);
    keys(&ed, cx, "ctrl-n ctrl-f alt-o b");
    assert!(fmt(&ed, cx, 1, 1).bold);
    keys(&ed, cx, "ctrl-c ctrl-t");
    keys(&ed, cx, "ctrl-c ctrl-t");
    assert!(
        fmt(&ed, cx, 1, 1).bold,
        "bold on B2 lost by grid → text → grid with no edits and no warning"
    );
}

#[gpui::test]
fn regression_write_to_own_path_leaves_buffer_modified(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, CSV, false);
    scheme(&ed, cx, "(grid-set-cell 1 0 \"kiwi\")");
    let p = path.to_string_lossy().into_owned();
    ed.update(cx, |e, cx| e.write_buffer_to(&p, cx));
    settle(&ed, cx);
    assert!(std::fs::read_to_string(&path).unwrap().contains("kiwi"));
    // Another program then edits the file: auto-revert should pick it up
    // (the buffer equals what's on disk), but it thinks there are unsaved edits.
    std::fs::write(&path, "name,qty,note\nplum,1,\n").unwrap();
    ed.update(cx, |e, cx| e.auto_revert_file(&path, cx));
    settle(&ed, cx);
    let (modified, a2) = ed.read_with(cx, |e, _| {
        (
            e.active.modified,
            e.active.grid.as_ref().unwrap().cell(1, 0).to_string(),
        )
    });
    assert!(
        !modified && a2 == "plum",
        ":w <own file> left modified={modified}, so the outside change was ignored (A2 = {a2:?})"
    );
}

#[gpui::test]
fn regression_write_xlsx_grid_as_csv_writes_a_zip(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "a\n1\n", false);
    let x = path.with_extension("xlsx");
    let cells = [
        (0, 0, crate::xlsx::XValue::Text("n".into()), None::<&str>),
        (1, 0, crate::xlsx::XValue::Number(4.0), None),
    ];
    let bytes = crate::xlsx::write(
        "Data",
        cells.iter().map(|(r, c, v, f)| crate::xlsx::WriteCell {
            row: *r,
            col: *c,
            value: v,
            formula: *f,
            style: None,
        }),
        false,
    );
    std::fs::write(&x, &bytes).unwrap();
    ed.update(cx, |e, cx| {
        e.open_grid("data.xlsx".into(), x.clone(), bytes, cx)
    });
    settle(&ed, cx);
    let out = path.with_file_name("export.csv");
    let o = out.to_string_lossy().into_owned();
    // `:w export.csv`
    ed.update(cx, |e, cx| e.write_buffer_to(&o, cx));
    settle(&ed, cx);
    let written = std::fs::read(&out).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&written),
        "n\n4\n",
        "export.csv holds {} bytes starting {:?}",
        written.len(),
        &written[..4.min(written.len())]
    );
}

/// What cell `(r, c)` shows under its number format, and whether it aligns
/// right; `None` without one.
fn shown(
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
    r: usize,
    c: usize,
) -> Option<(String, bool)> {
    editor.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        g.formats
            .get(r, c)
            .num
            .and_then(|n| g.number_formatted(r, c, n))
    })
}

const PRICES: &str = "item,price,when\na,1234.5,2024-03-01\nb,0.125,45000\n";

#[gpui::test]
fn number_formats_show_values_without_changing_them(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, PRICES, false);
    let s = |text: &str, right| Some((text.to_string(), right));
    // B2: currency, then a decimal place more and four fewer.
    keys(&ed, cx, "ctrl-n ctrl-f alt-o $");
    assert_eq!(shown(&ed, cx, 1, 1), s("$1,234.50", true));
    assert_eq!(cell(&ed, cx, 1, 1), "1234.5", "the value is untouched");
    keys(&ed, cx, "alt-o .");
    assert_eq!(shown(&ed, cx, 1, 1), s("$1,234.500", true));
    keys(&ed, cx, "alt-o , alt-o , alt-o , alt-o ,");
    assert_eq!(shown(&ed, cx, 1, 1), s("$1,235", true));
    keys(&ed, cx, "alt-o ,");
    assert!(ed.read_with(cx, |e, _| e.status.contains("No fewer decimal places")));
    // Each change is one undo step.
    keys(&ed, cx, "ctrl-/");
    assert_eq!(shown(&ed, cx, 1, 1), s("$1,234.5", true));
    // B3: percent; General steps decimals from what the value shows.
    keys(&ed, cx, "ctrl-n alt-o %");
    assert_eq!(shown(&ed, cx, 2, 1), s("12.50%", true));
    keys(&ed, cx, "alt-o ~");
    assert_eq!(shown(&ed, cx, 2, 1), None);
    keys(&ed, cx, "alt-o .");
    assert_eq!(shown(&ed, cx, 2, 1), s("0.1250", true));
    // C2 holds date text, C3 a date serial: both show as dates.
    keys(&ed, cx, "ctrl-p ctrl-f shift-down alt-o #");
    assert_eq!(shown(&ed, cx, 1, 2), s("2024-03-01", true));
    assert_eq!(shown(&ed, cx, 2, 2), s("2023-03-15", true));
    // Text keeps a number as typed, on the left.
    keys(&ed, cx, "ctrl-g ctrl-p ctrl-b alt-o @");
    assert_eq!(shown(&ed, cx, 1, 1), s("1234.5", false));
    // Any format code, from the prompt or from Scheme.
    keys(&ed, cx, "alt-o n");
    submit(&ed, cx, "#,##0.0 \"kg\"");
    assert_eq!(shown(&ed, cx, 1, 1), s("1,234.5 kg", true));
    keys(&ed, cx, "alt-o n");
    submit(&ed, cx, "currency");
    assert_eq!(shown(&ed, cx, 1, 1), s("$1,234.50", true));
    scheme(
        &ed,
        cx,
        "(grid-set-cell-number-format 2 \"C\" \"d mmm yyyy\")",
    );
    assert_eq!(shown(&ed, cx, 2, 2), s("15 Mar 2023", true));
    scheme(&ed, cx, "(grid-set-cell-number-format 2 \"C\" 'general)");
    assert_eq!(shown(&ed, cx, 2, 2), None);
    // Clearing formatting clears the number format too.
    keys(&ed, cx, "alt-o d");
    assert!(fmt(&ed, cx, 1, 1).is_plain());
}

#[gpui::test]
fn formatted_csv_saves_as_xlsx_and_reopens_formatted(cx: &mut TestAppContext) {
    use crate::grid::cellfmt::{NumCode, color_index};
    let (ed, path) = setup(cx, PRICES, true);
    // Evil: `f` is the format prefix. B2 currency + bold; an empty E2 filled.
    keys(&ed, cx, "j l f $ f b l l l f f");
    assert!(fmt(&ed, cx, 1, 1).bold);
    assert_eq!(fmt(&ed, cx, 1, 4).fill, color_index("red"));
    // Saving offers Save As .xlsx, naming why.
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let mb = e.minibuffer.as_ref().expect("a save-as prompt");
        assert!(matches!(mb.purpose, MinibufferPurpose::GridSaveAsXlsx));
        assert!(mb.prompt.contains("formatting"), "{}", mb.prompt);
    });
    ed.update(cx, |e, _| e.minibuffer = None);
    let ok = ed.update(cx, |e, cx| e.save_all_modified(cx));
    assert!(!ok, "quit-time save refuses a formatted CSV");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), PRICES);
    // The .xlsx keeps the formatting and opens with it again, unprotected.
    let xlsx = path.with_extension("xlsx");
    ed.update(cx, |e, cx| e.grid_save_as_xlsx(&xlsx.to_string_lossy(), cx));
    settle(&ed, cx);
    let l = super::grid_xlsx::load(&std::fs::read(&xlsx).unwrap(), 0, &Default::default()).unwrap();
    assert!(l.lossy.is_empty(), "{:?}", l.lossy);
    let mut formats = l.formats.clone();
    formats.sort_by_key(|(at, _)| *at);
    assert_eq!(
        formats,
        [
            (
                (1, 1),
                crate::grid::cellfmt::CellFmt {
                    bold: true,
                    num: NumCode::intern("$#,##0.00"),
                    ..Default::default()
                }
            ),
            (
                (1, 4),
                crate::grid::cellfmt::CellFmt {
                    fill: color_index("red"),
                    ..Default::default()
                }
            ),
        ]
    );
    assert_eq!(l.wb.cell(SHEET, 1, 1), "1234.5");
    // Saving the .xlsx in place keeps it too.
    keys(&ed, cx, "f %");
    save(&ed, cx);
    let l = super::grid_xlsx::load(&std::fs::read(&xlsx).unwrap(), 0, &Default::default()).unwrap();
    let e2 = l.formats.iter().find(|(at, _)| *at == (1, 4)).unwrap().1;
    assert_eq!(e2.num, NumCode::intern("0.00%"));
}

#[gpui::test]
fn helix_grid_format_keys(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, PRICES, true);
    ed.update(cx, |e, cx| e.run_script_blocking("(helix-mode)", cx));
    settle(&ed, cx);
    assert!(ed.read_with(cx, |e, _| e.modal_helix));
    keys(&ed, cx, "j l f %");
    assert_eq!(shown(&ed, cx, 1, 1), Some(("123450.00%".to_string(), true)));
    keys(&ed, cx, "f ,");
    assert_eq!(shown(&ed, cx, 1, 1), Some(("123450.0%".to_string(), true)));
    keys(&ed, cx, "f ~");
    assert_eq!(shown(&ed, cx, 1, 1), None);
}

/// Wrap Text: `M-o w` (`f w` under evil) and `grid-set-cell-wrap`, one undo
/// step each; the flag and the row's automatic height move with a sort; an
/// `.xlsx` keeps the flag and heights set by hand, and opens unprotected.
#[gpui::test]
fn wrap_text_toggles_follows_sorts_and_saves_to_xlsx(cx: &mut TestAppContext) {
    use crate::grid::view::DEFAULT_ROW_HEIGHT;
    let (ed, path) = setup(cx, NUMS, false);
    keys(&ed, cx, "ctrl-n ctrl-f alt-o w");
    assert!(fmt(&ed, cx, 1, 1).wrap);
    assert!(ed.read_with(cx, |e, _| e.status.contains("Wrap text")));
    keys(&ed, cx, "ctrl-/");
    assert!(!fmt(&ed, cx, 1, 1).wrap);
    keys(&ed, cx, "ctrl-shift-/");
    assert!(fmt(&ed, cx, 1, 1).wrap);
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cell 1 0 \"one two three four five six\")
                (grid-set-cell-wrap 1 0 #t))",
    );
    assert!(fmt(&ed, cx, 1, 0).wrap);
    let m = crate::grid::wrap::Metrics {
        char_px: 8.0,
        line_px: 18.0,
        pad_px: 3.0,
    };
    let fit = |cx: &mut TestAppContext| {
        ed.update(cx, |e, _| {
            let g = e.active.grid.as_deref_mut().unwrap();
            g.view.cols.set(0, 64.0);
            g.fit_wrapped_rows(0..6, m);
        })
    };
    let height = |cx: &mut TestAppContext, r| {
        ed.read_with(cx, |e, _| e.active.grid.as_ref().unwrap().view.rows.size(r))
    };
    fit(cx);
    // "one two" / "three" / "four" / "five" / "six".
    let tall = DEFAULT_ROW_HEIGHT + 4.0 * 18.0;
    assert_eq!(height(cx, 1), tall);
    // Sorting n descending takes row 2 (n = 1) to the bottom, wrap and
    // height and all; undo brings them back.
    keys(&ed, cx, "ctrl-c ctrl-r");
    assert_eq!(cell(&ed, cx, 4, 0), "one two three four five six");
    assert!(fmt(&ed, cx, 4, 0).wrap && fmt(&ed, cx, 4, 1).wrap);
    assert!(!fmt(&ed, cx, 1, 0).wrap);
    assert_eq!(height(cx, 4), tall);
    assert_eq!(height(cx, 1), DEFAULT_ROW_HEIGHT);
    keys(&ed, cx, "ctrl-/");
    assert!(fmt(&ed, cx, 1, 0).wrap && !fmt(&ed, cx, 4, 0).wrap);
    fit(cx);
    assert_eq!(height(cx, 1), tall);
    // A height set by hand wins, and saves.
    scheme(&ed, cx, "(grid-set-row-height 2 45)");
    let xlsx = path.with_extension("xlsx");
    ed.update(cx, |e, cx| e.grid_save_as_xlsx(&xlsx.to_string_lossy(), cx));
    settle(&ed, cx);
    let l = super::grid_xlsx::load(&std::fs::read(&xlsx).unwrap(), 0, &Default::default()).unwrap();
    assert!(l.lossy.is_empty(), "{:?}", l.lossy);
    let wrapped: Vec<_> = l
        .formats
        .iter()
        .filter(|(_, f)| f.wrap)
        .map(|(at, _)| *at)
        .collect();
    assert_eq!(wrapped.len(), 2);
    assert!(wrapped.contains(&(1, 0)) && wrapped.contains(&(1, 1)));
    assert_eq!(l.rows, [(2, 45.0)], "only heights set by hand read back");
    assert_eq!(l.wb.cell(SHEET, 1, 0), "one two three four five six");
}

#[gpui::test]
fn evil_wrap_text_key(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, NUMS, true);
    keys(&ed, cx, "j f w");
    assert!(fmt(&ed, cx, 1, 0).wrap);
    keys(&ed, cx, "f d");
    assert!(fmt(&ed, cx, 1, 0).is_plain());
}
