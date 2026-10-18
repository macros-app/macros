//! Undo/redo tree and undo grouping for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use gpui::Context;

use super::*;

/// A reversible edit at byte `at`: it replaced `removed` with `inserted`.
/// Undo restores `removed` over the inserted span; redo re-applies `inserted`
/// over the removed span. Storing both texts makes undo and redo symmetric,
/// which is what the undo *tree* (branching history) needs.
#[derive(Clone)]
pub(super) struct UndoRecord {
    pub at: usize,
    pub removed: String,
    pub inserted: String,
}

/// One state in the undo tree. The root (index 0) is the buffer as loaded;
/// every other node carries the edit group that transforms its parent into it.
/// Branches are preserved: undoing then typing adds a *sibling* child rather
/// than discarding the redo branch (the whole point of an undo tree / vundo).
#[derive(Clone)]
pub(super) struct UndoNode {
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// Edits (in application order) that take `parent`'s state to this node's.
    pub edit: Vec<UndoRecord>,
}

/// A document's branching undo history: the nodes (index 0 is the root, the
/// buffer as loaded) and which one the buffer is at now.
#[derive(Clone)]
pub(super) struct UndoTree {
    pub nodes: Vec<UndoNode>,
    pub current: usize,
}

impl Default for UndoTree {
    /// A fresh tree: a single root node with no edits.
    fn default() -> Self {
        Self {
            nodes: vec![UndoNode {
                parent: None,
                children: Vec::new(),
                edit: Vec::new(),
            }],
            current: 0,
        }
    }
}

impl UndoTree {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Start a NEW node holding `record`, branching off the current one, and
    /// make it current — undoing then editing keeps the old branch as a
    /// sibling (undo tree, not a stack).
    pub fn record(&mut self, record: UndoRecord) {
        let node = self.nodes.len();
        let parent = self.current;
        self.nodes.push(UndoNode {
            parent: Some(parent),
            children: Vec::new(),
            edit: vec![record],
        });
        self.nodes[parent].children.push(node);
        self.current = node;
    }

    /// Append `record` to the current node's edit group. At the root (history
    /// was just reset under an open group) there is no group to extend — the
    /// root can never be undone — so it starts a new node instead.
    pub fn extend_current(&mut self, record: UndoRecord) {
        if self.current == 0 {
            self.record(record);
        } else {
            self.nodes[self.current].edit.push(record);
        }
    }

    /// What undo replays: the current node's parent and the edit group that
    /// leads from it here. `None` at the root.
    pub fn undo_group(&self) -> Option<(usize, Vec<UndoRecord>)> {
        let node = self.nodes.get(self.current)?;
        Some((node.parent?, node.edit.clone()))
    }

    /// What redo replays: the most recently created child of the current node
    /// (undo-tree semantics — the last branch you came from / created) and its
    /// edit group.
    pub fn redo_child(&self) -> Option<(usize, Vec<UndoRecord>)> {
        let child = *self.nodes.get(self.current)?.children.last()?;
        Some((child, self.nodes.get(child)?.edit.clone()))
    }

    /// How many redo branches leave node `idx`.
    pub fn branches(&self, idx: usize) -> usize {
        self.nodes[idx].children.len()
    }

    /// Fold every node added since the tree had `base_len` nodes into the
    /// first of them, so a multi-run command (`3dd`) undoes in one step. Only
    /// when the new nodes form a straight chain off `base_current` ending at
    /// the current node — anything else (an undo mid-way) is left untouched.
    pub fn squash_since(&mut self, base_len: usize, base_current: usize) {
        if self.nodes.len() <= base_len + 1 {
            return;
        }
        let chain = (base_len..self.nodes.len()).all(|i| {
            let parent = if i == base_len { base_current } else { i - 1 };
            self.nodes[i].parent == Some(parent)
        });
        if !chain || self.current != self.nodes.len() - 1 {
            return;
        }
        let rest: Vec<UndoRecord> = self
            .nodes
            .drain(base_len + 1..)
            .flat_map(|node| node.edit)
            .collect();
        let first = &mut self.nodes[base_len];
        first.edit.extend(rest);
        first.children.clear();
        self.current = base_len;
    }

