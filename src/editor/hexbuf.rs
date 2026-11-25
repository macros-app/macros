//! Hex buffers for [`Editor`]: a binary file shown and edited as bytes.
//!
//! A hex buffer is a special [`Document`] (`hex-mode`) whose rope stays empty;
//! the bytes and view live in [`HexDoc`] on the document, so the buffer list,
//! perspectives, `kill-buffer` and sessions treat it like any file buffer.
//! `HexElement` (`hexbuf_element.rs`) paints it.
//!
//! Keys: `scheme/hex.scm` binds the [`HexCmd`]s in the `hex-mode` keymap (and
//! the `evil-normal:hex-mode` / `helix-normal:hex-mode` overlays). Typing is
//! native: in the hex column hex digits write nibbles, in the ASCII column
//! printable keys write characters. Under evil/helix typing is armed with
//! `i` / `R` and disarmed with Escape, so plain keys stay commands otherwise.
//! The generic motion and editing commands (`C-f`, `C-n`, `C-v`, `M-<`,
//! `C-SPC`, `C-/`, `M-w`, `C-y`, …) mean their byte-wise twins here, so the
//! usual bindings work without a hex-specific map.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{Context, Keystroke, MouseButton, Pixels, Point, SharedString};

use super::input::Handled;
use super::*;
use crate::hexview::cmd::{HexCmd, HexOp};
use crate::hexview::view::{GROUPS, WIDTHS};
use crate::hexview::{self, CopyFormat, HexData, HexView};

/// A hex buffer's state (on its [`Document`]).
pub(super) struct HexDoc {
    pub data: HexData,
    pub view: HexView,
    /// Fingerprint of the file as last read or written, so auto-revert can
    /// tell our own save from a real change.
    pub disk_hash: u64,
}

impl HexDoc {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            disk_hash: crate::grid::bytes_hash(&bytes),
            data: HexData::new(bytes),
            view: HexView::default(),
        }
    }

    /// The bytes were just written to the file.
    pub fn saved(&mut self) {
        self.data.mark_saved();
        self.disk_hash = crate::grid::bytes_hash(self.data.bytes());
    }

    pub fn snapshot(&self) -> crate::script::HexSnapshot {
        crate::script::HexSnapshot {
            offset: self.view.cursor,
            size: self.data.len(),
            selection: self
                .view
                .selection(self.data.len())
                .map(|r| (r.start, r.end)),
            byte: self.data.get(self.view.cursor),
        }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    /// Move point to `at` (the selection, if any, extends to it).
    fn goto(&mut self, at: usize) {
        let len = self.len();
        self.view.set_cursor(at, len);
    }

    fn selection(&self) -> Option<std::ops::Range<usize>> {
        self.view.selection(self.len())
    }
}

/// Where a separator space goes in the hex column: after every group, or
/// every 8 bytes when bytes aren't grouped (the classic hexdump split).
pub(super) fn sep_every(group: usize) -> usize {
    if group > 1 { group } else { 8 }
}

/// The character column (within the hex column) byte `i` of a row starts at.
pub(super) fn hex_char_x(i: usize, group: usize) -> usize {
    i * 3 + i / sep_every(group)
}

/// Characters the hex column of a `width`-byte row takes.
pub(super) fn hex_chars(width: usize, group: usize) -> usize {
    if width == 0 {
        0
    } else {
        hex_char_x(width - 1, group) + 2
    }
}

/// Hex digits in the offset column for a buffer of `len` bytes (at least 8).
pub(super) fn offset_digits(len: usize) -> usize {
    let mut digits = 8;
    while digits < 16 && (len as u128) >= 1u128 << (digits * 4) {
        digits += 2;
    }
    digits
}

/// A hex pane's painted geometry (set by its element each frame), for mouse
/// hits. Lengths are screen px; `origin` is the pane's top-left.
#[derive(Clone, Copy, Debug)]
pub(super) struct HexGeom {
    pub origin: Point<Pixels>,
    pub char_w: f32,
    pub line_h: f32,
    /// Top of the first byte row, from `origin`.
    pub rows_y: f32,
    pub rows: usize,
    /// Left edge of the hex column, from `origin`.
    pub hex_x: f32,
    /// Left edge of the ASCII column, from `origin`.
    pub ascii_x: f32,
    pub width: usize,
    pub group: usize,
    pub top: usize,
}

/// Where a point in a hex pane lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HexHit {
    pub offset: usize,
    pub ascii: bool,
    pub low_nibble: bool,
}

impl HexGeom {
    /// The byte under `pos` (clamped to the columns), or `None` outside the
    /// rows on screen or left of the hex column.
    pub fn hit(&self, pos: Point<Pixels>, len: usize) -> Option<HexHit> {
        let x = f32::from(pos.x) - f32::from(self.origin.x);
        let y = f32::from(pos.y) - f32::from(self.origin.y);
        if y < self.rows_y || x < self.hex_x - self.char_w || self.char_w <= 0.0 {
            return None;
        }
        let shown = ((y - self.rows_y) / self.line_h.max(1.0)) as usize;
        if shown >= self.rows {
            return None; // the footer
        }
        let row = self.top + shown;
        let ascii = x >= self.ascii_x - self.char_w;
        let (col, low) = if ascii {
            let c = ((x - self.ascii_x) / self.char_w).max(0.0) as usize;
            (c.min(self.width - 1), false)
        } else {
            let cx = ((x - self.hex_x) / self.char_w).max(0.0) as usize;
            // The last byte whose cell starts at or before this column.
            let i = (0..self.width)
                .rev()
                .find(|&i| hex_char_x(i, self.group) <= cx)
                .unwrap_or(0);
            (i, cx > hex_char_x(i, self.group))
        };
        let offset = (row * self.width + col).min(len);
        Some(HexHit {
            offset,
            ascii,
            low_nibble: low && offset < len,
        })
    }
}

/// Editor-wide hex state.
#[derive(Default)]
pub(super) struct HexUi {
    pub geom: HashMap<usize, HexGeom>,
    /// A mouse selection in progress: the pane and the byte it started on.
    pub drag: Option<(usize, usize)>,
    /// Fractional wheel rows not yet scrolled.
    pub wheel: f32,
}

/// What a hex command asks the editor to do once it's applied.
#[derive(Default, Debug)]
pub(super) struct Outcome {
    pub message: Option<String>,
    /// Text for the clipboard.
    pub copy: Option<String>,
    /// Switch the buffer to its text view.
    pub view_as_text: bool,
    /// Open the search prompt (a search-next with no search yet).
    pub prompt_search: bool,
}

impl Outcome {
    fn say(msg: impl Into<String>) -> Self {
        Self {
            message: Some(msg.into()),
            ..Default::default()
        }
    }
}

