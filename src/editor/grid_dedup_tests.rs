//! Remove duplicates (`grid-remove-duplicates`) end to end: a real `Editor`
//! with the bundled Scheme, so the keymaps, the prompt and the Scheme API in
//! `scheme/grid.scm` are the ones exercised.

use std::path::PathBuf;

use gpui::{Entity, Keystroke, TestAppContext};

use super::*;

fn setup(cx: &mut TestAppContext, csv: &str, mode: &str) -> Entity<Editor> {
    cx.update(|cx| {
        cx.set_global(crate::theme::Theme::default());
        cx.set_global(crate::settings::Settings::default());
    });
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-grid-dedup-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("data.csv");
    std::fs::write(&path, csv).unwrap();
    let editor = cx.new(Editor::new);
    editor.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        if mode != "emacs" {
            e.run_script_blocking("(evil-mode)", cx);
        }
        e.mark_config_loaded(cx);
        if mode != "emacs" {
            e.evil = EvilState::Normal;
        }
        e.open_grid("data.csv".into(), path.clone(), csv.as_bytes().to_vec(), cx);
        if mode == "helix" {
            e.run_script_blocking("(helix-mode)", cx);
        }
    });
    settle(&editor, cx);
    editor
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
        editor.update(cx, |e, cx| {
            let ks: Keystroke = if k.chars().count() == 1 {
                super::ex::token_keystroke(k).expect("printable key")
            } else {
                Keystroke::parse(k).expect("keystroke")
            };
            e.dispatch_keystroke(&ks, cx);
        });
        settle(editor, cx);
    }
}

fn scheme(editor: &Entity<Editor>, cx: &mut TestAppContext, src: &str) {
    editor.update(cx, |e, _| {
        e.publish_point();
        e.script.dispatch(src, false);
    });
    settle(editor, cx);
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

/// Column `c` as shown, every row.
fn column(editor: &Entity<Editor>, cx: &mut TestAppContext, c: usize) -> Vec<String> {
    editor.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        (0..g.used().0).map(|r| g.cell(r, c).to_string()).collect()
    })
}

/// The last echo-area message (and clear them).
fn message(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
    editor.update(cx, |e, _| {
        let last = e.messages.last().cloned().unwrap_or_default();
        e.messages.clear();
        last
    })
}

fn undo(editor: &Entity<Editor>, cx: &mut TestAppContext) {
    scheme(editor, cx, "(grid-undo)");
}

const PEOPLE: &str = "email,name,n\n\
    a@x.com,Ann,1\n\
    b@x.com,Bob,2\n\
    A@X.COM,Ann,1.0\n\
    c@x.com,Cy,3\n\
    b@x.com,Bobby,2\n";

#[gpui::test]
fn key_column_subsets(cx: &mut TestAppContext) {
    let ed = setup(cx, PEOPLE, "emacs");
    let all = column(&ed, cx, 0);
    // By header name: case is ignored.
    scheme(&ed, cx, "(grid-remove-duplicates '(\"email\"))");
    assert_eq!(
        column(&ed, cx, 0),
        ["email", "a@x.com", "b@x.com", "c@x.com"]
    );
    assert_eq!(message(&ed, cx), "Removed 2 duplicate rows, 3 remain");
    undo(&ed, cx);
    assert_eq!(column(&ed, cx, 0), all, "one undo step");
    ed.read_with(cx, |e, _| assert!(!e.active.modified));
    // Every column (the default): only row 3 repeats row 1 (1 = 1.0).
    scheme(&ed, cx, "(grid-remove-duplicates '())");
    assert_eq!(column(&ed, cx, 1), ["name", "Ann", "Bob", "Cy", "Bobby"]);
    assert_eq!(message(&ed, cx), "Removed 1 duplicate row, 4 remain");
    undo(&ed, cx);
    // Two columns, a letter, an index, a symbol, a comma-separated string.
    for (cols, left) in [
        ("'(\"email\" \"name\")", 4),
        ("'(\"B\")", 4),
        ("'(2)", 3),
        ("'(n)", 3),
        ("\"name, n\"", 4),
        ("\"Name (B)\"", 4),
    ] {
        scheme(&ed, cx, &format!("(grid-remove-duplicates {cols})"));
        assert_eq!(column(&ed, cx, 0).len(), left + 1, "{cols}");
        undo(&ed, cx);
        assert_eq!(column(&ed, cx, 0), all, "{cols}");
    }
    // A column that doesn't exist changes nothing.
    message(&ed, cx);
    scheme(&ed, cx, "(grid-remove-duplicates '(\"phone\"))");
    assert_eq!(column(&ed, cx, 0), all);
    assert_eq!(
        message(&ed, cx),
        "grid-remove-duplicates: no column \"phone\""
    );
}

