//! Emacs file and buffer commands for [`Editor`]: `write-file`,
//! `rename-buffer`, `insert-file`, the `C-x 4` other-window family,
//! `read-only-mode`, and the per-file housekeeping behind the
//! `make-backup-files` and `create-lockfiles` options (see [`crate::save`] and
//! [`crate::filelock`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{Context, px};

use super::buffers::file_identity;
use super::*;

impl Editor {
    /// Apply the file options that live outside the editor — where backups go
    /// and whether lock files are taken. Run at startup and on every
    /// `set-option`.
    pub(super) fn sync_file_options(&mut self, cx: &mut Context<Self>) {
        // Some unit tests build an editor without app settings.
        let Some(settings) = cx.try_global::<crate::settings::Settings>() else {
            return;
        };
        let backups = settings
            .make_backup_files()
            .then(|| settings.backup_directory());
        crate::save::set_backup_directory(backups);
        crate::filelock::set_enabled(settings.create_lockfiles());
        self.reconcile_locks(cx);
    }

    // --- write-file / rename-buffer / insert-file ---------------------------

    /// `C-x C-w` (Emacs `write-file`): save the buffer under `input` and visit
    /// that file from now on, renaming the buffer after it. A directory (or a
    /// path ending in `/`) gets the buffer's file name. Overwriting another
    /// existing file asks first unless `confirmed`.
    pub(super) fn write_file(&mut self, input: &str, confirmed: bool, cx: &mut Context<Self>) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        let mut target = PathBuf::from(expand_path(input));
        if input.ends_with('/') || target.is_dir() {
            let name = self
                .active
                .path
                .as_deref()
                .and_then(Path::file_name)
                .map_or_else(|| self.active.name.clone(), |n| n.to_string_lossy().into());
            target.push(name);
        }
        if self.active.special.is_some() {
            self.echo(format!("{} can't visit a file", self.active.name), cx);
            return;
        }
        // A grid keeps its own file (its formats and sheets ride on it); `:w`
        // semantics — write a copy — are the safe reading.
        if self.active.grid.is_some() {
            self.write_buffer_to(&target.to_string_lossy(), cx);
            return;
        }
        let identity = file_identity(&target);
        let own = self
            .active
            .path
            .as_deref()
            .is_some_and(|p| file_identity(p) == identity);
        if !own
            && let Some(other) = self.inactive.iter().find(|d| {
                d.path
                    .as_deref()
                    .is_some_and(|p| file_identity(p) == identity)
            })
        {
            let message = format!("{} is already visited by {}", target.display(), other.name);
            self.echo(message, cx);
            return;
        }
        if !own && !confirmed && target.exists() {
            self.open_confirm(
                &format!("File {} exists; overwrite?", target.display()),
                PendingAction::WriteFile {
                    buffer: self.active.name.clone(),
                    path: target.to_string_lossy().into_owned(),
                },
                cx,
            );
            return;
        }
        if !own {
            self.set_visited_file(target, cx);
        }
        self.save_buffer_ex(false, cx);
    }

    /// Point the active buffer at `path` (Emacs `set-visited-file-name`): the
    /// buffer takes the file's name, language servers see the old document
    /// close and the new one open, and the file watch follows.
    fn set_visited_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let old_uri = document_uri(&self.active);
        self.lsp_close_document(&old_uri);
        let base = path
            .file_name()
            .map_or_else(|| self.active.name.clone(), |n| n.to_string_lossy().into());
        self.active.path = Some(path.clone());
        // The file is new to this buffer: its baseline is whatever is there now
        // (we're about to overwrite it, having asked), so a save deferred by
        // format-on-save doesn't ask again.
        let stamp = crate::save::stamp(&path);
        let on_disk = std::fs::read(&path).unwrap_or_default();
        crate::save::note_synced(&path, stamp, &on_disk);
        if base != self.active.name {
            let name = self.unique_buffer_name(&base, &path);
            self.rename_active(name);
        }
        if let Some(language) = self.active_language() {
            self.lsp_open_active(&language);
        }
        self.sync_buffers();
        self.reconcile_watches(cx);
    }

    /// `M-x rename-buffer`: give the active buffer a new, unused name.
    pub(super) fn rename_buffer(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() {
            self.echo("Empty buffer name", cx);
            return;
        }
        if name == self.active.name {
            return;
        }
        if self.has_buffer(name) {
            self.echo(format!("Buffer name `{name}' is in use"), cx);
            return;
        }
        // An untitled buffer's language-server URI is its name.
        let old_uri = document_uri(&self.active);
        let renamed_uri = self.active.path.is_none();
        if renamed_uri {
            self.lsp_close_document(&old_uri);
        }
        self.rename_active(name.to_string());
        if renamed_uri && let Some(language) = self.active_language() {
            self.lsp_open_active(&language);
        }
        self.sync_buffers();
        cx.notify();
    }

    /// Rename the active buffer, carrying along everything that refers to it
    /// by name: the windows showing it, its terminal and processes, and a
    /// render redirect.
    pub(super) fn rename_active(&mut self, name: String) {
        let old = std::mem::replace(&mut self.active.name, name.clone());
        for pane in self.panes.iter_mut().filter(|p| p.buffer == old) {
            pane.buffer = name.clone();
        }
        for terminal in self.terminals.values_mut() {
            if terminal.buffer_name == old {
                terminal.buffer_name = name.clone();
            }
        }
        self.rename_process_buffers(&old, &name);
        if self.render_redirect.as_deref() == Some(old.as_str()) {
            self.render_redirect = Some(name);
        }
    }

    /// `C-x i` (Emacs `insert-file`): insert the contents of a file at point,
    /// leaving point before the inserted text.
    pub(super) fn insert_file(&mut self, input: &str, cx: &mut Context<Self>) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        let path = PathBuf::from(expand_path(input));
        if self.active.grid.is_some() || self.active.special.is_some() {
            self.echo("Can't insert a file into this buffer", cx);
            return;
        }
        if self.refuse_read_only(cx) {
            return;
        }
        let text = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    self.echo(format!("{} is not UTF-8 text", path.display()), cx);
                    return;
                }
            },
            Err(err) => {
                self.echo(format!("Can't insert {}: {err}", path.display()), cx);
                return;
            }
        };
        let at = self.point();
        self.edit(at..at, &text);
        self.collapse_to(at);
        self.ensure_cursor_visible();
        self.echo(format!("Inserted {}", path.display()), cx);
    }

    // --- C-x 4: other window ----------------------------------------------

    /// The window `C-x 4` commands show things in: the next one in cycle order,
    /// or a new one split off the selected window when it's the only one. The
    /// selection doesn't move.
    fn other_window_for_display(&mut self, cx: &mut Context<Self>) -> usize {
        let leaves = self.layout.leaves();
        if leaves.len() < 2 {
            let new_id = self.next_pane_id; // `split_window` gives the new pane this id
            self.split_window(SplitAxis::Horizontal, cx);
            return new_id;
        }
        let pos = leaves
            .iter()
            .position(|&id| id == self.selected_pane)
            .unwrap_or(0);
        leaves[(pos + 1) % leaves.len()]
    }

    /// `C-x 4 f`: visit a file in another window, and select it.
    pub(super) fn find_file_other_window(&mut self, path: &str, cx: &mut Context<Self>) {
        if path.trim().is_empty() {
            return;
        }
        let target = self.other_window_for_display(cx);
        self.select_pane(target, cx);
        self.find_file(path, cx);
    }

    /// `C-x 4 b`: show a buffer in another window, and select it.
    pub(super) fn switch_to_buffer_other_window(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if !self.has_buffer(name) {
            self.echo(format!("No buffer named {name}"), cx);
            return;
        }
        let target = self.other_window_for_display(cx);
        self.select_pane(target, cx);
        self.switch_to_buffer(name, cx);
    }

    /// `C-x 4 C-o` (Emacs `display-buffer`): show a buffer in another window
    /// without selecting it.
    pub(super) fn display_buffer(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        let point = if name == self.active.name {
            self.point()
        } else if let Some(doc) = self.inactive_doc(name) {
            doc.selected_range.end
        } else {
            self.echo(format!("No buffer named {name}"), cx);
            return;
        };
        let target = self.other_window_for_display(cx);
        if let Some(pane) = self.panes.iter_mut().find(|p| p.id == target) {
            pane.buffer = name.to_string();
            pane.point = point;
            pane.scroll = px(0.0);
        }
        cx.notify();
    }

    // --- read-only-mode ----------------------------------------------------

    /// `C-x C-q` (Emacs `read-only-mode`): toggle whether the buffer's text can
    /// be changed. The mode line shows `%%` while it can't.
    pub(super) fn toggle_read_only(&mut self, cx: &mut Context<Self>) {
        if self.active.special.is_some() || self.active.grid.is_some() {
            self.echo(
                format!("{} has its own editing rules", self.active.name),
                cx,
            );
            return;
        }
        self.active.read_only = !self.active.read_only;
        let state = if self.active.read_only {
            "enabled"
        } else {
            "disabled"
        };
        self.echo(
            format!("Read-Only mode {state} in {}", self.active.name),
            cx,
        );
    }

    /// If the active buffer is read-only, say so and return `true` — the
    /// check every text change makes first.
    pub(super) fn refuse_read_only(&mut self, cx: &mut Context<Self>) -> bool {
        if self.active.read_only {
            self.echo(format!("Buffer is read-only: {}", self.active.name), cx);
        }
        self.active.read_only
    }

    // --- Lock files --------------------------------------------------------

    /// This window's id as a lock holder (several windows share a process).
    fn lock_holder(cx: &Context<Self>) -> u64 {
        cx.entity_id().as_u64()
    }

    /// Hold a lock on exactly the files this window has unsaved changes to
    /// (none when `create-lockfiles` is off), warning once per file when
    /// another live process holds its lock. Cheap when there's nothing to do;
    /// run after every command and keystroke.
    pub(super) fn reconcile_locks(&mut self, cx: &mut Context<Self>) {
        let holder = Self::lock_holder(cx);
        let enabled = crate::filelock::enabled();
        let held: HashSet<PathBuf> = crate::filelock::held_by(holder).into_iter().collect();
        if !enabled && held.is_empty() {
            return;
        }
        let wanted: HashSet<PathBuf> = if enabled {
            self.docs()
                .filter(|d| d.modified)
                .filter_map(|d| d.path.as_deref())
                .map(crate::save::write_target)
                .collect()
        } else {
            HashSet::new()
        };
        for path in held.difference(&wanted) {
            crate::filelock::release(path);
        }
        let mut warning = None;
        for path in wanted.difference(&held) {
            if let Err(other) = crate::filelock::acquire(holder, path)
                && crate::filelock::first_warning(path)
            {
                warning = Some(lock_warning(path, &other));
            }
        }
        if let Some(warning) = warning {
            self.echo(warning, cx);
        }
    }

    /// On visiting `path`: warn if another live process is editing it.
    pub(super) fn warn_if_locked(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !crate::filelock::enabled() {
            return;
        }
        let path = crate::save::write_target(path);
        if let Some(other) = crate::filelock::locked_by_other(&path) {
            crate::filelock::first_warning(&path);
            self.echo(lock_warning(&path, &other), cx);
        }
    }

    /// Give up every lock this window holds (it's closing or quitting).
    pub(super) fn release_locks(&mut self, cx: &mut Context<Self>) {
        for path in crate::filelock::held_by(Self::lock_holder(cx)) {
            crate::filelock::release(&path);
        }
    }
}

fn lock_warning(path: &Path, other: &crate::filelock::LockOwner) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    format!("{name} is locked by {other} — another session may be editing it")
}
