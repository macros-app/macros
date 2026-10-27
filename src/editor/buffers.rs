//! Buffer list, file I/O, save/kill, and FS watching for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::{Context, ExternalPaths, SharedString, Window, px};
use notify_debouncer_mini::notify::RecursiveMode;

use crate::buffer::Buffer;

use super::*;

/// The longest the UI thread waits on a Scheme call it must block on
/// (`before-save-hook`, Org fontify) before interrupting it. Generous enough
/// for a formatter shelling out; short of the editor looking hung for good.
const SYNC_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Returns `true` when `name` looks like a special buffer (`*scratch*`,
/// `*Messages*`, etc.) — i.e. it starts and ends with `*`.
fn is_special_buffer_name(name: &str) -> bool {
    name.starts_with('*') && name.ends_with('*') && name.len() > 1
}

/// Does killing `doc` need a confirm? Emacs semantics: only a FILE-VISITING
/// buffer guards unsaved changes — a scratch-like buffer (*scratch*, a
/// package's query pane) has nowhere to save to, so killing it just discards.
/// A modified grid counts even without a file (a workbook's sheet, an imported
/// table): killing it loses the edits.
fn doc_needs_kill_confirm(doc: &Document) -> bool {
    doc.modified && (doc.path.is_some() || doc.grid.is_some())
}

/// What an `edit-file-text` read was based on.
#[derive(Debug)]
pub(super) enum FileEditBase {
    /// The text of the buffer visiting the file, at this edit tick.
    Buffer { name: String, tick: u64 },
    /// The file itself, as stamped before reading (`None`: it didn't exist).
    File(Option<crate::save::DiskStamp>),
}

/// An `edit-file-text` read awaiting its `set-file-text`.
#[derive(Debug)]
pub(super) struct FileEdit {
    callback: String,
    base: FileEditBase,
    attempt: u8,
}

#[cfg(test)]
impl FileEdit {
    /// Point a recorded read at a different callback (for tests that answer
    /// it by hand).
    pub(super) fn set_callback_for_test(&mut self, callback: &str) {
        self.callback = callback.to_string();
    }
}

/// The span where `old` and `new` differ, as `(start, old_end, new_end)`: the
/// longest common prefix and suffix (not overlapping, on char boundaries) are
/// left out, so `old[start..old_end]` → `new[start..new_end]` is the change.
fn differing_span(old: &str, new: &str) -> (usize, usize, usize) {
    let mut start = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let max_suffix = (old.len() - start).min(new.len() - start);
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    (start, old.len() - suffix, new.len() - suffix)
}

/// Why a save skipped `name`: its file changed on disk since it was read.
pub(super) fn changed_on_disk_failure(name: &str) -> String {
    format!("{name}: changed on disk since it was read — C-x C-s to save over it, or revert")
}

/// Whether `doc` visits the file `identity` (a [`file_identity`]).
fn visits_identity(doc: &Document, identity: &Path) -> bool {
    doc.path
        .as_deref()
        .is_some_and(|p| file_identity(p) == identity)
}

/// The identity of the file at `path` for "is it already open?" checks: the
/// canonical path (symlinks, `.`/`..`, and relative spellings resolved). A file
/// that doesn't exist yet canonicalizes its directory and keeps its name; a
/// relative path resolves against the process cwd (the editor never `chdir`s).
pub(super) fn file_identity(path: &Path) -> PathBuf {
    // A remote file's identity is its normalized spelling (never resolved
    // against the local filesystem).
    if let Some(remote) = crate::remote::canonical(path) {
        return remote;
    }
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let absolute = || std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => std::fs::canonicalize(dir)
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| absolute()),
        _ => absolute(),
    }
}

impl Editor {
    /// Names of all open buffers (active first).
    pub(super) fn buffer_names(&self) -> Vec<String> {
        self.docs().map(|d| d.name.clone()).collect()
    }

    /// Publish the buffer list to Scheme (for `(buffer-names)`), plus the
    /// name → absolute path map for file-visiting buffers (`buffer-file-name-of`).
    pub(super) fn sync_buffers(&self) {
        self.script.set_buffer_names(self.buffer_names());
        let abs = |p: &Path| -> String {
            if p.is_absolute() || crate::remote::is_remote(p) {
                p.to_string_lossy().into_owned()
            } else {
                // Resolve against the startup cwd (the editor never `chdir`s),
                // avoiding a `current_dir` syscall — see `process_cwd`'s doc.
                Path::new(&self.process_cwd)
                    .join(p)
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let paths = self
            .docs()
            .filter_map(|d| Some((d.name.clone(), abs(d.path.as_deref()?))))
            .collect();
        self.script.set_buffer_paths(paths);
    }

    pub fn buffer_name(&self) -> &str {
        &self.active.name
    }

    /// Snapshot the active buffer's state into a [`Document`], leaving a blank
    /// placeholder behind (the caller installs another document next).
    pub(super) fn snapshot_active(&mut self) -> Document {
        // Flush any pending full-sync change while this buffer is still active —
        // after the swap below `self.active` is a different document.
        self.flush_lsp_fullsync();
        std::mem::replace(
            &mut self.active,
            blank_document(String::new(), None, "", ModeStack::default()),
        )
    }

    /// The index of the inactive document [`render_redirect`](Self::render_redirect)
    /// names. Resolved by name on every use, so reshuffling `inactive` (a
    /// remove/retain/insert shifts every later index) can never make it address
    /// the wrong document; `None` when no redirect is set or that buffer is no
    /// longer inactive.
    pub(super) fn redirect_index(&self) -> Option<usize> {
        self.render_redirect
            .as_deref()
            .and_then(|name| self.inactive_index(name))
    }

    /// End [`render_redirect`](Self::render_redirect) if it names `name` — the
    /// buffer just became active or was killed, so decorations target the
    /// active buffer from now on (right when it was just activated), and a
    /// later buffer that reuses the name never inherits the redirect.
    fn end_redirect_to(&mut self, name: &str) {
        if self.render_redirect.as_deref() == Some(name) {
            self.render_redirect = None;
        }
    }

    pub(super) fn install_active(&mut self, doc: Document) {
        self.active = doc;
        // A keyboard selection belongs to the buffer just left.
        self.special_select_drop();
        if self.render_redirect.as_deref() == Some(self.active.name.as_str()) {
            self.render_redirect = None; // see `end_redirect_to`
        }
        // Markers are positions in the previous buffer; drop them on a switch.
        self.script.clear_markers();
        // Secondary selections / snippet fields are marker-backed, so they're
        // gone with the markers — forget their (now dangling) head+anchor pairs
        // and any avy hints.
        self.extra_cursors.clear();
        self.snippet_fields.clear();
        self.avy_hints.clear();
        // Everything else that pins a RAW byte range in the buffer we just left
        // goes too. These are re-armed per command in the normal flow, but a
        // switch can land between the arm and the use — click into another
        // pane right after `C-y`, then press `M-y`, and `yank_pop` would splice
        // the old buffer's range into the new one (a panic outright when the
        // new buffer is shorter). Same for a completion popup left open when a
        // hook or process output surfaces a different buffer.
        self.last_yank = None;
        self.last_was_yank = false;
        self.completion_popup = None;
        self.completion_popup_hit = None;
        self.completion_range = None;
        self.completion_auto_pending = false;
        // The selected window now shows this buffer (Emacs: switching the active
        // buffer changes what the selected window displays).
        let active = self.active.name.clone();
        let selected = self.selected_pane;
        if let Some(pane) = self.panes.iter_mut().find(|p| p.id == selected) {
            pane.buffer = active;
        }
        // Invalidate the layout cache; it's recomputed on the next paint.
        self.lines.clear();
        self.last_bounds = None;
        // Any buffer switch/create/kill flows through here — keep Scheme's view
        // of the buffer list current.
        self.sync_buffers();
        // An in-flight completion was for the buffer we just left; its answer
        // must not pop up (with that buffer's byte range) in this one.
        for server in self.servers.values_mut() {
            if let Some(pending) = server.pending_completion.take() {
                server.client.cancel(pending.id);
            }
        }
        // A buffer shown again after its server (re)started — or after it was
        // closed in it — needs a fresh `didOpen` (no-op when already open).
        if let Some(language) = self.active_language()
            && self.servers.contains_key(&language)
        {
            self.lsp_open_active(&language);
        }
    }

    /// Make the buffer `name` active, snapshotting the current one.
    ///
    /// `deliberate` marks a user-initiated switch (C-x b, find-file reuse, an
    /// LSP jump) as opposed to an incidental activation (pane-focus dance,
    /// process-output surfacing). Both fire `buffer-switch-hook`; only a
    /// deliberate switch also fires `buffer-select-hook` — see [`switch_hooks`].
    pub(super) fn activate(&mut self, index: usize, deliberate: bool, cx: &mut Context<Self>) {
        let target = self.inactive.remove(index);
        self.switch_to_doc(target);
        self.after_activation(deliberate, cx);
    }

    /// The common tail of every way a buffer becomes active: parse it, bring
    /// point into view, run its mode hooks (once) and the switch hooks.
    pub(super) fn after_activation(&mut self, deliberate: bool, cx: &mut Context<Self>) {
        self.parse_full();
        self.ensure_cursor_visible();
        self.run_mode_hooks_once(cx);
        self.switch_hooks(deliberate);
        cx.notify();
    }

    /// Fire the buffer-change hooks after the active buffer was replaced.
    ///
    /// Re-activating an existing buffer never fires `find-file-hook`, so
    /// anything tracking "which buffer is the user in" needs its own signal.
    /// `buffer-switch-hook` fires on EVERY activation (bookkeeping: spell,
    /// perspective resume files); `buffer-select-hook` fires only on a
    /// DELIBERATE switch, so the perspectives title bar can follow the user
    /// into another project without incidental pane-focus activations
    /// hijacking it. Both are free when nothing is registered.
    pub(super) fn switch_hooks(&mut self, deliberate: bool) {
        self.run_hook("buffer-switch-hook");
        if deliberate {
            self.run_hook("buffer-select-hook");
        }
    }

    /// `C-x <right>`: cycle forward through the buffer ring (the active buffer
    /// goes to the back, the front buffer becomes active).
    pub(super) fn next_buffer(&mut self, cx: &mut Context<Self>) {
        if self.inactive.is_empty() {
            self.echo("No other buffer", cx);
            return;
        }
        let target = self.inactive.remove(0);
        self.cycle_to(target, true, cx);
    }

    /// `C-x <left>`: cycle backward (the active buffer goes to the front, the
    /// back buffer becomes active) — the exact inverse of `next-buffer`.
    pub(super) fn previous_buffer(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.inactive.pop() else {
            self.echo("No other buffer", cx);
            return;
        };
        self.cycle_to(target, false, cx);
    }

    pub(super) fn cycle_to(
        &mut self,
        target: Document,
        current_to_back: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self.snapshot_active();
        if current_to_back {
            self.inactive.push(current);
        } else {
            self.inactive.insert(0, current);
        }
        self.install_active(target);
        self.after_activation(true, cx); // C-x <arrow> cycling is a deliberate switch
        let name = self.active.name.clone();
        self.echo(name, cx);
    }

    /// `C-x C-f`: visit a file, reusing its buffer if already open.
    ///
    /// "Already open" compares canonical paths ([`file_identity`]), so
    /// `./src/main.rs`, an absolute spelling, and a symlink to it all reuse one
    /// buffer, and a visit while the same file's read is still in flight is
    /// dropped rather than racing to create a second buffer.
    pub(super) fn find_file(&mut self, input: &str, cx: &mut Context<Self>) {
        let path = PathBuf::from(expand_path(input.trim()));
        if path.as_os_str().is_empty() {
            return;
        }
        let path = crate::remote::canonical(&path).unwrap_or(path);
        let identity = file_identity(&path);
        if self.switch_to_visiting(&identity, cx) {
            return;
        }
        if !self.pending_opens.insert(identity.clone()) {
            return; // the first visit's read will install (and show) it
        }

        let name = path.file_name().map_or_else(
            || input.trim().to_string(),
            |f| f.to_string_lossy().into_owned(),
        );

        // CSV/TSV (by `grid-extensions`) open as a grid; a huge one asks first.
        let as_grid = self.is_grid_path(&path);
        if as_grid && self.grid_guard_large(&path, cx) {
            self.pending_opens.remove(&identity);
            return;
        }
        self.read_and_open(name, path, identity, as_grid, cx);
    }

    /// Visit each of `paths` as `C-x C-f` would — the files the OS asked
    /// Macros to open (command line, Finder, "Open with"; see src/launch.rs).
    /// The last one ends up as the active buffer.
    pub fn visit_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            self.find_file(&path.to_string_lossy(), cx);
        }
    }