#[gpui::test]
fn header_row_is_never_removed(cx: &mut TestAppContext) {
    let csv = "id,v\nid,v\nx,1\nid,v\n";
    let ed = setup(cx, csv, "emacs");
    scheme(&ed, cx, "(grid-set-header #t)");
    scheme(&ed, cx, "(grid-remove-duplicates '())");
    // Row 2 repeats the header but is the first data row of its kind.
    assert_eq!(column(&ed, cx, 0), ["id", "id", "x"]);
    undo(&ed, cx);
    // Without a header row, row 1 is data like any other.
    scheme(&ed, cx, "(grid-set-header #f)");
    scheme(&ed, cx, "(grid-remove-duplicates '())");
    assert_eq!(column(&ed, cx, 0), ["id", "x"]);
}

#[gpui::test]
fn comparison_edge_cases(cx: &mut TestAppContext) {
    // Whitespace counts, numbers compare by value, a formula by its result.
    let csv = "k\na\na \n1e0\n1\n2\n\n\nTRUE\ntrue\n";
    let ed = setup(cx, csv, "emacs");
    scheme(&ed, cx, "(grid-set-header #t)");
    scheme(&ed, cx, "(grid-set-cell 10 0 \"=1+1\")");
    scheme(&ed, cx, "(grid-set-cell 11 0 \"=\\\"A\\\"\")");
    assert_eq!(column(&ed, cx, 0)[10..], ["2", "A"]);
    scheme(&ed, cx, "(grid-remove-duplicates '())");
    assert_eq!(column(&ed, cx, 0), ["k", "a", "a ", "1e0", "2", "", "TRUE"]);
    assert_eq!(message(&ed, cx), "Removed 5 duplicate rows, 6 remain");
}

#[gpui::test]
fn formulas_and_formatting_follow_the_rows(cx: &mut TestAppContext) {
    let csv = "k,v\na,1\nb,2\na,3\nc,4\nb,5\n";
    let ed = setup(cx, csv, "emacs");
    scheme(
        &ed,
        cx,
        "(begin (grid-set-cell 1 2 \"=SUM(B2:B6)\") (grid-set-cell 2 2 \"=B4\") \
         (grid-set-cell 4 2 \"=B5*10\") (grid-set-cell 6 0 \"=COUNTA(A2:A6)\") \
         (grid-set-cell-bold 4 0 #t))",
    );
    let before = column(&ed, cx, 2);
    scheme(&ed, cx, "(grid-remove-duplicates '(\"k\"))");
    assert_eq!(message(&ed, cx), "Removed 2 duplicate rows, 4 remain");
    let source = |ed: &Entity<Editor>, cx: &mut TestAppContext, r, c| {
        ed.read_with(cx, |e, _| {
            e.active.grid.as_ref().unwrap().source(r, c).to_string()
        })
    };
    assert_eq!(column(&ed, cx, 0), ["k", "a", "b", "c", "3"]);
    // A range shrinks, a reference to a deleted row breaks, others follow.
    assert_eq!(source(&ed, cx, 1, 2), "=SUM(B2:B4)");
    assert_eq!(column(&ed, cx, 2)[1], "7");
    assert_eq!(source(&ed, cx, 2, 2), "=#REF!");
    assert_eq!(source(&ed, cx, 3, 2), "=B4*10");
    assert_eq!(column(&ed, cx, 2)[3], "40");
    assert_eq!(source(&ed, cx, 4, 0), "=COUNTA(A2:A4)");
    // Bold moved up with its row.
    ed.read_with(cx, |e, _| {
        let g = e.active.grid.as_ref().unwrap();
        assert!(g.formats.get(3, 0).bold && !g.formats.get(4, 0).bold);
    });
    // One undo puts back the rows, the formulas and the formatting.
    undo(&ed, cx);
    assert_eq!(column(&ed, cx, 0)[..6], ["k", "a", "b", "a", "c", "b"]);
    assert_eq!(column(&ed, cx, 2), before);
    assert_eq!(source(&ed, cx, 2, 2), "=B4");
    ed.read_with(cx, |e, _| {
        assert!(e.active.grid.as_ref().unwrap().formats.get(4, 0).bold)
    });
    scheme(&ed, cx, "(grid-redo)");
    assert_eq!(source(&ed, cx, 2, 2), "=#REF!");
}