/// Apply `cmd` (`count` times where that means something) to `h`.
/// `clipboard` is the clipboard's text, read beforehand for a paste.
pub(super) fn apply(h: &mut HexDoc, cmd: HexCmd, count: usize, clipboard: Option<&str>) -> Outcome {
    use HexCmd::*;
    let n = count.max(1);
    let len = h.len();
    let w = h.view.row_width();
    let g = h.view.group.max(1);
    let cur = h.view.cursor;
    let mut out = Outcome::default();
    match cmd {
        Left => h.goto(cur.saturating_sub(n)),
        Right => h.goto(cur.saturating_add(n)),
        Up => h.goto(cur.saturating_sub(n.saturating_mul(w))),
        Down => {
            let to = cur.saturating_add(n.saturating_mul(w));
            // From the last row, ↓ stops at the end rather than not moving.
            h.goto(if to > len { len } else { to })
        }
        NextGroup => h.goto((cur / g).saturating_add(n).saturating_mul(g)),
        PreviousGroup => {
            let mut c = cur;
            for _ in 0..n {
                c = if c.is_multiple_of(g) {
                    c.saturating_sub(g)
                } else {
                    c - c % g
                };
            }
            h.goto(c)
        }
        RowStart => h.goto(cur - cur % w),
        RowEnd => h.goto((cur - cur % w + w - 1).min(len)),
        PageDown | PageUp => {
            let rows = h.view.rows_visible.saturating_sub(1).max(1) * n;
            let delta = rows.saturating_mul(w);
            let to = if cmd == PageDown {
                cur.saturating_add(delta).min(len)
            } else {
                cur.saturating_sub(delta)
            };
            h.view.top = if cmd == PageDown {
                h.view.top.saturating_add(rows)
            } else {
                h.view.top.saturating_sub(rows)
            };
            h.view.clamp_top(len);
            h.goto(to);
        }
        BufferStart => h.goto(0),
        BufferEnd => h.goto(len.saturating_sub(1)),
        TogglePane => {
            h.view.ascii = !h.view.ascii;
            h.view.low_nibble = false;
        }
        SetMark => {
            h.view.mark = Some(cur);
            out = Outcome::say("Mark set");
        }
        Cancel => {
            h.view.mark = None;
            h.view.low_nibble = false;
            h.view.editing = false;
        }
        SelectAll => {
            if len > 0 {
                h.view.mark = Some(0);
                h.goto(len - 1);
            }
        }
        EditStart => {
            h.view.editing = true;
            out = Outcome::say(if h.view.insert {
                "-- HEX INSERT -- (Esc to stop)"
            } else {
                "-- HEX OVERWRITE -- (Esc to stop)"
            });
        }
        EditEnd => {
            h.view.editing = false;
            h.view.low_nibble = false;
        }
        ToggleInsert => {
            h.view.insert = !h.view.insert;
            h.view.low_nibble = false;
            out = Outcome::say(if h.view.insert {
                "Typing inserts bytes"
            } else {
                "Typing overwrites bytes"
            });
        }
        Delete | DeleteBackward => {
            if let Some(sel) = h.selection() {
                h.data.splice(sel.start, sel.len(), &[], cur);
                h.view.mark = None;
                h.goto(sel.start);
            } else if cmd == Delete {
                let k = n.min(len.saturating_sub(cur));
                if k > 0 {
                    h.data.splice(cur, k, &[], cur);
                    h.goto(cur);
                }
            } else if h.view.insert {
                let k = n.min(cur);
                if k > 0 {
                    h.data.splice(cur - k, k, &[], cur);
                    h.goto(cur - k);
                }
            } else {
                // Overwriting never changes the size: backspace steps back.
                h.goto(cur.saturating_sub(n));
            }
        }
        InsertNull => {
            h.data.splice(cur, 0, &vec![0; n], cur);
            h.goto(cur);
        }
        Undo => match h.data.undo() {
            Some(c) => {
                h.view.mark = None;
                h.goto(c);
                if !h.data.modified() {
                    out = Outcome::say("Undo (back to the saved file)");
                }
            }
            None => out = Outcome::say("No further undo information"),
        },
        Redo => match h.data.redo() {
            Some(c) => {
                h.view.mark = None;
                h.goto(c);
            }
            None => out = Outcome::say("No further redo information"),
        },
        Paste => {
            let Some(text) = clipboard.filter(|t| !t.is_empty()) else {
                return Outcome::say("The clipboard is empty");
            };
            let bytes = hexview::paste_bytes(text);
            let (at, remove) = match h.selection() {
                Some(sel) => (sel.start, sel.len()),
                None if h.view.insert => (cur, 0),
                None => (cur, bytes.len()),
            };
            h.data.splice(at, remove, &bytes, cur);
            h.view.mark = None;
            h.goto(at + bytes.len());
            out = Outcome::say(format!("Pasted {} bytes", bytes.len()));
        }
        Copy(fmt) => {
            let range = h.selection().unwrap_or(cur..(cur + 1).min(len));
            if range.is_empty() {
                return Outcome::say("Nothing to copy");
            }
            let text = fmt.render(h.data.slice(range.clone()));
            h.view.mark = None;
            out.message = Some(format!("Copied {} bytes as {}", range.len(), fmt.label()));
            out.copy = Some(text);
        }
        Cut => {
            let Some(sel) = h.selection() else {
                return Outcome::say("Select bytes to cut (C-SPC, then move)");
            };
            let text = CopyFormat::Hex.render(h.data.slice(sel.clone()));
            h.data.splice(sel.start, sel.len(), &[], cur);
            h.view.mark = None;
            h.goto(sel.start);
            out.message = Some(format!("Cut {} bytes", sel.len()));
            out.copy = Some(text);
        }
        SearchNext | SearchPrevious => {
            let Some((pat, label)) = h.view.search.clone() else {
                return Outcome {
                    prompt_search: true,
                    ..Default::default()
                };
            };
            let forward = cmd == SearchNext;
            out = search(
                h,
                &pat,
                &label,
                if forward { cur + 1 } else { cur },
                forward,
            );
        }
        ToggleInspector => h.view.inspector = !h.view.inspector,
        ToggleEndian => {
            h.view.big_endian = !h.view.big_endian;
            out = Outcome::say(if h.view.big_endian {
                "Inspector reads big-endian"
            } else {
                "Inspector reads little-endian"
            });
        }
        WiderRows | NarrowerRows => {
            h.view.width = HexView::cycle(&WIDTHS, h.view.width, cmd == WiderRows);
            h.view.scroll_to_cursor(len);
            out = Outcome::say(match h.view.width {
                0 => "Rows fit the window".to_string(),
                n => format!("{n} bytes per row"),
            });
        }
        CycleGroup => {
            h.view.group = HexView::cycle(&GROUPS, h.view.group, true);
            // In fit mode the row width follows the group size.
            h.view.scroll_to_cursor(len);
            out = Outcome::say(format!("Groups of {} bytes", h.view.group));
        }
        Recenter => {
            let row = cur / w;
            h.view.top = row.saturating_sub(h.view.rows_visible / 2);
            h.view.clamp_top(len);
        }
        ViewAsText => out.view_as_text = true,
        // Handled by the editor (it replaces the document).
        HexMode => {}
    }
    out
}

