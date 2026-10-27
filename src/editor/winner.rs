//! winner-mode: a history of window layouts (the split tree, its ratios and
//! which buffer each window shows) that `C-c left` / `C-c right` step through.
//!
//! The editor offers the live layout to [`Winner::observe`] after every render;
//! a layout whose shape differs from the newest entry is pushed. `winner-undo`
//! walks back (each repeat goes one further), `winner-redo` walks forward
//! again; any new window change while walking drops the forward entries, like
//! browser history. Restoring goes through `set_window_layout`, the same path
//! the desktop restore uses. Off with `(set-option "winner-mode" #f)`.

use gpui::Context;

use super::*;
use crate::script::LayoutSpec;

/// Emacs `winner-ring-size`.
const WINNER_RING_MAX: usize = 200;

/// The layout history. `history` is oldest first; `pos` is the entry shown
/// while undoing/redoing (`None` = the newest, not walking).
#[derive(Default)]
pub(super) struct Winner {
    history: Vec<LayoutSpec>,
    pos: Option<usize>,
    /// The newest entry was pushed for a ratio-only change: a further one
    /// replaces it, so dragging a divider (a change per frame) or a run of
    /// resize commands is one step, not hundreds.
    resizing: bool,
}

/// How two layouts differ, ignoring points and which window is selected.
#[derive(Debug, PartialEq)]
enum Diff {
    Same,
    /// Same tree and buffers; only split ratios moved.
    Ratios,
    Shape,
}

fn diff(a: &LayoutSpec, b: &LayoutSpec) -> Diff {
    match (a, b) {
        (LayoutSpec::Leaf { buffer: x, .. }, LayoutSpec::Leaf { buffer: y, .. }) => {
            if x == y {
                Diff::Same
            } else {
                Diff::Shape
            }
        }
        (
            LayoutSpec::Split {
                vertical: v1,
                ratio: r1,
                a: a1,
                b: b1,
            },
            LayoutSpec::Split {
                vertical: v2,
                ratio: r2,
                a: a2,
                b: b2,
            },
        ) => {
            if v1 != v2 {
                return Diff::Shape;
            }
            let (da, db) = (diff(a1, a2), diff(b1, b2));
            if da == Diff::Shape || db == Diff::Shape {
                Diff::Shape
            } else if r1 != r2 || da == Diff::Ratios || db == Diff::Ratios {
                Diff::Ratios
            } else {
                Diff::Same
            }
        }
        _ => Diff::Shape,
    }
}

impl Winner {
    /// Record `current` if it differs from the entry being shown. A same-shape
    /// layout just refreshes that entry (its points and selected window), so
    /// undoing back to it lands where the cursors last were.
    pub(super) fn observe(&mut self, current: LayoutSpec) {
        let shown = self.pos.unwrap_or(self.history.len().wrapping_sub(1));
        let Some(tip) = self.history.get(shown) else {
            self.history.push(current);
            return;
        };
        match diff(tip, &current) {
            Diff::Same => self.history[shown] = current,
            change => {
                // A new change while walking drops the redo entries.
                if self.pos.take().is_some() {
                    self.history.truncate(shown + 1);
                    self.resizing = false;
                }
                let ratios = change == Diff::Ratios;
                if ratios && self.resizing {
                    *self.history.last_mut().expect("non-empty") = current;
                } else {
                    self.history.push(current);
                    if self.history.len() > WINNER_RING_MAX {
                        self.history.remove(0);
                    }
                }
                self.resizing = ratios;
            }
        }
    }

    /// Step back one layout: `(layout, position, count)`, 1-based, or `None`
    /// at the oldest.
    pub(super) fn undo(&mut self) -> Option<(LayoutSpec, usize, usize)> {
        let shown = self.pos.unwrap_or(self.history.len().checked_sub(1)?);
        let target = shown.checked_sub(1)?;
        self.pos = Some(target);
        self.resizing = false;
        Some((self.history[target].clone(), target + 1, self.history.len()))
    }

