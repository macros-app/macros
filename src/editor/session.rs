//! Session persistence for [`Editor`] (scheme/session.scm's savehist + desktop):
//! the search ring / command history the editor keeps for it, publishing the
//! rings + buffers + window tree on demand ([`SessionState`]), and the commands
//! that restore them (`set-session-ring!`, `restore-file-buffers`,
//! `set-window-layout`). Also the two lifecycle hooks the Scheme side hangs off:
//! `after-init-hook` (once config has loaded) and `kill-emacs-hook` (on quit).

use gpui::Context;

use super::*;
use crate::script::{LayoutSpec, SessionState};

/// Cap on remembered search strings (Emacs `search-ring-max` is 16; a little
/// more is cheap and makes the persisted ring useful).
const SEARCH_RING_MAX: usize = 64;
/// Cap on remembered `M-x` commands.
const COMMAND_HISTORY_MAX: usize = 200;

/// History rings that exist only for session persistence (the minibuffer
/// history and kill ring live in their own fields).
pub(super) struct SessionRings {
    /// Completed search strings, oldest first.
    pub(super) search_ring: BoundedRing,
    /// Commands run via `M-x`, oldest first; `helm-M-x` lists them first.
    pub(super) command_history: BoundedRing,
}

impl Default for SessionRings {
    fn default() -> Self {
        SessionRings {
            search_ring: BoundedRing::new(SEARCH_RING_MAX),
            command_history: BoundedRing::new(COMMAND_HISTORY_MAX),
        }
    }
}

/// A capped ring of strings, oldest first: a push past `cap` evicts the
/// oldest entry (O(1), from the front). Backs the session rings and the
/// `*Messages*` / `*lsp-log*` / MCP scrollback logs.
pub(super) struct BoundedRing {
    items: VecDeque<String>,
    cap: usize,
}

impl BoundedRing {
    pub(super) fn new(cap: usize) -> Self {
        BoundedRing {
            items: VecDeque::new(),
            cap,
        }
    }

    /// Append `item` as the newest entry, evicting the oldest past the cap.
    pub(super) fn push(&mut self, item: String) {
        self.items.push_back(item);
        while self.items.len() > self.cap {
            self.items.pop_front();
        }
    }

    /// Like [`Self::push`], but an older duplicate of `item` is dropped first
    /// (so it moves to the newest slot), and a blank item is ignored.
    pub(super) fn push_dedup(&mut self, item: &str) {
        if item.is_empty() {
            return;
        }
        self.items.retain(|s| s != item);
        self.push(item.to_string());
    }

    /// Replace the contents with the newest `cap` of `items` (oldest first).
    pub(super) fn replace(&mut self, items: Vec<String>) {
        self.items = newest(items, self.cap).into();
    }

    /// Empty the ring, returning its old contents as a ring of the same cap.
    pub(super) fn take(&mut self) -> BoundedRing {
        std::mem::replace(self, BoundedRing::new(self.cap))
    }

    pub(super) fn iter(&self) -> std::collections::vec_deque::Iter<'_, String> {
        self.items.iter()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn last(&self) -> Option<&String> {
        self.items.back()
    }

    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.items.clear();
    }

    /// The entries, oldest first, joined by `sep`.
    pub(super) fn join(&self, sep: &str) -> String {
        self.join_newest(self.items.len(), sep)
    }

    /// The newest `count` entries, oldest first, joined by `sep`.
    pub(super) fn join_newest(&self, count: usize, sep: &str) -> String {
        let skip = self.items.len().saturating_sub(count);
        let mut out = String::new();
        for (i, item) in self.items.iter().skip(skip).enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push_str(item);
        }
        out
    }

    pub(super) fn to_vec(&self) -> Vec<String> {
        self.items.iter().cloned().collect()
    }
}

/// Keep only the newest `max` entries of `items` (oldest first).
fn newest(mut items: Vec<String>, max: usize) -> Vec<String> {
    if items.len() > max {
        let excess = items.len() - max;
        items.drain(..excess);
    }
    items
}

/// Largest char boundary `<= pos` in `text` (clamped to its length).
fn char_floor(text: &str, pos: usize) -> usize {
    text.floor_char_boundary(pos)
}

