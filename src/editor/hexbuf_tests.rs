//! Hex buffers end to end: a real `Editor` on gpui's headless test platform
//! with the bundled Scheme, so `scheme/hex.scm`'s keymaps are the ones
//! exercised. Opening binary files, typing in both columns under Emacs and
//! evil keys, the generic commands acting byte-wise, search, saving byte for
//! byte, and switching between the hex and text views.

use std::path::PathBuf;

use gpui::{Entity, Keystroke, TestAppContext};

use super::*;

fn temp_dir() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "macros-hex-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An editor (under `mode`: "emacs" or "evil") visiting `name` holding
/// `bytes` through `find_file`, the path C-x C-f and a drop take.
fn setup(
    cx: &mut TestAppContext,
    name: &str,
    bytes: &[u8],
    mode: &str,
) -> (Entity<Editor>, PathBuf) {
    test_context(cx);
    let path = temp_dir().join(name);
    std::fs::write(&path, bytes).unwrap();
    let editor = cx.new(Editor::new);
    let modal = mode != "emacs";
    editor.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        if modal {
            e.run_script_blocking("(evil-mode)", cx);
        }
        e.mark_config_loaded(cx);
        if modal {
            e.evil = EvilState::Normal;
        }
    });
    let p = path.to_string_lossy().into_owned();
    editor.update(cx, |e, cx| e.find_file(&p, cx));
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

/// Feed keys: space-separated tokens. Action keys go through the hex
/// action-key path the way the platform routes them; everything else
/// through `dispatch_keystroke`.
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
                assert!(e.hex_action_key(k, cx), "hex did not take action key {k}");
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

fn submit(editor: &Entity<Editor>, cx: &mut TestAppContext, input: &str) {
    editor.update(cx, |e, cx| {
        let mb = e.minibuffer.as_mut().expect("a minibuffer read");
        mb.input = input.to_string();
        mb.cursor = input.len();
        e.mb_submit(cx);
    });
    settle(editor, cx);
}

fn bytes(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Vec<u8> {
    editor.read_with(cx, |e, _| {
        e.active
            .hex
            .as_ref()
            .expect("a hex buffer")
            .data
            .bytes()
            .to_vec()
    })
}

fn cursor(editor: &Entity<Editor>, cx: &mut TestAppContext) -> usize {
    editor.read_with(cx, |e, _| e.active.hex.as_ref().unwrap().view.cursor)
}

fn modified(editor: &Entity<Editor>, cx: &mut TestAppContext) -> bool {
    editor.read_with(cx, |e, _| e.active.modified)
}

const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03\x00>\x00";

#[gpui::test]
fn a_binary_file_opens_as_hex(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "prog.bin", ELF, "emacs");
    ed.read_with(cx, |e, _| {
        assert_eq!(e.active.name, "prog.bin");
        assert_eq!(e.active.special.as_deref(), Some("hex-mode"));
        assert_eq!(e.active.modes.major(), "hex-mode");
    });
    assert_eq!(bytes(&ed, cx), ELF);
    assert!(!modified(&ed, cx));
}

#[gpui::test]
fn invalid_utf8_without_nuls_opens_as_hex(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "latin1.txt", b"caf\xe9\n", "emacs");
    assert_eq!(bytes(&ed, cx), b"caf\xe9\n");
}

#[gpui::test]
fn a_text_file_still_opens_as_text(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "notes.txt", b"hello\n", "emacs");
    ed.read_with(cx, |e, _| {
        assert!(e.active.hex.is_none());
        assert_eq!(e.active.buffer.full_text(), "hello\n");
    });
}

#[gpui::test]
fn emacs_typing_overwrites_and_saving_writes_the_bytes(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "a.bin", ELF, "emacs");
    keys(&ed, cx, "c a f e");
    assert_eq!(&bytes(&ed, cx)[..2], &[0xca, 0xfe]);
    assert_eq!(cursor(&ed, cx), 2);
    assert!(modified(&ed, cx));
    keys(&ed, cx, "ctrl-x ctrl-s");
    let mut want = ELF.to_vec();
    want[0] = 0xca;
    want[1] = 0xfe;
    assert_eq!(std::fs::read(&path).unwrap(), want);
    assert!(!modified(&ed, cx));
}