/// Find `pat` from `from` and select the match.
fn search(
    h: &mut HexDoc,
    pat: &hexview::Pattern,
    label: &str,
    from: usize,
    forward: bool,
) -> Outcome {
    match hexview::find(h.data.bytes(), pat, from, forward) {
        Some((at, wrapped)) => {
            h.goto(at);
            h.view.mark = Some(at + pat.len() - 1);
            if wrapped {
                Outcome::say(format!("Wrapped: {label} at 0x{at:x}"))
            } else {
                Outcome::say(format!("{label} at 0x{at:x}"))
            }
        }
        None => Outcome::say(format!("Not found: {label}")),
    }
}

/// Type `text` at point: hex digits in the hex column, characters in the
/// ASCII column. Returns a message when something was refused.
pub(super) fn type_text(h: &mut HexDoc, text: &str) -> Option<String> {
    // Typing over a selection starts at its first byte.
    if let Some(sel) = h.view.selection(h.len()) {
        h.goto(sel.start);
    }
    h.view.mark = None;
    if h.view.ascii {
        for b in text.bytes() {
            let at = h.view.cursor;
            h.data.type_byte(at, b, h.view.insert, at);
            h.goto(at + 1);
        }
        return None;
    }
    for c in text.chars() {
        let Some(d) = c.to_digit(16) else {
            return Some(format!(
                "{c} isn't a hex digit (Tab types in the text column)"
            ));
        };
        let d = d as u8;
        let at = h.view.cursor;
        let low = h.view.low_nibble;
        if h.view.insert && !low {
            // A new byte, high nibble first; the low nibble then overwrites
            // it within the same undo step.
            h.data.type_byte(at, d << 4, true, at);
        } else {
            let old = h.data.get(at).unwrap_or(0);
            let new = if low {
                (old & 0xf0) | d
            } else {
                (d << 4) | (old & 0x0f)
            };
            h.data.type_byte(at, new, false, at);
        }
        if low {
            h.goto(at + 1);
        } else {
            h.view.low_nibble = true;
        }
    }
    None
}

/// Result of resolving a key sequence against the hex keymaps.
enum HexKey {
    Bound(String),
    Prefix,
    Miss,
}

impl Editor {
    // --- Lookup ---------------------------------------------------------------

    pub(super) fn hex_active(&self) -> bool {
        self.active.hex.is_some()
    }

    /// The hex buffer shown in pane `pane`, if it shows one.
    pub(super) fn pane_hex(&self, pane: usize) -> Option<&HexDoc> {
        let name = self.pane(pane)?.buffer.clone();
        self.doc(&name)?.hex.as_deref()
    }

    pub(super) fn pane_hex_mut(&mut self, pane: usize) -> Option<&mut HexDoc> {
        let name = self.pane(pane)?.buffer.clone();
        self.doc_mut(&name)?.hex.as_deref_mut()
    }

    /// Record a hex pane's painted geometry and how much it shows (called by
    /// its element each frame).
    pub(super) fn set_hex_geometry(&mut self, pane: usize, geom: HexGeom, fit_width: usize) {
        self.hex_ui.geom.insert(pane, geom);
        if let Some(h) = self.pane_hex_mut(pane) {
            h.view.rows_visible = geom.rows.max(1);
            h.view.fit_width = fit_width.max(1);
            // A resize (or a new fit width) must not leave point off screen.
            let len = h.data.len();
            h.view.scroll_to_cursor(len);
        }
    }

    // --- Opening ----------------------------------------------------------------

    /// Install `bytes`, read from `path`, as a new hex buffer.
    pub(super) fn open_hex(
        &mut self,
        name: String,
        path: PathBuf,
        bytes: Vec<u8>,
        cx: &mut Context<Self>,
    ) {
        if self.switch_to_visiting(&super::buffers::file_identity(&path), cx) {
            return;
        }
        let name = self.unique_buffer_name(&name, &path);
        let read_only = path.exists() && !crate::save::writable(&path);
        let size = bytes.len();
        self.install_hex_doc(
            name.clone(),
            Some(path.clone()),
            HexDoc::new(bytes),
            true,
            cx,
        );
        self.active.read_only = read_only;
        self.echo(
            format!(
                "Visiting {name} as hex ({}) — C-s searches, M-g g goes to an offset",
                human_size(size)
            ),
            cx,
        );
        self.warn_if_locked(&path, cx);
        self.run_hook("find-file-hook");
        self.reconcile_watches(cx);
    }

    /// Make `hex` the active buffer and run its mode hooks. With
    /// `keep_current`, the buffer shown now stays open behind it; otherwise
    /// the hex buffer replaces it (a text buffer turned into hex).
    fn install_hex_doc(
        &mut self,
        name: String,
        path: Option<PathBuf>,
        hex: HexDoc,
        keep_current: bool,
        cx: &mut Context<Self>,
    ) {
        let mut doc = hex_doc_with(name, path, hex);
        doc.hooks_ran = false;
        if keep_current {
            let current = self.snapshot_active();
            self.inactive.push(current);
        }
        self.install_active(doc);
        self.run_mode_hooks_once(cx);
        cx.notify();
    }

    /// `(hex-find-file path)`: visit a file as hex, whatever it holds. A file
    /// already open as text switches to hex in place.
    pub(super) fn hex_find_file(&mut self, input: &str, cx: &mut Context<Self>) {
        let path = PathBuf::from(crate::util::expand_tilde(input.trim()));
        if path.as_os_str().is_empty() {
            return;
        }
        let path = crate::remote::canonical(&path).unwrap_or(path);
        let identity = super::buffers::file_identity(&path);
        if self.switch_to_visiting(&identity, cx) {
            if !self.hex_active() {
                self.hex_mode_from_text(cx);
            }
            return;
        }
        if path.is_dir() {
            self.echo(format!("{} is a directory", path.display()), cx);
            return;
        }
        if !self.pending_opens.insert(identity.clone()) {
            return;
        }
        let name = path.file_name().map_or_else(
            || input.trim().to_string(),
            |f| f.to_string_lossy().into_owned(),
        );
        self.hex_forced.insert(identity.clone());
        self.read_and_open(name, path, identity, false, cx);
    }