impl Editor {
    /// Record a completed search string (isearch RET, evil `/` `?`).
    pub(super) fn note_search(&mut self, query: &str) {
        self.session.search_ring.push_dedup(query);
    }

    /// Record a command run via `M-x` (minibuffer or Helm).
    pub(super) fn note_command(&mut self, name: &str) {
        self.session.command_history.push_dedup(name);
        self.emacs_note_mx(name);
    }

    /// `candidates` reordered so recently run commands come first (newest
    /// first), the rest keeping their order — helm-M-x's recency ranking.
    pub(super) fn commands_by_recency(&self, candidates: &[String]) -> Vec<String> {
        let recent: Vec<String> = self
            .session
            .command_history
            .iter()
            .rev()
            .filter(|c| candidates.contains(c))
            .cloned()
            .collect();
        let mut out = recent.clone();
        out.extend(candidates.iter().filter(|c| !recent.contains(c)).cloned());
        out
    }

    /// Absolute path string for a document path (relative paths resolve against
    /// the startup cwd, as in `sync_buffers`).
    fn session_abs(&self, path: &Path) -> String {
        if path.is_absolute() {
            path.to_string_lossy().into_owned()
        } else {
            Path::new(&self.process_cwd)
                .join(path)
                .to_string_lossy()
                .into_owned()
        }
    }

    /// The visited-file path of buffer `name` ("" if fileless or unknown).
    fn session_path_of(&self, name: &str) -> String {
        let doc_path = self.doc(name).and_then(|d| d.path.as_deref());
        doc_path.map_or_else(String::new, |p| self.session_abs(p))
    }

    /// The live window tree as a [`LayoutSpec`].
    pub(super) fn session_layout(&self, layout: &WindowLayout) -> LayoutSpec {
        match layout {
            WindowLayout::Leaf(id) => {
                let buffer = if *id == self.selected_pane {
                    self.active.name.clone()
                } else {
                    self.pane(*id)
                        .map_or_else(|| self.active.name.clone(), |p| p.buffer.clone())
                };
                LayoutSpec::Leaf {
                    path: self.session_path_of(&buffer),
                    buffer,
                    point: self.pane_point(*id),
                    selected: *id == self.selected_pane,
                }
            }
            WindowLayout::Split { axis, ratio, a, b } => LayoutSpec::Split {
                vertical: axis.is_vertical(),
                ratio: *ratio,
                a: Box::new(self.session_layout(a)),
                b: Box::new(self.session_layout(b)),
            },
        }
    }

    /// Publish the rings, file buffers, and window tree for Scheme to read.
    pub(super) fn publish_session(&self) {
        let mut file_buffers: Vec<(String, usize)> = self
            .inactive
            .iter()
            // Grid buffers are special (read-only rope) but visit a file, so
            // they're restored like any file buffer.
            .filter(|d| d.special.is_none() || d.grid.is_some())
            .filter_map(|d| {
                let path = d.path.as_deref()?;
                let point = if d.selection_reversed {
                    d.selected_range.start
                } else {
                    d.selected_range.end
                };
                Some((self.session_abs(path), point))
            })
            .collect();
        if (self.active.special.is_none() || self.active.grid.is_some())
            && let Some(path) = self.active.path.as_deref()
        {
            file_buffers.push((self.session_abs(path), self.cursor_offset()));
        }
        self.script.set_session(SessionState {
            minibuffer_history: self.minibuffer_history.entries().to_vec(),
            kill_ring: self.kill_ring.clone(),
            search_ring: self.session.search_ring.to_vec(),
            command_history: self.session.command_history.to_vec(),
            file_buffers,
            layout: Some(self.session_layout(&self.layout)),
        });
    }

