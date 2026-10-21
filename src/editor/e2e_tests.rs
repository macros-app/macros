//! End-to-end evil tests: a real `Editor` on gpui's headless test platform,
//! the bundled Scheme loaded, keys fed through the same layers as typing.
//! Each key waits for the Scheme engine to drain (a blocking eval queues
//! behind everything already dispatched) and for the outcomes to apply.

use gpui::{Entity, Keystroke, TestAppContext};

use super::*;

fn setup(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
    test_context(cx); // foreign-thread wakes: see new_fs_debouncer in mod.rs
    let editor = cx.new(Editor::new);
    editor.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        e.run_script_blocking("(evil-mode)", cx);
        e.mark_config_loaded(cx);
        // A plain-text buffer with the test's contents.
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("test.txt".into(), None, text));
        e.evil = EvilState::Normal;
    });
    settle(&editor, cx);
    editor
}

/// Let every dispatched Scheme command run and its commands apply (replays
/// chain several rounds).
fn settle(editor: &Entity<Editor>, cx: &mut TestAppContext) {
    for _ in 0..12 {
        cx.run_until_parked();
        editor.read_with(cx, |e, _| {
            let _ = e.script.eval("0");
        });
        cx.run_until_parked();
    }
}

fn keystroke(k: &str) -> Keystroke {
    if k.chars().count() == 1 {
        super::ex::token_keystroke(k).expect("printable key")
    } else {
        Keystroke::parse(k).expect("keystroke")
    }
}

/// Feed keys: space-separated tokens (`"d 2 w"`, `"ctrl-w l"`), each a
/// single char or a gpui keystroke name. In Insert state printable keys type.
fn keys(editor: &Entity<Editor>, cx: &mut TestAppContext, seq: &str) {
    for k in seq.split(' ').filter(|k| !k.is_empty()) {
        let k = if k == "SPC" { " " } else { k };
        let ks = keystroke(k);
        editor.update(cx, |e, cx| {
            // A pending one-key capture (`C-r x`) sees the key first, as it
            // does for a real keystroke; otherwise Insert-state text types.
            let capturing = e.vim.capture.is_some() || e.read_char_pending.is_some();
            if e.evil == EvilState::Insert
                && !capturing
                && !ks.modifiers.control
                && k.chars().count() == 1
            {
                e.type_text(None, k, cx);
            } else {
                e.dispatch_keystroke(&ks, cx);
            }
        });
        settle(editor, cx);
    }
}

fn text(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
    editor.read_with(cx, |e, _| e.active.buffer.full_text())
}

fn point(editor: &Entity<Editor>, cx: &mut TestAppContext) -> usize {
    editor.read_with(cx, |e, _| e.point())
}

fn ex(editor: &Entity<Editor>, cx: &mut TestAppContext, line: &str) {
    editor.update(cx, |e, cx| e.run_ex_command(line, cx));
    settle(editor, cx);
}

#[gpui::test]
fn operators_counts_and_dot(cx: &mut TestAppContext) {
    let ed = setup(cx, "one two three four five six\n");
    keys(&ed, cx, "d w");
    assert_eq!(text(&ed, cx), "two three four five six\n");
    keys(&ed, cx, "2 d 2 w");
    assert_eq!(text(&ed, cx), "six\n");
    // dot repeats the last change, 3. with a new count.
    let ed = setup(cx, "abcdefgh\n");
    keys(&ed, cx, "x");
    keys(&ed, cx, ".");
    assert_eq!(text(&ed, cx), "cdefgh\n");
    keys(&ed, cx, "3 .");
    assert_eq!(text(&ed, cx), "fgh\n");
    // cw + typed text + Esc, then . on the next word; u undoes one change.
    let ed = setup(cx, "foo bar baz\n");
    keys(&ed, cx, "c w X Y escape");
    assert_eq!(text(&ed, cx), "XY bar baz\n");
    keys(&ed, cx, "w .");
    assert_eq!(text(&ed, cx), "XY XY baz\n");
    keys(&ed, cx, "u");
    assert_eq!(text(&ed, cx), "XY bar baz\n");
    keys(&ed, cx, "u");
    assert_eq!(text(&ed, cx), "foo bar baz\n");
    // dd / p, and dot of p.
    let ed = setup(cx, "a\nb\nc\n");
    keys(&ed, cx, "d d p");
    assert_eq!(text(&ed, cx), "b\na\nc\n");
    // Visual operator, gUiw, >j, dfx, dt with count.
    let ed = setup(cx, "hello world\n");
    keys(&ed, cx, "g shift-u i w");
    assert_eq!(text(&ed, cx), "HELLO world\n");
    let ed = setup(cx, "a.b.c.d\n");
    keys(&ed, cx, "d 2 f .");
    assert_eq!(text(&ed, cx), "c.d\n");
    // r is repeatable.
    let ed = setup(cx, "abcd\n");
    keys(&ed, cx, "r x l .");
    assert_eq!(text(&ed, cx), "xxcd\n");
    // A failed motion cancels: dh at column 0.
    let ed = setup(cx, "abc\n");
    keys(&ed, cx, "d h");
    assert_eq!(text(&ed, cx), "abc\n");
}

#[gpui::test]
fn registers_and_macros(cx: &mut TestAppContext) {
    let ed = setup(cx, "one\ntwo\n");
    keys(&ed, cx, "\" a y y j \" a p");
    assert_eq!(text(&ed, cx), "one\ntwo\none\n");
    // Black hole: the deleted line doesn't replace the unnamed register.
    let ed = setup(cx, "keep\ndrop\n");
    keys(&ed, cx, "y y j \" _ d d");
    assert_eq!(text(&ed, cx), "keep\n");
    keys(&ed, cx, "p");
    assert_eq!(text(&ed, cx), "keep\nkeep\n");
    // Macros in registers: qa A ; Esc j q, then @a and @@.
    let ed = setup(cx, "a\nb\nc\nd\n");
    keys(&ed, cx, "q a shift-a ; escape j q");
    assert_eq!(text(&ed, cx), "a;\nb\nc\nd\n");
    keys(&ed, cx, "@ a");
    assert_eq!(text(&ed, cx), "a;\nb;\nc\nd\n");
    keys(&ed, cx, "@ @");
    assert_eq!(text(&ed, cx), "a;\nb;\nc;\nd\n");
}

#[gpui::test]
fn marks_jumps_and_windows(cx: &mut TestAppContext) {
    let ed = setup(cx, "l1\nl2\nl3\nl4\n");
    keys(&ed, cx, "j m a shift-g");
    assert_eq!(point(&ed, cx), 9);
    keys(&ed, cx, "' a");
    assert_eq!(point(&ed, cx), 3);
    keys(&ed, cx, "' '");
    assert_eq!(point(&ed, cx), 9);
    // Marks follow edits: delete the first line, 'a still finds l2.
    keys(&ed, cx, "g g d d ' a");
    assert_eq!(point(&ed, cx), 0);
    // d'a — linewise to the mark.
    let ed = setup(cx, "l1\nl2\nl3\nl4\n");
    keys(&ed, cx, "j j m a g g d ' a");
    assert_eq!(text(&ed, cx), "l4\n");
    // The jump list: G then C-o comes back, C-i goes again.
    let ed = setup(cx, "l1\nl2\nl3\nl4\n");
    keys(&ed, cx, "shift-g");
    assert_eq!(point(&ed, cx), 9);
    keys(&ed, cx, "ctrl-o");
    assert_eq!(point(&ed, cx), 0);
    keys(&ed, cx, "ctrl-i");
    assert_eq!(point(&ed, cx), 9);
    // Windows: C-w v splits, C-w l / C-w h move between them.
    let first = ed.read_with(cx, |e, _| e.selected_pane);
    keys(&ed, cx, "ctrl-w v");
    let leaves = ed.read_with(cx, |e, _| e.layout.leaves());
    assert_eq!(leaves.len(), 2);
    keys(&ed, cx, "ctrl-w l");
    let right = ed.read_with(cx, |e, _| e.selected_pane);
    assert_ne!(right, first);
    keys(&ed, cx, "ctrl-w h");
    assert_eq!(ed.read_with(cx, |e, _| e.selected_pane), first);
    keys(&ed, cx, "ctrl-w o");
    let leaves = ed.read_with(cx, |e, _| e.layout.leaves());
    assert_eq!(leaves.len(), 1);
}