#[gpui::test]
fn saving_untouched_reproduces_the_file_exactly(cx: &mut TestAppContext) {
    let all: Vec<u8> = (0..=255u8).collect();
    let (ed, path) = setup(cx, "all.bin", &all, "emacs");
    // Edit, undo back, then save.
    keys(&ed, cx, "1 2 ctrl-/");
    assert!(!modified(&ed, cx));
    ed.update(cx, |e, cx| e.save_buffer(cx));
    settle(&ed, cx);
    assert_eq!(std::fs::read(&path).unwrap(), all);
}

#[gpui::test]
fn generic_motion_keys_move_by_byte_and_row(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "m.bin", &[0u8; 64], "emacs");
    keys(&ed, cx, "ctrl-f ctrl-f");
    assert_eq!(cursor(&ed, cx), 2);
    keys(&ed, cx, "ctrl-n");
    assert_eq!(cursor(&ed, cx), 18);
    keys(&ed, cx, "ctrl-e");
    assert_eq!(cursor(&ed, cx), 31);
    keys(&ed, cx, "ctrl-a ctrl-b");
    assert_eq!(cursor(&ed, cx), 15);
    keys(&ed, cx, "alt-> ");
    assert_eq!(cursor(&ed, cx), 63);
    keys(&ed, cx, "alt-<");
    assert_eq!(cursor(&ed, cx), 0);
    keys(&ed, cx, "right down");
    assert_eq!(cursor(&ed, cx), 17);
}

#[gpui::test]
fn tab_types_text_in_the_ascii_column(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "t.bin", &[0u8; 8], "emacs");
    keys(&ed, cx, "tab H i");
    assert_eq!(&bytes(&ed, cx)[..3], b"Hi\0");
    // Back in the hex column a non-hex letter is a command, not typing.
    keys(&ed, cx, "tab z");
    assert_eq!(&bytes(&ed, cx)[..3], b"Hi\0");
}

#[gpui::test]
fn copy_and_paste_round_trip_as_hex(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "c.bin", b"\x00\x01\x02\x03\x04\x05", "emacs");
    keys(&ed, cx, "ctrl-space ctrl-f ctrl-f alt-w");
    let clip = cx.read_from_clipboard().and_then(|i| i.text());
    assert_eq!(clip.as_deref(), Some("00 01 02"));
    keys(&ed, cx, "alt-> ctrl-y");
    assert_eq!(bytes(&ed, cx), b"\x00\x01\x02\x03\x04\x00\x01\x02");
}

#[gpui::test]
fn search_finds_hex_and_text(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "s.bin", b"\x00\x00PNG\x00\xde\xad\xbe\xef\x00", "emacs");
    keys(&ed, cx, "ctrl-s");
    submit(&ed, cx, "de ad ?? ef");
    assert_eq!(cursor(&ed, cx), 6);
    keys(&ed, cx, "ctrl-s");
    submit(&ed, cx, "\"PNG\"");
    assert_eq!(cursor(&ed, cx), 2);
    ed.read_with(cx, |e, _| {
        assert_eq!(
            e.active.hex.as_ref().unwrap().view.selection(11),
            Some(2..5)
        );
    });
}

#[gpui::test]
fn goto_offset_prompt(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "g.bin", &[0u8; 300], "emacs");
    keys(&ed, cx, "alt-g g");
    submit(&ed, cx, "0x100");
    assert_eq!(cursor(&ed, cx), 256);
    keys(&ed, cx, "alt-g g");
    submit(&ed, cx, "-16");
    assert_eq!(cursor(&ed, cx), 240);
}

