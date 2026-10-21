//! Buffer and window primitives: names, files, buffer-locals and window
//! layout.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared {
        queue,
        buffers,
        buffer_paths,
        snapshot,
        ..
    } = ctx;
    reg.section(Section::Buffers);
    {
        // Read-side bridge: Scheme can query the open buffers (for pickers).
        let shared = buffers.clone();
        reg.register_fn("buffer-names", move || -> Vec<String> {
            shared.lock().unwrap().clone()
        });
    }
    {
        // `(buffer-file-name-of name)` → that buffer's absolute file path, or ""
        // if the buffer has no file (or doesn't exist). Unlike `buffer-file-name`
        // (active buffer only), this resolves any open buffer by name.
        let shared = buffer_paths.clone();
        reg.register_fn("buffer-file-name-of", move |name: String| -> String {
            shared
                .lock()
                .unwrap()
                .get(&name)
                .cloned()
                .unwrap_or_default()
        });
    }
    register_unary(
        reg,
        queue,
        "switch-to-buffer",
        EditorCommand::SwitchToBufferNamed,
    );
    register_unary(
        reg,
        queue,
        "kill-buffer-named",
        EditorCommand::KillBufferNamed,
    );
    // `(retarget-file-buffers old new)`: after moving `old` to `new` on disk,
    // point every buffer visiting `old` (or a file under it) at the new path.
    register_binary(
        reg,
        queue,
        "retarget-file-buffers",
        EditorCommand::RetargetFileBuffers,
    );
    register_unary(reg, queue, "set-major-mode", EditorCommand::SetMajorMode);
    register_unary(reg, queue, "open-file", EditorCommand::OpenFile);
    register_unary(
        reg,
        queue,
        "open-file-other-window",
        EditorCommand::OpenFileOtherWindow,
    );
    register_unary(reg, queue, "select-window", EditorCommand::SelectWindow);
    // Any real number (an integer like `1` included); clamped when applied.
    register_unary(
        reg,
        queue,
        "split-window-right-ratio",
        |ratio: steel::rvals::SteelVal| {
            EditorCommand::SplitWindowRightRatio(steel_f64(&ratio).unwrap_or(0.5))
        },
    );
    {
        // `(window-list)` → pane ids in cycle order; `(selected-window)` → the
        // focused pane id. Read bridges over the snapshot.
        register_snapshot_read(reg, snapshot, "window-list", |s| s.windows.clone());
        register_snapshot_read(reg, snapshot, "selected-window", |s| s.selected_window);
        {
            // `(get-buffer-window name)` → the pane id displaying buffer `name`,
            // or -1 if it isn't shown in any window (Emacs's `get-buffer-window`).
            let shared = snapshot.clone();
            reg.register_fn("get-buffer-window", move |name: String| -> isize {
                shared
                    .load()
                    .window_buffers
                    .iter()
                    .find(|(_, b)| *b == name)
                    .map_or(-1, |(id, _)| *id as isize)
            });
        }
        // `(visible-lines)` → (first-line last-line) currently on screen.
        register_snapshot_read(reg, snapshot, "visible-lines", |s| {
            let (a, b) = s.visible_lines;
            vec![a, b]
        });
        register_snapshot_read(reg, snapshot, "window-cursor-row", |s| {
            s.cursor_screen_row as i64
        });
    }
    register_binary(reg, queue, "buffer-local-set", |key, value| {
        EditorCommand::BufferLocalSet { key, value }
    });
    {
        // Value of buffer-local `key` on the active buffer, or "".
        let shared = snapshot.clone();
        reg.register_fn("buffer-local-get", move |key: String| -> String {
            let snap = shared.load();
            snap.locals
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        });
    }
}
