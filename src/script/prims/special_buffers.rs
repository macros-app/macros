//! Special (read-only, program-generated) buffers, the magit primitives:
//! rendering, copying and highlighting lines across buffers, and transients.

use steel::rvals::SteelVal;

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { queue, .. } = ctx;
    reg.section(Section::SpecialBuffers);
    // --- Special buffers / transient (the magit primitives) ---------------
    // `(render-buffer name mode text faces)` — faces is a list of
    // `(line-number face-name)`. Replaces/creates a read-only buffer. The
    // `-quiet` variant refreshes in place without switching/focusing (sidebar
    // auto-refresh).
    register_render_buffer(reg, queue, "render-buffer", false);
    register_render_buffer(reg, queue, "render-buffer-quiet", true);
    register_unary(reg, queue, "goto-line", |n: isize| {
        EditorCommand::GotoLine(scheme_pos(n))
    });
    register_unary(
        reg,
        queue,
        "scroll-cursor-to-row",
        EditorCommand::ScrollCursorToRow,
    );
    register_ternary(reg, queue, "diff-buffers", |a, b, callback: Callback| {
        EditorCommand::DiffBuffers {
            a,
            b,
            callback: callback.0,
        }
    });
    register_unary(reg, queue, "buffer-clear-highlights", |name| {
        EditorCommand::BufferClearHighlights { name }
    });
    {
        // `(buffer-copy-lines src ss se dst ds de)` — copy a line range between
        // named buffers (ediff hunk copy). Six args, so registered directly.
        let q = queue.clone();
        reg.register_fn(
            "buffer-copy-lines",
            move |src: String,
                  NonNeg(ss): NonNeg,
                  NonNeg(se): NonNeg,
                  dst: String,
                  NonNeg(ds): NonNeg,
                  NonNeg(de): NonNeg| {
                q.push(EditorCommand::BufferCopyLines {
                    src,
                    src_start: ss,
                    src_end: se,
                    dst,
                    dst_start: ds,
                    dst_end: de,
                });
            },
        );
    }
    {
        // `(buffer-highlight-lines name start end face)` — paint a face on a line
        // range of any named buffer (works on the non-selected ediff pane too).
        let q = queue.clone();
        reg.register_fn(
            "buffer-highlight-lines",
            move |name: String, NonNeg(start): NonNeg, NonNeg(end): NonNeg, face: String| {
                q.push(EditorCommand::BufferHighlightLines {
                    name,
                    start,
                    end,
                    face,
                });
            },
        );
    }
    // `(transient title rows)` — `rows` is a list of `(key label command)`, or
    // a list of `(column-title (key label command) …)` for columns. An empty
    // `title` ("") shows no header row, just the columns. In the
    // `-sticky` variant selecting a row keeps the popup open — the row's
    // command re-opens it (to refresh state) or calls `transient-quit`, for
    // menus where you toggle several options before dismissing.
    register_transient(reg, queue, "transient", false);
    register_transient(reg, queue, "transient-sticky", true);
    register_nullary(reg, queue, "transient-quit", EditorCommand::TransientQuit);
    // `(set-special-mode "occur-mode")` makes the active buffer a read-only
    // special buffer on that keymap; `(set-special-mode "")` makes it editable.
    register_unary(
        reg,
        queue,
        "set-special-mode",
        EditorCommand::SetSpecialMode,
    );
}

/// Register one of the `render-buffer` / `render-buffer-quiet` pair — same
/// signature, differing only in whether the rendered buffer is switched to.
pub(crate) fn register_render_buffer(
    reg: &mut Registrar,
    queue: &Queue,
    fn_name: &'static str,
    quiet: bool,
) {
    let q = queue.clone();
    reg.register_fn(
        fn_name,
        move |name: String, mode: String, text: String, faces: SteelVal| {
            let line_faces = parse_line_faces(&faces);
            q.push(EditorCommand::RenderBuffer {
                name,
                mode,
                text,
                line_faces,
                quiet,
            });
        },
    );
}

/// Register one of the `transient` / `transient-sticky` pair, differing only
/// in whether selecting a row keeps the popup open.
pub(crate) fn register_transient(
    reg: &mut Registrar,
    queue: &Queue,
    name: &'static str,
    sticky: bool,
) {
    let q = queue.clone();
    reg.register_fn(name, move |title: String, rows: SteelVal| {
        q.push(EditorCommand::Transient {
            title,
            columns: parse_transient_columns(&rows),
            sticky,
        });
    });
}