    /// A textual dump for the vundo visualizer: one
    /// "idx\tparent\tcurrent?\tdepth\tsize" line per node (depth = distance from
    /// root, size = edits in the node's group).
    pub fn dump(&self) -> String {
        self.nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let mut depth = 0;
                let mut p = n.parent;
                while let Some(parent) = p {
                    depth += 1;
                    p = self.nodes[parent].parent;
                }
                let parent = n.parent.map_or(-1, |x| x as isize);
                let current = if i == self.current { 1 } else { 0 };
                format!("{i}\t{parent}\t{current}\t{depth}\t{}", n.edit.len())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The editor-wide undo grouping state: while a group is open, edits append
/// to one undo node (so a multi-edit Scheme command undoes as one unit).
#[derive(Default)]
pub(super) struct UndoGrouping {
    /// Nesting depth of open undo groups. While > 0, edits append to the current
    /// undo group; otherwise each edit is its own group. Counted, so a nested
    /// begin/end (a `C-u N` repeat or `:s` inside a Scheme batch) merges into
    /// the outer group.
    depth: usize,
    /// Bumped each time an outermost undo group opens, so grid edits can tell
    /// which Scheme command batch they belong to (one batch = one undo step).
    batch_seq: u64,
    /// Whether the current group has received an edit yet (lazily created).
    started: bool,
    /// The buffer the open group's node lives in. An edit in another buffer
    /// (the batch switched buffers) starts its own node there instead of
    /// appending to whatever that buffer's current node happens to be.
    buffer: String,
}

impl UndoGrouping {
    /// The open outermost group's batch number, `None` outside any group.
    pub fn batch(&self) -> Option<u64> {
        (self.depth > 0).then_some(self.batch_seq)
    }

    /// Close the current group's node: the next edit starts a new one.
    pub fn boundary(&mut self) {
        self.started = false;
    }
}

impl Document {
    /// Forget all undo history. Required whenever the text changes WITHOUT an
    /// undo record (a special buffer re-rendered wholesale, an unrecorded
    /// rewrite of an inactive buffer): the old records address text that no
    /// longer exists, and replaying one would corrupt the buffer.
    pub(super) fn reset_undo(&mut self) {
        self.undo = UndoTree::default();
    }
}

/// Whether `expect` is exactly the text at byte `at` — i.e. an undo record
/// still describes the buffer. Every record is checked before it's replayed,
/// so a history gone stale through some unrecorded mutation is detected and
/// dropped instead of splicing text at wrong (possibly mid-char) offsets.
fn record_matches(buffer: &Buffer, at: usize, expect: &str) -> bool {
    let end = at + expect.len();
    end <= buffer.len()
        && buffer.snap(at) == at
        && buffer.snap(end) == end
        && buffer.slice_to_string(at..end) == expect
}

impl Editor {
    /// Record `record` (an edit about to apply) in the active buffer's undo
    /// tree: appended to the open group's node when grouping (one Scheme
    /// command = one undo), otherwise as a new node branching off the current
    /// one.
    pub(super) fn record_undo(&mut self, record: UndoRecord) {
        let g = &mut self.undo_grouping;
        let grouping = g.depth > 0;
        if grouping && g.started && g.buffer == self.active.name {
            self.active.undo.extend_current(record);
        } else {
            self.active.undo.record(record);
            g.started = grouping;
            if grouping && g.buffer != self.active.name {
                g.buffer.clone_from(&self.active.name);
            }
        }
    }

    /// Replay one node's edit group — inverted (undo, last record first) or
    /// forward (redo) — through [`Self::apply_tracked_replace`], so overlays,
    /// markers, marks and LSP stay in step exactly as with `edit()`. Returns
    /// the cursor after the last applied record, and whether every record
    /// applied (`false` = history was stale; replay stopped at the first
    /// record that no longer matched the text).
    fn replay_undo_group(&mut self, mut group: Vec<UndoRecord>, forward: bool) -> (usize, bool) {
        let mut cursor = self.point();
        if !forward {
            group.reverse();
        }
        for record in group {
            let (old, new) = if forward {
                (&record.removed, &record.inserted)
            } else {
                (&record.inserted, &record.removed)
            };
            if !record_matches(&self.active.buffer, record.at, old) {
                return (cursor, false);
            }
            cursor = self.apply_tracked_replace(record.at..record.at + old.len(), new);
        }
        (cursor, true)
    }

    /// Shared tail of undo/redo: settle the buffer after a replay. On a stale
    /// history (`ok == false`) the tree is dropped — the text is intact, only
    /// the ability to step further is lost — and the user is told why.
    fn finish_undo_replay(&mut self, cursor: usize, ok: bool, cx: &mut Context<Self>) -> bool {
        self.undo_grouping.boundary(); // next edit branches a new node
        self.active.modified = true;
        self.parse_full();
        self.collapse_to(self.active.buffer.snap(cursor));
        self.ensure_cursor_visible();
        // Emacs runs `after-change-functions` for undo like any other edit —
        // this is what schedules org/markdown's debounced refontify.
        self.run_hook("after-change-functions");
        if !ok {
            self.active.reset_undo();
            self.echo(
                "Undo history no longer matches the buffer; history cleared",
                cx,
            );
        }
        ok
    }

    pub(super) fn undo(&mut self, cx: &mut Context<Self>) {
        let Some((parent, group)) = self.active.undo.undo_group() else {
            self.echo("No further undo information", cx);
            return;
        };
        // Undo mutates the buffer directly (below), bypassing `edit()` — so the
        // ghost suggestion must be dropped here too, or a stale one could
        // re-anchor over rewritten content when a later move/redo puts point
        // back on its byte.
        self.active.ghost = None;
        // Invert the current node's edit group (last edit undone first):
        // restore `removed` over the inserted span.
        let (cursor, ok) = self.replay_undo_group(group, false);
        if ok {
            self.active.undo.current = parent;
        }
        if !self.finish_undo_replay(cursor, ok, cx) {
            return;
        }
        let branches = self.active.undo.branches(parent);
        if branches > 1 {
            self.echo(format!("Undo (branch point: {branches} redo branches)"), cx);
        } else {
            self.echo("Undo", cx);
        }
    }

    /// Redo: re-apply the most recently created child branch of the current node
    /// (undo-tree semantics — the last branch you came from / created).
    pub(super) fn redo(&mut self, cx: &mut Context<Self>) {
        let Some((child, group)) = self.active.undo.redo_child() else {
            self.echo("No further redo information", cx);
            return;
        };
        self.active.ghost = None; // direct buffer mutation, like undo() above
        // Re-apply the child's edit group forward: `inserted` over the removed
        // span.
        let (cursor, ok) = self.replay_undo_group(group, true);
        if ok {
            self.active.undo.current = child;
        }
        if self.finish_undo_replay(cursor, ok, cx) {
            self.echo("Redo", cx);
        }
    }

    /// The active buffer's undo tree for the vundo visualizer (see
    /// [`UndoTree::dump`]).
    pub(super) fn undo_tree_dump(&self) -> String {
        self.active.undo.dump()
    }

    /// Group all edits made by one Scheme command batch into a single undo unit.
    /// Groups nest: only the outermost begin opens a node and only the matching
    /// outermost end closes it, so an inner group (a `C-u N` repeat, `:s`, a
    /// block op inside a batch) merges into the enclosing one.
    pub(super) fn begin_undo_group(&mut self) {
        let g = &mut self.undo_grouping;
        if g.depth == 0 {
            g.started = false;
            g.batch_seq += 1;
        }
        g.depth += 1;
    }

    pub(super) fn end_undo_group(&mut self) {
        let g = &mut self.undo_grouping;
        g.depth = g.depth.saturating_sub(1);
        if g.depth == 0 {
            g.started = false;
        }
    }

    /// `(undo-boundary)`: close the current group so the next edit starts a new
    /// one even within the same command batch.
    pub(super) fn undo_boundary(&mut self) {
        self.undo_grouping.boundary();
    }

    /// Where the undo tree stands now, for [`Self::squash_undo_since`].
    pub(super) fn undo_base(&self) -> UndoBase {
        UndoBase {
            buffer: self.active.name.clone(),
            len: self.active.undo.len(),
            current: self.active.undo.current,
            grid: self.active.grid.as_deref().map(|g| g.hist_len()),
        }
    }

    /// Fold every undo node added since `base` into the first of them, so a
    /// multi-run command (`3dd`) undoes in one step. Only when the new nodes
    /// form a straight chain off `base.current` in the same buffer — anything
    /// else (an undo mid-way, a buffer switch) is left untouched.
    pub(super) fn squash_undo_since(&mut self, base: &UndoBase) {
        if self.active.name == base.buffer {
            self.active.undo.squash_since(base.len, base.current);
        }
    }

    // --- Programmatic range editing ---------------------------------------
}

#[cfg(test)]
mod tests {
    use super::record_matches;
    use crate::buffer::Buffer;

    #[test]
    fn stale_undo_records_are_detected_not_replayed() {
        // A record replays only when its text is still exactly where it says.
        let b = Buffer::new("hello wörld");
        assert!(record_matches(&b, 6, "wörld"));
        assert!(record_matches(&b, 3, "")); // pure insertion point
        // The special-buffer re-render case: same offsets, different text.
        assert!(!record_matches(&b, 6, "world"));
        // Past the end (the buffer shrank under the history).
        assert!(!record_matches(&b, 10, "abc"));
        assert!(!record_matches(&b, 99, ""));
        // Mid-codepoint (`ö` is bytes 7..9) — never splice there.
        assert!(!record_matches(&b, 8, ""));
    }
}
