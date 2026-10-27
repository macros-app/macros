//! Kill ring, yank and incremental-search tests: a windowed Emacs-keys
//! `Editor` with the bundled Scheme loaded, keys dispatched through the window
//! the way typing reaches them (see `key_tests.rs`), plus direct calls where a
//! test pins down one method's contract (`push_kill`, `isearch_run`).

use gpui::{ClipboardItem, Entity, TestAppContext, VisualTestContext};

use super::kill_ring::KILL_RING_MAX;
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

fn ring(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> Vec<String> {
    editor.read_with(vcx, |e, _| e.kill_ring.clone())
}

fn clipboard(vcx: &mut VisualTestContext) -> Option<String> {
    vcx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

fn set_clipboard(vcx: &mut VisualTestContext, text: &str) {
    vcx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.to_string())));
}

// --- push_kill ------------------------------------------------------------------

#[gpui::test]
fn push_kill_appends_only_while_kill_append_is_set(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "\n");
    ed.update(vcx, |e, _| {
        e.push_kill("a".into());
        e.push_kill("b".into());
        assert_eq!(e.kill_ring, ["a", "b"]);
        assert!(!e.kill_appended);
        // A counted repeat's later runs extend the first run's entry.
        e.kill_append = true;
        e.push_kill("c".into());
        assert_eq!(e.kill_ring, ["a", "bc"]);
        assert!(e.kill_appended);
        assert_eq!(e.kill_index, 1);
        e.kill_append = false;
        e.push_kill("d".into());
        assert_eq!(e.kill_ring, ["a", "bc", "d"]);
        assert_eq!(e.kill_index, 2);
    });
}

#[gpui::test]
fn push_kill_with_append_on_an_empty_ring_starts_an_entry(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "\n");
    ed.update(vcx, |e, _| {
        e.kill_append = true;
        e.push_kill("first".into());
        assert_eq!(e.kill_ring, ["first"]);
        assert!(!e.kill_appended, "nothing was appended to");
        assert_eq!(e.kill_index, 0);
    });
}

#[gpui::test]
fn push_kill_resets_linewise_and_evicts_past_the_cap(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "\n");
    ed.update(vcx, |e, _| {
        e.evil_yank_linewise = true;
        e.push_kill("x".into());
        assert!(!e.evil_yank_linewise, "a new kill is charwise");

        e.kill_ring.clear();
        for i in 0..KILL_RING_MAX + 5 {
            e.push_kill(format!("k{i}"));
        }
        assert_eq!(e.kill_ring.len(), KILL_RING_MAX);
        assert_eq!(e.kill_ring[0], "k5", "the oldest entries go first");
        assert_eq!(
            e.kill_ring.last().map(String::as_str),
            Some(format!("k{}", KILL_RING_MAX + 4).as_str())
        );
        assert_eq!(e.kill_index, KILL_RING_MAX - 1);
    });
}

// --- kill commands --------------------------------------------------------------

#[gpui::test]
fn kill_line_kills_to_eol_then_the_break_and_stops_at_buffer_end(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "one two\nthree");
    press(&ed, vcx, "C-f C-f C-f C-k");
    assert_eq!(text(&ed, vcx), "one\nthree");
    assert_eq!(ring(&ed, vcx).last().unwrap(), " two");
    // Kills feed the OS clipboard as well as the ring.
    assert_eq!(clipboard(vcx).as_deref(), Some(" two"));
    // At the end of a line C-k kills just the line break.
    press(&ed, vcx, "C-k");
    assert_eq!(text(&ed, vcx), "onethree");
    assert_eq!(ring(&ed, vcx).last().unwrap(), "\n");
    // At the end of the buffer there is nothing to kill.
    press(&ed, vcx, "C-e C-k");
    assert_eq!(text(&ed, vcx), "onethree");
    assert_eq!(ring(&ed, vcx).last().unwrap(), "\n");
}

#[gpui::test]
fn kill_word_and_backward_kill_word(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "alpha beta gamma\n");
    press(&ed, vcx, "M-d");
    assert_eq!(text(&ed, vcx), " beta gamma\n");
    assert_eq!(ring(&ed, vcx).last().unwrap(), "alpha");
    press(&ed, vcx, "C-e M-backspace");
    assert_eq!(text(&ed, vcx), " beta \n");
    assert_eq!(ring(&ed, vcx).last().unwrap(), "gamma");
    assert_eq!(point(&ed, vcx), 6);
    // Each kill is its own entry, newest last.
    assert_eq!(ring(&ed, vcx), ["alpha", "gamma"]);
}