    /// `M-x hex-mode`: show the current text buffer's bytes as hex, in place.
    fn hex_mode_from_text(&mut self, cx: &mut Context<Self>) {
        if self.hex_active() {
            return;
        }
        if self.active.grid.is_some() {
            self.echo(
                "A grid: C-c C-t shows its text first, then M-x hex-mode",
                cx,
            );
            return;
        }
        if self.active.special.is_some() {
            self.echo(format!("{} can't be shown as hex", self.active.name), cx);
            return;
        }
        let bytes = self.active.buffer.full_text().into_bytes();
        let modified = self.active.modified;
        let read_only = self.active.read_only;
        let (name, path) = (self.active.name.clone(), self.active.path.clone());
        let mut hex = HexDoc::new(bytes);
        if modified {
            hex.data.forget_saved();
            if let Some(disk) = path.as_deref().and_then(|p| std::fs::read(p).ok()) {
                hex.disk_hash = crate::grid::bytes_hash(&disk);
            }
        }
        self.install_hex_doc(name, path, hex, false, cx);
        self.active.read_only = read_only;
        self.echo("Hex view (C-c C-t for text)", cx);
    }

    /// `hex-view-as-text`: show this buffer as text, in place — when its bytes
    /// are UTF-8 (a text buffer could not save anything else back).
    fn hex_view_as_text(&mut self, cx: &mut Context<Self>) {
        let Some(h) = self.active.hex.as_deref() else {
            return;
        };
        let text = match std::str::from_utf8(h.data.bytes()) {
            Ok(t) => t.to_string(),
            Err(e) => {
                self.echo(
                    format!(
                        "Not UTF-8 text (invalid byte at 0x{:x}) — it stays hex",
                        e.valid_up_to()
                    ),
                    cx,
                );
                return;
            }
        };
        let modified = h.data.modified();
        let read_only = self.active.read_only;
        let (name, path) = (self.active.name.clone(), self.active.path.clone());
        let mut doc = make_document(name, path, &text);
        // `C-c C-t` there switches back (hex.scm binds it).
        doc.modes.push(crate::mode::Mode::new("hex-text-mode"));
        doc.modified = modified;
        doc.read_only = read_only;
        self.install_active(doc);
        self.parse_full();
        self.move_to(0, cx);
        self.run_mode_hooks_once(cx);
        self.echo("Text view (M-x hex-mode for hex)", cx);
    }

    // --- Saving / reverting ---------------------------------------------------

    /// `revert-buffer` in a hex buffer: re-read the file, dropping edits.
    pub(super) fn hex_revert(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active.path.clone() else {
            self.echo("Buffer is not visiting a file", cx);
            return;
        };
        if let Some(remote) = crate::remote::parse_path(&path) {
            // Off the UI thread, like a remote visit; the buffer that asked
            // is found again by its file when the bytes arrive.
            let read = crate::remote::spawn(move || crate::remote::read_for_buffer(&remote));
            cx.spawn(async move |this, cx| {
                let result = read
                    .await
                    .unwrap_or_else(|| Err(std::io::Error::other("remote read thread died")));
                this.update(cx, |editor, cx| match result {
                    Ok(bytes) => editor.hex_reload_path(&path, bytes, cx),
                    Err(err) => editor.echo(format!("Revert failed: {err}"), cx),
                })
                .ok();
            })
            .detach();
            return;
        }
        let stamp = crate::save::stamp(&path);
        match std::fs::read(&path) {
            Ok(bytes) => {
                crate::save::note_synced(&path, stamp, &bytes);
                self.hex_reload_path(&path, bytes, cx);
            }
            Err(err) => self.echo(format!("Revert failed: {err}"), cx),
        }
    }

    /// Replace the bytes of the hex buffer visiting `path` (revert).
    fn hex_reload_path(&mut self, path: &Path, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let Some(doc) = self
            .docs_mut()
            .find(|d| d.hex.is_some() && d.path.as_deref() == Some(path))
        else {
            return;
        };
        if let Some(h) = doc.hex.as_deref_mut() {
            reload(h, bytes);
        }
        doc.modified = false;
        let name = doc.name.clone();
        self.echo(format!("Reverted {name}"), cx);
        cx.notify();
    }

    /// A watched file changed: reload the hex buffers showing it that have no
    /// unsaved edits. Returns whether some hex buffer visits `path`.
    pub(super) fn hex_auto_revert(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let id = super::buffers::file_identity(path);
        let shows = |d: &Document| {
            d.hex.is_some()
                && d.path
                    .as_deref()
                    .is_some_and(|p| p == path || super::buffers::file_identity(p) == id)
        };
        if !self.docs().any(shows) {
            return false;
        }
        let stamp = crate::save::stamp(path);
        let Ok(bytes) = std::fs::read(path) else {
            return true;
        };
        let hash = crate::grid::bytes_hash(&bytes);
        let mut kept = false;
        let mut reverted = Vec::new();
        for doc in self.docs_mut().filter(|d| shows(d)) {
            let Some(h) = doc.hex.as_deref_mut() else {
                continue;
            };
            if h.disk_hash == hash {
                continue; // our own save, or no real change
            }
            if h.data.modified() {
                kept = true;
                continue;
            }
            reload(h, bytes.clone());
            doc.modified = false;
            reverted.push(doc.name.clone());
        }
        if !kept {
            crate::save::note_synced(path, stamp, &bytes);
        } else {
            self.echo(
                format!("{} changed on disk (unsaved edits kept)", path.display()),
                cx,
            );
        }
        if !reverted.is_empty() {
            self.echo(
                format!("Reverted {} (changed on disk)", reverted.join(", ")),
                cx,
            );
            cx.notify();
        }
        true
    }

    // --- Commands -----------------------------------------------------------------

    /// The hex command handler (first in `COMMAND_HANDLERS`, before the
    /// grid's): hex commands, and the generic motion/edit commands that mean
    /// their byte-wise versions in a hex buffer.
    pub(super) fn cmd_hex(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand as E;
        let cmd = match command {
            E::Hex(cmd) => cmd,
            E::HexOp(op) => {
                self.hex_op(op, cx);
                return Ok(());
            }
            E::HexFindFile(path) => {
                self.hex_find_file(&path, cx);
                return Ok(());
            }
            other if self.hex_active() => match other {
                E::ForwardChar => HexCmd::Right,
                E::BackwardChar => HexCmd::Left,
                E::NextLine => HexCmd::Down,
                E::PreviousLine => HexCmd::Up,
                E::ForwardWord => HexCmd::NextGroup,
                E::BackwardWord => HexCmd::PreviousGroup,
                E::BeginningOfLine => HexCmd::RowStart,
                E::EndOfLine => HexCmd::RowEnd,
                E::BeginningOfBuffer => HexCmd::BufferStart,
                E::EndOfBuffer => HexCmd::BufferEnd,
                E::ScrollUp => HexCmd::PageDown,
                E::ScrollDown => HexCmd::PageUp,
                E::SetMark => HexCmd::SetMark,
                E::MarkWholeBuffer => HexCmd::SelectAll,
                E::DeleteChar => HexCmd::Delete,
                E::DeleteBackwardChar => HexCmd::DeleteBackward,
                E::Undo => HexCmd::Undo,
                E::Redo => HexCmd::Redo,
                E::ClipboardCopy | E::KillRingSave => HexCmd::Copy(CopyFormat::Hex),
                E::ClipboardCut | E::KillRegion => HexCmd::Cut,
                E::ClipboardPaste | E::Yank => HexCmd::Paste,
                E::Recenter => HexCmd::Recenter,
                // `C-q` (and Scheme's `insert`): those bytes at point.
                E::Insert(text) => {
                    self.hex_insert_text(&text, cx);
                    return Ok(());
                }
                // C-g drops the selection, then quits as usual.
                E::KeyboardQuit | E::DeactivateMark => {
                    if let Some(h) = self.active.hex.as_deref_mut() {
                        apply(h, HexCmd::Cancel, 1, None);
                    }
                    cx.notify();
                    return Err(other);
                }
                other => return Err(other),
            },
            other => return Err(other),
        };
        self.hex_command(cmd, 1, cx);
        Ok(())
    }