#[gpui::test]
fn ex_commands(cx: &mut TestAppContext) {
    let ed = setup(cx, "a x\nb x x\nc\nd x\n");
    ex(&ed, cx, "%s/x/y/g");
    assert_eq!(text(&ed, cx), "a y\nb y y\nc\nd y\n");
    ex(&ed, cx, "2,3d");
    assert_eq!(text(&ed, cx), "a y\nd y\n");
    ex(&ed, cx, "$m0");
    assert_eq!(text(&ed, cx), "d y\na y\n");
    ex(&ed, cx, "1t$");
    assert_eq!(text(&ed, cx), "d y\na y\nd y\n");
    ex(&ed, cx, "g/^d/d");
    assert_eq!(text(&ed, cx), "a y\n");
    let ed = setup(cx, "b\na\nc\n");
    ex(&ed, cx, "sort");
    assert_eq!(text(&ed, cx), "a\nb\nc\n");
    ex(&ed, cx, "%norm A!");
    assert_eq!(text(&ed, cx), "a!\nb!\nc!\n");
    ex(&ed, cx, "1,2j");
    assert_eq!(text(&ed, cx), "a! b!\nc!\n");
    ex(&ed, cx, "g/c/s/!/?/");
    assert_eq!(text(&ed, cx), "a! b!\nc?\n");
    // & repeats the last :s on the current line.
    let ed = setup(cx, "foo foo\nfoo\n");
    ex(&ed, cx, "s/foo/bar/");
    keys(&ed, cx, "j &");
    assert_eq!(text(&ed, cx), "bar foo\nbar\n");
    // :s///c — y, n, then q.
    let ed = setup(cx, "x x x\n");
    ex(&ed, cx, "s/x/y/gc");
    keys(&ed, cx, "y n y");
    assert_eq!(text(&ed, cx), "y x y\n");
}

/// Type into the open minibuffer and submit it (RET is a gpui action).
fn minibuffer(editor: &Entity<Editor>, cx: &mut TestAppContext, input: &str) {
    editor.update(cx, |e, cx| {
        e.type_text(None, input, cx);
        e.mb_submit(cx);
    });
    settle(editor, cx);
}

fn setup_file(cx: &mut TestAppContext, name: &str, text: &str) -> Entity<Editor> {
    let ed = setup(cx, "");
    ed.update(cx, |e, _cx| {
        let current = e.snapshot_active();
        e.inactive.push(current);
        let path = std::path::PathBuf::from(format!("/nonexistent-macros-test/{name}"));
        e.install_active(make_document(name.into(), Some(path), text));
        e.parse_full();
        e.evil = EvilState::Normal;
    });
    settle(&ed, cx);
    ed
}

#[gpui::test]
fn more_changes_and_repeats(cx: &mut TestAppContext) {
    // o / A / J / >> / ~ with dot.
    let ed = setup(cx, "a\nb\n");
    keys(&ed, cx, "o x escape j .");
    assert_eq!(text(&ed, cx), "a\nx\nb\nx\n");
    let ed = setup(cx, "a\nb\n");
    keys(&ed, cx, "shift-a ; escape j .");
    assert_eq!(text(&ed, cx), "a;\nb;\n");
    let ed = setup(cx, "a\nb\nc\nd\n");
    keys(&ed, cx, "shift-j j .");
    assert_eq!(text(&ed, cx), "a b\nc d\n");
    let ed = setup(cx, "a\nb\n");
    keys(&ed, cx, "> > j .");
    let t = text(&ed, cx);
    assert!(t.starts_with(' ') && t.contains("\n "), "{t:?}");
    let ed = setup(cx, "abc\n");
    keys(&ed, cx, "~ .");
    assert_eq!(text(&ed, cx), "ABc\n");
    // ci" + text, then . inside another string.
    let ed = setup(cx, "x \"aa\" \"bb\"\n");
    keys(&ed, cx, "f a c i \" Z escape f b .");
    assert_eq!(text(&ed, cx), "x \"Z\" \"Z\"\n");
    // cc keeps one (empty) line and inserts there.
    let ed = setup(cx, "one\ntwo\n");
    keys(&ed, cx, "c c N E W escape");
    assert_eq!(text(&ed, cx), "NEW\ntwo\n");
    // Visual delete, then . deletes the same-shaped selection again.
    let ed = setup(cx, "abcdef\n");
    keys(&ed, cx, "v l d");
    assert_eq!(text(&ed, cx), "cdef\n");
    keys(&ed, cx, ".");
    assert_eq!(text(&ed, cx), "ef\n");
    // C-a increments with a count; . repeats it.
    let ed = setup(cx, "n = 7;\n");
    keys(&ed, cx, "5 ctrl-a");
    assert_eq!(text(&ed, cx), "n = 12;\n");
    keys(&ed, cx, ".");
    assert_eq!(text(&ed, cx), "n = 17;\n");
    // gc{motion} comments via the language leader, gcc toggles back.
    let ed = setup_file(cx, "t.py", "x = 1\ny = 2\n");
    keys(&ed, cx, "g c j");
    assert_eq!(text(&ed, cx), "# x = 1\n# y = 2\n");
    keys(&ed, cx, "g c c");
    assert_eq!(text(&ed, cx), "x = 1\n# y = 2\n");
}

#[gpui::test]
fn registers_more(cx: &mut TestAppContext) {
    // "A appends; "0 survives a delete; C-r in Insert.
    let ed = setup(cx, "one\ntwo\nthree\n");
    keys(&ed, cx, "\" a y y j \" A y y");
    keys(&ed, cx, "shift-g \" a p");
    assert_eq!(text(&ed, cx), "one\ntwo\nthree\none\ntwo\n");
    let ed = setup(cx, "keep\ngone\n");
    keys(&ed, cx, "y w j d d \" 0 shift-p");
    assert_eq!(text(&ed, cx), "keepkeep\n");
    let ed = setup(cx, "word\n");
    keys(&ed, cx, "\" b y i w shift-a SPC ctrl-r b escape");
    assert_eq!(text(&ed, cx), "word word\n");
    // 3@a.
    let ed = setup(cx, "1\n2\n3\n4\n5\n");
    keys(&ed, cx, "q a shift-a . escape j q 3 @ a");
    assert_eq!(text(&ed, cx), "1.\n2.\n3.\n4.\n5\n");
}

#[gpui::test]
fn search_jumps_visual_ex_and_tree_sitter(cx: &mut TestAppContext) {
    // / search then C-o back.
    let ed = setup(cx, "aa\nbb\ncc\n");
    keys(&ed, cx, "/");
    minibuffer(&ed, cx, "cc");
    assert_eq!(point(&ed, cx), 6);
    keys(&ed, cx, "ctrl-o");
    assert_eq!(point(&ed, cx), 0);
    // d/pat — the search motion of an operator.
    let ed = setup(cx, "one two three\n");
    keys(&ed, cx, "d /");
    minibuffer(&ed, cx, "three");
    assert_eq!(text(&ed, cx), "three\n");
    // : from Visual Line runs on the selection.
    let ed = setup(cx, "x\nx\nx\n");
    keys(&ed, cx, "j shift-v j :");
    minibuffer(&ed, cx, "s/x/y/");
    assert_eq!(text(&ed, cx), "x\ny\ny\n");
    // :g/…/normal.
    let ed = setup(cx, "a\nb\na\n");
    ex(&ed, cx, "g/a/normal Az");
    assert_eq!(text(&ed, cx), "az\nb\naz\n");
    // Tree-sitter: ]m motion, daf on a Rust function, cia.
    let src = "fn a(x: i32, y: i32) {\n    x;\n}\n\nfn b() {}\n";
    let ed = setup_file(cx, "t.rs", src);
    keys(&ed, cx, "] m");
    assert_eq!(point(&ed, cx), src.find("fn b").unwrap());
    let ed = setup_file(cx, "t.rs", src);
    keys(&ed, cx, "j d a f");
    assert_eq!(text(&ed, cx), "\nfn b() {}\n");
    let ed = setup_file(cx, "t.rs", src);
    keys(&ed, cx, "f y c i a z escape");
    assert!(
        text(&ed, cx).starts_with("fn a(x: i32, z) {"),
        "{:?}",
        text(&ed, cx)
    );
    // Windows: C-w s then C-w j / k / p.
    let ed = setup(cx, "x\n");
    let top = ed.read_with(cx, |e, _| e.selected_pane);
    keys(&ed, cx, "ctrl-w s ctrl-w j");
    let bottom = ed.read_with(cx, |e, _| e.selected_pane);
    assert_ne!(top, bottom);
    keys(&ed, cx, "ctrl-w k");
    assert_eq!(ed.read_with(cx, |e, _| e.selected_pane), top);
    keys(&ed, cx, "ctrl-w p");
    assert_eq!(ed.read_with(cx, |e, _| e.selected_pane), bottom);
}

