//! Read bridges over the published snapshot of the current buffer: point,
//! text, lines and regions.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { snapshot, .. } = ctx;
    reg.section(Section::BufferReads);
    // Read bridges over the editor's published snapshot (see `set_snapshot`).
    register_snapshot_read(reg, snapshot, "buffer-line", |s| s.line);
    register_snapshot_read(reg, snapshot, "current-buffer", |s| s.name.clone());
    // The active buffer's directory — its visited file's parent, or the cwd
    // for fileless buffers (Emacs's buffer-local `default-directory`).
    register_snapshot_read(reg, snapshot, "buffer-directory", |s| s.directory.clone());
    // The visited file's full path, or "" for a buffer with no file (Emacs's
    // `buffer-file-name`).
    register_snapshot_read(reg, snapshot, "buffer-file-name", |s| s.path.clone());
    // Point as a byte offset (Emacs `(point)`).
    register_snapshot_read(reg, snapshot, "point", |s| s.point);
    {
        // The whole buffer (Emacs `(buffer-string)`). Clone the cheap rope handle
        // under the lock, then materialize the String outside it so the snapshot
        // lock isn't held for a full-buffer copy.
        let shared = snapshot.clone();
        reg.register_fn("buffer-string", move || -> String {
            let rope = shared.load().text.clone();
            rope.to_string()
        });
    }
    // End of the accessible portion as a byte offset (Emacs `(point-max)`): the
    // buffer's end, or the narrowing's end while narrowed. O(1) on the rope, no
    // text materialized.
    register_snapshot_read(reg, snapshot, "point-max", |s| {
        s.narrow.map_or(s.text.len_bytes(), |(_, e)| e)
    });
    // Start of the accessible portion (Emacs `(point-min)`): 0, or the
    // narrowing's start while narrowed. Offsets stay absolute either way.
    register_snapshot_read(reg, snapshot, "point-min", |s| {
        s.narrow.map_or(0, |(b, _)| b)
    });
    // The whole buffer's length in bytes, narrowing or not (Emacs
    // `(buffer-size)`) — the end for a whole-buffer `replace-region`.
    register_snapshot_read(reg, snapshot, "buffer-size", |s| s.text.len_bytes());
    // Whether the buffer is narrowed (Emacs `buffer-narrowed-p`).
    register_snapshot_read(reg, snapshot, "buffer-narrowed?", |s| s.narrow.is_some());
    {
        // `(position->byte line character)` → byte offset of an LSP position
        // (0-based line, UTF-16 column) in the active buffer. Lets Scheme LSP
        // handlers turn server line/col coordinates into the byte offsets the
        // overlay/fold/goto primitives expect.
        let shared = snapshot.clone();
        reg.register_fn(
            "position->byte",
            move |NonNeg(line): NonNeg, NonNeg(character): NonNeg| -> usize {
                let rope = shared.load().text.clone();
                // A line past the end clamps to the end of the buffer.
                if line >= rope.len_lines() {
                    return rope.len_bytes();
                }
                // Jump straight to the line (O(log n)), then walk only its
                // chars to resolve the UTF-16 column: stop at the column, at
                // the line's newline, or (last line) at end of buffer.
                let mut byte = rope.line_to_byte(line);
                let mut col = 0usize;
                for c in rope.line(line).chars() {
                    if col >= character || c == '\n' {
                        return byte;
                    }
                    col += c.len_utf16();
                    byte += c.len_utf8();
                }
                byte
            },
        );
    }
    // `(lsp-server-ready?)` → #t if the active buffer has a ready LSP server.
    register_snapshot_read(reg, snapshot, "lsp-server-ready?", |s| s.lsp_ready);
    {
        // `(line-bounds)` → `(start end)` byte offsets of the line at point
        // (end points at the line's terminating newline, or buffer end).
        let shared = snapshot.clone();
        reg.register_fn("line-bounds", move || -> Vec<usize> {
            let (rope, p) = {
                let snap = shared.load();
                let p = snap.point.min(snap.text.len_bytes());
                (snap.text.clone(), p)
            };
            let line = rope.byte_to_line(p);
            let start = rope.line_to_byte(line);
            let line_str = rope.line(line).to_string();
            let end = start + line_str.find('\n').unwrap_or(line_str.len());
            vec![start, end]
        });
    }
    // `(line-count)` → total buffer lines. O(1) on the rope. Lets Scheme fold
    // logic walk lines by index without materializing the whole buffer.
    register_snapshot_read(reg, snapshot, "line-count", |s| s.text.len_lines());
    {
        // `(line-indentation line)` → leading-whitespace width (spaces, tabs count
        // as one column each) of buffer `line`, or -1 for a blank/whitespace-only
        // line so fold logic can skip blanks. The indentation-based folder builds
        // fold ranges from this.
        let shared = snapshot.clone();
        reg.register_fn("line-indentation", move |NonNeg(line): NonNeg| -> isize {
            let rope = shared.load().text.clone();
            if line >= rope.len_lines() {
                return -1;
            }
            let mut indent = 0;
            for c in rope.line(line).chars() {
                match c {
                    ' ' | '\t' => indent += 1,
                    // End of line (or a blank/whitespace-only line) before any
                    // content: report -1 so fold logic skips it.
                    '\n' | '\r' => return -1,
                    _ => return indent,
                }
            }
            -1
        });
    }
    {
        // Bytes `[a, b)` of the buffer, clamped (Emacs `(buffer-substring a b)`).
        let shared = snapshot.clone();
        reg.register_fn("buffer-substring", move |a: isize, b: isize| -> String {
            let (a, b) = (scheme_pos(a), scheme_pos(b));
            let rope = shared.load().text.clone();
            // Snap to char boundaries so we never slice mid-codepoint.
            let (start, end) = (snap_byte(&rope, a), snap_byte(&rope, b));
            if start >= end {
                return String::new();
            }
            rope.byte_slice(start..end).to_string()
        });
    }
    {
        // The active region's byte range, or (-1, -1) when no region is active.
        register_snapshot_read(reg, snapshot, "region-beginning", |s| {
            s.region.map_or(-1, |(a, _)| a as isize)
        });
        register_snapshot_read(reg, snapshot, "region-end", |s| {
            s.region.map_or(-1, |(_, b)| b as isize)
        });
        register_snapshot_read(reg, snapshot, "region-active?", |s| s.region.is_some());
        // `(selections)` → a flat list `(a0 h0 a1 h1 …)` of every selection's
        // anchor/head byte offsets, primary first (Helix multiple selections).
        // Flat (not pairs) so it round-trips straight into `(set-selections …)`.
        register_snapshot_read(reg, snapshot, "selections", |s| {
            s.selections
                .iter()
                .flat_map(|&(a, h)| [a, h])
                .collect::<Vec<usize>>()
        });
        // The selected text, or "" when no region is active (Emacs has no single
        // primitive for this; it's `(buffer-substring (region-beginning) …)`).
        let shared = snapshot.clone();
        reg.register_fn("region-text", move || -> String {
            let (rope, region) = {
                let snap = shared.load();
                (snap.text.clone(), snap.region)
            };
            match region {
                Some((a, b)) if b <= rope.len_bytes() && a < b => rope
                    .byte_slice(snap_byte(&rope, a)..snap_byte(&rope, b))
                    .to_string(),
                _ => String::new(),
            }
        });
    }
    {
        // Byte offset of the next occurrence of `needle` at/after point, or -1.
        let shared = snapshot.clone();
        reg.register_fn("search-forward", move |needle: String| -> isize {
            let (rope, p, narrow) = {
                let snap = shared.load();
                (snap.text.clone(), snap.point, snap.narrow)
            };
            // Only up to `point-max`: a narrowed buffer's hidden tail is out
            // of reach.
            let end = narrow.map_or(rope.len_bytes(), |(_, e)| snap_byte(&rope, e));
            let from = snap_byte(&rope, p).min(end);
            let tail = rope.byte_slice(from..end).to_string();
            match tail.find(&needle) {
                Some(off) => (from + off) as isize,
                None => -1,
            }
        });
    }
}