#[gpui::test]
fn evil_keys_are_commands_until_i(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "e.bin", &[0u8; 40], "evil");
    // `l` moves; `3l` counts; `a` (a hex digit) does nothing before `i`.
    keys(&ed, cx, "l 3 l j");
    assert_eq!(cursor(&ed, cx), 20);
    keys(&ed, cx, "a");
    assert_eq!(bytes(&ed, cx), vec![0u8; 40]);
    keys(&ed, cx, "i f f escape");
    assert_eq!(bytes(&ed, cx)[20], 0xff);
    // Disarmed again: `b` moves back a group, not types.
    keys(&ed, cx, "b");
    assert_eq!(bytes(&ed, cx)[21], 0x00);
    keys(&ed, cx, "g g");
    assert_eq!(cursor(&ed, cx), 0);
    keys(&ed, cx, "u");
    assert_eq!(bytes(&ed, cx), vec![0u8; 40]);
}

#[gpui::test]
fn text_and_hex_views_switch_in_place(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "v.txt", b"AB\n", "emacs");
    ed.update(cx, |e, cx| {
        e.hex_command(crate::hexview::cmd::HexCmd::HexMode, 1, cx)
    });
    settle(&ed, cx);
    assert_eq!(bytes(&ed, cx), b"AB\n");
    keys(&ed, cx, "4 3");
    keys(&ed, cx, "ctrl-c ctrl-t");
    ed.read_with(cx, |e, _| {
        assert!(e.active.hex.is_none());
        assert_eq!(e.active.buffer.full_text(), "CB\n");
        assert!(e.active.modified);
    });
    keys(&ed, cx, "ctrl-c ctrl-t");
    assert_eq!(bytes(&ed, cx), b"CB\n");
    assert!(modified(&ed, cx));
}

#[gpui::test]
fn non_utf8_bytes_refuse_the_text_view(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "x.bin", b"\xff\xfe", "emacs");
    keys(&ed, cx, "ctrl-c ctrl-t");
    ed.read_with(cx, |e, _| assert!(e.active.hex.is_some()));
}

#[gpui::test]
fn hex_find_file_opens_text_as_hex(cx: &mut TestAppContext) {
    test_context(cx);
    let path = temp_dir().join("plain.txt");
    std::fs::write(&path, "hi").unwrap();
    let editor = cx.new(Editor::new);
    editor.update(cx, |e, cx| {
        for src in crate::runtime::startup_scripts() {
            e.run_script_blocking(src, cx);
        }
        e.mark_config_loaded(cx);
    });
    let p = path.to_string_lossy().into_owned();
    editor.update(cx, |e, cx| {
        e.apply_command(EditorCommand::HexFindFile(p), cx)
    });
    settle(&editor, cx);
    assert_eq!(bytes(&editor, cx), b"hi");
}

#[gpui::test]
fn scheme_reads_the_hex_state(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "r.bin", b"\x00\x41\x42", "emacs");
    keys(&ed, cx, "ctrl-f");
    ed.update(cx, |e, _| e.publish_point());
    let eval = |ed: &Entity<Editor>, cx: &mut TestAppContext, src: &str| {
        ed.read_with(cx, |e, _| {
            e.script.eval(src).expect("eval").value.unwrap_or_default()
        })
    };
    assert_eq!(eval(&ed, cx, "(hex-buffer?)"), "#true");
    assert_eq!(eval(&ed, cx, "(hex-offset)"), "1");
    assert_eq!(eval(&ed, cx, "(hex-size)"), "3");
    assert_eq!(eval(&ed, cx, "(hex-byte)"), "65");
}

/// Type into the open minibuffer the way the IME delivers text.
fn type_into_minibuffer(editor: &Entity<Editor>, cx: &mut TestAppContext, text: &str) {
    editor.update(cx, |e, cx| {
        assert!(e.minibuffer.is_some(), "no minibuffer open");
        e.type_text(None, text, cx);
    });
    settle(editor, cx);
}