    /// Step forward again after an undo, or `None` when not walking / at the
    /// newest.
    pub(super) fn redo(&mut self) -> Option<(LayoutSpec, usize, usize)> {
        let target = self.pos? + 1;
        let len = self.history.len();
        if target >= len {
            return None;
        }
        self.pos = (target + 1 < len).then_some(target);
        Some((self.history[target].clone(), target + 1, len))
    }

    /// Forget the history (winner-mode turned off).
    fn clear(&mut self) {
        *self = Winner::default();
    }
}

impl Editor {
    fn winner_enabled(&self, cx: &Context<Self>) -> bool {
        cx.global::<crate::settings::Settings>().winner_mode()
    }

    /// Offer the live layout to the history (called each render).
    pub(super) fn winner_observe(&mut self, cx: &Context<Self>) {
        if self.winner_enabled(cx) {
            let spec = self.session_layout(&self.layout);
            self.winner.observe(spec);
        } else {
            self.winner.clear();
        }
    }

    /// `C-c left`: restore the previous window layout; repeat to go further.
    pub(super) fn winner_undo(&mut self, cx: &mut Context<Self>) {
        self.winner_step(true, cx);
    }

    /// `C-c right`: undo the last `winner-undo`.
    pub(super) fn winner_redo(&mut self, cx: &mut Context<Self>) {
        self.winner_step(false, cx);
    }