    /// Run one hex command against the active buffer.
    pub(super) fn hex_command(&mut self, cmd: HexCmd, count: usize, cx: &mut Context<Self>) {
        if cmd == HexCmd::HexMode {
            self.hex_mode_from_text(cx);
            return;
        }
        if !self.hex_active() {
            self.echo("Not a hex buffer (M-x hex-mode shows this one as hex)", cx);
            return;
        }
        if cmd.edits() && self.active.read_only {
            self.echo("Buffer is read-only", cx);
            return;
        }
        let clipboard = (cmd == HexCmd::Paste)
            .then(|| cx.read_from_clipboard().and_then(|i| i.text()))
            .flatten();
        let Some(h) = self.active.hex.as_deref_mut() else {
            return;
        };
        let out = apply(h, cmd, count, clipboard.as_deref());
        if let Some(text) = &out.copy {
            self.set_clipboard(text, cx);
            self.push_kill(text.clone());
        }
        if let Some(msg) = out.message {
            self.echo(msg, cx);
        }
        if out.prompt_search {
            self.run_script("(hex-search)", cx);
        }
        if out.view_as_text {
            self.hex_view_as_text(cx);
            return;
        }
        self.hex_after(cx);
    }

    /// A hex operation with arguments (from Scheme).
    fn hex_op(&mut self, op: HexOp, cx: &mut Context<Self>) {
        let read_only = self.active.read_only;
        let Some(h) = self.active.hex.as_deref_mut() else {
            self.echo("Not a hex buffer", cx);
            return;
        };
        let len = h.len();
        let cur = h.view.cursor;
        let msg: Option<String> = match op {
            HexOp::Goto(s) => match hexview::parse_offset(&s, cur, len) {
                Ok(at) if at > len => {
                    h.goto(len);
                    Some(format!("Past the end (0x{len:x} bytes): at the end"))
                }
                Ok(at) => {
                    h.view.mark = None;
                    h.goto(at);
                    None
                }
                Err(e) => Some(e),
            },
            HexOp::Search { pattern, forward } => match hexview::parse_pattern(&pattern) {
                Ok(pat) => {
                    let label = pattern.trim().to_string();
                    h.view.search = Some((pat.clone(), label.clone()));
                    search(h, &pat, &label, cur, forward).message
                }
                Err(e) => Some(e),
            },
            HexOp::Fill(_) | HexOp::Insert(_) | HexOp::Type(_) if read_only => {
                Some("Buffer is read-only".into())
            }
            HexOp::Type(text) => type_text(h, &text),
            HexOp::Fill(s) => match hexview::parse_pattern(&s)
                .and_then(|p| hexview::pattern_bytes(&p))
            {
                Ok(pat) => match h.selection() {
                    Some(sel) => {
                        let fill: Vec<u8> = pat.iter().copied().cycle().take(sel.len()).collect();
                        h.data.splice(sel.start, sel.len(), &fill, cur);
                        Some(format!("Filled {} bytes", sel.len()))
                    }
                    None => Some("Select the bytes to fill first (C-SPC, then move)".into()),
                },
                Err(e) => Some(e),
            },
            HexOp::Insert(s) => {
                match hexview::parse_pattern(&s).and_then(|p| hexview::pattern_bytes(&p)) {
                    Ok(bytes) => {
                        h.data.splice(cur, 0, &bytes, cur);
                        h.view.mark = None;
                        h.goto(cur + bytes.len());
                        Some(format!("Inserted {} bytes", bytes.len()))
                    }
                    Err(e) => Some(e),
                }
            }
            HexOp::RowWidth(n) => {
                h.view.width = n.min(256);
                h.view.scroll_to_cursor(len);
                None
            }
            HexOp::GroupSize(n) => {
                h.view.group = n.clamp(1, 16);
                None
            }
        };
        if let Some(m) = msg {
            self.echo(m, cx);
        }
        self.hex_after(cx);
    }

    /// After a hex change: mirror the data's saved state on the document
    /// (mode line `**`, kill prompts) and repaint.
    fn hex_after(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.active.hex.as_deref_mut() {
            let len = h.data.len();
            if h.view.cursor > len {
                h.view.cursor = len;
            }
            h.view.scroll_to_cursor(len);
            self.active.modified = h.data.modified();
        }
        cx.notify();
    }