#[gpui::test]
fn classic_commands_still_work(cx: &mut TestAppContext) {
    let cases: &[(&str, usize, &str, &str)] = &[
        // (text, point, keys, expected)
        ("foo bar baz\n", 5, "c i w X escape w .", "foo X X\n"),
        ("a b c\n", 0, "d a w .", "c\n"),
        ("p1\np1\n\np2\n", 0, "d a p", "p2\n"),
        ("x\ny\n", 0, "y i p shift-g p", "x\ny\nx\ny\n"),
        ("abc\ndef\n", 1, "d v j", "aef\n"),
        ("Hello\n", 0, "g shift-u shift-u", "HELLO\n"),
        ("Hello\n", 0, "g u u", "hello\n"),
        ("Hello\n", 0, "g ~ ~", "hELLO\n"),
        ("a\nb\nc\nd\n", 0, "2 c c Z escape", "Z\nc\nd\n"),
        ("abc def\n", 4, "shift-d", "abc \n"),
        ("abc def\n", 4, "shift-c Z escape", "abc Z\n"),
        ("abc\n", 0, "s Z escape", "Zbc\n"),
        ("abc\n", 2, "x x", "a\n"),
        ("one two\n", 0, "y w shift-p", "one one two\n"),
        ("a\nb\nc\nd\n", 0, "3 shift-j", "a b c\nd\n"),
        ("x1\nx2\nx3\n", 0, "d 2 j", ""),
        ("x1\nx2\nx3\n", 3, "d k", "x3\n"),
        ("  a\nb\n", 3, "= j", "  a\n  b\n"),
        ("a b\n", 0, "> > .", "        a b\n"),
        ("one two three\n", 0, "d t h", "hree\n"),
        ("one two three\n", 0, "d e .", " three\n"),
        ("one two three\n", 8, "d b", "one three\n"),
        ("one two three\n", 12, "d 0", "e\n"),
        ("    abc\n", 6, "d ^", "    c\n"),
        ("(a (b) c)\n", 4, "d i (", "(a () c)\n"),
        ("(a (b) c)\n", 4, "c a ( Z escape", "(a Z c)\n"),
        ("f(x, y)\n", 0, "d %", "\n"),
    ];
    for (text_in, pt, seq, want) in cases {
        let ed = setup(cx, text_in);
        ed.update(cx, |e, _| e.collapse_to(*pt));
        keys(&ed, cx, seq);
        assert_eq!(&text(&ed, cx), want, "{seq:?} on {text_in:?}");
    }
}

#[gpui::test]
fn marks_search_and_ex_extras(cx: &mut TestAppContext) {
    // Exact mark jump and a marked range in ex.
    let ed = setup(cx, "l1\nl2 xy\nl3\nl4\nl5\n");
    keys(&ed, cx, "j l l l m a j j m b g g ` a");
    assert_eq!(point(&ed, cx), 6);
    ex(&ed, cx, "'a,'bd");
    assert_eq!(text(&ed, cx), "l1\nl5\n");
    // dn after a search; @: repeats the last ex command.
    let ed = setup(cx, "a x b x c x\n");
    keys(&ed, cx, "/");
    minibuffer(&ed, cx, "x");
    keys(&ed, cx, "0 d n");
    assert_eq!(text(&ed, cx), "x b x c x\n");
    let ed = setup(cx, "a\na\na\n");
    ex(&ed, cx, "s/a/b/");
    keys(&ed, cx, "j @ :");
    assert_eq!(text(&ed, cx), "b\nb\na\n");
    // :s with groups and &.
    let ed = setup(cx, "key=val\n");
    ex(&ed, cx, r"s/(\w+)=(\w+)/\2=\1 [&]/");
    assert_eq!(text(&ed, cx), "val=key [key=val]\n");
    // :g!/pat/d and :v.
    let ed = setup(cx, "keep 1\ndrop\nkeep 2\n");
    ex(&ed, cx, "v/keep/d");
    assert_eq!(text(&ed, cx), "keep 1\nkeep 2\n");
    // Undo of a :g is one step.
    keys(&ed, cx, "u");
    assert_eq!(text(&ed, cx), "keep 1\ndrop\nkeep 2\n");
    // :{n} goes to a line; :$ the last; jump list records it.
    ex(&ed, cx, "2");
    assert_eq!(point(&ed, cx), 7);
    // Visual block delete still works.
    let ed = setup(cx, "abc\ndef\n");
    keys(&ed, cx, "ctrl-v j l d");
    assert_eq!(text(&ed, cx), "c\nf\n");
}