    fn winner_step(&mut self, back: bool, cx: &mut Context<Self>) {
        if !self.winner_enabled(cx) {
            self.echo("winner-mode is off", cx);
            return;
        }
        // Catch a change made since the last render (a command earlier in
        // the same keystroke batch).
        self.winner_observe(cx);
        let step = if back {
            self.winner.undo()
        } else {
            self.winner.redo()
        };
        match step {
            Some((spec, n, len)) => {
                self.set_window_layout(spec, cx);
                // Record what was actually built (a killed buffer resolves to
                // another), so the next render doesn't see it as a new change.
                let built = self.session_layout(&self.layout);
                self.winner.observe(built);
                let what = if back { "undo" } else { "redo" };
                self.echo(format!("Winner {what} ({n} / {len})"), cx);
            }
            None if back => self.echo("No further window configuration undo information", cx),
            None => self.echo("No further window configuration redo information", cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(buffer: &str) -> LayoutSpec {
        LayoutSpec::Leaf {
            buffer: buffer.into(),
            path: String::new(),
            point: 0,
            selected: false,
        }
    }

    fn split(ratio: f32, a: LayoutSpec, b: LayoutSpec) -> LayoutSpec {
        LayoutSpec::Split {
            vertical: true,
            ratio,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    fn with_point(spec: LayoutSpec, p: usize) -> LayoutSpec {
        match spec {
            LayoutSpec::Leaf {
                buffer,
                path,
                selected,
                ..
            } => LayoutSpec::Leaf {
                buffer,
                path,
                point: p,
                selected,
            },
            other => other,
        }
    }

    #[test]
    fn undo_walks_back_and_redo_forward() {
        let mut w = Winner::default();
        let one = leaf("a");
        let two = split(0.5, leaf("a"), leaf("a"));
        let three = split(0.5, leaf("a"), leaf("b"));
        w.observe(one.clone());
        w.observe(two.clone());
        w.observe(three.clone());
        // Repeated undo keeps going back, then stops at the oldest.
        assert_eq!(w.undo().unwrap(), (two.clone(), 2, 3));
        w.observe(two.clone()); // the restored layout renders: no new entry
        assert_eq!(w.undo().unwrap(), (one.clone(), 1, 3));
        assert!(w.undo().is_none());
        // Redo retraces, then stops at the newest.
        assert_eq!(w.redo().unwrap(), (two.clone(), 2, 3));
        assert_eq!(w.redo().unwrap(), (three.clone(), 3, 3));
        assert!(w.redo().is_none());
        assert_eq!(w.undo().unwrap().0, two);
    }

    #[test]
    fn a_new_change_while_walking_drops_redo() {
        let mut w = Winner::default();
        w.observe(leaf("a"));
        w.observe(split(0.5, leaf("a"), leaf("a")));
        assert_eq!(w.undo().unwrap().0, leaf("a"));
        w.observe(leaf("b")); // switch buffer after undoing
        assert!(w.redo().is_none());
        assert_eq!(w.undo().unwrap().0, leaf("a"));
    }

    #[test]
    fn points_refresh_the_entry_and_ratio_runs_coalesce() {
        let mut w = Winner::default();
        w.observe(leaf("a"));
        // Moving the cursor isn't a new layout, but undo restores the point.
        w.observe(with_point(leaf("a"), 42));
        w.observe(split(0.5, leaf("a"), leaf("a")));
        // A divider drag: many ratio-only changes become one entry.
        for r in [0.55, 0.6, 0.65, 0.7] {
            w.observe(split(r, leaf("a"), leaf("a")));
        }
        assert_eq!(w.undo().unwrap().0, split(0.5, leaf("a"), leaf("a")));
        assert_eq!(w.undo().unwrap().0, with_point(leaf("a"), 42));
        assert!(w.undo().is_none());
    }

    #[test]
    fn history_is_capped() {
        let mut w = Winner::default();
        for i in 0..WINNER_RING_MAX + 10 {
            w.observe(leaf(&i.to_string()));
        }
        assert_eq!(w.history.len(), WINNER_RING_MAX);
        assert_eq!(w.history[0], leaf("10"));
    }
}

/// Through a real window: the layout is recorded as it renders and the
/// `C-c left` / `C-c right` keys walk it.
#[cfg(test)]
mod e2e {
    use gpui::{Entity, TestAppContext, VisualTestContext};

    use super::super::mcp::parse_keys;
    use super::*;

    pub(super) fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, &mut VisualTestContext) {
        test_context(cx);
        cx.update(|cx| cx.bind_keys(super::super::key_bindings()));
        let (editor, vcx) = cx.add_window_view(|_, cx| Editor::new(cx));
        editor.update(vcx, |e, cx| {
            for src in crate::runtime::startup_scripts() {
                e.run_script_blocking(src, cx);
            }
            e.mark_config_loaded(cx);
            let current = e.snapshot_active();
            e.inactive.push(current);
            e.install_active(make_document("a.txt".into(), None, "alpha"));
        });
        let focus = editor.read_with(vcx, |e, _| e.focus_handle.clone());
        vcx.update(|window, cx| window.focus(&focus, cx));
        settle(&editor, vcx);
        (editor, vcx)
    }

    pub(super) fn settle(editor: &Entity<Editor>, vcx: &mut VisualTestContext) {
        for _ in 0..4 {
            vcx.run_until_parked();
            editor.read_with(vcx, |e, _| {
                let _ = e.script.eval("0");
            });
            vcx.update(|window, _| window.refresh());
            vcx.run_until_parked();
        }
    }

    pub(super) fn press(editor: &Entity<Editor>, vcx: &mut VisualTestContext, seq: &str) {
        for ks in parse_keys(seq).expect("valid keys") {
            vcx.update(|window, cx| {
                window.dispatch_keystroke(ks, cx);
            });
            settle(editor, vcx);
        }
    }

    fn windows(editor: &Entity<Editor>, vcx: &mut VisualTestContext) -> usize {
        editor.read_with(vcx, |e, _| e.layout.leaves().len())
    }

    #[gpui::test]
    fn ctrl_c_left_undoes_splits_and_right_redoes(cx: &mut TestAppContext) {
        let (editor, vcx) = setup(cx);
        assert_eq!(windows(&editor, vcx), 1);
        press(&editor, vcx, "ctrl-x 2");
        press(&editor, vcx, "ctrl-x 3");
        assert_eq!(windows(&editor, vcx), 3);
        // Repeated C-c left keeps going back.
        press(&editor, vcx, "ctrl-c left");
        assert_eq!(windows(&editor, vcx), 2);
        press(&editor, vcx, "ctrl-c left");
        assert_eq!(windows(&editor, vcx), 1);
        let status = editor.read_with(vcx, |e, _| e.status.to_string());
        assert!(status.starts_with("Winner undo ("), "{status}");
        press(&editor, vcx, "ctrl-c right");
        assert_eq!(windows(&editor, vcx), 2);
        press(&editor, vcx, "ctrl-c right");
        assert_eq!(windows(&editor, vcx), 3);
        // C-x 1 is itself a change winner can undo.
        press(&editor, vcx, "ctrl-x 1");
        assert_eq!(windows(&editor, vcx), 1);
        press(&editor, vcx, "ctrl-c left");
        assert_eq!(windows(&editor, vcx), 3);
    }

    #[gpui::test]
    fn winner_mode_off_records_nothing(cx: &mut TestAppContext) {
        let (editor, vcx) = setup(cx);
        editor.update(vcx, |e, cx| e.run_script_blocking("(winner-mode #f)", cx));
        settle(&editor, vcx);
        press(&editor, vcx, "ctrl-x 2");
        press(&editor, vcx, "ctrl-c left");
        assert_eq!(windows(&editor, vcx), 2);
        let status = editor.read_with(vcx, |e, _| e.status.to_string());
        assert!(status.contains("winner-mode is off"), "{status}");
    }
}

/// wdired through a real window (its plan/execute logic is unit-tested in
/// `script::wdired_tests`): enter it from a dired listing, the protected text
/// refuses edits, an edited name is renamed on disk, and the listing returns.
#[cfg(test)]
mod wdired_e2e {
    use gpui::TestAppContext;

    use super::e2e::{press, settle, setup};
    use super::*;

    #[gpui::test]
    fn edit_a_name_and_commit(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("macros-wdired-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one"), "1").unwrap();
        let d = format!("{}/", dir.display());
        let (editor, vcx) = setup(cx);
        let run = |editor: &gpui::Entity<Editor>, vcx: &mut gpui::VisualTestContext, src: &str| {
            editor.update(vcx, |e, cx| e.run_script_blocking(src, cx));
            settle(editor, vcx);
        };
        run(&editor, vcx, &format!("(dired-in \"{d}\")"));
        press(&editor, vcx, "ctrl-x ctrl-q");
        let (name, special) = editor.read_with(vcx, |e, _| {
            (e.active.name.clone(), e.active.special.is_some())
        });
        assert_eq!(name, "*dired*");
        let st = editor.read_with(vcx, |e, _| e.status.to_string());
        assert!(!special, "wdired makes the listing editable: {st}");
        // The header is protected.
        run(&editor, vcx, "(begin (goto-char 0) (insert \"zz\"))");
        let text = editor.read_with(vcx, |e, _| e.active.buffer.full_text());
        assert!(text.starts_with(&d), "{text}");
        // "  one" is line 2: append to the name.
        run(
            &editor,
            vcx,
            "(begin (goto-line 2) (move-end-of-line) (insert \"-two\"))",
        );
        let text = editor.read_with(vcx, |e, _| e.active.buffer.full_text());
        assert!(text.contains("  one-two"), "{text}");
        press(&editor, vcx, "ctrl-c ctrl-c");
        assert!(dir.join("one-two").exists());
        assert!(!dir.join("one").exists());
        let (text, special) = editor.read_with(vcx, |e, _| {
            (e.active.buffer.full_text(), e.active.special.is_some())
        });
        assert!(special, "back to the read-only listing");
        assert!(text.contains("  one-two"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