#[gpui::test]
fn kill_region_needs_a_region(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "hello world\n");
    press(&ed, vcx, "C-w");
    assert_eq!(text(&ed, vcx), "hello world\n");
    assert_eq!(status(&ed, vcx), "No region selected");
    assert!(ring(&ed, vcx).is_empty());
    // A backwards region (point before mark) kills the same text.
    press(&ed, vcx, "M-f C-SPC C-a C-w");
    assert_eq!(text(&ed, vcx), " world\n");
    assert_eq!(ring(&ed, vcx), ["hello"]);
}

#[gpui::test]
fn kill_ring_save_copies_without_editing(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "hello world\n");
    press(&ed, vcx, "M-w");
    assert_eq!(status(&ed, vcx), "No region selected");
    press(&ed, vcx, "C-SPC M-f M-w");
    assert_eq!(text(&ed, vcx), "hello world\n");
    assert_eq!(ring(&ed, vcx), ["hello"]);
    assert_eq!(clipboard(vcx).as_deref(), Some("hello"));
    assert_eq!(status(&ed, vcx), "Saved to kill ring");
    assert!(
        !ed.read_with(vcx, |e, _| e.active.mark_active),
        "M-w deactivates the mark"
    );
    // The saved text yanks back.
    press(&ed, vcx, "C-e C-y");
    assert_eq!(text(&ed, vcx), "hello worldhello\n");
}

// --- yank / yank-pop -------------------------------------------------------------

/// A ring of three kills, newest last, with the clipboard already holding the
/// newest (as it would after killing it here).
fn seed_ring(editor: &Entity<Editor>, vcx: &mut VisualTestContext) {
    editor.update(vcx, |e, _| {
        e.kill_ring = vec!["one".into(), "two".into(), "three".into()];
        e.kill_index = 2;
    });
    set_clipboard(vcx, "three");
}

#[gpui::test]
fn yank_pop_cycles_older_kills_and_wraps(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "<>\n");
    seed_ring(&ed, vcx);
    press(&ed, vcx, "C-f C-y");
    assert_eq!(text(&ed, vcx), "<three>\n");
    assert_eq!(point(&ed, vcx), 6);
    press(&ed, vcx, "M-y");
    assert_eq!(text(&ed, vcx), "<two>\n");
    assert_eq!(point(&ed, vcx), 4);
    press(&ed, vcx, "M-y");
    assert_eq!(text(&ed, vcx), "<one>\n");
    // Past the oldest kill the cycle wraps back to the newest.
    press(&ed, vcx, "M-y");
    assert_eq!(text(&ed, vcx), "<three>\n");
    assert_eq!(point(&ed, vcx), 6);
    // Yank-pop never adds to the ring.
    assert_eq!(ring(&ed, vcx), ["one", "two", "three"]);
}

#[gpui::test]
fn yank_pop_after_another_command_is_refused(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "ab\n");
    seed_ring(&ed, vcx);
    press(&ed, vcx, "M-y");
    assert_eq!(text(&ed, vcx), "ab\n");
    assert_eq!(status(&ed, vcx), "Previous command was not a yank");
    press(&ed, vcx, "C-y C-f M-y");
    assert_eq!(text(&ed, vcx), "threeab\n");
    assert_eq!(status(&ed, vcx), "Previous command was not a yank");
    // A fresh yank starts the cycle again from the newest kill.
    press(&ed, vcx, "C-y M-y");
    assert_eq!(text(&ed, vcx), "threeatwob\n");
}

#[gpui::test]
fn yank_with_nothing_to_yank_echoes(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "x\n");
    set_clipboard(vcx, "");
    press(&ed, vcx, "C-y");
    assert_eq!(text(&ed, vcx), "x\n");
    assert_eq!(status(&ed, vcx), "Kill ring is empty");
}

/// Something rewrote the buffer under the last yank without going through a
/// command: yank-pop clamps the recorded range instead of splicing past the end.
#[gpui::test]
fn yank_pop_clamps_a_stale_yank_range(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "\n");
    seed_ring(&ed, vcx);
    press(&ed, vcx, "C-y");
    assert_eq!(text(&ed, vcx), "three\n");
    ed.update(vcx, |e, cx| {
        let len = e.active.buffer.len();
        e.edit(0..len, "");
        e.collapse_to(0);
        e.yank_pop(cx);
    });
    assert_eq!(text(&ed, vcx), "two");
}

// --- the system clipboard --------------------------------------------------------

