//! End-to-end tests for saving over changes made outside the editor, and for
//! read-modify-write edits (`edit-file-text`) of files a buffer may have open.

use gpui::{Entity, TestAppContext};

use super::*;

/// An editor with the bundled Scheme loaded, and a fresh scratch directory.
fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, PathBuf) {
    test_context(cx);
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-save-e2e-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let editor = cx.new(Editor::new);
    editor.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        e.mark_config_loaded(cx);
    });
    settle(&editor, cx);
    (editor, dir)
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

/// Visit `path` the way `C-x C-f` does, and wait for it to open.
fn visit(editor: &Entity<Editor>, cx: &mut TestAppContext, path: &Path) {
    editor.update(cx, |e, cx| e.find_file(&path.to_string_lossy(), cx));
    settle(editor, cx);
    editor.read_with(cx, |e, _| assert_eq!(e.active.path.as_deref(), Some(path)));
}

/// Type `text` at the end of the active buffer.
fn append(editor: &Entity<Editor>, cx: &mut TestAppContext, text: &str) {
    editor.update(cx, |e, _| {
        let end = e.active.buffer.len();
        e.edit(end..end, text);
    });
}

fn save(editor: &Entity<Editor>, cx: &mut TestAppContext) {
    editor.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    settle(editor, cx);
}

fn prompting_to_save_over(editor: &Entity<Editor>, cx: &mut TestAppContext) -> bool {
    editor.read_with(cx, |e, _| {
        matches!(
            e.minibuffer.as_ref().map(|m| &m.purpose),
            Some(MinibufferPurpose::Confirm(PendingAction::SaveOverChanged(
                _
            )))
        )
    })
}

#[gpui::test]
fn saving_asks_before_overwriting_a_file_changed_on_disk(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("notes.txt");
    std::fs::write(&path, "one\n").unwrap();
    visit(&ed, cx, &path);
    append(&ed, cx, "mine\n");
    // Something else (git, a formatter) rewrites the file meanwhile.
    std::fs::write(&path, "one\ntheirs\n").unwrap();
    save(&ed, cx);
    assert!(prompting_to_save_over(&ed, cx), "C-x C-s must ask first");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntheirs\n");
    // Quit-time saves can't ask: they refuse, and the quit is called off.
    let ok = ed.update(cx, |e, cx| {
        e.minibuffer = None;
        e.save_all_modified(true, cx)
    });
    assert!(!ok);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntheirs\n");
    ed.read_with(cx, |e, _| assert!(e.active.modified));
    // `:wq` refuses too.
    let ok = ed.update(cx, |e, cx| e.save_active_sync(true, cx));
    assert!(!ok);
    // Saying yes overwrites, and the save is the new baseline.
    save(&ed, cx);
    ed.update(cx, |e, cx| {
        e.minibuffer = None;
        e.perform(PendingAction::SaveOverChanged("notes.txt".into()), cx)
    });
    settle(&ed, cx);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nmine\n");
    append(&ed, cx, "more\n");
    save(&ed, cx);
    assert!(!prompting_to_save_over(&ed, cx));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nmine\nmore\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn saving_an_unchanged_file_does_not_ask(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("plain.txt");
    std::fs::write(&path, "one\n").unwrap();
    visit(&ed, cx, &path);
    append(&ed, cx, "two\n");
    save(&ed, cx);
    assert!(!prompting_to_save_over(&ed, cx));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    // An external rewrite of the same bytes isn't a change either.
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "one\ntwo\n").unwrap();
    append(&ed, cx, "three\n");
    let ok = ed.update(cx, |e, cx| e.save_all_modified(true, cx));
    assert!(ok);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\nthree\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn reverting_takes_the_file_as_the_new_baseline(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("r.txt");
    std::fs::write(&path, "one\n").unwrap();
    visit(&ed, cx, &path);
    std::fs::write(&path, "one\ntheirs\n").unwrap();
    ed.update(cx, |e, cx| e.revert_buffer(cx));
    append(&ed, cx, "mine\n");
    save(&ed, cx);
    assert!(!prompting_to_save_over(&ed, cx));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntheirs\nmine\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A Scheme callback for `edit-file-text` that appends a line.
const APPEND_LINE: &str = r#"(define (test--append-line path text)
  (set-file-text path (string-append text "* TODO captured\n")))"#;

#[gpui::test]
fn file_edits_go_into_the_open_buffer_not_under_it(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("inbox.org");
    std::fs::write(&path, "* a\n").unwrap();
    visit(&ed, cx, &path);
    // Unsaved work in the buffer that the file on disk doesn't have yet.
    append(&ed, cx, "* unsaved\n");
    ed.update(cx, |e, cx| {
        e.run_script_blocking(APPEND_LINE, cx);
        e.apply_command(
            EditorCommand::EditFileText {
                path: path.to_string_lossy().into_owned(),
                callback: "test--append-line".into(),
            },
            cx,
        );
    });
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.active.buffer.full_text(),
            "* a\n* unsaved\n* TODO captured\n"
        );
        assert!(e.active.modified);
    });
    // The file is untouched until the buffer is saved — and saving it
    // doesn't ask, since nothing else changed the file.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "* a\n");
    save(&ed, cx);
    assert!(!prompting_to_save_over(&ed, cx));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "* a\n* unsaved\n* TODO captured\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn file_edits_reach_an_inactive_buffer_and_files_nobody_has_open(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let open = dir.join("open.org");
    std::fs::write(&open, "* a\n").unwrap();
    visit(&ed, cx, &open);
    let other = dir.join("other.txt");
    std::fs::write(&other, "x\n").unwrap();
    visit(&ed, cx, &other);
    let closed = dir.join("sub").join("closed.org");
    ed.update(cx, |e, cx| {
        e.run_script_blocking(APPEND_LINE, cx);
        for path in [&open, &closed] {
            e.apply_command(
                EditorCommand::EditFileText {
                    path: path.to_string_lossy().into_owned(),
                    callback: "test--append-line".into(),
                },
                cx,
            );
        }
    });
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let doc = e.doc("open.org").expect("still open");
        assert_eq!(doc.buffer.full_text(), "* a\n* TODO captured\n");
        assert!(doc.modified);
    });
    assert_eq!(std::fs::read_to_string(&open).unwrap(), "* a\n");
    // A file no buffer visits is written (its directory created) directly.
    assert_eq!(
        std::fs::read_to_string(&closed).unwrap(),
        "* TODO captured\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn a_file_edit_reruns_on_text_changed_in_between(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("busy.org");
    std::fs::write(&path, "* a\n").unwrap();
    visit(&ed, cx, &path);
    ed.update(cx, |e, cx| {
        e.run_script_blocking(APPEND_LINE, cx);
        // The read is handed out, then the user types before the answer lands.
        e.edit_file_text(
            path.to_string_lossy().into_owned(),
            "test--noop".into(),
            0,
            cx,
        );
        let end = e.active.buffer.len();
        e.edit(end..end, "* typed\n");
        // An answer computed from the stale text must not be applied...
        e.file_edits
            .get_mut(&super::buffers::file_identity(&path))
            .expect("read recorded")
            .set_callback_for_test("test--append-line");
        e.set_file_text(
            path.to_string_lossy().into_owned(),
            "* a\n* stale\n".into(),
            cx,
        );
    });
    settle(&ed, cx);
    // ...instead the callback ran again on the current text.
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.active.buffer.full_text(),
            "* a\n* typed\n* TODO captured\n"
        );
    });
    std::fs::remove_dir_all(&dir).ok();
}