#[gpui::test]
fn rows_a_filter_hides_take_no_part(cx: &mut TestAppContext) {
    let csv = "k,v\na,1\na,5\na,6\nb,7\nb,1\n";
    let ed = setup(cx, csv, "emacs");
    scheme(&ed, cx, "(grid-filter-rows \"B\" \">\" \"1\")");
    scheme(&ed, cx, "(grid-remove-duplicates '(\"k\"))");
    // Hidden rows 2 and 6 are neither deleted nor count as a first one.
    assert_eq!(column(&ed, cx, 1), ["v", "1", "5", "7", "1"]);
    assert_eq!(message(&ed, cx), "Removed 1 duplicate row, 2 remain");
}

#[gpui::test]
fn a_selection_limits_rows_and_default_columns(cx: &mut TestAppContext) {
    let csv = "k,v\na,1\na,2\nb,3\na,4\n";
    let ed = setup(cx, csv, "emacs");
    // Rows 2–4 of column A only: row 5's "a" is outside, column B ignored.
    scheme(&ed, cx, "(grid-select-range 1 0 3 0)");
    scheme(&ed, cx, "(grid-remove-duplicates '())");
    assert_eq!(column(&ed, cx, 1), ["v", "1", "3", "4"]);
}

#[gpui::test]
fn keys_prompt_for_columns(cx: &mut TestAppContext) {
    // Emacs: C-c C-d, empty input compares every column.
    let ed = setup(cx, PEOPLE, "emacs");
    keys(&ed, cx, "ctrl-c ctrl-d");
    let prompt = ed.read_with(cx, |e, _| e.minibuffer.as_ref().map(|m| m.prompt.clone()));
    assert!(prompt.unwrap().starts_with("Remove duplicates"));
    assert_eq!(
        ed.read_with(cx, |e, _| e.completing_candidates.clone()),
        ["email", "name", "n"],
        "header names offered"
    );
    submit(&ed, cx, "");
    assert_eq!(column(&ed, cx, 0).len(), 5);
    // Evil: d u, by name.
    let ed = setup(cx, PEOPLE, "evil");
    keys(&ed, cx, "d u");
    submit(&ed, cx, "email");
    assert_eq!(column(&ed, cx, 0).len(), 4);
    keys(&ed, cx, "u");
    assert_eq!(column(&ed, cx, 0).len(), 6);
    // Helix: g x.
    let ed = setup(cx, PEOPLE, "helix");
    keys(&ed, cx, "g x");
    submit(&ed, cx, "name, n");
    assert_eq!(column(&ed, cx, 0).len(), 5);
    // No header row: letters are offered.
    let ed = setup(cx, "1,2\n1,2\n", "emacs");
    scheme(&ed, cx, "(grid-set-header #f)");
    scheme(&ed, cx, "(grid-remove-duplicates)");
    assert_eq!(
        ed.read_with(cx, |e, _| e.completing_candidates.clone()),
        ["A", "B"]
    );
    submit(&ed, cx, "B");
    assert_eq!(column(&ed, cx, 0), ["1"]);
}

#[gpui::test]
fn an_m_x_command(cx: &mut TestAppContext) {
    let ed = setup(cx, PEOPLE, "emacs");
    let listed = ed.read_with(cx, |e, _| {
        e.script
            .eval("(if (member \"grid-remove-duplicates\" *interactive-commands*) 1 0)")
            .ok()
            .and_then(|v| v.value)
    });
    assert_eq!(listed.as_deref(), Some("1"));
}