#[gpui::test]
fn sync_clipboard_adopts_only_new_external_text(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "\n");
    let sync = |ed: &Entity<Editor>, vcx: &mut VisualTestContext| {
        ed.update(vcx, |e, cx| e.sync_clipboard_into_kill_ring(cx))
    };
    set_clipboard(vcx, "");
    assert!(!sync(&ed, vcx), "an empty clipboard is ignored");
    assert!(ring(&ed, vcx).is_empty());
    set_clipboard(vcx, "external");
    assert!(sync(&ed, vcx));
    assert_eq!(ring(&ed, vcx), ["external"]);
    // Already the ring's top: not pushed twice.
    assert!(!sync(&ed, vcx));
    assert_eq!(ring(&ed, vcx), ["external"]);
    // A kill made here replaces the clipboard, so it's no longer "external".
    ed.update(vcx, |e, _| e.push_kill("mine".into()));
    set_clipboard(vcx, "mine");
    assert!(!sync(&ed, vcx));
    assert_eq!(ring(&ed, vcx), ["external", "mine"]);
}

#[gpui::test]
fn yank_reflects_text_copied_in_another_app(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "\n");
    seed_ring(&ed, vcx);
    set_clipboard(vcx, "pasted");
    press(&ed, vcx, "C-y");
    assert_eq!(text(&ed, vcx), "pasted\n");
    assert_eq!(ring(&ed, vcx).last().unwrap(), "pasted");
    // The adopted copy is the newest kill; M-y continues into the older ones.
    press(&ed, vcx, "M-y");
    assert_eq!(text(&ed, vcx), "three\n");
}

// --- transpose-chars -------------------------------------------------------------

#[gpui::test]
fn transpose_chars_in_the_middle_and_at_the_ends(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "abc");
    // At the start there's no character before point: nothing happens.
    press(&ed, vcx, "C-t");
    assert_eq!(text(&ed, vcx), "abc");
    assert_eq!(point(&ed, vcx), 0);
    // In the middle the chars around point swap and point moves past them.
    press(&ed, vcx, "C-f C-t");
    assert_eq!(text(&ed, vcx), "bac");
    assert_eq!(point(&ed, vcx), 2);
    // At the end of the buffer the two chars before point swap.
    press(&ed, vcx, "C-e C-t");
    assert_eq!(text(&ed, vcx), "bca");
    assert_eq!(point(&ed, vcx), 3);
}

#[gpui::test]
fn transpose_chars_needs_two_chars(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "a");
    press(&ed, vcx, "C-e C-t");
    assert_eq!(text(&ed, vcx), "a");
    let (ed, vcx) = setup(cx, "");
    press(&ed, vcx, "C-t");
    assert_eq!(text(&ed, vcx), "");
}

#[gpui::test]
fn transpose_chars_swaps_whole_graphemes(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "éa");
    press(&ed, vcx, "C-f C-t");
    assert_eq!(text(&ed, vcx), "aé");
    assert_eq!(point(&ed, vcx), "aé".len());
}

// --- isearch ---------------------------------------------------------------------

/// Start an isearch at `from` and set its query, as typing it would.
fn isearch(e: &mut Editor, from: usize, dir: Direction, query: &str, cx: &mut Context<Editor>) {
    e.collapse_to(from);
    e.isearch_start(dir, cx);
    e.minibuffer.as_mut().unwrap().input = query.into();
    e.isearch_update(cx);
}

/// [`isearch`] for a regexp (`C-M-s`).
fn isearch_regexp(
    e: &mut Editor,
    from: usize,
    dir: Direction,
    query: &str,
    cx: &mut Context<Editor>,
) {
    e.collapse_to(from);
    e.isearch_start_with(dir, super::isearch::IsearchKind::Regexp, "", cx);
    e.minibuffer.as_mut().unwrap().input = query.into();
    e.isearch_update(cx);
}

fn isearch_message(e: &Editor) -> String {
    e.minibuffer.as_ref().unwrap().message.clone()
}

#[gpui::test]
fn isearch_forward_lands_after_each_match_and_wraps(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "foo bar foo baz\n");
    ed.update(vcx, |e, cx| {
        isearch(e, 0, Direction::Forward, "foo", cx);
        assert_eq!(e.point(), 3);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(0..3));
        e.isearch_repeat(Direction::Forward, cx);
        assert_eq!(e.point(), 11);
        // No match after the last one: wrap to the first.
        e.isearch_repeat(Direction::Forward, cx);
        assert_eq!(e.point(), 3);
        assert_eq!(isearch_message(e), "");
    });
}

#[gpui::test]
fn isearch_backward_lands_on_match_starts(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "foo bar foo baz\n");
    ed.update(vcx, |e, cx| {
        let end = e.active.buffer.len();
        isearch(e, end, Direction::Backward, "foo", cx);
        assert_eq!(e.point(), 8);
        e.isearch_repeat(Direction::Backward, cx);
        assert_eq!(e.point(), 0);
        // Turning around mid-search continues forward from the current match.
        e.isearch_repeat(Direction::Forward, cx);
        assert_eq!(e.point(), 11);
    });
}