    /// Session commands; `Err` hands anything else back to the dispatcher.
    pub(super) fn cmd_session(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            SessionRefresh(callback) => {
                self.publish_session();
                self.publish_point();
                self.script.dispatch_action(&callback, None);
            }
            SetSessionRing { ring, items } => self.set_session_ring(&ring, items),
            RestoreFileBuffers(entries) => self.restore_file_buffers(entries, cx),
            SetWindowLayout(spec) => self.set_window_layout(spec, cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// Replace one ring wholesale (savehist's restore).
    fn set_session_ring(&mut self, ring: &str, items: Vec<String>) {
        let items: Vec<String> = items.into_iter().filter(|s| !s.is_empty()).collect();
        match ring {
            "minibuffer" => {
                self.minibuffer_history.replace(items);
            }
            "kill" => {
                // The kill ring's own cap still applies (oldest evicted).
                self.kill_ring = newest(items, super::kill_ring::KILL_RING_MAX);
                self.kill_index = self.kill_ring.len().saturating_sub(1);
            }
            "search" => {
                self.session.search_ring.replace(items);
                // An empty C-s after a restart repeats the last search.
                if let Some(last) = self.session.search_ring.last() {
                    self.last_isearch = Some(last.clone());
                }
            }
            "command" => self.session.command_history.replace(items),
            _ => {}
        }
    }

    /// Visit each `(path, point)` file as a background buffer. Already-open
    /// files just get their point moved; missing or unreadable ones are
    /// skipped silently. Reads synchronously — this is a startup/restore path
    /// and the files are ones the user had open.
    pub(super) fn restore_file_buffers(
        &mut self,
        entries: Vec<(String, usize)>,
        cx: &mut Context<Self>,
    ) {
        let mut opened = 0;
        for (path, point) in entries {
            let path = PathBuf::from(expand_path(&path));
            // "Already open" by canonical identity, as `find_file` decides it —
            // a symlinked / relative spelling must not open a second buffer.
            let identity = super::buffers::file_identity(&path);
            let visits = |d: &Document| {
                d.path
                    .as_deref()
                    .is_some_and(|p| super::buffers::file_identity(p) == identity)
            };
            if visits(&self.active) {
                let p = crate::script::snap_byte(self.active.buffer.rope(), point);
                self.move_to(p, cx);
                continue;
            }
            if let Some(doc) = self.inactive.iter_mut().find(|d| visits(d)) {
                let p = crate::script::snap_byte(doc.buffer.rope(), point);
                doc.selected_range = p..p;
                doc.selection_reversed = false;
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue; // vanished / unreadable: skip silently
            };
            let Some(name) = path.file_name().map(|f| f.to_string_lossy().into_owned()) else {
                continue;
            };
            let p = char_floor(&contents, point);
            // Two restored `mod.rs` files get distinct names (`mod.rs<a>`), as
            // a visit would give them — buffer names are identity everywhere.
            let name = self.unique_buffer_name(&name, &path);
            let mut doc = make_document(name, Some(path), &contents);
            doc.selected_range = p..p;
            doc.selection_reversed = false;
            self.inactive.push(doc);
            opened += 1;
        }
        if opened > 0 {
            self.sync_buffers();
            self.reconcile_watches(cx);
        }
        cx.notify();
    }

    /// Build the pane tree for `spec`, pushing a fresh [`Pane`] per leaf.
    /// Returns the layout and the id of the leaf marked selected (if any).
    fn build_layout(
        &mut self,
        spec: &LayoutSpec,
        panes: &mut Vec<Pane>,
        selected: &mut Option<usize>,
    ) -> WindowLayout {
        match spec {
            LayoutSpec::Leaf {
                buffer,
                path,
                point,
                selected: is_selected,
            } => {
                let id = self.next_pane_id;
                self.next_pane_id += 1;
                let wanted = if path.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(path))
                };
                // By canonical identity, as the buffers were opened (a symlinked
                // or relative spelling must still find its window's buffer).
                let by_path = wanted.as_ref().and_then(|w| {
                    let identity = super::buffers::file_identity(w);
                    self.docs()
                        .find(|d| {
                            d.path
                                .as_deref()
                                .is_some_and(|p| super::buffers::file_identity(p) == identity)
                        })
                        .map(|d| d.name.clone())
                });
                let name = by_path
                    .or_else(|| self.has_buffer(buffer).then(|| buffer.clone()))
                    .unwrap_or_else(|| self.active.name.clone());
                panes.push(Pane {
                    id,
                    buffer: name,
                    point: *point,
                    scroll: px(0.),
                });
                if *is_selected && selected.is_none() {
                    *selected = Some(id);
                }
                WindowLayout::Leaf(id)
            }
            LayoutSpec::Split {
                vertical,
                ratio,
                a,
                b,
            } => {
                let a = self.build_layout(a, panes, selected);
                let b = self.build_layout(b, panes, selected);
                WindowLayout::Split {
                    axis: SplitAxis::from_vertical(*vertical),
                    ratio: ratio.clamp(0.1, 0.9),
                    a: Box::new(a),
                    b: Box::new(b),
                }
            }
        }
    }