fn minibuffer_input(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Option<String> {
    editor.read_with(cx, |e, _| e.minibuffer.as_ref().map(|m| m.input.clone()))
}

#[gpui::test]
fn evil_ex_w_saves_a_hex_buffer(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "w.bin", ELF, "evil");
    keys(&ed, cx, "i 0 0 escape");
    assert!(modified(&ed, cx));
    keys(&ed, cx, ":");
    assert_eq!(
        minibuffer_input(&ed, cx).as_deref(),
        Some(""),
        "`:` opens the ex prompt"
    );
    type_into_minibuffer(&ed, cx, "w");
    assert_eq!(minibuffer_input(&ed, cx).as_deref(), Some("w"));
    ed.update(cx, |e, cx| e.mb_submit(cx));
    settle(&ed, cx);
    let mut want = ELF.to_vec();
    want[0] = 0x00;
    assert_eq!(std::fs::read(&path).unwrap(), want);
    assert!(!modified(&ed, cx));
    // The saved state sticks: moving doesn't bring `modified` back.
    keys(&ed, cx, "l");
    assert!(!modified(&ed, cx));
}

#[gpui::test]
fn evil_ex_wq_writes_the_bytes_synchronously(cx: &mut TestAppContext) {
    let (ed, path) = setup(cx, "wq.bin", ELF, "evil");
    keys(&ed, cx, "i 1 1 escape");
    // `:wq`'s write half (the quit half would end the test app).
    ed.update(cx, |e, cx| assert!(e.save_active_sync(true, cx)));
    settle(&ed, cx);
    assert_eq!(std::fs::read(&path).unwrap()[0], 0x11);
    assert!(!modified(&ed, cx));
    keys(&ed, cx, "l");
    assert!(!modified(&ed, cx));
}

#[gpui::test]
fn the_kill_prompt_takes_typed_answers(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "k.bin", ELF, "emacs");
    keys(&ed, cx, "f f");
    keys(&ed, cx, "ctrl-x k");
    // `C-x k` may first ask which buffer: take the default.
    if minibuffer_input(&ed, cx).is_some_and(|i| i.is_empty())
        && ed.read_with(cx, |e, _| {
            !matches!(
                e.minibuffer.as_ref().map(|m| &m.purpose),
                Some(MinibufferPurpose::Confirm(_))
            )
        })
    {
        ed.update(cx, |e, cx| e.mb_submit(cx));
        settle(&ed, cx);
    }
    ed.read_with(cx, |e, _| {
        assert!(matches!(
            e.minibuffer.as_ref().map(|m| &m.purpose),
            Some(MinibufferPurpose::Confirm(_))
        ));
    });
    type_into_minibuffer(&ed, cx, "yes");
    assert_eq!(minibuffer_input(&ed, cx).as_deref(), Some("yes"));
    ed.update(cx, |e, cx| e.mb_submit(cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        assert!(e.minibuffer.is_none());
        assert!(e.doc("k.bin").is_none(), "the buffer was killed");
    });
}

#[gpui::test]
fn search_prompt_takes_typed_text(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "st.bin", b"\x00\x00\xab\xcd", "emacs");
    keys(&ed, cx, "ctrl-s");
    type_into_minibuffer(&ed, cx, "ab cd");
    ed.update(cx, |e, cx| e.mb_submit(cx));
    settle(&ed, cx);
    assert_eq!(cursor(&ed, cx), 2);
}

#[gpui::test]
fn evil_leader_and_global_chords_reach_their_layers(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "l.bin", ELF, "evil");
    // A leader binding fires from a hex buffer (SPC isn't swallowed).
    ed.update(cx, |e, cx| {
        e.run_script_blocking("(evil-leader-set \"x x\" 'hex-toggle-endian)", cx)
    });
    let big = |ed: &Entity<Editor>, cx: &mut TestAppContext| {
        ed.read_with(cx, |e, _| e.active.hex.as_ref().unwrap().view.big_endian)
    };
    assert!(!big(&ed, cx));
    keys(&ed, cx, "space x x");
    assert!(big(&ed, cx), "SPC x x ran its leader command");
    // M-x opens the command picker; C-w is the window prefix.
    keys(&ed, cx, "alt-x");
    assert!(
        ed.read_with(cx, |e, _| e.helm.is_some()),
        "M-x opens the picker"
    );
    keys(&ed, cx, "ctrl-g");
    keys(&ed, cx, "ctrl-w v");
    ed.read_with(cx, |e, _| assert_eq!(e.panes.len(), 2, "C-w v splits"));
}

