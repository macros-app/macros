//! End-to-end tests of remote (`/ssh:host:/path`) buffers: visit, save,
//! external-change refusal, revert and dired, through the local `/bin/sh`
//! stand-in for ssh (`crate::remote::TEST_HOST`).

use gpui::{Entity, TestAppContext};

use super::*;

fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, PathBuf) {
    test_context(cx);
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-remote-e2e-{}-{}",
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
    cx.run_until_parked();
    (editor, dir)
}

/// The remote spelling of local `path`.
fn remote(path: &Path) -> String {
    format!("/ssh:{}:{}", crate::remote::TEST_HOST, path.display())
}

/// Pump the foreground until `done` holds: remote work finishes on its own
/// thread, outside the test executor's control.
fn wait_for(
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
    what: &str,
    done: impl Fn(&Editor) -> bool,
) {
    for _ in 0..500 {
        cx.run_until_parked();
        if editor.read_with(cx, |e, _| done(e)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let status = editor.read_with(cx, |e, _| e.messages.join(" | "));
    panic!("timed out waiting for {what}; messages: {status}");
}

fn append(editor: &Entity<Editor>, cx: &mut TestAppContext, text: &str) {
    editor.update(cx, |e, _| {
        let end = e.active.buffer.len();
        e.edit(end..end, text);
        e.active.modified = true;
    });
}

#[gpui::test]
fn remote_files_visit_save_refuse_clobbering_and_revert(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    let file = dir.join("notes with 'quotes'.txt");
    std::fs::write(&file, "one\n").unwrap();
    let path = remote(&file);
    ed.update(cx, |e, cx| e.find_file(&path, cx));
    wait_for(&ed, cx, "the visit", |e| {
        e.active.path.as_deref() == Some(Path::new(&path))
    });
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.buffer.full_text(), "one\n");
        assert_eq!(e.active.name, "notes with 'quotes'.txt");
        // Local-only machinery leaves it alone.
        assert!(e.active_is_remote());
        assert!(!e.fs.watched.contains(Path::new(&path)));
    });

    // Save: lands on the "host", off the UI thread.
    append(&ed, cx, "two\n");
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    // (Until it reports, the save is still recording what it wrote.)
    wait_for(&ed, cx, "the save", |e| {
        e.messages.last().is_some_and(|m| m.starts_with("Wrote"))
    });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\n");
    ed.read_with(cx, |e, _| assert!(!e.active.modified));

    // Changed on the host meanwhile: the save is refused and put to the user.
    std::fs::write(&file, "theirs, a different length\n").unwrap();
    append(&ed, cx, "three\n");
    ed.update(cx, |e, cx| e.apply_command(EditorCommand::SaveBuffer, cx));
    wait_for(&ed, cx, "the save-over prompt", |e| {
        matches!(
            e.minibuffer.as_ref().map(|m| &m.purpose),
            Some(MinibufferPurpose::Confirm(PendingAction::SaveOverChanged(
                _
            )))
        )
    });
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "theirs, a different length\n"
    );
    ed.read_with(cx, |e, _| assert!(e.active.modified));
    // "Yes" saves over it.
    let name = ed.read_with(cx, |e, _| e.active.name.clone());
    ed.update(cx, |e, cx| {
        e.minibuffer = None;
        e.perform(PendingAction::SaveOverChanged(name), cx)
    });
    wait_for(&ed, cx, "the forced save", |_| {
        std::fs::read_to_string(&file).unwrap() == "one\ntwo\nthree\n"
    });

    // Revert re-reads from the host.
    std::fs::write(&file, "reverted\n").unwrap();
    ed.update(cx, |e, cx| e.revert_buffer(cx));
    wait_for(&ed, cx, "the revert", |e| {
        e.active.buffer.full_text() == "reverted\n"
    });

    // A second visit of another spelling reuses the buffer.
    let other = format!(
        "/ssh:{}:{}/./x/../{}",
        crate::remote::TEST_HOST,
        dir.display(),
        file.file_name().unwrap().to_string_lossy()
    );
    let count = ed.read_with(cx, |e, _| e.buffer_names().len());
    ed.update(cx, |e, cx| e.find_file(&other, cx));
    cx.run_until_parked();
    ed.read_with(cx, |e, _| assert_eq!(e.buffer_names().len(), count));
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui::test]
fn remote_directories_list_and_dired_operates_on_the_host(cx: &mut TestAppContext) {
    let (ed, dir) = setup(cx);
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    let root = remote(&dir);
    let eval = |e: &Editor, src: &str| e.script.eval(src).unwrap().value.unwrap_or_default();
    ed.read_with(cx, |e, _| {
        let listed = eval(e, &format!(r#"(list-directory "{root}/")"#));
        assert!(listed.contains(&format!("{root}/a.txt")), "{listed}");
        assert!(listed.contains(&format!("{root}/sub/")), "{listed}");
        assert_eq!(
            eval(e, &format!(r#"(file-directory? "{root}/sub")"#)),
            "#true"
        );
        assert_eq!(
            eval(e, &format!(r#"(file-exists? "{root}/nope")"#)),
            "#false"
        );
        assert_eq!(eval(e, &format!(r#"(file-remote? "{root}")"#)), "#true");
        assert_eq!(eval(e, r#"(file-remote? "/etc/hosts")"#), "#false");
        // dired's copy / rename run on the host.
        eval(
            e,
            &format!(r#"(dired-copy-file "{root}/a.txt" "{root}/" "b c.txt" (lambda (d) d))"#),
        );
        eval(
            e,
            &format!(r#"(dired-move-file "{root}/b c.txt" "{root}/" "sub" (lambda (d) d))"#),
        );
    });
    assert_eq!(
        std::fs::read_to_string(dir.join("sub/b c.txt")).unwrap(),
        "a"
    );
    assert!(!dir.join("b c.txt").exists());
    // Visiting a remote directory opens it in dired.
    ed.update(cx, |e, cx| e.find_file(&format!("{root}/sub"), cx));
    wait_for(&ed, cx, "dired", |e| e.has_buffer("*dired*"));
    ed.read_with(cx, |e, _| {
        assert_eq!(eval(e, "*dired-dir*"), format!("\"{root}/sub/\""));
    });
    std::fs::remove_dir_all(&dir).ok();
}