    /// Replace the window tree with `spec`. Leaves resolve their buffer by path,
    /// then by name, else show the current buffer. The selected leaf's buffer
    /// is activated INCIDENTALLY (buffer-switch-hook only): a restore must not
    /// re-point the perspectives title bar on its own.
    pub(super) fn set_window_layout(&mut self, spec: LayoutSpec, cx: &mut Context<Self>) {
        self.save_selected_pane();
        let mut panes = Vec::new();
        let mut selected = None;
        let layout = self.build_layout(&spec, &mut panes, &mut selected);
        let Some(target) = selected.or_else(|| panes.first().map(|p| p.id)) else {
            return;
        };
        let (target_buffer, target_point) = panes
            .iter()
            .find(|p| p.id == target)
            .map(|p| (p.buffer.clone(), p.point))
            .unwrap_or_default();
        // Snap every pane's point into its buffer (a file may have shrunk).
        for pane in &mut panes {
            let rope = self.doc(&pane.buffer).map(|d| d.buffer.rope());
            pane.point = rope.map_or(0, |r| crate::script::snap_byte(r, pane.point));
        }
        self.layout = layout;
        self.panes = panes;
        self.pane_bounds.clear();
        self.selected_pane = target;
        if target_buffer != self.active.name {
            self.switch_to_buffer_ex(&target_buffer, false, cx);
        }
        let p = crate::script::snap_byte(self.active.buffer.rope(), target_point);
        self.move_to(p, cx);
        cx.notify();
    }

    /// Quit the app once this window's unsaved buffers are dealt with (evil
    /// `:q` / `:wq` family). Other windows with unsaved buffers are still
    /// prompted; `prepare_quit` runs savehist / desktop hooks in each window.
    pub(super) fn quit_now(&mut self, cx: &mut Context<Self>) {
        // This window's unsaved buffers were already resolved by the caller;
        // route through the app-wide quit so other windows still get their
        // save prompt and every window runs `kill-emacs-hook`.
        crate::workspace::begin_app_quit(false, cx);
        self.finish_quit(false, cx);
    }

    /// Run `after-init-hook` synchronously, once startup + user config have
    /// loaded (Emacs `after-init-hook`). Session restore hangs off this so a
    /// user's init.scm can opt out first, and so it lands before the reveal.
    pub fn run_after_init(&mut self, cx: &mut Context<Self>) {
        self.run_hook_blocking("after-init-hook", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundedRing, char_floor, newest};

    #[test]
    fn push_dedup_dedupes_and_evicts_oldest() {
        let mut ring = BoundedRing::new(3);
        for s in ["a", "b", "", "a", "c", "d"] {
            ring.push_dedup(s);
        }
        // "" ignored; the re-pushed "a" moved to the newest slot, then the
        // cap of 3 evicted "b" (the oldest).
        assert_eq!(ring.to_vec(), vec!["a", "c", "d"]);
        assert_eq!(ring.join_newest(2, "|"), "c|d");
        assert_eq!(
            newest(vec!["1".into(), "2".into(), "3".into()], 2),
            vec!["2", "3"]
        );
    }

    #[test]
    fn push_drops_oldest_over_cap() {
        let mut ring = BoundedRing::new(3);
        for i in 0..5 {
            ring.push(format!("m{i}"));
        }
        // Keeps the newest `cap`, oldest dropped, order preserved.
        assert_eq!(ring.to_vec(), vec!["m2", "m3", "m4"]);
        assert_eq!(ring.join(" "), "m2 m3 m4");
    }

    #[test]
    fn char_floor_snaps_into_multibyte_text() {
        let s = "aé b"; // 'é' is 2 bytes at 1..3
        assert_eq!(char_floor(s, 2), 1);
        assert_eq!(char_floor(s, 3), 3);
        assert_eq!(char_floor(s, 99), s.len());
    }
}