    /// The read half of [`Self::find_file`] (also run after the large-grid
    /// confirm): read the file off the UI thread, then install it as a text
    /// buffer or, when `as_grid`, a grid.
    pub(super) fn read_and_open(
        &mut self,
        name: String,
        path: PathBuf,
        identity: PathBuf,
        as_grid: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(remote) = crate::remote::parse_path(&path) {
            return self.remote_read_and_open(name, path, identity, remote, as_grid, cx);
        }
        // Read off the UI thread so opening a large file never freezes the
        // editor; install the buffer back on the foreground when it's ready.
        // Raw bytes: a grid parses them directly (it may not even be UTF-8);
        // a text buffer validates them below.
        let read_path = path.clone();
        let read = cx.background_executor().spawn(async move {
            // Stamp first: a write racing the read then shows as a change.
            let stamp = crate::save::stamp(&read_path);
            (stamp, std::fs::read(&read_path))
        });
        cx.spawn(async move |this, cx| {
            let (stamp, result) = read.await;
            this.update(cx, |editor, cx| {
                editor.pending_opens.remove(&identity);
                // Remember what the new buffer is based on, unless the file
                // is already open (the existing buffer is shown instead, and
                // its own baseline must stand).
                if !editor.is_visiting(&identity) {
                    match &result {
                        Ok(bytes) => crate::save::note_synced(&path, stamp, bytes),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            crate::save::note_synced(&path, None, &[])
                        }
                        Err(_) => {}
                    }
                }
                let result = match result {
                    Ok(bytes) if as_grid => {
                        editor.open_grid(name, path, bytes, cx);
                        return;
                    }
                    Ok(bytes) => String::from_utf8(bytes)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    Err(err) => Err(err),
                };
                match result {
                    Ok(contents) => editor.open_file(name, path, contents, cx),
                    // A missing file is the ordinary "visit a new file" flow: an
                    // empty buffer that creates the file on first save.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound && as_grid => {
                        editor.open_grid(name, path, Vec::new(), cx)
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        editor.open_file(name, path, String::new(), cx)
                    }
                    // Anything else (permissions, non-UTF-8 data, …) must NOT open
                    // an empty file-visiting buffer — saving that buffer would
                    // truncate the file on disk. Report and leave the editor as-is.
                    Err(err) => editor.echo(format!("Could not open {name}: {err}"), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// If some buffer already visits the file `identity` (a [`file_identity`]),
    /// show it and return `true`.
    pub(super) fn switch_to_visiting(&mut self, identity: &Path, cx: &mut Context<Self>) -> bool {
        let visits = |doc: &Document| visits_identity(doc, identity);
        if visits(&self.active) {
            self.echo(format!("Already in {}", self.active.name), cx);
            return true;
        }
        if let Some(index) = self.inactive.iter().position(visits) {
            self.activate(index, true, cx);
            self.echo(format!("Switched to {}", self.active.name), cx);
            return true;
        }
        false
    }

    /// Whether some buffer already visits the file `identity` (a [`file_identity`]).
    pub(super) fn is_visiting(&self, identity: &Path) -> bool {
        self.docs().any(|doc| visits_identity(doc, identity))
    }

    /// Install a freshly-read file as the active buffer (foreground half of the
    /// async `find_file`, and of LSP's `visit_path`). Re-checks for an existing
    /// buffer on the file — another route may have opened it meanwhile — and
    /// gives the new buffer a unique name (see [`Self::unique_buffer_name`]).
    pub(super) fn open_file(
        &mut self,
        name: String,
        path: PathBuf,
        contents: String,
        cx: &mut Context<Self>,
    ) {
        let goto = super::server::take_server_goto(&path);
        if self.switch_to_visiting(&file_identity(&path), cx) {
            return;
        }
        let name = self.unique_buffer_name(&name, &path);
        self.switch_to_doc(make_document(name.clone(), Some(path), &contents));
        self.parse_full();
        self.move_to(0, cx);
        if let Some(goto) = goto {
            self.goto_line_column(goto, cx); // `macros --client +LINE:COL`
        }
        self.run_mode_hooks_once(cx);
        self.echo(format!("Visiting {name}"), cx);
        self.run_hook("find-file-hook"); // Emacs find-file-hook
        self.reconcile_watches(cx); // watch the newly-visited file (auto-revert)
    }

    /// A buffer name for visiting `path` that no open buffer uses. `base` (the
    /// file name) when free; otherwise Emacs uniquify's `post-forward-angle-
    /// brackets` style — `mod.rs<editor>`, adding parent directories until the
    /// suffix tells this file apart from every other open file of that name
    /// (`mod.rs<a/src>`), with a numeric `mod.rs<2>` as the last resort.
    ///
    /// Only the NEW buffer is named this way; existing buffers keep their
    /// names, since Scheme state (perspectives, buffer-locals, pane lists) is
    /// keyed by buffer name and a rename would orphan it.
    pub(super) fn unique_buffer_name(&self, base: &str, path: &Path) -> String {
        let taken = |n: &str| self.has_buffer(n);
        if !taken(base) {
            return base.to_string();
        }
        let identity = file_identity(path);
        // Other open files with the same file name, as canonical paths.
        let others: Vec<PathBuf> = self
            .docs()
            .filter_map(|d| d.path.as_deref())
            .map(file_identity)
            .filter(|p| *p != identity && p.file_name() == identity.file_name())
            .collect();
        uniquified_name(base, &identity, &others, taken)
    }
}

/// The naming rule behind [`Editor::unique_buffer_name`], pure for testing:
/// `base<dir>` with the fewest trailing parent directories of `path` that no
/// path in `others` shares, skipping names `taken` reports in use; numeric
/// `base<N>` when no directory suffix works.
fn uniquified_name(
    base: &str,
    path: &Path,
    others: &[PathBuf],
    taken: impl Fn(&str) -> bool,
) -> String {
    if !taken(base) {
        return base.to_string();
    }
    let dirs = |p: &Path| -> Vec<String> {
        p.parent()
            .map(|d| {
                d.components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let suffix = |parts: &[String], depth: usize| -> Option<String> {
        (depth <= parts.len()).then(|| parts[parts.len() - depth..].join("/"))
    };
    let mine = dirs(path);
    let theirs: Vec<Vec<String>> = others.iter().map(|p| dirs(p)).collect();
    for depth in 1..=mine.len() {
        let Some(s) = suffix(&mine, depth) else { break };
        let candidate = format!("{base}<{s}>");
        let distinct = theirs
            .iter()
            .all(|t| suffix(t, depth).as_deref() != Some(s.as_str()));
        if distinct && !taken(&candidate) {
            return candidate;
        }
    }
    (2..)
        .map(|n| format!("{base}<{n}>"))
        .find(|c| !taken(c))
        .expect("an unused numeric suffix exists")
}

impl Editor {
    /// Open files dropped onto the window from the OS (Finder, a terminal, etc.).
    /// Each dropped file is visited in its own buffer, in the pane it was
    /// dropped on; the last one ends up active. CSV/TSV files open as a grid
    /// (see `find_file`). Directories are skipped — `find_file` only reads
    /// regular files.
    pub(super) fn on_drop_files(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.pane_at_position(window.mouse_position())
            && pane != self.selected_pane
        {
            self.select_pane(pane, cx);
        }
        let mut skipped = 0;
        for path in paths.paths() {
            if path.is_dir() {
                skipped += 1;
                continue;
            }
            self.find_file(&path.to_string_lossy(), cx);
        }
        if skipped > 0 {
            self.echo(
                format!("Skipped {skipped} folder(s) — can only open files"),
                cx,
            );
        }
    }

    /// `:w <file>` — write the buffer's text to a given path (Vim save-as). The
    /// buffer keeps its own file; this just drops a copy.
    pub(super) fn write_buffer_to(&mut self, path: &str, cx: &mut Context<Self>) {
        let target = expand_path(path.trim());
        self.grid_commit_pending(cx);
        let same = |p: &Path| file_identity(p) == file_identity(Path::new(&target));
        let own_file = self.active.path.as_deref().is_some_and(same);
        // A sheet of a workbook with several sheets writes them all: over the
        // workbook's own file that's a save of it, elsewhere a copy.
        if let Some(book) = self.active_book()
            && book.borrow().names.len() > 1
        {
            let lossy = self
                .active
                .grid
                .as_deref()
                .is_some_and(|g| !g.xlsx.lossy.is_empty());
            let over_book = book.borrow().path.as_deref().is_some_and(same);
            if over_book && !lossy {
                self.grid_save_book(&book, cx);
                return;
            }
            self.grid_flush_book(&book);
        }
        // A grid written as `.tsv` (from a `.csv`, or the reverse) switches
        // delimiter to match the new extension.
        let bytes = match self.active.grid.as_deref() {
            // Only part of the file is parsed yet: writing now would save
            // just that part.
            Some(g) if g.loading.is_some() => Err(std::io::Error::other(
                "still loading — write it once the load finishes",
            )),
            // One sheet of a workbook written over the workbook would drop
            // its other sheets.
            Some(g) if g.xlsx.source.as_deref().is_some_and(same) => Err(std::io::Error::other(
                "this is one sheet of that workbook — writing it there would drop the others; \
                 save it under another name",
            )),
            // A protected .xlsx is never written over itself (save_bytes says
            // why).
            Some(g) if own_file && !g.xlsx.lossy.is_empty() => self.active.save_bytes(),
            Some(g) => g.bytes_for(Path::new(&target)),
            None => self.active.save_bytes(),
        };
        match bytes.and_then(|b| crate::save::write_now(Path::new(&target), &b)) {
            Ok(()) => {
                // Writing a grid to its own file is a save.
                if own_file && let Some(g) = self.active.grid.as_deref_mut() {
                    g.saved_externally();
                    self.active.modified = false;
                }
                self.echo(format!("Wrote {target}"), cx)
            }
            Err(err) => self.echo(format!("Save failed: {err}"), cx),
        }
    }

    /// Open `path` in a window *other* than the selected one — the sidebar
    /// pattern (dired-sidebar/dired): `RET` on a file keeps the tree and shows the
    /// file in the main editor. Uses the first other pane, or splits if the
    /// selected pane is the only one. Focus moves to the file's window.
    pub(super) fn open_file_other_window(&mut self, path: String, cx: &mut Context<Self>) {
        let other = self
            .layout
            .leaves()
            .into_iter()
            .find(|&id| id != self.selected_pane);
        let target = match other {
            Some(id) => id,
            None => {
                // Narrow split: the sidebar (the current pane) keeps this share.
                const SIDEBAR_SPLIT_RATIO: f32 = 0.22;
                let new_id = self.next_pane_id; // split assigns this id to the new pane
                self.split_window_ratio(SplitAxis::Horizontal, SIDEBAR_SPLIT_RATIO, cx);
                new_id
            }
        };
        self.select_pane(target, cx);
        self.find_file(&path, cx);
    }

    /// `:e!` / `M-x revert-buffer`: re-read the visited file from disk,
    /// discarding unsaved changes. Point is kept (clamped to the new length).
    pub(super) fn revert_buffer(&mut self, cx: &mut Context<Self>) {
        if self.active.grid.is_some() {
            self.grid_revert(cx);
            return;
        }
        let Some(path) = self.active.path.clone() else {
            self.echo("Buffer is not visiting a file", cx);
            return;
        };
        if let Some(remote) = crate::remote::parse_path(&path) {
            return self.remote_revert(path, remote, cx);
        }
        let stamp = crate::save::stamp(&path);
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let point = self.point();
                self.replace_whole_buffer(&contents, point, cx);
                self.active.modified = false;
                crate::save::note_synced(&path, stamp, contents.as_bytes());
                self.echo(format!("Reverted {}", self.active.name), cx);
            }
            Err(err) => self.echo(format!("Revert failed: {err}"), cx),
        }
    }

    /// Foreground handler for a debounced batch of changed paths. For each path:
    /// auto-revert the buffer visiting it (when `auto-revert` is on), and, if it
    /// lives under a watched directory, schedule that dir's `directory-changed-hook`
    /// (deduped so a 50-file change refreshes dired-sidebar/dired once, not 50 times).
    pub(super) fn on_fs_events(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let auto_revert = cx.global::<crate::settings::Settings>().auto_revert();
        let mut changed_dirs: Vec<PathBuf> = Vec::new();
        for path in &paths {
            if auto_revert {
                self.auto_revert_file(path, cx);
            }
            // A non-recursive dir watch reports its immediate children, so the
            // watched dir is the changed path's parent — or the path itself if a
            // watched dir's own entry (rename/attrs) is what fired.
            for dir in [path.parent(), Some(path.as_path())].into_iter().flatten() {
                if self.fs.dirs.contains(dir) && !changed_dirs.iter().any(|d| d == dir) {
                    changed_dirs.push(dir.to_path_buf());
                }
            }
            // Recursive roots: a change anywhere in the subtree fires the hook
            // with the ROOT, deduped so a bulk change refreshes magit once.
            for root in &self.fs.trees {
                if path.starts_with(root) && !changed_dirs.iter().any(|d| d == root) {
                    changed_dirs.push(root.clone());
                }
            }
        }
        for dir in changed_dirs {
            self.dispatch_dir_changed(&dir);
        }
    }

    /// React to an external change to `path`: if a file-visiting buffer holds it
    /// and has no unsaved edits, re-read it from disk. Buffers with unsaved changes
    /// are left intact (a one-line warning is echoed) so we never lose the user's
    /// work. Our own saves are absorbed for free: the on-disk text then equals the
    /// buffer's, so the content check below short-circuits with no revert.
    pub(super) fn auto_revert_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        // Only touch disk if some buffer actually visits this path — a watched
        // directory reports churn in files we don't have open, and reading those
        // would be pure waste.
        let visited = self.docs().any(|d| d.path.as_deref() == Some(path));
        if !visited {
            return;
        }
        if self.grid_auto_revert(path, cx) {
            return;
        }
        let stamp = crate::save::stamp(path);
        let Ok(disk) = std::fs::read_to_string(path) else {
            return; // unreadable / mid-rename / deleted — leave the buffer as-is
        };
        // The buffer now matches the file (or has just been made to).
        let synced = || crate::save::note_synced(path, stamp, disk.as_bytes());
        if self.active.path.as_deref() == Some(path) {
            if self.active.buffer.full_text() == disk {
                synced();
                return; // identical (e.g. our own write) — nothing to revert
            }
            if self.active.modified {
                self.echo(
                    format!("{} changed on disk (unsaved edits kept)", self.active.name),
                    cx,
                );
                return;
            }
            let point = self.point();
            self.replace_whole_buffer(&disk, point, cx);
            self.active.modified = false;
            synced();
            self.echo(
                format!("Reverted {} (changed on disk)", self.active.name),
                cx,
            );
            return;
        }
        if let Some(idx) = self
            .inactive
            .iter()
            .position(|d| d.path.as_deref() == Some(path))
        {
            let doc = &self.inactive[idx];
            if doc.buffer.full_text() == disk {
                synced();
                return;
            }
            let name = doc.name.clone();
            if doc.modified {
                self.echo(format!("{name} changed on disk (unsaved edits kept)"), cx);
                return;
            }
            // Replace the text in place: rebuilding the document would drop its
            // buffer-locals, breakpoint dots, overlays and window positions.
            self.replace_inactive_text(idx, &disk, false);
            self.inactive[idx].modified = false;
            synced();
            self.echo(format!("Reverted {name} (changed on disk)"), cx);
        }
    }

    /// Replace inactive document `idx`'s whole text in place, keeping its
    /// buffer-local state (locals, overlays, gutter dots, mode). `undoable`
    /// records the change as one undo step (an LSP edit); otherwise history
    /// restarts (a revert from disk, which old records no longer match). Its
    /// selection and every window showing it are clamped to the new text, and
    /// its language servers get the new text, and its highlighting is reparsed.
    pub(super) fn replace_inactive_text(&mut self, idx: usize, new: &str, undoable: bool) {
        let doc = &mut self.inactive[idx];
        let old = doc.buffer.full_text();
        if undoable {
            doc.undo.record(UndoRecord {
                at: 0,
                removed: old.clone(),
                inserted: new.to_string(),
            });
            doc.modified = true;
        } else {
            doc.undo = UndoTree::default();
        }
        doc.buffer.replace(0..old.len(), new);
        // A window still showing this buffer paints from its tree: reparse
        // now, or that pane reads stale node ranges against the new text.
        if let Some(h) = doc.highlighter.as_mut() {
            h.set_text(doc.buffer.rope());
        }
        let snap = |p: usize| doc.buffer.snap(p);
        doc.selected_range = snap(doc.selected_range.start)..snap(doc.selected_range.end);
        let (name, uri) = (doc.name.clone(), document_uri(doc));
        for pane in self.panes.iter_mut().filter(|p| p.buffer == name) {
            pane.point = self.inactive[idx].buffer.snap(pane.point);
        }
        self.lsp_sync_document(&uri, new);
    }

    /// Fire every `directory-changed-hook` function with `dir` as its argument, so
    /// a Scheme view (dired-sidebar/dired) can re-render if `dir` is the one it shows.
    /// No-op — and no thread hop — when nothing is registered on the hook.
    pub(super) fn dispatch_dir_changed(&mut self, dir: &Path) {
        self.run_hook_arg("directory-changed-hook", &dir.to_string_lossy());
    }

    /// Call every function registered on `hook` with the single string `arg`
    /// (`directory-changed-hook`'s dir, `terminal-exit-hook`'s buffer name).
    /// No-op — and no thread hop — when nothing is registered on the hook.
    pub(super) fn run_hook_arg(&mut self, hook: &str, arg: &str) {
        let Some(functions) = self.hooks.get(hook).filter(|f| !f.is_empty()).cloned() else {
            return;
        };
        self.publish_point();
        for function in functions {
            self.script.dispatch_action(&function, Some(arg));
        }
    }

    /// Sync the OS watcher with what should be watched right now: every Scheme-
    /// requested directory, plus — when `auto-revert` is on — every file-visiting
    /// buffer. Diffs against `fs_watched` so only the delta is (un)watched. Cheap
    /// and idempotent; call it whenever the buffer set or the option changes.
    pub(super) fn reconcile_watches(&mut self, cx: &mut Context<Self>) {
        if self.fs.debouncer.is_none() {
            return;
        }
        let mut desired: HashSet<PathBuf> = self.fs.dirs.clone();
        if cx.global::<crate::settings::Settings>().auto_revert() {
            desired.extend(self.docs().filter_map(|d| d.path.clone()));
        }
        // Remote files and directories can't be watched (auto-revert and
        // dired's live refresh are local-only): `g` / `revert-buffer` re-read.
        desired.retain(|p| !crate::remote::is_remote(p));
        let debouncer = self.fs.debouncer.as_mut().unwrap();
        for path in &desired {
            if !self.fs.watched.contains(path)
                && debouncer
                    .watcher()
                    .watch(path, RecursiveMode::NonRecursive)
                    .is_ok()
            {
                self.fs.watched.insert(path.clone());
            }
        }
        let stale: Vec<PathBuf> = self.fs.watched.difference(&desired).cloned().collect();
        for path in stale {
            let _ = debouncer.watcher().unwatch(&path);
            self.fs.watched.remove(&path);
        }

        // Recursive roots (magit repo watch) — same delta dance, recursive mode.
        let want_rec = self.fs.trees.clone();
        for path in &want_rec {
            if !self.fs.watched_recursive.contains(path)
                && debouncer
                    .watcher()
                    .watch(path, RecursiveMode::Recursive)
                    .is_ok()
            {
                self.fs.watched_recursive.insert(path.clone());
            }
        }
        let stale_rec: Vec<PathBuf> = self
            .fs
            .watched_recursive
            .difference(&want_rec)
            .cloned()
            .collect();
        for path in stale_rec {
            let _ = debouncer.watcher().unwatch(&path);
            self.fs.watched_recursive.remove(&path);
        }
    }

    /// `C-x b`: switch to an already-open buffer by name.
    pub(super) fn switch_to_buffer(&mut self, name: &str, cx: &mut Context<Self>) {
        self.switch_to_buffer_ex(name, true, cx);
    }

    /// [`switch_to_buffer`](Self::switch_to_buffer) with explicit intent:
    /// `deliberate` is false when a pane-focus change (`select_pane`) surfaces a
    /// buffer, so the perspectives title bar doesn't follow that incidental
    /// activation into another project.
    pub(super) fn switch_to_buffer_ex(
        &mut self,
        name: &str,
        deliberate: bool,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim();
        if name == self.active.name {
            cx.notify();
            return;
        }
        match self.inactive_index(name) {
            Some(index) => self.activate(index, deliberate, cx),
            None => self.echo(format!("No buffer named {name}"), cx),
        }
    }

    /// `C-x C-s`: write the active buffer back to its file.
    pub(super) fn save_buffer(&mut self, cx: &mut Context<Self>) {
        self.save_buffer_ex(true, cx);
    }

    /// [`save_buffer`](Self::save_buffer); with `check_disk`, first ask before
    /// overwriting a file something else changed since the buffer read it.
    pub(super) fn save_buffer_ex(&mut self, check_disk: bool, cx: &mut Context<Self>) {
        if !check_disk && let Some(path) = self.active_save_target() {
            crate::save::allow_overwrite(&path);
        }
        if check_disk
            && let Some(path) = self.active_save_target()
            && crate::save::changed_on_disk(&path)
        {
            let file = path.file_name().map_or_else(
                || path.display().to_string(),
                |f| f.to_string_lossy().into_owned(),
            );
            self.open_confirm(
                &format!("{file} changed on disk since it was read; save over it anyway?"),
                PendingAction::SaveOverChanged(self.active.name.clone()),
                cx,
            );
            return;
        }
        if self.active.grid.is_some() {
            self.grid_save(cx);
            return;
        }
        if self.active.path.is_none() {
            self.echo("Buffer is not visiting a file", cx);
            return;
        }
        if self
            .active_uri()
            .is_some_and(|uri| self.formatting_for_save.contains_key(&uri))
        {
            // The format's response (or its timeout) writes this buffer.
            self.echo("Save pending (waiting on formatting)", cx);
            return;
        }
        // Run synchronous before-save hooks (e.g. a shell formatter), then — if
        // this language formats via LSP on save — kick off the (async) format
        // and defer the write to its response.
        self.run_hook_blocking("before-save-hook", cx);
        if self.start_lsp_format_on_save(cx) {
            return;
        }
        self.write_active_to_disk(cx);
    }

    /// `(edit-file-text path callback)`: hand `callback` the text of `path` —
    /// its visiting buffer's when one is open, else the file's — remembering
    /// what that text was based on, so [`set_file_text`](Self::set_file_text)
    /// can tell whether it changed in between. `attempt` counts re-runs.
    pub(super) fn edit_file_text(
        &mut self,
        path: String,
        callback: String,
        attempt: u8,
        cx: &mut Context<Self>,
    ) {
        let file = PathBuf::from(crate::util::expand_tilde(&path));
        let identity = file_identity(&file);
        let visiting = self
            .docs()
            .find(|d| visits_identity(d, &identity))
            .map(|d| (d.name.clone(), d.grid.is_some(), d.buffer.tick()));
        let (text, base) = if let Some((name, grid, tick)) = visiting {
            if grid {
                self.echo(format!("Not changed: {name} is open as a grid"), cx);
                return;
            }
            let text = self
                .buffer_named(&name)
                .map(Buffer::full_text)
                .unwrap_or_default();
            (text, FileEditBase::Buffer { name, tick })
        } else {
            let stamp = crate::save::stamp(&file);
            match std::fs::read_to_string(&file) {
                Ok(text) => (text, FileEditBase::File(stamp)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    (String::new(), FileEditBase::File(None))
                }
                Err(err) => {
                    self.echo(format!("Not changed: {}: {err}", file.display()), cx);
                    return;
                }
            }
        };
        self.file_edits.insert(
            identity,
            FileEdit {
                callback: callback.clone(),
                base,
                attempt,
            },
        );
        self.publish_point();
        let call = format!(
            "({callback} {} {})",
            crate::script::steel_quote(&path),
            crate::script::steel_quote(&text)
        );
        self.script.dispatch(&call, false);
    }

    /// `(set-file-text path text)`, the answer to [`edit_file_text`]: write
    /// `text` into the visiting buffer (one undoable edit, left unsaved) or,
    /// when no buffer has the file, atomically into the file. If what the
    /// callback was given has changed since, it is run again on the new text.
    ///
    /// [`edit_file_text`]: Self::edit_file_text
    pub(super) fn set_file_text(&mut self, path: String, text: String, cx: &mut Context<Self>) {
        /// Re-runs before giving up on text that keeps changing underneath.
        const MAX_ATTEMPTS: u8 = 3;
        let file = PathBuf::from(crate::util::expand_tilde(&path));
        let identity = file_identity(&file);
        let Some(edit) = self.file_edits.remove(&identity) else {
            self.echo(
                format!("set-file-text: {path} wasn't read with edit-file-text"),
                cx,
            );
            return;
        };
        let visiting = self
            .docs()
            .find(|d| visits_identity(d, &identity))
            .map(|d| (d.name.clone(), d.buffer.tick()));
        let unchanged = match (&edit.base, &visiting) {
            (FileEditBase::Buffer { name, tick }, Some((now_name, now_tick))) => {
                name == now_name && tick == now_tick
            }
            (FileEditBase::File(stamp), None) => crate::save::stamp(&file) == *stamp,
            // Opened or killed in between: the text came from the other one.
            _ => false,
        };
        if !unchanged {
            if edit.attempt + 1 >= MAX_ATTEMPTS {
                self.echo(format!("Not changed: {path} kept changing; try again"), cx);
            } else {
                self.edit_file_text(path, edit.callback, edit.attempt + 1, cx);
            }
            return;
        }
        match visiting {
            Some((name, _)) => self.set_buffer_text(&name, &text, cx),
            None => {
                if let Some(dir) = file.parent()
                    && !dir.as_os_str().is_empty()
                {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(err) = crate::save::atomic_write(&file, text.as_bytes()) {
                    self.echo(format!("Not changed: {}: {err}", file.display()), cx);
                }
            }
        }
    }

    /// Make buffer `name`'s text `new` as one undoable edit that marks it
    /// modified. The active buffer gets only the span that differs, so point,
    /// marks and overlays outside it stay where they were.
    fn set_buffer_text(&mut self, name: &str, new: &str, cx: &mut Context<Self>) {
        if self.active.name == name {
            let old = self.active.buffer.full_text();
            let (start, old_end, new_end) = differing_span(&old, new);
            if start == old_end && start == new_end {
                return;
            }
            let point = self.point();
            self.edit(start..old_end, &new[start..new_end]);
            if point >= old_end {
                self.collapse_to(point - old_end + new_end);
            }
            cx.notify();
        } else if let Some(idx) = self.inactive_index(name) {
            self.replace_inactive_text(idx, new, true);
            cx.notify();
        }
    }

    /// The file saving the active buffer writes: its workbook's, for a sheet of
    /// a workbook that saves whole; otherwise its own.
    pub(super) fn active_save_target(&self) -> Option<PathBuf> {
        if let Some(book) = self.active_book()
            && book.borrow().saves_whole(self.active.path.is_some())
        {
            return book.borrow().path.clone();
        }
        self.active.path.clone()
    }

    /// Save every modified file-visiting buffer (Emacs `save-some-buffers`).
    /// Inactive buffers are written synchronously here; the active one goes
    /// through the normal (async, hook-running) save path.
    pub(super) fn save_some_buffers(&mut self, cx: &mut Context<Self>) {
        let mut saved = 0;
        // Failed inactive writes are reported (and keep `modified` set) rather
        // than being silently dropped from the count.
        let mut failed: Vec<String> = Vec::new();
        let mut saved_uris = Vec::new();
        let mut written: Vec<PathBuf> = Vec::new();
        // Remote buffers save asynchronously (network I/O stays off the UI
        // thread), each reporting its own failure.
        let mut remote = Vec::new();
        // Workbooks with several sheets save whole, once each.
        failed.extend(self.grid_save_modified_books(true));
        for doc in &mut self.inactive {
            if doc.modified
                && let Some(path) = doc.path.clone()
            {
                if crate::remote::is_remote(&path) && doc.grid.is_none() {
                    doc.modified = false;
                    saved += 1;
                    let text = doc.buffer.full_text().into_bytes();
                    remote.push((doc.name.clone(), path, text, document_uri(doc)));
                    continue;
                }
                if crate::save::changed_on_disk(&path) {
                    failed.push(changed_on_disk_failure(&doc.name));
                    continue;
                }
                match doc
                    .save_bytes()
                    .and_then(|b| crate::save::write_now(&path, &b))
                {
                    Ok(()) => {
                        if let Some(g) = doc.grid.as_deref_mut() {
                            g.saved_externally();
                        }
                        doc.modified = false;
                        saved += 1;
                        saved_uris.push(document_uri(doc));
                        written.push(path);
                    }
                    Err(err) => failed.push(format!("{}: {err}", doc.name)),
                }
            }
        }
        for uri in saved_uris {
            self.lsp_did_save(&uri);
        }
        for (name, path, text, uri) in remote {
            self.save_remote(name, path, text, uri, cx);
        }
        for path in written {
            self.rewatch_file(&path, cx);
        }
        if self.active.path.is_some()
            && self.active.modified
            && self
                .active_save_target()
                .is_some_and(|p| crate::save::changed_on_disk(&p))
        {
            failed.push(changed_on_disk_failure(&self.active.name));
        } else if self.active.path.is_some() && self.active.modified {
            // Async; reports its own failure (and restores `modified`) later.
            if self.active.grid.is_some() {
                self.grid_save(cx);
            } else {
                self.write_active_to_disk(cx);
            }
            saved += 1;
        }
        let mut message = match saved {
            0 => "No unsaved buffers".to_string(),
            1 => "Saved 1 buffer".to_string(),
            n => format!("Saved {n} buffers"),
        };
        if !failed.is_empty() {
            message.push_str(&format!("; {} failed: {}", failed.len(), failed.join("; ")));
        }
        self.echo(message, cx);
    }

    /// Write the active buffer to its file off the UI thread. Optimistically
    /// clears `modified`; restores it (and reports) if the write fails.
    ///
    /// The write is atomic and ordered per file (see [`crate::save`]): the
    /// ticket taken here, with the text snapshot, guarantees an older save
    /// still in flight can't land after this one.
    pub(super) fn write_active_to_disk(&mut self, cx: &mut Context<Self>) {
        if self.active.grid.is_some() {
            self.grid_save(cx);
            return;
        }
        let Some(path) = self.active.path.clone() else {
            return;
        };
        let text = self.active.buffer.full_text();
        let name = self.active.name.clone();
        let uri = document_uri(&self.active);
        self.active.modified = false;
        if crate::remote::is_remote(&path) {
            return self.save_remote(name, path, text.into_bytes(), uri, cx);
        }
        let ticket = crate::save::ticket(&path);
        // Until the result is handled, a quit waits for this save.
        let pending = crate::save::begin_pending();
        let write_path = path.clone();
        let write = cx.background_executor().spawn(async move {
            let outcome = crate::save::write_sequenced(&ticket, &write_path, text.as_bytes());
            (ticket, outcome)
        });
        cx.spawn(async move |this, cx| {
            let (ticket, result) = write.await;
            if result.is_err() {
                pending.failed();
            }
            this.update(cx, |editor, cx| match result {
                Ok(crate::save::SaveOutcome::Written) => {
                    editor.rewatch_file(&path, cx);
                    editor.echo(format!("Wrote {name}"), cx);
                    editor.lsp_did_save(&uri);
                    editor.run_hook("after-save-hook"); // Emacs after-save-hook
                }
                // A newer save of this file already landed and reported.
                Ok(crate::save::SaveOutcome::Superseded) => {}
                Err(err) => {
                    // Re-dirty the buffer that was SAVED (by its file), not
                    // whatever happens to be active now — and only if no newer
                    // save owns its state.
                    if crate::save::is_latest(&ticket) {
                        editor.mark_path_modified(&path);
                    }
                    editor.echo(format!("Save failed: {name}: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Re-arm the auto-revert watch on `path` after we saved it. An atomic save
    /// renames a new inode over the file; inode-based backends (inotify) drop
    /// the old watch with it, which would silently end auto-revert for this
    /// file. (Path-based FSEvents doesn't need it, but re-arming is harmless.)
    pub(super) fn rewatch_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.fs.watched.remove(path) {
            return;
        }
        if let Some(debouncer) = self.fs.debouncer.as_mut() {
            let _ = debouncer.watcher().unwatch(path);
        }
        self.reconcile_watches(cx);
    }

    /// Mark whichever buffer visits `path` (active or not) as modified.
    fn mark_path_modified(&mut self, path: &Path) {
        if let Some(doc) = self.docs_mut().find(|d| d.path.as_deref() == Some(path)) {
            doc.modified = true;
        }
    }

    /// If the active buffer's language formats via LSP on save and a server is
    /// ready, trigger the format (the response's edits re-enter `save_buffer` to
    /// write). Returns whether a format was started.
    pub(super) fn start_lsp_format_on_save(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(language) = self.active_language() else {
            return false;
        };
        if !self.lsp_format_on_save.contains(&language) {
            return false;
        }
        let Some(uri) = self.active_uri() else {
            return false;
        };
        let ready = self
            .servers
            .get(&language)
            .is_some_and(|s| s.ready && s.open.contains(&uri));
        if !ready {
            return false;
        }
        self.format_save_seq += 1;
        let token = self.format_save_seq;
        self.formatting_for_save.insert(uri.clone(), token);
        if !self.lsp_format_request(Some(token), cx) {
            self.formatting_for_save.remove(&uri);
            return false;
        }
        // A server that never answers must not wedge the save: after the
        // timeout, write this document unformatted (unless the response or a
        // server exit already completed this save — the token tells).
        let timer = cx.background_executor().timer(FORMAT_ON_SAVE_TIMEOUT);
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |editor, cx| {
                if editor.finish_format_on_save(&uri, token, cx) {
                    editor.echo("Formatting timed out; saved unformatted", cx);
                }
            })
            .ok();
        })
        .detach();
        true
    }

    /// Run a hook's functions synchronously (blocking on the engine thread),
    /// applying each one's enqueued commands immediately. Used for
    /// `before-save-hook`, where a formatter must modify the buffer *before* the
    /// write. Async work a hook starts (e.g. an LSP request) still completes
    /// later — only the synchronous part is guaranteed done here.
    pub(super) fn run_hook_blocking(&mut self, hook: &str, cx: &mut Context<Self>) {
        let Some(functions) = self.hooks.get(hook).filter(|f| !f.is_empty()).cloned() else {
            return;
        };
        for function in functions {
            self.publish_point();
            let (commands, error) = self.script.call_action_within(&function, SYNC_HOOK_TIMEOUT);
            for command in commands {
                self.apply_command(command, cx);
            }
            if let Some(error) = error {
                self.echo(format!("{hook}: {function}: {error}"), cx);
            }
        }
    }

    pub fn any_modified(&self) -> bool {
        self.docs()
            .any(|d| d.modified && !is_special_buffer_name(&d.name))
    }

    pub fn is_modified(&self) -> bool {
        self.active.modified
    }

    /// Handle an OS window-close request (the title-bar close button) and
    /// `C-x C-c`. If any buffer has unsaved changes, offer to save them first:
    /// `yes` saves all modified file buffers then quits, `no` quits discarding,
    /// `C-g` cancels. With nothing unsaved, quit straight away. Always returns
    /// `false` so the bare window-close is cancelled — quitting (`cx.quit()`)
    /// tears the whole app down rather than leaving a windowless process on
    /// macOS.
    pub fn confirm_quit(&mut self, cx: &mut Context<Self>) -> bool {
        self.confirm_quit_ex(false, cx)
    }

    /// The update pill's "relaunch into the staged build": the same unsaved-
    /// buffer prompt as a normal quit, relaunching only if the quit goes ahead
    /// (a cancelled prompt leaves no relaunch helper behind).
    pub fn confirm_quit_and_relaunch(&mut self, cx: &mut Context<Self>) {
        self.confirm_quit_ex(true, cx);
    }

    fn confirm_quit_ex(&mut self, relaunch: bool, cx: &mut Context<Self>) -> bool {
        // A fresh, user-initiated app quit: forget any earlier (cancelled)
        // quit's per-window answers so every dirty window is asked again.
        crate::workspace::begin_app_quit(relaunch, cx);
        self.prompt_quit_save(relaunch, cx);
        false
    }

    /// Ask (only if needed) whether to save this window's modified buffers as
    /// part of an app quit already in progress — the app-wide quit flow
    /// (`workspace::continue_app_quit`) calls this for each further dirty
    /// window without resetting the answers already given.
    pub fn prompt_quit_save(&mut self, relaunch: bool, cx: &mut Context<Self>) {
        if self.any_modified() {
            let listing = self.modified_buffer_names().join(", ");
            self.echo(format!("Modified buffers: {listing}"), cx);
            self.open_minibuffer(
                "Save modified buffers before quitting? (yes/no, C-g to cancel) ",
                MinibufferPurpose::QuitSave { relaunch },
                "",
                cx,
            );
        } else {
            self.finish_quit(relaunch, cx);
        }
    }

    /// The OS close button on a window with unsaved buffers while other windows
    /// stay open: the same save/discard/cancel prompt as a quit, but answering
    /// it closes just this window (see `workspace::quit_confirmed`). Returns
    /// `false` so the bare close is cancelled until the prompt resolves.
    pub fn confirm_close_window(&mut self, cx: &mut Context<Self>) -> bool {
        crate::workspace::begin_window_close(cx.entity_id(), cx);
        let listing = self.modified_buffer_names().join(", ");
        self.echo(format!("Modified buffers: {listing}"), cx);
        self.open_minibuffer(
            "Save modified buffers before closing this window? (yes/no, C-g to cancel) ",
            MinibufferPurpose::QuitSave { relaunch: false },
            "",
            cx,
        );
        false
    }

    /// The per-window part of quitting: publish session state and run
    /// `kill-emacs-hook` synchronously (savehist / desktop), without quitting.
    /// The app-wide quit flow runs this for every window before `cx.quit()`.
    pub fn prepare_quit(&mut self, cx: &mut Context<Self>) {
        self.publish_session();
        self.run_hook_blocking("kill-emacs-hook", cx);
    }

    /// Names of the buffers [`any_modified`](Self::any_modified) counts as
    /// unsaved, active first.
    pub(super) fn modified_buffer_names(&self) -> Vec<String> {
        self.docs()
            .filter(|d| d.modified && !is_special_buffer_name(&d.name))
            .map(|d| d.name.clone())
            .collect()
    }

    /// This window is cleared to quit (the user has confirmed, or nothing was
    /// unsaved). This window's answer is handed to the app-wide quit flow
    /// (`workspace::quit_confirmed`, deferred so no entity is mid-update): it
    /// either closes just this window (a close-button prompt), or moves on to
    /// the next window with unsaved buffers, and only once every window is
    /// resolved runs each window's `kill-emacs-hook` and quits — so another
    /// window's edits are never discarded unasked. With `relaunch`, the helper
    /// that reopens the staged update is spawned there, at the very end; if
    /// that fails the app stays running and says so.
    pub(super) fn finish_quit(&mut self, relaunch: bool, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        cx.defer(move |cx| crate::workspace::quit_confirmed(id, relaunch, cx));
    }

    /// Write every modified, file-visiting buffer (active + inactive) to disk
    /// synchronously. Used on quit, so writes are blocking (no UI to keep
    /// responsive) and skip the async format-on-save path. Buffers not visiting
    /// a file (e.g. `*scratch*`) can't be saved and are left untouched.
    ///
    /// Returns whether every write succeeded; failed buffers keep `modified` set
    /// and are reported in the echo area, so the quit path can abort rather than
    /// silently discard the changes. With `check_disk`, a file changed on disk
    /// since its buffer read it counts as failed rather than being overwritten.
    pub(super) fn save_all_modified(&mut self, check_disk: bool, cx: &mut Context<Self>) -> bool {
        self.grid_commit_pending(cx);
        // Workbooks with several sheets save whole, once each.
        let mut failed: Vec<String> = self.grid_save_modified_books(check_disk);
        let conflicts = |path: &Path| check_disk && crate::save::changed_on_disk(path);
        // A modified grid with no file of its own (a sheet of a workbook, an
        // imported table) can't be saved here: say so rather than quit and
        // lose it.
        for doc in self.docs() {
            if doc.modified && doc.path.is_none() && doc.grid.is_some() {
                failed.push(format!(
                    "{}: no file to save to (save it with :w)",
                    doc.name
                ));
            }
        }
        if self.active.modified
            && let Some(path) = self.active.path.clone()
            && conflicts(&path)
        {
            failed.push(changed_on_disk_failure(&self.active.name));
        } else if self.active.modified
            && let Some(path) = self.active.path.clone()
        {
            self.grid_commit_pending(cx);
            match self
                .active
                .save_bytes()
                .and_then(|b| crate::save::write_now(&path, &b))
            {
                Ok(()) => {
                    if let Some(g) = self.active.grid.as_deref_mut() {
                        g.saved_externally();
                    }
                    self.active.modified = false;
                }
                Err(err) => failed.push(format!("{}: {err}", self.active.name)),
            }
        }
        for doc in &mut self.inactive {
            if doc.modified
                && let Some(path) = doc.path.clone()
            {
                if conflicts(&path) {
                    failed.push(changed_on_disk_failure(&doc.name));
                    continue;
                }
                match doc
                    .save_bytes()
                    .and_then(|b| crate::save::write_now(&path, &b))
                {
                    Ok(()) => {
                        if let Some(g) = doc.grid.as_deref_mut() {
                            g.saved_externally();
                        }
                        doc.modified = false;
                    }
                    Err(err) => failed.push(format!("{}: {err}", doc.name)),
                }
            }
        }
        if failed.is_empty() {
            true
        } else {
            self.echo(format!("Save failed — {}", failed.join("; ")), cx);
            false
        }
    }

    /// Write the active buffer to its file synchronously — the quit path
    /// (`:wq` / `:x`), where the write must complete before `cx.quit()` tears
    /// the app down (the async `write_active_to_disk` would be cancelled
    /// mid-flight). Runs the synchronous `before-save-hook` but, like
    /// [`save_all_modified`](Self::save_all_modified), skips the async LSP
    /// format-on-save. Returns whether the buffer is now safely on disk. With
    /// `check_disk`, refuses to overwrite a file changed on disk since it was
    /// read (`:wq`; `:wq!` overwrites).
    pub(super) fn save_active_sync(&mut self, check_disk: bool, cx: &mut Context<Self>) -> bool {
        if !check_disk && let Some(path) = self.active_save_target() {
            crate::save::allow_overwrite(&path);
        }
        if check_disk
            && self
                .active_save_target()
                .is_some_and(|p| crate::save::changed_on_disk(&p))
        {
            let message = changed_on_disk_failure(&self.active.name);
            self.echo(format!("Not saved — {message} (! overwrites)"), cx);
            return false;
        }
        if let Some(book) = self.active_book()
            && book.borrow().saves_whole(self.active.path.is_some())
        {
            self.grid_commit_pending(cx);
            let failed = self.grid_save_modified_books(false);
            if !failed.is_empty() {
                self.echo(format!("Save failed — {}", failed.join("; ")), cx);
            }
            return failed.is_empty() && !self.active.modified;
        }
        let Some(path) = self.active.path.clone() else {
            self.echo("Buffer is not visiting a file", cx);
            return false;
        };
        self.run_hook_blocking("before-save-hook", cx);
        self.grid_commit_pending(cx);
        match self
            .active
            .save_bytes()
            .and_then(|b| crate::save::write_now(&path, &b))
        {
            Ok(()) => {
                if let Some(g) = self.active.grid.as_deref_mut() {
                    g.saved_externally();
                }
                self.active.modified = false;
                self.echo(format!("Wrote {}", self.active.name), cx);
                let uri = document_uri(&self.active);
                self.lsp_did_save(&uri);
                self.run_hook("after-save-hook");
                true
            }
            Err(err) => {
                self.echo(format!("Save failed: {err}"), cx);
                false
            }
        }
    }

    /// Does killing the active buffer need a confirm? See [`doc_needs_kill_confirm`].
    fn kill_needs_confirm(&self) -> bool {
        doc_needs_kill_confirm(&self.active)
    }

    /// `(kill-buffer-named name)` from Scheme — ibuffer's `d`/`x`, helm's "Kill
    /// buffer", packages: the same unsaved-changes guard as `C-x k`, for any
    /// buffer rather than just the active one. Internal callers that discard on
    /// purpose (a grid cell popup, a deleted sheet) use [`Self::kill_buffer_named`].
    pub(super) fn kill_buffer_named_confirming(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.doc(name).is_some_and(doc_needs_kill_confirm) {
            self.open_confirm(
                &format!("Buffer {name} modified; kill anyway?"),
                PendingAction::KillBuffer(name.to_string()),
                cx,
            );
        } else {
            self.kill_buffer_named(name, cx);
        }
    }

    /// `C-x k`: kill the active buffer (confirming first if it's a modified
    /// file buffer), then show the most-recent other buffer (or a fresh scratch).
    pub(super) fn kill_buffer(&mut self, cx: &mut Context<Self>) {
        if self.kill_needs_confirm() {
            self.open_confirm(
                "Buffer modified; kill anyway?",
                PendingAction::KillBuffer(self.active.name.clone()),
                cx,
            );
        } else {
            self.kill_active_buffer(cx);
        }
    }

    /// Kill the active buffer *and* close its window (Emacs `kill-buffer-and-
    /// window`), confirming first if it's a modified file buffer. With a sole
    /// window, the buffer is killed and `delete_window` just declines.
    pub(super) fn kill_buffer_and_window(&mut self, cx: &mut Context<Self>) {
        if self.kill_needs_confirm() {
            self.open_confirm(
                "Buffer modified; kill anyway?",
                PendingAction::KillBufferAndWindow(self.active.name.clone()),
                cx,
            );
        } else {
            self.kill_active_buffer(cx);
            self.delete_window(cx);
        }
    }

    pub(super) fn kill_active_buffer(&mut self, cx: &mut Context<Self>) {
        let killed = self.active.name.clone();
        // `didClose` it — which also discards any pending full-sync for it, so
        // the replacement's text is never flushed under the killed URI (this
        // path swaps buffers without `snapshot_active`'s flush).
        let killed_uri = document_uri(&self.active);
        self.lsp_close_document(&killed_uri);
        super::server::server_buffer_killed(self.active.path.as_deref());
        let replacement = self
            .inactive
            .pop()
            .unwrap_or_else(|| make_document("*scratch*".to_string(), None, ""));
        self.install_active(replacement);
        // `install_active` re-pointed only the selected window; every other
        // window that showed the killed buffer follows it to the replacement
        // instead of keeping a dangling name (which selecting it would later
        // resurrect as an empty buffer).
        let shown = self.active.name.clone();
        let point = self.point();
        self.repoint_panes_showing(&killed, &shown, point);
        self.drop_terminal_for_buffer(&killed);
        // The revealed buffer was activated: its mode hooks (a fresh
        // `*scratch*`'s first) and the switch hooks fire, as for any switch.
        self.after_activation(false, cx);
        self.echo(format!("Killed {killed}"), cx);
        self.reconcile_watches(cx); // stop watching the killed buffer's file
    }

    /// Point every window showing the (killed) buffer `from` at buffer `to`,
    /// with its saved point at `point` and scrolled to the top.
    fn repoint_panes_showing(&mut self, from: &str, to: &str, point: usize) {
        for pane in self.panes.iter_mut().filter(|p| p.buffer == from) {
            pane.buffer = to.to_string();
            pane.point = point;
            pane.scroll = px(0.0);
        }
    }

    /// Shut down and forget any embedded terminal backing buffer `name` (its
    /// `Drop` closes the PTY and ends the event-loop thread), and terminate any
    /// `make-process` child bound to it via `set-process-buffer`.
    pub(super) fn drop_terminal_for_buffer(&mut self, name: &str) {
        self.kill_processes_for_buffer(name);
        if let Some(id) = self
            .terminals
            .values()
            .find(|t| t.buffer_name == name)
            .map(|t| t.id)
        {
            self.terminals.remove(&id);
            self.terminal_geom.remove(&id);
            if self.terminal_drag == Some(id) {
                self.terminal_drag = None;
            }
        }
    }

    /// Kill a buffer by name: the active one (via the normal path), or just drop
    /// an inactive [`Document`] (ibuffer/dired). Last buffer stays as scratch.
    /// Kill `name` outright, unsaved changes and all (see
    /// [`Self::kill_buffer_named_confirming`] for the guarded path).
    pub(super) fn kill_buffer_named(&mut self, name: &str, cx: &mut Context<Self>) {
        if name == self.active.name {
            self.kill_active_buffer(cx);
        } else if let Some(idx) = self.inactive_index(name) {
            let doc = self.inactive.remove(idx);
            super::server::server_buffer_killed(doc.path.as_deref());
            self.end_redirect_to(name);
            self.lsp_close_document(&document_uri(&doc));
            // Non-selected windows that showed it switch to the most recent
            // other buffer (or the active one) rather than keep a dangling name.
            let (shown, point) = match self.inactive.last() {
                Some(d) => (d.name.clone(), d.selected_range.end),
                None => (self.active.name.clone(), self.point()),
            };
            self.repoint_panes_showing(name, &shown, point);
            self.drop_terminal_for_buffer(name);
            self.sync_buffers();
            self.echo(format!("Killed {name}"), cx);
            self.reconcile_watches(cx); // stop watching the killed buffer's file
        }
    }

    /// `old` was moved to `new` on disk (a dired rename): every buffer visiting
    /// `old`, or a file under it when it's a directory, now visits the matching
    /// path under `new`, so saving writes there rather than resurrecting the old
    /// file. Buffer names are kept (Scheme state is keyed by them — see
    /// [`Self::unique_buffer_name`]). Language servers forget the old URIs; the
    /// active buffer reopens under its new one, the rest when next shown.
    pub(super) fn retarget_file_buffers(&mut self, old: &str, new: &str, cx: &mut Context<Self>) {
        let old = PathBuf::from(crate::util::expand_tilde(old.trim_end_matches('/')));
        let new = PathBuf::from(crate::util::expand_tilde(new.trim_end_matches('/')));
        if old.as_os_str().is_empty() || new.as_os_str().is_empty() {
            return; // "" (or "/") would otherwise match every buffer
        }
        let mut moved = Vec::new();
        for doc in self.docs_mut() {
            let Some(rest) = doc.path.as_deref().and_then(|p| p.strip_prefix(&old).ok()) else {
                continue;
            };
            let target = if rest.as_os_str().is_empty() {
                new.clone()
            } else {
                new.join(rest)
            };
            moved.push(document_uri(doc));
            doc.path = Some(target);
        }
        if moved.is_empty() {
            return;
        }
        for uri in &moved {
            self.lsp_close_document(uri);
        }
        if let Some(language) = self.active_language() {
            self.lsp_open_active(&language);
        }
        self.sync_buffers();
        self.reconcile_watches(cx);
        cx.notify();
    }

    /// `C-x C-c` / `cmd-q`: quit, offering to save first if any buffer is
    /// unsaved (same save/discard/cancel prompt as the title-bar close button).
    pub(super) fn quit(&mut self, cx: &mut Context<Self>) {
        self.confirm_quit(cx);
    }

    // --- Special (read-only, program-generated) buffers & transients ------

    /// Render `text` into the read-only buffer `name` (major mode `mode`). If the
    /// buffer is already on screen — the active buffer, or shown in another pane
    /// (e.g. a *compilation* window) — it refreshes in place wherever it lives,
    /// preserving the point line and NOT stealing focus (magit's revert-and-keep-
    /// position). Only when it isn't shown anywhere does it switch the selected
    /// window to it.
    pub(super) fn render_buffer(
        &mut self,
        name: String,
        mode: String,
        text: String,
        line_faces: Vec<(usize, String)>,
        cx: &mut Context<Self>,
    ) {
        // Shown somewhere (active pane or another window): refresh it in place via
        // the quiet path, which never changes focus.
        if self.panes.iter().any(|p| p.buffer == name) {
            self.render_buffer_quiet(name, mode, text, line_faces, cx);
            return;
        }
        let faces: HashMap<usize, SharedString> = line_faces
            .into_iter()
            .map(|(line, face)| (line, SharedString::from(face)))
            .collect();
        // Not shown anywhere: switch the selected window to a fresh buffer.
        let current = self.snapshot_active();
        // Replace any stale buffer of the same name, then install fresh.
        self.inactive.retain(|d| d.name != name);
        self.inactive.push(current);
        self.install_active(make_special_document(name, &mode, &text));
        self.active.line_faces = faces;
        self.move_to(0, cx);
        self.active.modified = false;
        cx.notify();
    }

    /// Like [`render_buffer`](Self::render_buffer), but never switches buffers or
    /// moves focus: it refreshes the named buffer *wherever it already lives* —
    /// in place if it's active, or in its inactive [`Document`] (the dired-sidebar/dired
    /// sidebar pane) otherwise — and no-ops if the buffer isn't shown at all. This
    /// is the auto-refresh path (a watched directory changed on disk), so a passive
    /// FS event never yanks the user out of the buffer they're editing. Icon/overlay
    /// commands later in the same batch are redirected to the sidebar via
    /// [`render_redirect`](Self::render_redirect).
    pub(super) fn render_buffer_quiet(
        &mut self,
        name: String,
        mode: String,
        text: String,
        line_faces: Vec<(usize, String)>,
        cx: &mut Context<Self>,
    ) {
        let faces: HashMap<usize, SharedString> = line_faces
            .into_iter()
            .map(|(line, face)| (line, SharedString::from(face)))
            .collect();
        if self.active.name == name {
            // Active: identical to the in-place branch of `render_buffer`.
            self.active.line_icons.clear();
            let line = self.active.buffer.line_index_at_byte(self.point());
            // A refresh keeps the view where the user left it: restoring point's
            // line must not scroll back to point (they may have wheel-scrolled
            // away from it, which leaves point off-screen on purpose).
            let scroll = self.active.scroll_offset;
            // New text: a keyboard selection's range means nothing in it.
            if self.special_selecting() {
                self.special_select_drop();
                self.active.mark_active = false;
            }
            self.active.buffer = Buffer::new(&text);
            // The old undo records address the replaced text; replaying one
            // against the new text corrupts it (or panics). Start fresh.
            self.active.reset_undo();
            self.undo_grouping.boundary();
            self.active.line_faces = faces;
            self.lines.clear();
            let last = self.active.buffer.line_count().saturating_sub(1);
            let offset = self.active.buffer.line_range(line.min(last)).start;
            self.move_to(offset, cx);
            self.cursor_follow_pending = false;
            let max = self.max_scroll_offset(f32::from(self.viewport_height));
            self.active.scroll_offset = px(f32::from(scroll).clamp(0.0, max.max(0.0)));
            self.active.modified = false;
            self.render_redirect = None;
        } else if let Some(idx) = self.inactive_index(&name) {
            // Shown in a non-selected pane: rebuild that inactive buffer in place,
            // keeping the viewing pane's cursor on the same line (clamped).
            let viewing_point = self
                .panes
                .iter()
                .find(|p| p.buffer == name)
                .map(|p| p.point);
            let doc = &mut self.inactive[idx];
            let old_line = viewing_point
                .map(|pt| doc.buffer.line_index_at_byte(pt.min(doc.buffer.len())))
                .unwrap_or(0);
            doc.buffer = Buffer::new(&text);
            doc.reset_undo(); // see the active branch above
            doc.line_faces = faces;
            doc.line_icons.clear();
            doc.overlays = Overlays::default();
            doc.modified = false;
            doc.special = Some(mode);
            let last = doc.buffer.line_count().saturating_sub(1);
            let offset = doc.buffer.line_range(old_line.min(last)).start;
            let new_len = doc.buffer.len();
            for pane in self.panes.iter_mut().filter(|p| p.buffer == name) {
                pane.point = offset.min(new_len);
            }
            self.render_redirect = Some(name);
            cx.notify();
        }
        // Not shown anywhere → nothing to refresh passively.
    }

    /// Append `text` to the end of buffer `name` (creating it if needed) and
    /// switch to it — an output-buffer primitive (Emacs `append-to-buffer`).
    /// Unlike `render-buffer`, the target is a normal editable buffer.
    pub(super) fn append_to_buffer(&mut self, name: String, text: String, cx: &mut Context<Self>) {
        if self.active.name != name {
            // If the buffer is displayed in another (non-selected) window, append
            // there in place — output flows into the buffer's own window without
            // yanking focus away from what the user is editing (e.g. a streaming
            // *compilation* pane). Mirrors `render_buffer`'s follow-the-buffer rule.
            let shown_elsewhere = self
                .panes
                .iter()
                .any(|p| p.buffer == name && p.id != self.selected_pane);
            if let Some(doc) = self.inactive_doc_mut(&name).filter(|_| shown_elsewhere) {
                let end = doc.buffer.len();
                doc.buffer.replace(end..end, &text);
                let (new_end, line_count) = (doc.buffer.len(), doc.buffer.line_count());
                let line_h = f32::from(self.line_height).max(1.0);
                let content_h = line_count as f32 * line_h;
                for pane in self.panes.iter_mut().filter(|p| p.buffer == name) {
                    // Point at the tail so focusing the window lands on the latest
                    // output; scroll the (unfocused) pane to keep that tail in view
                    // as it streams, so the window doesn't look frozen.
                    pane.point = new_end;
                    if let Some((_, b)) = self.pane_bounds.iter().find(|(pid, _)| *pid == pane.id) {
                        pane.scroll = px((content_h - f32::from(b.size.height)).max(0.0));
                    }
                }
                cx.notify();
                return;
            }
            if let Some(idx) = self.inactive_index(&name) {
                self.activate(idx, false, cx); // process output surfacing, not a user switch
            } else {
                self.switch_to_doc(make_document(name, None, ""));
            }
        }
        let end = self.active.buffer.len();
        self.active.buffer.replace(end..end, &text);
        self.lines.clear();
        let new_end = self.active.buffer.len();
        self.move_to(new_end, cx);
        self.parse_full();
        cx.notify();
    }

    /// Re-fontify Org buffer `name` in place, whether or not it is active. The
    /// active path is the ordinary current-buffer fontify; for a buffer living in
    /// an inactive [`Document`] (e.g. agent-shell streaming a reply into a
    /// non-selected pane) we publish a snapshot *of that buffer* so `org--fontify`'s
    /// reads (`buffer-string`, `current-buffer`, `buffer-local-get`) see it, and
    /// point [`render_redirect`](Self::render_redirect) at it so the overlay/indent
    /// writes it emits land there too — then restore the active snapshot. This is
    /// the `run_hook_blocking` pattern (publish → `call_action` → apply), so it
    /// reuses the exact Org fontifier without disturbing focus or the active buffer.
    pub(super) fn fontify_org_buffer(&mut self, name: String, cx: &mut Context<Self>) {
        if self.active.name == name {
            self.publish_point();
            self.run_org_fontify(cx);
            return;
        }
        let Some(idx) = self.inactive_index(&name) else {
            // Not shown anywhere (or unknown) → nothing on screen to refresh.
            return;
        };
        self.publish_inactive_snapshot(idx);
        self.render_redirect = Some(name);
        self.run_org_fontify(cx);
        self.render_redirect = None;
        // Restore the active-buffer snapshot for the next read bridge.
        self.publish_point();
    }

    /// Run `org--fontify` against the published snapshot and apply its
    /// overlays, reporting (rather than hanging on) a fontifier that errors
    /// or runs past [`SYNC_HOOK_TIMEOUT`].
    fn run_org_fontify(&mut self, cx: &mut Context<Self>) {
        let (commands, error) = self
            .script
            .call_action_within("org--fontify", SYNC_HOOK_TIMEOUT);
        for command in commands {
            self.apply_command(command, cx);
        }
        if let Some(error) = error {
            self.echo(format!("org--fontify: {error}"), cx);
        }
    }

    /// The [`Document`] named `name` — the active buffer or an inactive
    /// snapshot. The one lookup every "operate on buffer N by name" method
    /// resolves through, so the active-vs-inactive dispatch lives here.
    pub(super) fn doc(&self, name: &str) -> Option<&Document> {
        if self.active.name == name {
            Some(&self.active)
        } else {
            self.inactive_doc(name)
        }
    }

    /// Mutable twin of [`Self::doc`]. NOTE: text edits made through the
    /// returned document bypass the active buffer's edit pipeline (undo, LSP
    /// did-change, marker adjustment, re-highlight) — callers that modify the
    /// ACTIVE buffer's *text* must use the `edit`-based paths instead; this is
    /// for decorations/overlays and for inactive-buffer surgery.
    pub(super) fn doc_mut(&mut self, name: &str) -> Option<&mut Document> {
        if self.active.name == name {
            Some(&mut self.active)
        } else {
            self.inactive_doc_mut(name)
        }
    }

    /// Every open buffer's [`Document`], the active one first.
    pub(super) fn docs(&self) -> impl Iterator<Item = &Document> {
        std::iter::once(&self.active).chain(&self.inactive)
    }

    /// Mutable twin of [`Self::docs`] (same caveat as [`Self::doc_mut`]).
    pub(super) fn docs_mut(&mut self) -> impl Iterator<Item = &mut Document> {
        std::iter::once(&mut self.active).chain(&mut self.inactive)
    }

    /// Whether some open buffer (active or inactive) is named `name`.
    pub(super) fn has_buffer(&self, name: &str) -> bool {
        self.doc(name).is_some()
    }

    /// The position of the inactive buffer `name` in `inactive`.
    pub(super) fn inactive_index(&self, name: &str) -> Option<usize> {
        self.inactive.iter().position(|d| d.name == name)
    }

    /// The inactive [`Document`] named `name` (never the active buffer).
    pub(super) fn inactive_doc(&self, name: &str) -> Option<&Document> {
        self.inactive.iter().find(|d| d.name == name)
    }

    /// Mutable twin of [`Self::inactive_doc`].
    pub(super) fn inactive_doc_mut(&mut self, name: &str) -> Option<&mut Document> {
        self.inactive.iter_mut().find(|d| d.name == name)
    }

    /// Make `doc` the active buffer, moving the current one to the back of the
    /// inactive list. Only the swap: callers parse, run hooks, and notify.
    pub(super) fn switch_to_doc(&mut self, doc: Document) {
        let current = self.snapshot_active();
        self.inactive.push(current);
        self.install_active(doc);
    }

    /// Borrow the rope buffer named `name` — the active buffer or any inactive
    /// pane buffer. Used by the cross-buffer ediff primitives.
    pub(super) fn buffer_named(&self, name: &str) -> Option<&Buffer> {
        self.doc(name).map(|d| &d.buffer)
    }

    /// Diff buffers `a` and `b` and hand each non-equal hunk to Steel `callback`.
    pub(super) fn diff_buffers(&self, a: &str, b: &str, callback: &str) {
        let records = match (self.buffer_named(a), self.buffer_named(b)) {
            (Some(abuf), Some(bbuf)) => {
                let (ta, tb) = (abuf.full_text(), bbuf.full_text());
                let diff = similar::TextDiff::from_lines(&ta, &tb);
                diff.ops()
                    .iter()
                    .filter_map(|op| {
                        let (tag, ar, br) = op.as_tag_tuple();
                        let name = match tag {
                            similar::DiffTag::Equal => return None,
                            similar::DiffTag::Delete => "delete",
                            similar::DiffTag::Insert => "insert",
                            similar::DiffTag::Replace => "replace",
                        };
                        let (abs, abe) = line_span(abuf, ar.start, ar.end);
                        let (bbs, bbe) = line_span(bbuf, br.start, br.end);
                        // Hunk text, escaped so each hunk stays a single tab-
                        // delimited record for Scheme: newlines to U+001F, tabs
                        // (Go, Makefiles, TSV) to U+001E. The hunk's final line
                        // break is dropped, or it decoded to an extra empty line.
                        let escape = |t: String| {
                            t.strip_suffix('\n')
                                .unwrap_or(&t)
                                .replace('\n', "\u{1f}")
                                .replace('\t', "\u{1e}")
                        };
                        let atext = escape(abuf.slice_to_string(abs..abe));
                        let btext = escape(bbuf.slice_to_string(bbs..bbe));
                        Some(format!(
                            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            ar.start, ar.end, br.start, br.end, abs, abe, bbs, bbe, atext, btext
                        ))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => String::new(),
        };
        self.script.dispatch_action(callback, Some(&records));
    }

    /// Replace lines [ds,de) of buffer `dst` with lines [ss,se) of buffer `src`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn buffer_copy_lines(
        &mut self,
        src: &str,
        ss: usize,
        se: usize,
        dst: &str,
        ds: usize,
        de: usize,
        cx: &mut Context<Self>,
    ) {
        let text = match self.buffer_named(src) {
            Some(buf) => {
                let (s, e) = line_span(buf, ss, se);
                buf.slice_to_string(s..e)
            }
            None => return,
        };
        if self.active.name == dst {
            let (s, e) = line_span(&self.active.buffer, ds, de);
            self.replace_region(s, e, &text, cx);
        } else if let Some(doc) = self.inactive_doc_mut(dst) {
            let (len, uri, full) = {
                let (s, e) = line_span(&doc.buffer, ds, de);
                doc.buffer.replace(s..e, &text);
                doc.reset_undo(); // unrecorded edit: old records are stale
                doc.modified = true;
                if let Some(h) = doc.highlighter.as_mut() {
                    h.set_text(doc.buffer.rope());
                }
                let len = doc.buffer.len();
                if doc.selected_range.start > len {
                    doc.selected_range.start = len;
                }
                if doc.selected_range.end > len {
                    doc.selected_range.end = len;
                }
                (len, document_uri(doc), doc.buffer.full_text())
            };
            // Like `replace_inactive_text`: an open LSP document must see the
            // new text, or later positions/diagnostics are computed against
            // the stale copy.
            self.lsp_sync_document(&uri, &full);
            // Keep any pane viewing this buffer in bounds after the edit.
            for p in &mut self.panes {
                if p.buffer == dst && p.point > len {
                    p.point = len;
                }
            }
            cx.notify();
        }
    }

    /// Paint `face` over lines [sline,eline) of buffer `name` (active or not).
    pub(super) fn buffer_highlight_lines(
        &mut self,
        name: &str,
        sline: usize,
        eline: usize,
        face: String,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc_mut(name) else {
            return;
        };
        let (s, e) = line_span(&doc.buffer, sline, eline);
        if s < e {
            doc.overlays.faces.push((s..e, SharedString::from(face)));
            cx.notify();
        }
    }

    /// Drop all face overlays from buffer `name` (active or inactive).
    pub(super) fn buffer_clear_highlights(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(doc) = self.doc_mut(name)
            && !doc.overlays.faces.is_empty()
        {
            doc.overlays.faces.clear();
            cx.notify();
        }
    }

    // --- yasnippet session ------------------------------------------------
}

#[cfg(test)]
mod tests {
    use super::{file_identity, uniquified_name};
    use std::path::PathBuf;

    #[test]
    fn duplicate_file_names_get_distinguishing_dir_suffixes() {
        let p = PathBuf::from;
        let taken = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        // Free name → as-is.
        assert_eq!(
            uniquified_name("mod.rs", &p("/x/src/script/mod.rs"), &[], taken(&[])),
            "mod.rs"
        );
        // The nearest differing parent dir.
        assert_eq!(
            uniquified_name(
                "mod.rs",
                &p("/x/src/script/mod.rs"),
                &[p("/x/src/editor/mod.rs")],
                taken(&["mod.rs"]),
            ),
            "mod.rs<script>"
        );
        // Same immediate parent → go up until they differ.
        assert_eq!(
            uniquified_name(
                "lib.rs",
                &p("/w/b/src/lib.rs"),
                &[p("/w/a/src/lib.rs")],
                taken(&["lib.rs"]),
            ),
            "lib.rs<b/src>"
        );
        // A non-file buffer holding the name (no path to compare) still gets
        // a dir suffix; an occupied suffixed name falls through to numbers.
        assert_eq!(
            uniquified_name("notes", &p("/n/notes"), &[], taken(&["notes"])),
            "notes<n>"
        );
        assert_eq!(
            uniquified_name("notes", &p("/notes"), &[], taken(&["notes"])),
            "notes<2>"
        );
    }

    #[test]
    fn file_identity_unifies_spellings_of_one_file() {
        let dir = std::env::temp_dir().join(format!("macros-identity-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("sub/f.txt");
        std::fs::write(&file, "x").unwrap();
        let dotted = dir.join("sub/../sub/./f.txt");
        assert_eq!(file_identity(&dotted), file_identity(&file));
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert_eq!(file_identity(&link), file_identity(&file));
        }
        // Not yet on disk: canonical dir + name, still matching other spellings.
        assert_eq!(
            file_identity(&dir.join("sub/../new.txt")),
            file_identity(&dir.join("new.txt"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Editor-level regressions for buffer kill / redirect / pane bookkeeping, on
/// gpui's headless test platform (no Scheme runtime needed).
#[cfg(test)]
mod editor_state_tests {
    use gpui::{Entity, TestAppContext};

    use super::*;

    fn editor_with(cx: &mut TestAppContext, names: &[&str]) -> Entity<Editor> {
        crate::editor::test_context(cx);
        let editor = cx.new(Editor::new);
        editor.update(cx, |e, _| {
            for name in names {
                e.switch_to_doc(make_document(name.to_string(), None, "aéb\n"));
            }
        });
        editor
    }

    #[gpui::test]
    fn killing_a_modified_file_buffer_by_name_asks_first(cx: &mut TestAppContext) {
        // ibuffer `d`/`x` and helm's "Kill buffer" go through
        // `kill-buffer-named`: unsaved edits must get the same prompt as C-x k.
        let ed = editor_with(cx, &["notes.txt", "scratch", "main.rs"]);
        ed.update(cx, |e, cx| {
            let i = e.inactive_index("notes.txt").unwrap();
            e.inactive[i].path = Some(PathBuf::from("/tmp/notes.txt"));
            e.inactive[i].modified = true;
            e.kill_buffer_named_confirming("notes.txt", cx);
            assert!(e.doc("notes.txt").is_some(), "killed without asking");
            let purpose = e.minibuffer.as_ref().map(|m| m.purpose.clone());
            assert!(matches!(
                &purpose,
                Some(MinibufferPurpose::Confirm(PendingAction::KillBuffer(n))) if n == "notes.txt"
            ));
            // Declining keeps it; confirming kills it.
            e.mb_abort(cx);
            assert!(e.doc("notes.txt").is_some());
            e.perform(PendingAction::KillBuffer("notes.txt".into()), cx);
            assert!(e.doc("notes.txt").is_none());

            // The active buffer is guarded the same way.
            e.active.path = Some(PathBuf::from("/tmp/main.rs"));
            e.active.modified = true;
            e.kill_buffer_named_confirming("main.rs", cx);
            assert_eq!(e.active.name, "main.rs");
            assert!(e.minibuffer.is_some());
            e.mb_abort(cx);

            // A fileless buffer (or an unmodified file) just goes.
            let i = e.inactive_index("scratch").unwrap();
            e.inactive[i].modified = true;
            e.kill_buffer_named_confirming("scratch", cx);
            assert!(e.doc("scratch").is_none());
            assert!(e.minibuffer.is_none());
        });
    }

    fn inactive_names(e: &Editor) -> Vec<String> {
        e.inactive.iter().map(|d| d.name.clone()).collect()
    }

    #[gpui::test]
    fn render_redirect_follows_its_buffer_across_inactive_mutations(cx: &mut TestAppContext) {
        let ed = editor_with(cx, &["a", "b", "c", "d"]);
        ed.update(cx, |e, cx| {
            // Active "d"; redirect at "c".
            e.render_redirect = Some("c".into());
            // Removing an earlier inactive buffer shifts "c" down one slot.
            let first = e.inactive[0].name.clone();
            e.kill_buffer_named(&first, cx);
            let c = e.inactive_index("c");
            assert_eq!(e.redirect_index(), c, "{:?}", inactive_names(e));
            // Activating the redirected buffer ends the redirect (it's active).
            e.activate(c.unwrap(), false, cx);
            assert_eq!(e.active.name, "c");
            assert_eq!(e.render_redirect, None);
            // Killing the redirected buffer ends the redirect, so a later
            // buffer reusing the name doesn't inherit it.
            e.render_redirect = Some("b".into());
            e.kill_buffer_named("b", cx);
            assert_eq!(e.render_redirect, None);
            // Popping the last inactive buffer on an active kill.
            let last = e.inactive.last().unwrap().name.clone();
            e.render_redirect = Some(last);
            e.kill_active_buffer(cx);
            assert_eq!(e.render_redirect, None);
        });
    }

    #[gpui::test]
    fn killing_a_buffer_repoints_every_window_showing_it(cx: &mut TestAppContext) {
        let ed = editor_with(cx, &["a", "b"]);
        ed.update(cx, |e, cx| {
            // Two windows on "b".
            e.split_window(SplitAxis::Vertical, cx);
            assert_eq!(e.panes.len(), 2);
            assert!(e.panes.iter().all(|p| p.buffer == "b"));
            e.kill_active_buffer(cx);
            let shown = e.active.name.clone();
            assert_ne!(shown, "b");
            assert!(
                e.panes.iter().all(|p| p.buffer == shown),
                "every window left the killed buffer"
            );
            // An inactive buffer shown in the other window.
            let other = e.panes.iter().find(|p| p.id != e.selected_pane).unwrap().id;
            e.switch_to_doc(make_document("c".into(), None, "x"));
            let c_pane = e.panes.iter_mut().find(|p| p.id == other).unwrap();
            c_pane.buffer = "zzz".into();
            e.inactive.push(make_document("zzz".into(), None, "y"));
            e.kill_buffer_named("zzz", cx);
            let pane = e.panes.iter().find(|p| p.id == other).unwrap();
            assert_ne!(pane.buffer, "zzz");
            assert!(e.doc(&pane.buffer).is_some(), "repointed at a live buffer");
        });
    }

    #[gpui::test]
    fn retargeting_a_moved_path_repoints_the_buffers_visiting_it(cx: &mut TestAppContext) {
        let ed = editor_with(cx, &[]);
        ed.update(cx, |e, cx| {
            let visit = |e: &mut Editor, name: &str, path: &str| {
                e.switch_to_doc(make_document(name.into(), Some(path.into()), "x"));
            };
            visit(e, "one", "/r/x/one.txt");
            visit(e, "two", "/r/x/sub/two.txt");
            visit(e, "three", "/r/xy/three.txt");
            let path = |e: &Editor, name: &str| e.doc(name).and_then(|d| d.path.clone());
            // A moved directory carries the files under it (a sibling sharing the
            // name prefix does not move).
            e.retarget_file_buffers("/r/x/sub/", "/r/z", cx);
            assert_eq!(path(e, "two"), Some("/r/z/two.txt".into()));
            assert_eq!(path(e, "three"), Some("/r/xy/three.txt".into()));
            // A moved file (the active buffer is retargeted too), name kept.
            e.retarget_file_buffers("/r/xy/three.txt", "/r/3.txt", cx);
            assert_eq!(e.active.name, "three");
            assert_eq!(path(e, "three"), Some("/r/3.txt".into()));
            assert_eq!(path(e, "one"), Some("/r/x/one.txt".into()));
            // An empty path never matches everything.
            e.retarget_file_buffers("", "/elsewhere", cx);
            assert_eq!(path(e, "one"), Some("/r/x/one.txt".into()));
        });
    }

    #[gpui::test]
    fn selecting_a_pane_snaps_its_saved_point_to_a_char_boundary(cx: &mut TestAppContext) {
        let ed = editor_with(cx, &["a"]);
        ed.update(cx, |e, cx| {
            e.split_window(SplitAxis::Vertical, cx);
            let other = e.panes.iter().find(|p| p.id != e.selected_pane).unwrap().id;
            // Byte 2 is inside "é" (bytes 1..3 of "aéb").
            e.panes.iter_mut().find(|p| p.id == other).unwrap().point = 2;
            e.select_pane(other, cx);
            assert!(e.active.buffer.full_text().is_char_boundary(e.point()));
        });
    }

    #[gpui::test]
    fn displaced_grammar_prompt_is_asked_again(cx: &mut TestAppContext) {
        let ed = editor_with(cx, &["a"]);
        ed.update(cx, |e, cx| {
            e.pending_grammar_installs
                .push_back(crate::grammar::InstallSpec {
                    id: "nim".into(),
                    repo: "https://example.invalid/tree-sitter-nim".into(),
                    rev: "main".into(),
                    extensions: vec!["nim".into()],
                    subdir: None,
                    symbol: None,
                    highlights_path: None,
                });
            e.resume_grammar_prompt(cx);
            let purpose = |e: &Editor| e.minibuffer.as_ref().map(|m| m.purpose.clone());
            assert!(matches!(
                purpose(e),
                Some(MinibufferPurpose::ConfirmGrammarInstall)
            ));
            // Another read replaces the prompt; closing it re-asks.
            e.open_minibuffer("Find file: ", MinibufferPurpose::FindFile, "", cx);
            e.mb_abort(cx);
            assert!(matches!(
                purpose(e),
                Some(MinibufferPurpose::ConfirmGrammarInstall)
            ));
            assert_eq!(e.pending_grammar_installs.len(), 1);
            // Declining advances the queue and leaves the minibuffer free.
            e.mb_abort(cx);
            assert!(e.pending_grammar_installs.is_empty());
            assert!(e.minibuffer.is_none());
        });
    }
}