    /// `C-q` text: each char up to U+00FF is that byte (so `C-q 377` is
    /// 0xff), anything else its UTF-8. Written as typing would: overwriting
    /// or inserting.
    fn hex_insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.active.read_only {
            self.echo("Buffer is read-only", cx);
            return;
        }
        let bytes: Vec<u8> = if text.chars().all(|c| (c as u32) <= 0xff) {
            text.chars().map(|c| c as u8).collect()
        } else {
            text.as_bytes().to_vec()
        };
        let Some(h) = self.active.hex.as_deref_mut() else {
            return;
        };
        let at = h.view.cursor;
        let remove = if h.view.insert { 0 } else { bytes.len() };
        h.data.splice(at, remove, &bytes, at);
        h.view.mark = None;
        h.goto(at + bytes.len());
        self.hex_after(cx);
    }

    /// Typed text in the hex buffer.
    fn hex_type(&mut self, text: &str, cx: &mut Context<Self>) {
        // A `C-u` before a typed digit means nothing here; don't let it
        // carry over to the next command.
        self.uarg.pending = None;
        if self.active.read_only {
            self.echo("Buffer is read-only", cx);
            return;
        }
        if !self.hex_active() {
            return;
        }
        self.record_event(MacroEvent::Command(format!(
            "(hex-type {})",
            crate::script::steel_quote(text)
        )));
        let Some(h) = self.active.hex.as_deref_mut() else {
            return;
        };
        if let Some(msg) = type_text(h, text) {
            self.echo(msg, cx);
        }
        self.hex_after(cx);
    }

    // --- Keys ---------------------------------------------------------------------

    /// The hex buffer's key layer (after the grid's in `dispatch_keystroke`).
    pub(super) fn key_hex(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> Handled {
        if !self.hex_active() || ks.key.is_empty() {
            return Handled::No;
        }
        if self.uarg.reading && self.handle_prefix_arg(ks, cx) {
            cx.stop_propagation();
            return Handled::Yes;
        }
        let chord = chord_string(ks);
        let modal = self.evil_intercepts();
        let plain = !ks.modifiers.control && !ks.modifiers.alt && !ks.modifiers.platform;
        let (ascii, editing) = self
            .active
            .hex
            .as_deref()
            .map_or((false, false), |h| (h.view.ascii, h.view.editing));
        // Evil Insert carried over from a text buffer: Escape is evil's (back
        // to Normal), not ours.
        if chord == "escape" && self.evil == EvilState::Insert {
            return Handled::No;
        }
        // Escape disarms typing (evil `i`), or drops a selection.
        if chord == "escape" && self.pending_keys.is_empty() && (editing || modal) {
            self.hex_command(
                if editing {
                    HexCmd::EditEnd
                } else {
                    HexCmd::Cancel
                },
                1,
                cx,
            );
            self.modal_count = None;
            cx.stop_propagation();
            return Handled::Yes;
        }
        // Typing: hex digits in the hex column, any printable text in the
        // ASCII column (under evil/helix only once armed). While armed, every
        // printable key types — a non-hex one is refused with a message
        // rather than running a Normal-state command (`x`, `p`, `u`, …).
        if self.pending_keys.is_empty()
            && plain
            && (!modal || editing)
            && let Some(text) = ks
                .key_char
                .clone()
                .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
            && (ascii || editing || text.chars().all(|c| c.is_ascii_hexdigit()))
        {
            self.hex_type(&text, cx);
            cx.stop_propagation();
            return Handled::Yes;
        }
        // Vim count prefix (`5l`, `3j`).
        if modal
            && self.pending_keys.is_empty()
            && plain
            && let Some(d) = chord.parse::<usize>().ok().filter(|&d| d < 10)
            && (d != 0 || self.modal_count.is_some())
        {
            let n = self.modal_count.unwrap_or(0).saturating_mul(10) + d;
            self.modal_count = Some(n.min(10_000));
            self.status = format!("{n}").into();
            cx.notify();
            cx.stop_propagation();
            return Handled::Yes;
        }
        let pending = self.pending_keys.join(" ");
        let sequence = if pending.is_empty() {
            chord.clone()
        } else {
            format!("{pending} {chord}")
        };
        match self.hex_resolve(&sequence) {
            HexKey::Bound(expr) => {
                self.pending_keys.clear();
                self.status = SharedString::default();
                self.hex_run(&expr, cx);
                cx.stop_propagation();
                Handled::Yes
            }
            HexKey::Prefix => {
                self.pending_keys.push(chord);
                self.status = format!("{}-", pretty_sequence(&sequence)).into();
                cx.notify();
                cx.stop_propagation();
                Handled::Yes
            }
            HexKey::Miss => {
                // A modal leader sequence (`SPC f f`): the leader layer's.
                if modal
                    && !ks.modifiers.platform
                    && !matches!(
                        self.keymap.resolve_leader(None, &sequence),
                        crate::mode::LeaderMatch::Miss
                    )
                {
                    return Handled::No;
                }
                // Under evil/helix, a Ctrl/Alt sequence the hex maps don't
                // bind is the user's normal-state one (`C-w v`, `C-o`, their
                // own chords) — the evil layer skips special buffers, so
                // resolve it here. Plain keys stay hex commands: the
                // normal map's text edits mean nothing on bytes.
                let chorded = self
                    .pending_keys
                    .first()
                    .map_or(!plain, |k| k.starts_with("ctrl-") || k.starts_with("alt-"));
                if modal && chorded && !ks.modifiers.platform {
                    let km = self.evil_keymap();
                    if let Some(expr) = self.keymap.resolve_mode(km, &sequence).map(str::to_string)
                    {
                        self.pending_keys.clear();
                        self.status = SharedString::default();
                        self.dispatch_recorded(&expr, cx);
                        cx.stop_propagation();
                        return Handled::Yes;
                    }
                    if self.keymap.is_prefix_mode(km, &sequence) {
                        self.pending_keys.push(chord);
                        self.status = format!("{}-", pretty_sequence(&sequence)).into();
                        cx.notify();
                        cx.stop_propagation();
                        return Handled::Yes;
                    }
                }
                // The global maps know it (`C-x C-s`, `M-x`): theirs.
                if self.keymap.resolve(&self.active.modes, &sequence).is_some()
                    || self.keymap.is_prefix(&self.active.modes, &sequence)
                {
                    return Handled::No;
                }
                let ours =
                    !pending.is_empty() && matches!(self.hex_resolve(&pending), HexKey::Prefix);
                if ours || (pending.is_empty() && plain) {
                    // A dead hex sequence or an unbound plain key: swallowed,
                    // never inserted into the (empty) rope.
                    self.pending_keys.clear();
                    self.modal_count = None;
                    if ours {
                        self.status = format!("{} is undefined", pretty_sequence(&sequence)).into();
                    }
                    cx.notify();
                    cx.stop_propagation();
                    return Handled::Yes;
                }
                Handled::No
            }
        }
    }

    /// Resolve against the active modal overlay, then the base `hex-mode` map.
    fn hex_resolve(&self, sequence: &str) -> HexKey {
        let overlay = self
            .evil_intercepts()
            .then(|| format!("{}:hex-mode", self.modal_collection_prefix()));
        for km in overlay.as_deref().into_iter().chain(["hex-mode"]) {
            if let Some(expr) = self.keymap.resolve_mode(km, sequence) {
                return HexKey::Bound(expr.to_string());
            }
            if self.keymap.is_prefix_mode(km, sequence) {
                return HexKey::Prefix;
            }
        }
        HexKey::Miss
    }

    /// Run a bound expression under the pending count (`5 l`, `C-u 8 C-f`).
    fn hex_run(&mut self, expr: &str, cx: &mut Context<Self>) {
        let count = self.modal_count.take().unwrap_or(0);
        let count = match self.uarg.pending.take() {
            Some(n) if count == 0 => n.max(1) as usize,
            _ => count,
        };
        let name = expr
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(char::is_whitespace)
            .next()
            .unwrap_or("")
            .to_string();
        self.record_event(MacroEvent::Command(expr.to_string()));
        match HexCmd::from_name(&name) {
            Some(cmd) => self.hex_command(cmd, count.max(1), cx),
            None => {
                for _ in 0..count.clamp(1, 10_000) {
                    self.run_script(expr, cx);
                }
            }
        }
    }

    /// An OS-special key (arrows, Tab, Enter, Backspace, …) in a hex buffer:
    /// resolved against the hex keymaps. True when the buffer is hex (an
    /// unbound one is swallowed rather than editing the empty rope).
    pub(super) fn hex_action_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if !self.hex_active() {
            return false;
        }
        if self.uarg.reading {
            self.uarg.pending = Some(super::prefix_arg_value(self.uarg.digits, self.uarg.cu));
            self.reset_uarg();
        }
        let pending = self.pending_keys.join(" ");
        let sequence = if pending.is_empty() {
            key.to_string()
        } else {
            format!("{pending} {key}")
        };
        match self.hex_resolve(&sequence) {
            HexKey::Bound(expr) => {
                self.pending_keys.clear();
                self.hex_run(&expr, cx);
            }
            HexKey::Prefix => {
                self.pending_keys.push(key.to_string());
                cx.notify();
            }
            HexKey::Miss => {
                // A global binding of the key (`C-x <left>`) still works.
                if self.keymap.resolve(&self.active.modes, &sequence).is_some() {
                    return false;
                }
                self.pending_keys.clear();
                self.modal_count = None;
            }
        }
        true
    }

    // --- Mouse ------------------------------------------------------------------------

    /// A mouse press on a hex pane: put point on the byte (shift extends the
    /// selection) and start a drag selection. Returns whether a hex pane took it.
    pub(super) fn hex_mouse_down(
        &mut self,
        pos: Point<Pixels>,
        button: MouseButton,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if button != MouseButton::Left {
            return false;
        }
        let Some(pane) = self.pane_at_position(pos) else {
            return false;
        };
        if self.pane_hex(pane).is_none() {
            return false;
        }
        if pane != self.selected_pane {
            self.select_pane(pane, cx);
        }
        let Some(geom) = self.hex_ui.geom.get(&pane).copied() else {
            return true;
        };
        let Some(h) = self.active.hex.as_deref_mut() else {
            return true;
        };
        let Some(hit) = geom.hit(pos, h.data.len()) else {
            return true;
        };
        let before = h.view.cursor;
        h.view.mark = if shift {
            Some(h.view.mark.unwrap_or(before))
        } else {
            None
        };
        h.view.ascii = hit.ascii;
        h.goto(hit.offset);
        h.view.low_nibble = hit.low_nibble;
        let anchor = h.view.mark.unwrap_or(hit.offset);
        self.hex_ui.drag = Some((pane, anchor));
        cx.notify();
        true
    }

    /// Mouse motion during a hex drag: extend the selection.
    pub(super) fn hex_mouse_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((pane, anchor)) = self.hex_ui.drag else {
            return false;
        };
        let Some(geom) = self.hex_ui.geom.get(&pane).copied() else {
            return true;
        };
        let Some(h) = self.pane_hex_mut(pane) else {
            return true;
        };
        let len = h.data.len();
        // Off the rows (above, below, or left): the nearest row shown.
        let top = geom.origin.y + gpui::px(geom.rows_y);
        let bottom = top + gpui::px(geom.rows as f32 * geom.line_h - 1.0);
        let clamped = Point {
            x: pos.x.max(geom.origin.x + gpui::px(geom.hex_x)),
            y: pos.y.clamp(top, bottom),
        };
        let Some(hit) = geom.hit(clamped, len) else {
            return true;
        };
        let mark = (hit.offset != anchor).then_some(anchor);
        if hit.offset != h.view.cursor || h.view.mark != mark {
            h.view.mark = mark;
            h.view.cursor = hit.offset;
            h.view.low_nibble = false;
            cx.notify();
        }
        true
    }

    pub(super) fn hex_mouse_up(&mut self) {
        self.hex_ui.drag = None;
    }

    /// The wheel over a hex pane scrolls it. Returns whether one took it.
    pub(super) fn hex_scroll(
        &mut self,
        pos: Point<Pixels>,
        dy: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self.pane_at_position(pos) else {
            return false;
        };
        let line_h = match self.hex_ui.geom.get(&pane) {
            Some(g) if self.pane_hex(pane).is_some() => g.line_h.max(1.0),
            _ => return false,
        };
        self.hex_ui.wheel -= dy / line_h;
        let rows = self.hex_ui.wheel.trunc();
        self.hex_ui.wheel -= rows;
        if rows != 0.0
            && let Some(h) = self.pane_hex_mut(pane)
        {
            let len = h.data.len();
            h.view.scroll(rows as isize, len);
            cx.notify();
        }
        true
    }
}