/// The MCP server, end to end: options start it, and over real HTTP an
/// `eval` edits the buffer and reports its value and messages, which
/// `buffer_text` then sees. Turning the option off stops it.
#[gpui::test]
fn mcp_server_eval_and_buffer_text(cx: &mut TestAppContext) {
    use std::io::{Read as _, Write as _};

    let editor = setup(cx, "");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    editor.update(cx, |e, cx| {
        e.run_script_blocking(
            &format!(
                r#"(set-option "mcp-server-token" "test-token")
                   (set-option "mcp-server-port" {port})
                   (set-option "mcp-server" #t)"#
            ),
            cx,
        )
    });
    settle(&editor, cx);
    assert!(
        editor.read_with(cx, |e, _| e.mcp.is_some()),
        "server should be running"
    );

    // POST one tools/call from a worker thread, pumping the editor's executor
    // until it answers (the server blocks on foreground replies).
    let call = |cx: &mut TestAppContext,
                tool: &str,
                args: serde_json::Value|
     -> serde_json::Value {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                      "params": {"name": tool, "arguments": args}})
        .to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                s,
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer test-token\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            tx.send(out).unwrap();
        });
        for _ in 0..1000 {
            cx.run_until_parked();
            if let Ok(out) = rx.try_recv() {
                let json = out.split("\r\n\r\n").nth(1).unwrap();
                return serde_json::from_str(json).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no MCP response");
    };
    let reply = |r: &serde_json::Value| {
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let r = call(
        cx,
        "eval",
        serde_json::json!({"code": r#"(insert "hi from mcp") (message "inserted") (+ 40 2)"#}),
    );
    let out = reply(&r);
    assert!(out.starts_with("=> 42"), "{out}");
    assert!(out.contains("inserted"), "{out}");
    assert_eq!(text(&editor, cx), "hi from mcp");

    let r = call(cx, "buffer_text", serde_json::json!({}));
    let out = reply(&r);
    assert!(
        out.contains("buffer: test.txt") && out.ends_with("---\nhi from mcp"),
        "{out}"
    );

    let r = call(cx, "eval", serde_json::json!({"code": "(car '())"}));
    assert_eq!(r["result"]["isError"], true, "{r}");

    let r = call(
        cx,
        "describe",
        serde_json::json!({"name": "plot-sparkline"}),
    );
    assert!(reply(&r).starts_with("plot-sparkline"), "{r}");

    editor.update(cx, |e, cx| {
        e.run_script_blocking(r#"(set-option "mcp-server" #f)"#, cx)
    });
    settle(&editor, cx);
    assert!(
        editor.read_with(cx, |e, _| e.mcp.is_none()),
        "server should stop"
    );
}

/// Refreshing a special buffer in place keeps the view where the user left
/// it, even when they wheel-scrolled away from point (dashboards that
/// auto-refresh used to snap back to the cursor on every redraw).
#[gpui::test]
fn render_buffer_refresh_keeps_scroll_position(cx: &mut TestAppContext) {
    let ed = setup(cx, "");
    let body: String = (0..300).map(|i| format!("row {i}\\n")).collect();
    let render = format!("(render-buffer \"*rows*\" \"rows-mode\" \"{body}\" '())");
    ed.update(cx, |e, cx| e.run_script_blocking(&render, cx));
    settle(&ed, cx);
    let wheel = ed.update(cx, |e, _| {
        // The headless platform never lays out, so give the view real geometry.
        e.viewport_height = px(400.);
        e.line_height = px(20.);
        // Wheel-scroll far down without moving point (it stays on line 0).
        let to = px(20. * 120.);
        e.active.scroll_offset = to;
        e.cursor_follow_pending = false;
        to
    });
    ed.update(cx, |e, cx| e.run_script_blocking(&render, cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.name, "*rows*");
        assert_eq!(e.active.scroll_offset, wheel, "refresh scrolled the view");
        assert_eq!(
            e.active.buffer.line_index_at_byte(e.point()),
            0,
            "point kept its line"
        );
    });
}

/// `]` (last changed char) lands on the START of a multibyte last char, never
/// inside it.
#[gpui::test]
fn change_end_mark_is_a_char_boundary(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    ed.update(cx, |e, _| {
        e.edit(0..0, "aé");
    });
    ed.read_with(cx, |e, _| {
        let m = e.vim.marks.get("test.txt").expect("marks");
        assert_eq!(m.get(&']'), Some(&1), "`]` on é's first byte");
        assert_eq!(m.get(&'['), Some(&0));
    });
}

/// An electric-pair opener refused by read-only text leaves point alone
/// (it used to step back by the closer's length — underflowing at 0).
#[gpui::test]
fn electric_pair_on_read_only_text_keeps_point(cx: &mut TestAppContext) {
    // Next char is a space, so `(` would pair (`()`) were the text writable.
    let ed = setup(cx, " ab\n");
    ed.update(cx, |e, cx| {
        e.electric_pair = true;
        e.active
            .overlays
            .props
            .push((0..3, "read-only".into(), "t".into()));
        e.collapse_to(0);
        assert!(e.electric_pair_insert(&(0..0), "(", cx));
        assert_eq!(e.point(), 0);
        assert_eq!(e.active.buffer.full_text(), " ab\n");
    });
}

/// Text inserted at the start of a hidden range (a markdown heading's `## `)
/// lands before the hide instead of being swallowed by it, and newlines inserted
/// at column 0 carry the line's scale down with the heading. Markdown only
/// repaints when the heading-level sequence changes, so a hide stretched over
/// typed text used to blank the whole line whenever point left it.
#[gpui::test]
fn insert_at_hidden_range_start_stays_visible(cx: &mut TestAppContext) {
    let ed = setup(cx, "a\n## H\nb\n");
    ed.update(cx, |e, _| {
        e.active.overlays.hidden.push(2..5);
        e.active.overlays.scales.insert(1, 1.4);
        // Enter at column 0 of the heading.
        e.edit(2..2, "\n");
        assert_eq!(e.active.overlays.hidden, vec![3..6]);
        assert_eq!(e.active.overlays.scales.get(&2), Some(&1.4));
        assert_eq!(e.active.overlays.scales.get(&1), None);
        // Typing on the opened line, right before the heading's newline… and
        // directly at the hide's start.
        e.edit(2..2, "xy");
        e.edit(5..5, "z");
        assert_eq!(e.active.buffer.full_text(), "a\nxy\nz## H\nb\n");
        assert_eq!(e.active.overlays.hidden, vec![6..9]);
        // Typing after the hide (the title) never grows it.
        e.edit(9..9, "Q");
        assert_eq!(e.active.overlays.hidden, vec![6..9]);
    });
}

/// Nested undo groups merge into the outer one; an edit in another buffer
/// mid-group starts its own node there.
#[gpui::test]
fn undo_groups_nest_and_respect_buffer_switches(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    ed.update(cx, |e, _| {
        let before = e.active.undo.len();
        e.begin_undo_group();
        e.edit(0..0, "a");
        e.begin_undo_group(); // e.g. a `C-u 3` repeat inside a Scheme batch
        e.edit(1..1, "b");
        e.end_undo_group();
        e.edit(2..2, "c"); // still inside the outer group
        e.end_undo_group();
        assert_eq!(e.active.undo.len(), before + 1, "one undo node");
        assert_eq!(e.active.undo.nodes[e.active.undo.current].edit.len(), 3);

        // Group open in buffer A, then the batch switches to B and edits.
        e.begin_undo_group();
        e.edit(0..0, "x");
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("other.txt".into(), None, "zz"));
        e.edit(0..0, "y");
        e.end_undo_group();
        // B's edit is its own node off B's root — not appended to the root.
        assert_eq!(e.active.undo.len(), 2);
        assert_eq!(e.active.undo.current, 1);
        assert!(e.active.undo.nodes[0].edit.is_empty());
    });
}

/// Backward isearch repeat from a match at byte 0 wraps to the last match.
#[gpui::test]
fn isearch_backward_repeat_wraps_from_buffer_start(cx: &mut TestAppContext) {
    let ed = setup(cx, "foo bar foo\n");
    ed.update(cx, |e, cx| {
        e.collapse_to(0);
        e.isearch_start(Direction::Backward, cx);
        e.minibuffer.as_mut().unwrap().input = "foo".into();
        e.isearch_update(cx);
        assert_eq!(e.point(), 0);
        e.isearch_repeat(Direction::Backward, cx);
        assert_eq!(e.point(), 8, "wrapped to the last match");
    });
}

/// Trimming an inactive log buffer's head shifts point back with its text.
#[gpui::test]
fn log_buffer_trim_shifts_selection(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    ed.update(cx, |e, _| {
        let body: String = (0..LOG_BUFFER_LINES_CAP)
            .map(|i| format!("l{i:05}\n"))
            .collect();
        let mut doc = make_log_document("*log*".into(), &body);
        // Point on the start of line 100 (`l00100`).
        let at = doc.buffer.line_range(100).start;
        doc.selected_range = at..at;
        e.inactive.push(doc);
        e.append_to_log_buffer("*log*", "new");
        let doc = e.inactive.iter().find(|d| d.name == "*log*").unwrap();
        let p = doc.selected_range.start;
        let line = doc.buffer.line_range_at(p);
        assert_eq!(doc.buffer.slice_to_string(line), "l00100");
    });
}

/// A chained counted run whose script errors ends the chain.
#[gpui::test]
fn counted_chain_error_clears_count_repeat(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    ed.update(cx, |e, cx| {
        e.count_repeat = Some(CountRepeat {
            source: "(begin (boom) (count-repeat-step))".into(),
            total: 3,
            done: 0,
            undo_base: e.undo_base(),
        });
        e.apply_script_outcome(
            crate::script::ScriptOutcome::Commands {
                commands: Vec::new(),
                message: Some("error: boom".into()),
            },
            cx,
        );
        assert!(e.count_repeat.is_none());
    });
}

/// Async completion replies computed against since-changed text are dropped;
/// fresh ones are snapped to char boundaries.
#[gpui::test]
fn stale_completion_replies_are_dropped(cx: &mut TestAppContext) {
    let ed = setup(cx, "é\n");
    ed.update(cx, |e, cx| {
        e.evil = EvilState::Insert; // completion runs while typing
        e.collapse_to(2);
        e.stamp_completion_request();
        e.edit(0..0, "zz"); // typed after the request went out
        e.apply_capf_result(0, 2, vec![("éa".into(), None)], cx);
        assert!(e.completion_popup.is_none(), "stale reply dropped");

        e.collapse_to(4);
        e.stamp_completion_request();
        // A mid-codepoint start (byte 3 is inside `é`) is snapped, not sliced.
        e.apply_capf_result(3, 4, vec![("éa".into(), None)], cx);
        let anchor = e.completion_popup.as_ref().map(|p| p.anchor);
        assert_eq!(anchor, Some(2));
        let _ = e.completion_view();
    });
}

/// Session restore names two same-named files uniquely and treats a symlink
/// to an open file as already open.
#[cfg(unix)]
#[gpui::test]
fn restore_file_buffers_uniquifies_and_dedups(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("macros-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::write(dir.join("a/mod.rs"), "a\n").unwrap();
    std::fs::write(dir.join("b/mod.rs"), "b\n").unwrap();
    std::os::unix::fs::symlink(dir.join("a/mod.rs"), dir.join("link.rs")).unwrap();
    let ed = setup(cx, "\n");
    ed.update(cx, |e, cx| {
        let s = |p: &str| dir.join(p).to_string_lossy().into_owned();
        let entries = vec![(s("a/mod.rs"), 0), (s("b/mod.rs"), 0), (s("link.rs"), 1)];
        e.restore_file_buffers(entries, cx);
        let mods: Vec<&str> = e
            .inactive
            .iter()
            .filter(|d| d.name.starts_with("mod.rs") || d.name == "link.rs")
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(mods.len(), 2, "symlink reused the open buffer: {mods:?}");
        assert_ne!(mods[0], mods[1], "unique names: {mods:?}");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// A workspace edit for a file open through a symlink edits the buffer.
#[cfg(unix)]
#[gpui::test]
fn workspace_edit_matches_buffers_by_file_identity(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("macros-wsedit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real.txt");
    std::fs::write(&real, "old\n").unwrap();
    let link = dir.join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let ed = setup(cx, "\n");
    ed.update(cx, |e, cx| {
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document(
            "link.txt".into(),
            Some(link.clone()),
            "old\n",
        ));
        let edit = lsp::WorkspaceEdit {
            changes: vec![(
                lsp::path_to_uri(&real),
                vec![lsp::TextEdit {
                    start: lsp::Position::new(0, 0),
                    end: lsp::Position::new(0, 3),
                    new_text: "new".into(),
                }],
            )],
        };
        let failed = e.apply_workspace_edit(edit, cx);
        assert!(failed.is_empty());
        assert_eq!(e.active.buffer.full_text(), "new\n");
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui::test]
fn ex_move_keeps_anchors_and_marks(cx: &mut TestAppContext) {
    // :g/x/m$ moves every match, not just the first (anchors survive :m).
    let ed = setup(cx, "x1\na\nx2\nb\n");
    ex(&ed, cx, "g/x/m$");
    assert_eq!(text(&ed, cx), "a\nb\nx1\nx2\n");
    ex(&ed, cx, "g/x/m0");
    assert_eq!(text(&ed, cx), "x2\nx1\na\nb\n");
    // A mark on a line in between stays on it; one on the moved line moves.
    let ed = setup(cx, "one\ntwo\nthree\n");
    keys(&ed, cx, "m a j m b");
    ex(&ed, cx, "1m$");
    assert_eq!(text(&ed, cx), "two\nthree\none\n");
    keys(&ed, cx, "' b");
    assert_eq!(point(&ed, cx), 0);
    keys(&ed, cx, "' a");
    assert_eq!(point(&ed, cx), 10);
    // Moving up over the buffer's last (unterminated) line.
    let ed = setup(cx, "a\nb");
    ex(&ed, cx, "2m0");
    assert_eq!(text(&ed, cx), "b\na");
    ex(&ed, cx, "1m$");
    assert_eq!(text(&ed, cx), "a\nb");
}

#[gpui::test]
fn dot_counts_and_registers(cx: &mut TestAppContext) {
    // "add then 3. deletes three lines (the count skips the register prefix).
    let ed = setup(cx, "1\n2\n3\n4\n5\n6\n");
    keys(&ed, cx, "\" a d d");
    assert_eq!(text(&ed, cx), "2\n3\n4\n5\n6\n");
    keys(&ed, cx, "3 .");
    assert_eq!(text(&ed, cx), "5\n6\n");
    // 3rx then . replaces three more.
    let ed = setup(cx, "abcdefgh\n");
    keys(&ed, cx, "3 r x");
    assert_eq!(text(&ed, cx), "xxxdefgh\n");
    keys(&ed, cx, "l .");
    assert_eq!(text(&ed, cx), "xxxxxxgh\n");
}

#[gpui::test]
fn ex_put_zero_and_crlf(cx: &mut TestAppContext) {
    // :0put goes above the first line.
    let ed = setup(cx, "a\nb\n");
    keys(&ed, cx, "j y y");
    ex(&ed, cx, "0put");
    assert_eq!(text(&ed, cx), "b\na\nb\n");
    // CRLF: :d removes the whole break, :t / :put / :m use CRLF.
    let ed = setup(cx, "a\r\nb\r\n");
    ex(&ed, cx, "1d");
    assert_eq!(text(&ed, cx), "b\r\n");
    let ed = setup(cx, "a\r\nb\r\n");
    ex(&ed, cx, "1t$");
    assert_eq!(text(&ed, cx), "a\r\nb\r\na\r\n");
    ex(&ed, cx, "1m$");
    assert_eq!(text(&ed, cx), "b\r\na\r\na\r\n");
    keys(&ed, cx, "g g y y");
    ex(&ed, cx, "$put");
    assert_eq!(text(&ed, cx), "b\r\na\r\na\r\nb\r\n");
    // The last line (no trailing break) takes the CRLF before it.
    let ed = setup(cx, "a\r\nb");
    ex(&ed, cx, "2d");
    assert_eq!(text(&ed, cx), "a");
}

#[gpui::test]
fn block_ops_respect_registers(cx: &mut TestAppContext) {
    // "_ block delete leaves the unnamed register alone.
    let ed = setup(cx, "keep\nabc\nabc\n");
    keys(&ed, cx, "y w j ctrl-v j l \" _ d");
    assert_eq!(text(&ed, cx), "keep\nc\nc\n");
    keys(&ed, cx, "g g shift-p");
    assert_eq!(text(&ed, cx), "keepkeep\nc\nc\n");
    // "a block change fills register a.
    let ed = setup(cx, "abc\nabc\n");
    keys(&ed, cx, "ctrl-v j \" a c Z escape");
    assert_eq!(text(&ed, cx), "Zbc\nZbc\n");
    let a = ed.update(cx, |e, cx| e.evil_register_value('a', cx).map(|v| v.text));
    assert_eq!(a.as_deref(), Some("a\na"));
    // "_ block yank doesn't touch the unnamed register either.
    let ed = setup(cx, "keep\nxy\n");
    keys(&ed, cx, "y w j ctrl-v l \" _ y");
    keys(&ed, cx, "g g shift-p");
    assert_eq!(text(&ed, cx), "keepkeep\nxy\n");
}

#[gpui::test]
fn sub_pattern_case_atoms_and_n(cx: &mut TestAppContext) {
    let ed = setup(cx, "Foo\nbar\nfoo\nFOO\n");
    ex(&ed, cx, "s/foo\\c/X/");
    assert_eq!(text(&ed, cx), "X\nbar\nfoo\nFOO\n");
    keys(&ed, cx, "n");
    assert_eq!(point(&ed, cx), 6);
    keys(&ed, cx, "n");
    assert_eq!(point(&ed, cx), 10);
    let ed = setup(cx, "a\nfoo\nFoo\n");
    ex(&ed, cx, "g/\\cfoo/y");
    keys(&ed, cx, "g g n");
    assert_eq!(point(&ed, cx), 2);
    keys(&ed, cx, "n");
    assert_eq!(point(&ed, cx), 6);
}

#[gpui::test]
fn mc_mark_next_keeps_the_original_cursor(cx: &mut TestAppContext) {
    let ed = setup(cx, "foo bar foo baz foo\n");
    ed.update(cx, |e, cx| e.mc_mark_next(cx));
    let cursors = ed.read_with(cx, |e, _| e.all_cursor_positions());
    assert_eq!(cursors, vec![0, 8]);
    ed.update(cx, |e, cx| e.mc_mark_next(cx));
    let cursors = ed.read_with(cx, |e, _| e.all_cursor_positions());
    assert_eq!(cursors, vec![0, 8, 16]);
    ed.update(cx, |e, cx| e.mc_mark_next(cx));
    let cursors = ed.read_with(cx, |e, _| e.all_cursor_positions());
    assert_eq!(cursors, vec![0, 8, 16]);
}

#[gpui::test]
fn marks_and_jumps_into_gone_buffers(cx: &mut TestAppContext) {
    // A file mark / jump into a fileless buffer that no longer exists is
    // reported gone — it must not land in some other fileless buffer.
    let ed = setup(cx, "l1\nl2\n");
    ed.update(cx, |e, _| {
        e.vim.global_marks.insert(
            'A',
            vim::GlobalMark {
                buffer: "*gone*".into(),
                path: None,
                pos: 3,
            },
        );
    });
    keys(&ed, cx, "' shift-a");
    let (name, pt) = ed.read_with(cx, |e, _| (e.active.name.clone(), e.point()));
    assert_eq!((name.as_str(), pt), ("test.txt", 0));
    ed.update(cx, |e, _| {
        e.vim.jumps = vec![vim::Jump {
            buffer: "*gone*".into(),
            path: None,
            pos: 3,
        }];
        e.vim.jump_idx = 1;
    });
    keys(&ed, cx, "ctrl-o");
    let (name, n) = ed.read_with(cx, |e, _| (e.active.name.clone(), e.vim.jumps.len()));
    assert_eq!(name, "test.txt");
    assert_eq!(n, 1, "the gone entry is dropped, the current spot stays");
}

/// `read-passwd` renders masked, skips history (both recording and M-p
/// browsing), and keeps the input verbatim (spaces are part of a password).
#[gpui::test]
fn read_passwd_masks_and_skips_history(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    ed.update(cx, |e, cx| {
        e.minibuffer_history.push("older entry");
        e.script
            .eval("(define got-pw #f) (define (take-pw pw) (set! got-pw pw))")
            .unwrap();
        e.apply_command(
            EditorCommand::ReadPasswd {
                prompt: "Password: ".into(),
                callback: "take-pw".into(),
                on_cancel: None,
            },
            cx,
        );
        assert!(e.mb_is_secret());
        e.mb_insert(" pä ss ", cx);
        let view = e.minibuffer_view().unwrap();
        assert_eq!(view.before, "•••••••", "one bullet per char, no plaintext");
        // M-p must not pull history into (or out of) a password read.
        e.mb_history(-1, cx);
        assert_eq!(e.minibuffer.as_ref().unwrap().input, " pä ss ");
        e.mb_submit(cx);
        assert!(e.minibuffer.is_none());
        assert_eq!(e.minibuffer_history.entries(), ["older entry".to_string()]);
        // The engine is FIFO: this sync eval runs after the dispatched call.
        assert_eq!(
            e.script.eval("got-pw").unwrap().value.as_deref(),
            Some("\" pä ss \""),
            "callback gets the input untrimmed"
        );
    });
}

/// C-g on a password read, or another read displacing it, runs on-cancel.
#[gpui::test]
fn read_passwd_cancel_and_displace_run_on_cancel(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    ed.update(cx, |e, cx| {
        e.script
            .eval("(define cancels 0) (define (pw-cancel _) (set! cancels (+ cancels 1)))")
            .unwrap();
        let open = |e: &mut Editor, cx: &mut Context<Editor>| {
            e.apply_command(
                EditorCommand::ReadPasswd {
                    prompt: "Password: ".into(),
                    callback: "ignore".into(),
                    on_cancel: Some("pw-cancel".into()),
                },
                cx,
            );
        };
        open(e, cx);
        e.mb_abort(cx);
        assert!(e.minibuffer.is_none());
        open(e, cx);
        e.find_file_prompt(cx); // displaces the password read
        assert!(!e.mb_is_secret());
        assert_eq!(
            e.script.eval("cancels").unwrap().value.as_deref(),
            Some("2")
        );
    });
}

/// cmd-v / C-y paste into a minibuffer read. A secret read takes the
/// clipboard (one line, newline dropped) and leaves the kill ring alone; a
/// normal read's C-y yanks the kill ring's top.
#[gpui::test]
fn minibuffer_paste_and_yank(cx: &mut TestAppContext) {
    let ed = setup(cx, "\n");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("s3 cret\n".into()));
    ed.update(cx, |e, cx| {
        e.apply_command(
            EditorCommand::ReadPasswd {
                prompt: "Password: ".into(),
                callback: "ignore".into(),
                on_cancel: None,
            },
            cx,
        );
    });
    keys(&ed, cx, "cmd-v");
    keys(&ed, cx, "ctrl-y");
    ed.update(cx, |e, _| {
        assert_eq!(e.minibuffer.as_ref().unwrap().input, "s3 crets3 cret");
        assert_eq!(e.minibuffer_view().unwrap().before, "••••••••••••••");
        assert!(
            e.kill_ring.is_empty(),
            "a secret never enters the kill ring"
        );
        assert_eq!(
            e.active.buffer.full_text(),
            "\n",
            "nothing pasted into the buffer behind"
        );
    });
    // A normal read: C-y yanks (clipboard adopted into the ring first).
    ed.update(cx, |e, cx| {
        e.mb_abort(cx);
        e.apply_command(
            EditorCommand::ReadString {
                prompt: "Name: ".into(),
                initial: String::new(),
                callback: "ignore".into(),
            },
            cx,
        );
    });
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("one\ntwo".into()));
    keys(&ed, cx, "ctrl-y");
    ed.update(cx, |e, _| {
        let mb = e.minibuffer.as_ref().unwrap();
        assert_eq!(mb.input, "one");
        assert_eq!(mb.message, "[Pasted the first line only]");
        assert_eq!(e.kill_ring.last().map(String::as_str), Some("one\ntwo"));
    });
}

/// A JSON buffer (Emacs bindings, no evil) with the bundled Scheme loaded, so
/// `tab-width:json` is the shipped 2.
fn setup_json(cx: &mut TestAppContext, name: &str, text: &str) -> Entity<Editor> {
    test_context(cx);
    let editor = cx.new(Editor::new);
    editor.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        e.mark_config_loaded(cx);
        let current = e.snapshot_active();
        e.inactive.push(current);
        let path = std::path::PathBuf::from(format!("/nonexistent/{name}"));
        e.install_active(make_document(name.into(), Some(path), text));
        // Opening a file parses it up front; install_active alone doesn't.
        e.parse_full();
        let end = e.active.buffer.len();
        e.collapse_to(end);
    });
    settle(&editor, cx);
    editor
}

/// Type `seq` the way a user does: `\n` is RET, every other char self-inserts.
fn type_json(editor: &Entity<Editor>, cx: &mut TestAppContext, seq: &str) {
    for ch in seq.chars() {
        editor.update(cx, |e, cx| {
            if ch == '\n' {
                e.apply_command(EditorCommand::Newline, cx);
            } else {
                e.type_text(None, &ch.to_string(), cx);
            }
        });
    }
    settle(editor, cx);
}

/// Typing a document from scratch (electric-pair off, so every bracket is
/// unclosed while its body is typed) indents each body one step past its
/// opener, and a typed `]` / `}` snaps back to its opener's column.
#[gpui::test]
fn json_typed_from_scratch_indents(cx: &mut TestAppContext) {
    let editor = setup_json(cx, "t.json", "");
    type_json(&editor, cx, "{\n\"a\": [\n1,\n2\n]\n}");
    assert_eq!(text(&editor, cx), "{\n  \"a\": [\n    1,\n    2\n  ]\n}");
    // Nested objects behave the same.
    let editor = setup_json(cx, "u.json", "");
    type_json(&editor, cx, "{\n\"o\": {\n\"k\": true\n}\n}");
    assert_eq!(text(&editor, cx), "{\n  \"o\": {\n    \"k\": true\n  }\n}");
}

/// A closer typed mid-line (after content) is left where it is.
#[gpui::test]
fn json_closer_after_content_is_not_reindented(cx: &mut TestAppContext) {
    let editor = setup_json(cx, "t.json", "{\n  \"a\": [1");
    type_json(&editor, cx, "]");
    assert_eq!(text(&editor, cx), "{\n  \"a\": [1]");
}

/// `json-pretty-print-buffer` (as Scheme / format-on-save calls it) lays out
/// minified JSON, keeping key order and number spelling.
#[gpui::test]
fn json_pretty_print_buffer_formats(cx: &mut TestAppContext) {
    let editor = setup_json(cx, "t.json", r#"{"b":1.0,"a":[1e3,{}],"c":{"d":null}}"#);
    editor.update(cx, |e, cx| {
        e.run_script_blocking("(json-pretty-print-buffer)", cx)
    });
    settle(&editor, cx);
    assert_eq!(
        text(&editor, cx),
        "{\n  \"b\": 1.0,\n  \"a\": [\n    1e3,\n    {}\n  ],\n  \"c\": {\n    \"d\": null\n  }\n}\n"
    );
}

/// A JSON buffer that doesn't parse is never touched.
#[gpui::test]
fn json_pretty_print_refuses_syntax_errors(cx: &mut TestAppContext) {
    let src = r#"{"a": 1 "b": 2}"#;
    let editor = setup_json(cx, "t.json", src);
    editor.update(cx, |e, cx| {
        e.apply_command(EditorCommand::JsonPrettyPrintBuffer, cx)
    });
    settle(&editor, cx);
    assert_eq!(text(&editor, cx), src);
    editor.read_with(cx, |e, _| {
        assert!(e.status.contains("syntax errors"), "{}", e.status)
    });
}

/// `json-pretty-print` over a region formats just that value, nested at the
/// region's line indentation.
#[gpui::test]
fn json_pretty_print_region_keeps_base_indent(cx: &mut TestAppContext) {
    let editor = setup_json(cx, "t.json", "{\n  \"x\": {\"y\":[1,2]}\n}\n");
    editor.update(cx, |e, cx| {
        let t = e.active.buffer.full_text();
        let start = t.find("{\"y\"").unwrap();
        let end = t.find("]}").unwrap() + 2;
        e.active.selected_range = start..end;
        e.active.mark_active = true;
        e.apply_command(EditorCommand::JsonPrettyPrintRegion, cx);
    });
    settle(&editor, cx);
    assert_eq!(
        text(&editor, cx),
        "{\n  \"x\": {\n    \"y\": [\n      1,\n      2\n    ]\n  }\n}\n"
    );
}

/// `reindent-region` with no region reindents the whole buffer structurally;
/// in a buffer with no indent rules it refuses rather than flattening.
#[gpui::test]
fn reindent_region_fixes_whole_buffer(cx: &mut TestAppContext) {
    let editor = setup_json(
        cx,
        "t.json",
        "{\n\"a\": [\n        1\n],\n      \"b\": 2\n    }\n",
    );
    editor.update(cx, |e, cx| {
        e.apply_command(EditorCommand::ReindentRegion, cx)
    });
    settle(&editor, cx);
    assert_eq!(
        text(&editor, cx),
        "{\n  \"a\": [\n    1\n  ],\n  \"b\": 2\n}\n"
    );

    let plain = "one\n    two\n";
    let editor = setup_json(cx, "notes.txt", plain);
    editor.update(cx, |e, cx| {
        e.apply_command(EditorCommand::ReindentRegion, cx)
    });
    settle(&editor, cx);
    assert_eq!(text(&editor, cx), plain);
}

/// A new object opened after a member's `},` (the parse is an error until it
/// gets a key): RET after `{` indents the body, and the `}` typed on that
/// empty line lines up with the `{`. Same under evil Insert state.
#[gpui::test]
fn json_new_object_after_member_aligns(cx: &mut TestAppContext) {
    let before = "{\n  \"attribution\": {\n    \"commit\": \"\",\n    \"pr\": \"\"\n  },\n}\n";
    let after =
        "{\n  \"attribution\": {\n    \"commit\": \"\",\n    \"pr\": \"\"\n  },\n  {\n  }\n}\n";
    for evil in [false, true] {
        let editor = setup_json(cx, "settings.json", before);
        editor.update(cx, |e, cx| {
            if evil {
                e.run_script_blocking("(evil-mode)", cx);
                e.evil = EvilState::Insert;
            }
            let at = e.active.buffer.full_text().find("},").unwrap() + 2;
            e.collapse_to(at);
        });
        type_json(&editor, cx, "\n{\n");
        let body = editor.read_with(cx, |e, _| e.active.buffer.full_text());
        assert!(
            body.contains("  {\n    \n}"),
            "evil={evil}: body line {body:?}"
        );
        type_json(&editor, cx, "}");
        assert_eq!(text(&editor, cx), after, "evil={evil}");
    }
}

/// The text fallback isn't fooled by brackets in strings, or by a Rust
/// lifetime's lone `'`.
#[gpui::test]
fn electric_closer_text_match_skips_strings_and_lifetimes(cx: &mut TestAppContext) {
    // Unclosed outer object (parse error), with a `{` inside a string.
    let editor = setup_json(cx, "t.json", "{\n  \"a\": {\n    \"s\": \"{\"\n    ");
    type_json(&editor, cx, "}");
    assert_eq!(text(&editor, cx), "{\n  \"a\": {\n    \"s\": \"{\"\n  }");

    let editor = setup_json(
        cx,
        "t.rs",
        "fn f<'a>(x: &'a str) {\n    let y = (\n        ",
    );
    type_json(&editor, cx, ")");
    assert_eq!(
        text(&editor, cx),
        "fn f<'a>(x: &'a str) {\n    let y = (\n    )"
    );
}

/// A completion popup left up after Esc caught the Enter of `:w<RET>` (the
/// popup is checked before the minibuffer), so the first `:w` did nothing.
#[gpui::test]
fn escape_and_ex_prompt_close_completion_popup(cx: &mut TestAppContext) {
    let popup = || {
        let items = crate::lsp::parse_completion(&serde_json::json!([{"label": "foobar"}]));
        Some(CompletionPopup {
            items,
            anchor: 0,
            selected: 0,
        })
    };
    let ed = setup(cx, "\n");
    keys(&ed, cx, "i f o");
    ed.update(cx, |e, _| e.completion_popup = popup());
    keys(&ed, cx, "escape");
    ed.read_with(cx, |e, _| assert!(e.completion_popup.is_none()));
    // However a popup got there, the `:` prompt closes it.
    ed.update(cx, |e, _| e.completion_popup = popup());
    keys(&ed, cx, ": w");
    ed.read_with(cx, |e, _| {
        assert!(e.minibuffer.is_some());
        assert!(e.completion_popup.is_none());
    });
}

/// An async completion reply arriving after Esc (or after `:` opened) must
/// not open the popup: it would catch the prompt's RET.
#[gpui::test]
fn late_completion_reply_is_dropped_outside_insert(cx: &mut TestAppContext) {
    let items = || crate::lsp::parse_completion(&serde_json::json!([{"label": "foobar"}]));
    let ed = setup(cx, "\n");
    keys(&ed, cx, "i f o escape");
    ed.update(cx, |e, cx| e.show_completion(items(), 0..2, cx));
    ed.read_with(cx, |e, _| assert!(e.completion_popup.is_none()));
    keys(&ed, cx, ": w");
    ed.update(cx, |e, cx| e.show_completion(items(), 0..2, cx));
    ed.read_with(cx, |e, _| {
        assert!(e.completion_popup.is_none());
        assert!(e.minibuffer.is_some());
    });
}

/// OS-special keys (gpui actions) share one precedence order: a modal surface
/// swallows every one of them — arrows included, which used to slip past an
/// open transient and move point underneath it.
#[gpui::test]
fn action_keys_share_one_precedence_order(cx: &mut TestAppContext) {
    use super::input::{KEY_BACKSPACE, KEY_LEFT, KEY_UP};
    let ed = setup(cx, "abcdef");
    ed.update(cx, |e, cx| {
        e.move_to(3, cx);
        e.transient = Some(TransientState {
            title: "menu".into(),
            columns: Vec::new(),
            sticky: false,
        });
        for key in [&KEY_LEFT, &KEY_UP, &KEY_BACKSPACE] {
            assert!(
                e.route_action_key(key, cx),
                "{} swallowed by the transient",
                key.name
            );
        }
        assert_eq!(e.point(), 3, "nothing moved under the transient");
        assert_eq!(e.active.buffer.full_text(), "abcdef");

        e.transient = None;
        assert!(
            !e.route_action_key(&KEY_LEFT, cx),
            "plain buffer: fallback runs"
        );

        // A special buffer consumes editing keys, but motion keys still move.
        e.active.special = Some("test-special".into());
        assert!(e.route_action_key(&KEY_BACKSPACE, cx));
        assert!(!e.route_action_key(&KEY_LEFT, cx));
    });
}

/// When a terminal's shell exits, the bundled `terminal-exit-hook` kills its
/// buffer (and the terminal backing it).
#[gpui::test]
fn terminal_exit_kills_its_buffer(cx: &mut TestAppContext) {
    let ed = setup(cx, "");
    ed.update(cx, |e, cx| {
        e.run_script_blocking(
            r#"(make-terminal "*terminal*" "/bin/sh" (list "-c" "sleep 30") "/tmp")"#,
            cx,
        );
    });
    settle(&ed, cx);
    let id = ed.read_with(cx, |e, _| {
        assert!(e.has_buffer("*terminal*"));
        e.terminals
            .values()
            .find(|t| t.buffer_name == "*terminal*")
            .map(|t| t.id)
            .expect("terminal spawned")
    });

    ed.update(cx, |e, cx| e.on_terminal_event(id, TermEvent::Exit, cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        assert!(
            !e.has_buffer("*terminal*"),
            "buffers: {:?}",
            e.buffer_names()
        );
        assert!(e.terminals.is_empty());
    });
}

/// Lisp reindent leaves multi-line string content alone — including a docstring
/// line that starts with `(`, which must not be taken for a top-level form.
#[gpui::test]
fn lisp_reindent_skips_string_content(cx: &mut TestAppContext) {
    let editor = setup_json(
        cx,
        "t.scm",
        "(define (f)\n\"doc\n(f) returns 1\n  kept\")\n(g\nx)\n",
    );
    editor.update(cx, |e, cx| {
        e.apply_command(EditorCommand::ReindentRegion, cx)
    });
    settle(&editor, cx);
    assert_eq!(
        text(&editor, cx),
        "(define (f)\n  \"doc\n(f) returns 1\n  kept\")\n(g\n x)\n"
    );
}

/// In a Lisp buffer `'` is a quote prefix, not a paired delimiter.
#[gpui::test]
fn lisp_electric_pair_leaves_quote_unpaired(cx: &mut TestAppContext) {
    let editor = setup_json(cx, "t.scm", "");
    editor.update(cx, |e, cx| {
        e.electric_pair = true;
        assert!(!e.electric_pair_insert(&(0..0), "'", cx));
        assert!(e.electric_pair_insert(&(0..0), "(", cx));
        assert_eq!(e.active.buffer.full_text(), "()");
    });
}

/// Wait (real time) until the ANVIL REPL transcript contains `needle`: results
/// come from the engine's own OS thread, which `run_until_parked` can't see.
fn wait_for_repl(editor: &Entity<Editor>, cx: &mut TestAppContext, needle: &str) -> String {
    for _ in 0..400 {
        settle(editor, cx);
        let t = editor.read_with(cx, |e, _| {
            if e.active.name == super::anvil_repl::ANVIL_REPL_BUFFER {
                e.active.buffer.full_text()
            } else {
                String::new()
            }
        });
        if t.contains(needle) {
            return t;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("REPL never showed {needle:?}");
}

/// The ANVIL REPL takes multi-line input: RET inside an unfinished form opens
/// an indented continuation line, RET on a complete form evaluates it, and the
/// result lands above the input region as a comment. M-p recalls the input.
#[gpui::test]
fn anvil_repl_multiline_input_and_history(cx: &mut TestAppContext) {
    let ed = setup_json(cx, "t.txt", "");
    ed.update(cx, |e, cx| e.anvil_repl_open(cx));
    settle(&ed, cx);
    let type_and_ret = |ed: &Entity<Editor>, cx: &mut TestAppContext, text: &str| {
        ed.update(cx, |e, cx| {
            e.type_text(None, text, cx);
            e.anvil_send_input(cx);
        });
    };
    type_and_ret(&ed, cx, "(define (sq x)");
    let t = ed.read_with(cx, |e, _| e.active.buffer.full_text());
    assert!(
        t.ends_with("(define (sq x)\n  "),
        "continuation line, got {t:?}"
    );
    type_and_ret(&ed, cx, "(* x x))");
    wait_for_repl(&ed, cx, ";; => (no value)");
    // While a slow eval runs, typed input stays below its result.
    type_and_ret(&ed, cx, "(begin (displayln \"hi\") (sq 7))");
    ed.update(cx, |e, cx| e.type_text(None, "next", cx));
    let t = wait_for_repl(&ed, cx, ";; => 49");
    assert!(t.contains(";; hi\n;; => 49\nnext"), "got {t:?}");
    // M-p replaces the input with the last submitted form.
    ed.update(cx, |e, cx| e.anvil_history(true, cx));
    let t = ed.read_with(cx, |e, _| e.active.buffer.full_text());
    assert!(
        t.ends_with("(begin (displayln \"hi\") (sq 7))"),
        "got {t:?}"
    );
}

/// Structural editing in a Steel buffer: C-M-f moves over a sexp (a paren in a
/// string doesn't stop it), and slurp is a single undo step.
#[gpui::test]
fn steel_structural_motion_and_slurp(cx: &mut TestAppContext) {
    let ed = setup_json(cx, "t.scm", "(a \")\" b) c");
    ed.update(cx, |e, cx| e.move_to(0, cx));
    ed.update(cx, |e, cx| {
        e.dispatch_keystroke(&keystroke("ctrl-alt-f"), cx)
    });
    settle(&ed, cx);
    assert_eq!(ed.read_with(cx, |e, _| e.point()), 9);
    ed.update(cx, |e, cx| {
        e.move_to(1, cx);
        e.apply_command(EditorCommand::Structural(StructuralOp::SlurpForward), cx);
    });
    assert_eq!(text(&ed, cx), "(a \")\" b c)");
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::Undo, cx));
    settle(&ed, cx);
    assert_eq!(text(&ed, cx), "(a \")\" b) c");
}

/// `;` after `tx` moves to before the NEXT `x` (it used to re-find the same
/// one and stay put), and `~` toggles a multibyte letter.
#[gpui::test]
fn evil_till_repeat_and_multibyte_toggle_case(cx: &mut TestAppContext) {
    let ed = setup(cx, "a x b x c\n");
    keys(&ed, cx, "t x");
    assert_eq!(ed.read_with(cx, |e, _| e.point()), 1);
    keys(&ed, cx, ";");
    assert_eq!(ed.read_with(cx, |e, _| e.point()), 5);
    let ed = setup(cx, "éa\n");
    keys(&ed, cx, "~");
    assert_eq!(text(&ed, cx), "Éa\n");
}

/// A process's on-exit callback sees the buffer the user is in now, not the
/// one published when it was spawned: a process whose stdout goes to a file
/// fires only the exit callback, so it can't lean on the Line branch's
/// republish.
#[cfg(unix)]
#[gpui::test]
fn process_on_exit_sees_current_buffer(cx: &mut TestAppContext) {
    let ed = setup_json(cx, "before.txt", "");
    ed.update(cx, |e, cx| {
        e.script
            .eval("(define exit-buffer #f) (define (on-done code) (set! exit-buffer (current-buffer)))")
            .unwrap();
        e.publish_point();
        // Switch without republishing, as a render that installs a buffer
        // and spawns a process from the same command does.
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document("after.txt".into(), None, ""));
        e.make_process(
            crate::script::MakeProcessArgs {
                id: 4242,
                program: "true".into(),
                args: vec![],
                env: Default::default(),
                on_output: "ignore".into(),
                on_exit: Some("on-done".into()),
            },
            cx,
        );
    });
    for _ in 0..400 {
        settle(&ed, cx);
        let got = ed.read_with(cx, |e, _| e.script.eval("exit-buffer").unwrap().value);
        if got.as_deref() != Some("#false") {
            assert_eq!(got.as_deref(), Some("\"after.txt\""));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("on-exit never ran");
}

/// LSP completion in a Lisp buffer anchors on a symbol boundary: typing
/// `(plot-b` in a `*.macros.scm` file against the builtin `macros-scheme`
/// server opens a popup with `plot-bars`. A word-boundary anchor (`b`) used to
/// filter every `plot-…` label the server returned, so nothing showed.
#[gpui::test]
fn lisp_completion_keeps_hyphenated_prefix(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("macros-lispcomp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.macros.scm");
    std::fs::write(&path, "\n").unwrap();
    let ed = setup(cx, "\n");
    ed.update(cx, |e, cx| {
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document(
            "test.macros.scm".into(),
            Some(path.clone()),
            "\n",
        ));
        assert_eq!(e.active_language().as_deref(), Some("macros-scheme"));
        e.maybe_start_lsp(cx);
    });
    // The builtin server answers on its own thread: pump until it's ready
    // and has the buffer open.
    let ready = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            let uri = e.active_uri().unwrap();
            e.servers
                .get("macros-scheme")
                .is_some_and(|s| s.ready && s.open.contains(&uri))
        })
    };
    for _ in 0..200 {
        if ready(&ed, cx) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
        settle(&ed, cx);
    }
    assert!(
        ready(&ed, cx),
        "builtin macros-scheme server never became ready"
    );

    keys(&ed, cx, "i ( p l o t - b");
    ed.update(cx, |e, cx| {
        e.completion_popup = None;
        e.completion_auto_pending = false;
        e.completion_at_point(cx);
    });
    let labels = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            e.completion_view()
                .map(|v| v.items.into_iter().map(|r| r.label).collect::<Vec<_>>())
        })
    };
    for _ in 0..200 {
        if labels(&ed, cx).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
        settle(&ed, cx);
    }
    let labels = labels(&ed, cx).expect("completion popup is visible");
    assert!(labels.iter().any(|l| l == "plot-bars"), "{labels:?}");
    let anchor = ed.read_with(cx, |e, _| e.completion_popup.as_ref().map(|p| p.anchor));
    assert_eq!(anchor, Some(1), "anchored at the symbol start, after `(`");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Helm multi-select acts on exactly what was marked: marks are per source (a
/// candidate string shared with another source isn't swept in), RET runs each
/// mark's own source action, and the M-a action menu applies the chosen action
/// to every mark in that source — not just the highlighted candidate.
#[gpui::test]
fn helm_marks_drive_accept_and_the_action_menu(cx: &mut TestAppContext) {
    let ed = setup(cx, "");
    let open = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.update(cx, |e, cx| {
            e.run_script_blocking(
                r#"(begin
                     (set! *picked* '())
                     (helm (list (list "One" (list "x" "y")
                                       (list (list "A" 'pick-a) (list "B" 'pick-b)))
                                 (list "Two" (list "x" "z") 'pick-c))))"#,
                cx,
            );
        });
        settle(ed, cx);
    };
    let picked = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            e.script.eval("(reverse *picked*)").unwrap().value
        })
    };
    ed.update(cx, |e, cx| {
        e.run_script_blocking(
            r#"(begin
                 (define *picked* '())
                 (define (pick-a c) (set! *picked* (cons (string-append "a:" c) *picked*)))
                 (define (pick-b c) (set! *picked* (cons (string-append "b:" c) *picked*)))
                 (define (pick-c c) (set! *picked* (cons (string-append "c:" c) *picked*))))"#,
            cx,
        );
    });

    // Mark One/x and One/y (C-SPC advances); RET leaves Two/x alone.
    open(&ed, cx);
    ed.update(cx, |e, cx| {
        e.helm_toggle_mark(cx);
        e.helm_toggle_mark(cx);
        e.helm_accept(cx);
    });
    settle(&ed, cx);
    assert_eq!(picked(&ed, cx).as_deref(), Some(r#"("a:x" "a:y")"#));

    // A mark in each source: each runs its own source's action.
    open(&ed, cx);
    ed.update(cx, |e, cx| {
        e.helm_toggle_mark(cx); // One/x
        e.helm_next(cx); // skip One/y, land on Two/x
        e.helm_toggle_mark(cx);
        e.helm_accept(cx);
    });
    settle(&ed, cx);
    assert_eq!(picked(&ed, cx).as_deref(), Some(r#"("a:x" "c:x")"#));

    // M-a → "B" runs on both marks in the source, not just the selection.
    open(&ed, cx);
    ed.update(cx, |e, cx| {
        e.helm_toggle_mark(cx);
        e.helm_toggle_mark(cx);
        e.helm_previous(cx); // back from Two/x into the marked source
        e.helm_action_menu(cx);
        e.helm_next(cx); // "A" → "B"
        e.helm_accept(cx);
    });
    settle(&ed, cx);
    assert_eq!(picked(&ed, cx).as_deref(), Some(r#"("b:x" "b:y")"#));
}
