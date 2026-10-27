//! Tests for the Emacs file and buffer commands (`write-file`, `rename-buffer`,
//! `insert-file`, `find-file-at-point`, the `C-x 4` family, `read-only-mode`)
//! and the `make-backup-files` / `create-lockfiles` options.

use gpui::{Entity, TestAppContext};
use ropey::Rope;

use super::*;
use crate::script::{EditorSnapshot, ScriptEngine};
use crate::settings::OptionValue;

/// An editor with the bundled Scheme loaded, and a fresh scratch directory.
fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, PathBuf) {
    test_context(cx);
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-files-e2e-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
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

fn visit(editor: &Entity<Editor>, cx: &mut TestAppContext, path: &Path) {
    editor.update(cx, |e, cx| e.find_file(&path.to_string_lossy(), cx));
    settle(editor, cx);
    editor.read_with(cx, |e, _| assert_eq!(e.active.path.as_deref(), Some(path)));
}

fn append(editor: &Entity<Editor>, cx: &mut TestAppContext, text: &str) {
    editor.update(cx, |e, _| {
        let end = e.active.buffer.len();
        e.edit(end..end, text);
    });
}

fn run(editor: &Entity<Editor>, cx: &mut TestAppContext, command: EditorCommand) {
    editor.update(cx, |e, cx| e.apply_command(command, cx));
    settle(editor, cx);
}

fn set_option(editor: &Entity<Editor>, cx: &mut TestAppContext, key: &str, value: OptionValue) {
    run(editor, cx, EditorCommand::SetOption(key.into(), value));
}

fn text(editor: &Entity<Editor>, cx: &mut TestAppContext) -> String {
    editor.read_with(cx, |e, _| e.active.buffer.full_text())
}

#[gpui::test]
fn write_file_saves_under_a_new_name_and_visits_it(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let a = dir.join("a.txt");
    std::fs::write(&a, "one\n").unwrap();
    visit(&ed, cx, &a);
    append(&ed, cx, "two\n");
    let b = dir.join("b.txt");
    run(
        &ed,
        cx,
        EditorCommand::WriteFile(b.to_string_lossy().into()),
    );
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "one\ntwo\n");
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "one\n",
        "the old file is left alone"
    );
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.path.as_deref(), Some(b.as_path()));
        assert_eq!(e.active.name, "b.txt");
        assert!(!e.active.modified);
        assert!(e.panes.iter().all(|p| p.buffer != "a.txt"));
    });
    // Later saves go to the new file.
    append(&ed, cx, "three\n");
    run(&ed, cx, EditorCommand::SaveBuffer);
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "one\ntwo\nthree\n");
    // A directory takes the buffer's file name.
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    run(
        &ed,
        cx,
        EditorCommand::WriteFile(format!("{}/", sub.display())),
    );
    assert_eq!(
        std::fs::read_to_string(sub.join("b.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn write_file_asks_before_overwriting_and_refuses_another_buffers_file(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let other = dir.join("other.txt");
    std::fs::write(&other, "theirs\n").unwrap();
    visit(&ed, cx, &other);
    let mine = dir.join("mine.txt");
    std::fs::write(&mine, "mine\n").unwrap();
    visit(&ed, cx, &mine);
    // Another buffer visits other.txt: writing there would leave two buffers
    // fighting over one file.
    run(
        &ed,
        cx,
        EditorCommand::WriteFile(other.to_string_lossy().into()),
    );
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "theirs\n");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.path.as_deref(), Some(mine.as_path()))
    });
    // An existing file nobody visits: ask first.
    let existing = dir.join("existing.txt");
    std::fs::write(&existing, "keep\n").unwrap();
    run(
        &ed,
        cx,
        EditorCommand::WriteFile(existing.to_string_lossy().into()),
    );
    let pending = ed.read_with(cx, |e, _| match e.minibuffer.as_ref().map(|m| &m.purpose) {
        Some(MinibufferPurpose::Confirm(action @ PendingAction::WriteFile { .. })) => {
            Some(action.clone())
        }
        _ => None,
    });
    let action = pending.expect("overwriting must ask");
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep\n");
    ed.update(cx, |e, cx| {
        e.minibuffer = None;
        e.perform(action, cx);
    });
    settle(&ed, cx);
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "mine\n");
    ed.read_with(cx, |e, _| assert_eq!(e.active.name, "existing.txt"));
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn rename_buffer_renames_and_refuses_a_taken_name(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("r.txt");
    std::fs::write(&path, "x\n").unwrap();
    visit(&ed, cx, &path);
    run(&ed, cx, EditorCommand::RenameBuffer("notes".into()));
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.name, "notes");
        assert_eq!(e.active.path.as_deref(), Some(path.as_path()));
        assert!(e.panes.iter().any(|p| p.buffer == "notes"));
        assert!(e.buffer_names().contains(&"notes".to_string()));
    });
    run(&ed, cx, EditorCommand::RenameBuffer("*scratch*".into()));
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.name, "notes");
        assert!(e.status.contains("in use"), "{}", e.status);
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn insert_file_inserts_at_point_and_leaves_point_before_it(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("host.txt");
    std::fs::write(&path, "ab").unwrap();
    let part = dir.join("part.txt");
    std::fs::write(&part, "XY").unwrap();
    visit(&ed, cx, &path);
    ed.update(cx, |e, cx| e.move_to(1, cx));
    run(
        &ed,
        cx,
        EditorCommand::InsertFile(part.to_string_lossy().into()),
    );
    assert_eq!(text(&ed, cx), "aXYb");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.point(), 1);
        assert!(e.active.modified);
    });
    // A missing file changes nothing.
    run(
        &ed,
        cx,
        EditorCommand::InsertFile(dir.join("nope").to_string_lossy().into()),
    );
    assert_eq!(text(&ed, cx), "aXYb");
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn read_only_mode_refuses_edits_and_shows_in_the_mode_line(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("ro.txt");
    std::fs::write(&path, "fixed\n").unwrap();
    visit(&ed, cx, &path);
    let marker = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| {
            e.mode_line_segments()
                .first()
                .map(|(s, _)| s.to_string())
                .unwrap_or_default()
        })
    };
    assert!(marker(&ed, cx).contains("--"));
    run(&ed, cx, EditorCommand::ToggleReadOnly);
    assert!(marker(&ed, cx).contains("%%"));
    append(&ed, cx, "nope");
    run(&ed, cx, EditorCommand::Newline);
    run(
        &ed,
        cx,
        EditorCommand::InsertFile(path.to_string_lossy().into()),
    );
    assert_eq!(text(&ed, cx), "fixed\n");
    ed.read_with(cx, |e, _| {
        assert!(!e.active.modified);
        assert!(e.status.contains("read-only"), "{}", e.status);
    });
    run(&ed, cx, EditorCommand::ToggleReadOnly);
    append(&ed, cx, "ok");
    assert_eq!(text(&ed, cx), "fixed\nok");
    assert!(marker(&ed, cx).contains("**"));
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn other_window_commands_show_things_beside_the_selected_window(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let a = dir.join("left.txt");
    std::fs::write(&a, "l\n").unwrap();
    visit(&ed, cx, &a);
    // C-x 4 f with one window splits, and selects the file's window.
    let b = dir.join("right.txt");
    std::fs::write(&b, "r\n").unwrap();
    run(
        &ed,
        cx,
        EditorCommand::FindFileOtherWindow(b.to_string_lossy().into()),
    );
    let first = ed.read_with(cx, |e, _| {
        assert_eq!(e.layout.leaves().len(), 2);
        assert_eq!(e.active.name, "right.txt");
        let other = e.panes.iter().find(|p| p.id != e.selected_pane).unwrap();
        assert_eq!(other.buffer, "left.txt");
        e.selected_pane
    });
    // C-x 4 C-o shows a buffer in the other window but keeps the selection.
    run(&ed, cx, EditorCommand::DisplayBuffer("*scratch*".into()));
    ed.read_with(cx, |e, _| {
        assert_eq!(e.selected_pane, first);
        assert_eq!(e.active.name, "right.txt");
        assert_eq!(e.layout.leaves().len(), 2, "two windows are enough");
        let other = e.panes.iter().find(|p| p.id != first).unwrap();
        assert_eq!(other.buffer, "*scratch*");
    });
    // C-x 4 b moves there.
    run(
        &ed,
        cx,
        EditorCommand::SwitchToBufferOtherWindow("left.txt".into()),
    );
    ed.read_with(cx, |e, _| {
        assert_ne!(e.selected_pane, first);
        assert_eq!(e.active.name, "left.txt");
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn the_first_save_of_a_session_keeps_a_backup(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let backups = dir.join("backups");
    set_option(
        &ed,
        cx,
        "backup-directory",
        OptionValue::Str(backups.to_string_lossy().into()),
    );
    let path = dir.join("kept.txt");
    std::fs::write(&path, "original\n").unwrap();
    visit(&ed, cx, &path);
    append(&ed, cx, "edit 1\n");
    run(&ed, cx, EditorCommand::SaveBuffer);
    append(&ed, cx, "edit 2\n");
    run(&ed, cx, EditorCommand::SaveBuffer);
    let backup = crate::save::backup_path(&backups, &crate::save::write_target(&path));
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), "original\n");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "original\nedit 1\nedit 2\n"
    );
    // Nothing lands next to the file itself.
    assert!(!dir.join("kept.txt~").exists());
    // With make-backup-files off, no backup is made.
    set_option(&ed, cx, "make-backup-files", OptionValue::Bool(false));
    let other = dir.join("unkept.txt");
    std::fs::write(&other, "x\n").unwrap();
    visit(&ed, cx, &other);
    append(&ed, cx, "y\n");
    run(&ed, cx, EditorCommand::SaveBuffer);
    assert!(!crate::save::backup_path(&backups, &crate::save::write_target(&other)).exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn lock_files_follow_unsaved_changes_when_enabled(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let path = dir.join("locked.txt");
    let lock = dir.join(".#locked.txt");
    std::fs::write(&path, "a\n").unwrap();
    visit(&ed, cx, &path);
    // Off by default.
    append(&ed, cx, "b\n");
    run(&ed, cx, EditorCommand::EndOfBuffer);
    assert!(std::fs::symlink_metadata(&lock).is_err());
    run(&ed, cx, EditorCommand::SaveBuffer);
    set_option(&ed, cx, "create-lockfiles", OptionValue::Bool(true));
    append(&ed, cx, "c\n");
    run(&ed, cx, EditorCommand::EndOfBuffer);
    assert!(
        std::fs::symlink_metadata(&lock).is_ok(),
        "an unsaved change takes the lock"
    );
    run(&ed, cx, EditorCommand::SaveBuffer);
    assert!(
        std::fs::symlink_metadata(&lock).is_err(),
        "saving releases it"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[gpui::test]
fn visiting_a_file_another_live_process_has_locked_warns(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    set_option(&ed, cx, "create-lockfiles", OptionValue::Bool(true));
    let path = dir.join("busy.txt");
    std::fs::write(&path, "a\n").unwrap();
    // pid 1 is always running.
    let host = crate::filelock::LockOwner::me().host;
    std::os::unix::fs::symlink(format!("someone@{host}.1"), dir.join(".#busy.txt")).unwrap();
    visit(&ed, cx, &path);
    ed.read_with(cx, |e, _| {
        assert!(e.status.contains("locked by someone@"), "{}", e.status);
    });
    // Editing anyway leaves their lock alone.
    append(&ed, cx, "b\n");
    run(&ed, cx, EditorCommand::EndOfBuffer);
    assert_eq!(
        crate::filelock::owner(&crate::save::write_target(&path)).map(|o| o.pid),
        Some(1)
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// An engine with core.scm and files.scm loaded, showing `text` with point at
/// `point` in a buffer whose directory is `dir`.
fn scheme_engine(dir: &Path, text: &str, point: usize) -> ScriptEngine {
    let (engine, _rx) = ScriptEngine::new();
    engine
        .eval(include_str!("../../scheme/core.scm"))
        .expect("core.scm");
    engine
        .eval(include_str!("../../scheme/files.scm"))
        .expect("files.scm");
    engine.set_snapshot(EditorSnapshot {
        text: Rope::from_str(text),
        point,
        directory: dir.to_string_lossy().into(),
        ..Default::default()
    });
    engine
}

#[test]
fn find_file_at_point_reads_paths_lines_and_urls() {
    let dir = std::env::temp_dir().join(format!("macros-ffap-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    let guess = |text: &str, point: usize| {
        let engine = scheme_engine(&dir, text, point);
        engine
            .eval("(ffap-guess)")
            .unwrap()
            .value
            .unwrap_or_default()
    };
    // A relative path with a line, at the end of a sentence.
    let g = guess("error at src/lib.rs:12:5.\n", 12);
    assert!(
        g.contains("file") && g.contains("src/lib.rs") && g.contains("12"),
        "{g}"
    );
    assert!(!g.contains(":12"), "the line suffix is split off: {g}");
    // No line.
    let g = guess("(see src/lib.rs)\n", 8);
    assert!(g.contains("src/lib.rs") && g.contains("#f"), "{g}");
    // A URL.
    let g = guess("docs: https://example.com/a?b=1 here\n", 10);
    assert!(
        g.contains("url") && g.contains("https://example.com/a?b=1"),
        "{g}"
    );
    // A missing file, or whitespace at point: nothing.
    assert!(guess("nothing/here.rs\n", 3).contains("#f"));
    assert!(guess("a  b\n", 2).contains("#f"));
    // Opening it visits the file and lands on the line.
    let engine = scheme_engine(&dir, "src/lib.rs:3\n", 0);
    let cmds = engine.eval("(find-file-at-point)").unwrap().commands;
    assert!(
        cmds.iter()
            .any(|c| matches!(c, EditorCommand::OpenFile(p) if p.ends_with("src/lib.rs"))),
        "{cmds:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn write_file_keeps_its_two_argument_script_form() {
    let dir = std::env::temp_dir().join(format!("macros-wf-{}", std::process::id()));
    let engine = scheme_engine(&dir, "", 0);
    let file = dir.join("deep/out.txt");
    let out = engine
        .eval(&format!(r#"(write-file "{}" "hi")"#, file.display()))
        .unwrap();
    assert!(out.value.unwrap_or_default().contains("#t"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");
    // One argument: the Emacs command, on the current buffer.
    let cmds = engine
        .eval(r#"(write-file "/tmp/x.txt")"#)
        .unwrap()
        .commands;
    assert!(
        cmds.iter()
            .any(|c| matches!(c, EditorCommand::WriteFile(p) if p == "/tmp/x.txt")),
        "{cmds:?}"
    );
    // No arguments: prompt for the path.
    let cmds = engine.eval("(write-file)").unwrap().commands;
    assert!(
        cmds.iter()
            .any(|c| matches!(c, EditorCommand::ReadFileName { .. })),
        "{cmds:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