/// A `hex-mode` document for `hex`, visiting `path`.
fn hex_doc_with(name: String, path: Option<PathBuf>, hex: HexDoc) -> Document {
    let mut doc = make_special_document(name, "hex-mode", "");
    doc.path = path;
    doc.modified = hex.data.modified();
    doc.hex = Some(Box::new(hex));
    doc
}

/// A hex document holding `bytes` read from `path`, point at `cursor` — for a
/// buffer restored from a session (its mode hooks run when it's first shown).
pub(super) fn hex_document(name: String, path: PathBuf, bytes: Vec<u8>, cursor: usize) -> Document {
    let mut hex = HexDoc::new(bytes);
    let len = hex.len();
    hex.view.set_cursor(cursor, len);
    let read_only = path.exists() && !crate::save::writable(&path);
    let mut doc = hex_doc_with(name, Some(path), hex);
    doc.read_only = read_only;
    doc.hooks_ran = false;
    doc
}

/// Replace `h`'s bytes with the file's (revert), keeping point in range.
fn reload(h: &mut HexDoc, bytes: Vec<u8>) {
    h.disk_hash = crate::grid::bytes_hash(&bytes);
    h.data.reset(bytes);
    h.view.mark = None;
    let len = h.data.len();
    h.view.set_cursor(h.view.cursor, len);
}