#[gpui::test]
fn isearch_regexp_query_is_a_case_sensitive_regex(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "Foo a.c abc foo\n");
    ed.update(vcx, |e, cx| {
        // `.` is a regex wildcard: the first hit is the literal `a.c`, the next
        // `abc`. Escaping it searches for the dot only.
        isearch_regexp(e, 0, Direction::Forward, "a.c", cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(4..7));
        e.isearch_repeat(Direction::Forward, cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(8..11));
        e.mb_abort(cx);
        isearch_regexp(e, 0, Direction::Forward, r"a\.c", cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(4..7));
        e.isearch_repeat(Direction::Forward, cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(4..7));
        e.mb_abort(cx);
        // Case matters unless the query asks for folding with `(?i)`.
        isearch_regexp(e, 0, Direction::Forward, "foo", cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(12..15));
        e.mb_abort(cx);
        isearch_regexp(e, 0, Direction::Forward, "(?i)foo", cx);
        assert_eq!(e.isearch.as_ref().unwrap().matched, Some(0..3));
    });
}

#[gpui::test]
fn isearch_reports_invalid_and_failing_queries(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "foo bar\n");
    ed.update(vcx, |e, cx| {
        isearch_regexp(e, 2, Direction::Forward, "(", cx);
        assert_eq!(isearch_message(e), " [invalid regex]");
        assert_eq!(e.point(), 2, "an invalid query doesn't move point");
        // Fixing the query clears the hint.
        e.minibuffer.as_mut().unwrap().input = "bar".into();
        e.isearch_update(cx);
        assert_eq!(isearch_message(e), "");
        assert_eq!(e.point(), 7);
        e.minibuffer.as_mut().unwrap().input = "zzz".into();
        e.isearch_update(cx);
        assert_eq!(isearch_message(e), " [failing]");
        assert_eq!(e.isearch.as_ref().unwrap().matched, None);
        assert_eq!(
            e.point(),
            7,
            "a failing search leaves point on the last hit"
        );
        // An emptied query drops the highlight.
        e.minibuffer.as_mut().unwrap().input.clear();
        e.isearch_update(cx);
        assert_eq!(e.search_query, None);
        assert_eq!(isearch_message(e), "");
    });
}

#[gpui::test]
fn isearch_keys_repeat_finish_and_abort(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "ab ab ab\n");
    press(&ed, vcx, "C-s \"ab\"");
    assert_eq!(point(&ed, vcx), 2);
    press(&ed, vcx, "C-s C-s");
    assert_eq!(point(&ed, vcx), 8);
    // RET finishes on the match and remembers the query.
    press(&ed, vcx, "RET");
    assert!(ed.read_with(vcx, |e, _| e.minibuffer.is_none() && e.isearch.is_none()));
    assert_eq!(point(&ed, vcx), 8);
    // C-r C-r on an empty query recalls it; C-g returns to where it began.
    press(&ed, vcx, "C-r C-r");
    assert_eq!(point(&ed, vcx), 6);
    press(&ed, vcx, "C-r");
    assert_eq!(point(&ed, vcx), 3);
    press(&ed, vcx, "C-g");
    assert_eq!(point(&ed, vcx), 8);
    assert!(ed.read_with(vcx, |e, _| e.minibuffer.is_none()));
}

#[gpui::test]
fn search_match_ranges_are_scoped_to_the_window(cx: &mut TestAppContext) {
    let (ed, vcx) = setup(cx, "ab\nab\nab\n");
    ed.update(vcx, |e, _| {
        assert!(
            e.search_match_ranges_in(0, 9).is_empty(),
            "no search active"
        );
        e.search_query = Some(String::new());
        assert!(e.search_match_ranges_in(0, 9).is_empty(), "empty query");
        e.search_query = Some("ab".into());
        assert_eq!(e.search_match_ranges_in(0, 9), [0..2, 3..5, 6..8]);
        // Offsets stay absolute when the window starts mid-buffer.
        assert_eq!(e.search_match_ranges_in(3, 6), vec![3..5]);
        // An end past the buffer clamps; a start past the end yields nothing.
        assert_eq!(e.search_match_ranges_in(6, 1_000), vec![6..8]);
        assert!(e.search_match_ranges_in(50, 1_000).is_empty());
        // Zero-width matches aren't highlighted; an invalid regex shows none.
        e.search_query = Some("x*".into());
        assert!(e.search_match_ranges_in(0, 9).is_empty());
        e.search_query = Some("(".into());
        assert!(e.search_match_ranges_in(0, 9).is_empty());
        // The compiled regex is cached per query, and a new query replaces it.
        e.search_query = Some("b\n".into());
        assert_eq!(e.search_match_ranges_in(0, 9), [1..3, 4..6, 7..9]);
    });
}