#[gpui::test]
fn emacs_global_chords_still_work(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "eg.bin", ELF, "emacs");
    keys(&ed, cx, "alt-x");
    assert!(
        ed.read_with(cx, |e, _| e.helm.is_some()),
        "M-x opens the picker"
    );
    keys(&ed, cx, "ctrl-g");
    keys(&ed, cx, "ctrl-x 2");
    ed.read_with(cx, |e, _| assert_eq!(e.panes.len(), 2, "C-x 2 splits"));
}

#[gpui::test]
fn escape_from_evil_insert_reaches_evil(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "ins.bin", ELF, "evil");
    // Insert state carried over from a text buffer.
    ed.update(cx, |e, _| e.evil = EvilState::Insert);
    keys(&ed, cx, "escape");
    ed.read_with(cx, |e, _| assert!(e.evil == EvilState::Normal));
}

#[gpui::test]
fn armed_typing_never_runs_normal_commands(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "arm.bin", ELF, "evil");
    keys(&ed, cx, "i x p u");
    // `x` / `p` / `u` are refused as non-hex digits, not delete/paste/undo.
    assert_eq!(bytes(&ed, cx), ELF);
    assert_eq!(cursor(&ed, cx), 0);
    keys(&ed, cx, "escape x");
    // Disarmed, `x` deletes the byte at point again.
    assert_eq!(bytes(&ed, cx), &ELF[1..]);
}

#[gpui::test]
fn a_prefix_before_a_typed_digit_does_not_leak(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "pre.bin", &[0u8; 16], "emacs");
    keys(&ed, cx, "ctrl-u a ctrl-f");
    assert_eq!(bytes(&ed, cx)[0], 0xa0);
    // Low nibble pending, then one C-f: a single byte forward.
    assert_eq!(cursor(&ed, cx), 1);
}

#[gpui::test]
fn insert_mode_typing_undoes_in_one_step(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "insu.bin", &[0u8], "emacs");
    keys(&ed, cx, "insert 1 2 3 4");
    assert_eq!(bytes(&ed, cx), vec![0x12, 0x34, 0]);
    keys(&ed, cx, "ctrl-/");
    assert_eq!(bytes(&ed, cx), vec![0]);
}

#[gpui::test]
fn sessions_restore_binary_files_as_hex(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "front.txt", b"text\n", "emacs");
    let bin = temp_dir().join("restored.bin");
    std::fs::write(&bin, ELF).unwrap();
    let entries = vec![(bin.to_string_lossy().into_owned(), 5)];
    ed.update(cx, |e, cx| e.restore_file_buffers(entries, cx));
    settle(&ed, cx);
    ed.read_with(cx, |e, _| {
        let doc = e.doc("restored.bin").expect("restored");
        let h = doc.hex.as_ref().expect("as hex");
        assert_eq!(h.data.bytes(), ELF);
        assert_eq!(h.view.cursor, 5);
    });
}

#[gpui::test]
fn write_file_copies_a_hex_buffer(cx: &mut TestAppContext) {
    let (ed, _) = setup(cx, "src.bin", ELF, "emacs");
    let copy = temp_dir().join("copy.bin");
    let c = copy.to_string_lossy().into_owned();
    ed.update(cx, |e, cx| e.write_file(&c, false, cx));
    settle(&ed, cx);
    assert_eq!(std::fs::read(&copy).unwrap(), ELF);
}