/// `1.2 MB`-style size.
pub(super) fn human_size(n: usize) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(bytes: &[u8]) -> HexDoc {
        HexDoc::new(bytes.to_vec())
    }

    #[test]
    fn motion_by_byte_row_group_and_page() {
        let mut h = doc(&[0; 100]);
        apply(&mut h, HexCmd::Right, 3, None);
        assert_eq!(h.view.cursor, 3);
        apply(&mut h, HexCmd::Down, 1, None);
        assert_eq!(h.view.cursor, 19);
        apply(&mut h, HexCmd::RowStart, 1, None);
        assert_eq!(h.view.cursor, 16);
        apply(&mut h, HexCmd::RowEnd, 1, None);
        assert_eq!(h.view.cursor, 31);
        h.view.group = 4;
        apply(&mut h, HexCmd::PreviousGroup, 1, None);
        assert_eq!(h.view.cursor, 28);
        apply(&mut h, HexCmd::NextGroup, 2, None);
        assert_eq!(h.view.cursor, 36);
        apply(&mut h, HexCmd::BufferEnd, 1, None);
        assert_eq!(h.view.cursor, 99);
        apply(&mut h, HexCmd::Right, 5, None);
        assert_eq!(h.view.cursor, 100, "stops at the append slot");
        apply(&mut h, HexCmd::Down, 1, None);
        assert_eq!(h.view.cursor, 100);
        apply(&mut h, HexCmd::BufferStart, 1, None);
        h.view.rows_visible = 3;
        apply(&mut h, HexCmd::PageDown, 1, None);
        assert_eq!(h.view.cursor, 32);
    }

    #[test]
    fn typing_hex_nibbles_overwrites() {
        let mut h = doc(&[0x00, 0x11, 0x22]);
        assert_eq!(type_text(&mut h, "a"), None);
        assert_eq!(h.data.bytes(), &[0xa0, 0x11, 0x22]);
        assert!(h.view.low_nibble);
        type_text(&mut h, "bc");
        assert_eq!(h.data.bytes(), &[0xab, 0xc1, 0x22]);
        assert_eq!(h.view.cursor, 1);
        assert!(type_text(&mut h, "z").is_some());
        // One undo step for the run.
        apply(&mut h, HexCmd::Undo, 1, None);
        assert_eq!(h.data.bytes(), &[0x00, 0x11, 0x22]);
        assert!(!h.data.modified());
    }

    #[test]
    fn typing_in_the_ascii_column_and_insert_mode() {
        let mut h = doc(b"....");
        apply(&mut h, HexCmd::TogglePane, 1, None);
        type_text(&mut h, "hi");
        assert_eq!(h.data.bytes(), b"hi..");
        apply(&mut h, HexCmd::ToggleInsert, 1, None);
        type_text(&mut h, "!");
        assert_eq!(h.data.bytes(), b"hi!..");
        apply(&mut h, HexCmd::TogglePane, 1, None);
        type_text(&mut h, "4");
        assert_eq!(h.data.bytes(), b"hi!@..");
        type_text(&mut h, "1");
        assert_eq!(h.data.bytes(), b"hi!A..");
    }

    #[test]
    fn appending_at_the_end() {
        let mut h = doc(&[1]);
        apply(&mut h, HexCmd::Right, 1, None);
        type_text(&mut h, "ff");
        assert_eq!(h.data.bytes(), &[1, 0xff]);
        assert_eq!(h.view.cursor, 2);
    }

    #[test]
    fn select_copy_cut_paste() {
        let mut h = doc(&[0xde, 0xad, 0xbe, 0xef]);
        apply(&mut h, HexCmd::SetMark, 1, None);
        apply(&mut h, HexCmd::Right, 1, None);
        let out = apply(&mut h, HexCmd::Copy(CopyFormat::Hex), 1, None);
        assert_eq!(out.copy.as_deref(), Some("de ad"));
        assert_eq!(h.view.mark, None);
        // No selection: the byte at point.
        let out = apply(&mut h, HexCmd::Copy(CopyFormat::CArray), 1, None);
        assert_eq!(out.copy.as_deref(), Some("{\n    0xad\n}"));
        h.view.mark = Some(2);
        let out = apply(&mut h, HexCmd::Cut, 1, None);
        assert_eq!(out.copy.as_deref(), Some("ad be"));
        assert_eq!(h.data.bytes(), &[0xde, 0xef]);
        assert_eq!(h.view.cursor, 1);
        // Overwrite paste of hex text.
        apply(&mut h, HexCmd::Paste, 1, Some("01 02 03"));
        assert_eq!(h.data.bytes(), &[0xde, 0x01, 0x02, 0x03]);
        assert_eq!(h.view.cursor, 4);
    }

    #[test]
    fn delete_and_backspace() {
        let mut h = doc(&[1, 2, 3, 4]);
        h.view.cursor = 1;
        apply(&mut h, HexCmd::Delete, 1, None);
        assert_eq!(h.data.bytes(), &[1, 3, 4]);
        // Overwrite mode: backspace only steps back.
        apply(&mut h, HexCmd::DeleteBackward, 1, None);
        assert_eq!(h.data.bytes(), &[1, 3, 4]);
        assert_eq!(h.view.cursor, 0);
        h.view.insert = true;
        h.view.cursor = 2;
        apply(&mut h, HexCmd::DeleteBackward, 1, None);
        assert_eq!(h.data.bytes(), &[1, 4]);
        assert_eq!(h.view.cursor, 1);
    }

    #[test]
    fn search_next_selects_the_match() {
        let mut h = doc(b"xxABxxAB");
        h.view.search = Some((hexview::parse_pattern("\"AB").unwrap(), "AB".into()));
        apply(&mut h, HexCmd::SearchNext, 1, None);
        assert_eq!(h.view.cursor, 2);
        assert_eq!(h.view.selection(8), Some(2..4));
        apply(&mut h, HexCmd::SearchNext, 1, None);
        assert_eq!(h.view.cursor, 6);
        let out = apply(&mut h, HexCmd::SearchNext, 1, None);
        assert_eq!(h.view.cursor, 2);
        assert!(out.message.unwrap().starts_with("Wrapped"));
        h.view.search = None;
        assert!(apply(&mut h, HexCmd::SearchNext, 1, None).prompt_search);
    }

    #[test]
    fn geometry_hits_bytes_and_nibbles() {
        let g = HexGeom {
            origin: Point::default(),
            char_w: 10.0,
            line_h: 20.0,
            rows_y: 20.0,
            rows: 10,
            hex_x: 100.0,
            ascii_x: 600.0,
            width: 16,
            group: 1,
            top: 0,
        };
        let at = |x: f32, y: f32| g.hit(Point::new(gpui::px(x), gpui::px(y)), 1000);
        assert_eq!(at(100.0, 25.0).unwrap().offset, 0);
        assert!(!at(100.0, 25.0).unwrap().low_nibble);
        assert!(at(115.0, 25.0).unwrap().low_nibble);
        assert_eq!(at(130.0, 25.0).unwrap().offset, 1);
        // Byte 8 sits after the mid-row gap: 8*3 + 1 = 25 chars in.
        assert_eq!(at(350.0, 45.0).unwrap().offset, 24);
        assert_eq!(
            at(605.0, 45.0).unwrap(),
            HexHit {
                offset: 16,
                ascii: true,
                low_nibble: false
            }
        );
        assert_eq!(at(50.0, 25.0), None);
        assert_eq!(at(100.0, 5.0), None);
        // Below the 10 rows shown: the footer, not a byte.
        assert_eq!(at(100.0, 20.0 + 10.0 * 20.0 + 5.0), None);
    }

    #[test]
    fn layout_helpers() {
        assert_eq!(hex_chars(16, 1), 16 * 3 + 1 - 1);
        assert_eq!(hex_char_x(4, 4), 13);
        assert_eq!(offset_digits(100), 8);
        assert_eq!(offset_digits(0x1_0000_0000), 10);
        assert_eq!(human_size(10), "10 bytes");
        assert_eq!(human_size(2048), "2.0 KB");
    }
}
