//! App-wide quit coordination across every [`Workspace`] window.

use super::*;

/// App-wide quit coordination across every [`Workspace`] window. Each window
/// has its own editor (and buffers), so quitting must resolve the unsaved
/// buffers of *all* of them — one prompt at a time — before `cx.quit()` tears
/// the whole app down, and must run every window's `kill-emacs-hook`.
#[derive(Default)]
struct QuitFlow {
    /// Editors (by entity id) whose unsaved buffers the user already resolved
    /// (saved or discarded) during the quit in progress.
    confirmed: HashSet<gpui::EntityId>,
    /// The quit came from the update pill: relaunch into the staged build.
    relaunch: bool,
    /// A close-button prompt is pending in this editor: answering it closes
    /// just its window instead of continuing an app quit.
    close_window: Option<gpui::EntityId>,
}

impl gpui::Global for QuitFlow {}

/// Every open [`Workspace`] window.
fn workspace_windows(cx: &App) -> Vec<gpui::WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .collect()
}

/// How many [`Workspace`] windows are open (the close button only closes a
/// single window while another remains).
pub fn workspace_window_count(cx: &App) -> usize {
    workspace_windows(cx).len()
}

/// Start a fresh, user-initiated app quit, forgetting any earlier cancelled
/// quit's per-window answers.
pub fn begin_app_quit(relaunch: bool, cx: &mut App) {
    cx.set_global(QuitFlow {
        relaunch,
        ..QuitFlow::default()
    });
}

/// A close-button prompt for one dirty window (with others still open) is
/// starting in the editor `editor`.
pub fn begin_window_close(editor: gpui::EntityId, cx: &mut App) {
    cx.set_global(QuitFlow {
        close_window: Some(editor),
        ..QuitFlow::default()
    });
}

/// The editor `editor` resolved its quit prompt (or had nothing unsaved).
/// Either close just its window (a close-button prompt, while another window
/// remains), or record the answer and carry the app quit on.
pub fn quit_confirmed(editor: gpui::EntityId, relaunch: bool, cx: &mut App) {
    let windows = workspace_windows(cx);
    if cx.default_global::<QuitFlow>().close_window == Some(editor) {
        cx.global_mut::<QuitFlow>().close_window = None;
        let target = windows.iter().find(|w| {
            w.read_with(cx, |ws, _| ws.editor.entity_id() == editor)
                .unwrap_or(false)
        });
        if windows.len() > 1
            && let Some(target) = target
        {
            target
                .update(cx, |_, window, _| window.remove_window())
                .ok();
            return;
        }
        // It became the last window meanwhile: closing it quits the app.
    }
    let flow = cx.default_global::<QuitFlow>();
    flow.confirmed.insert(editor);
    flow.relaunch |= relaunch;
    continue_app_quit(cx);
}

/// Carry an app quit forward: prompt the next window that still has unsaved
/// buffers the user hasn't answered for, or — once none remain — run every
/// window's `kill-emacs-hook` and quit.
pub fn continue_app_quit(cx: &mut App) {
    let windows = workspace_windows(cx);
    let confirmed = cx.default_global::<QuitFlow>().confirmed.clone();
    let relaunch = cx.global::<QuitFlow>().relaunch;
    let pending = windows.iter().find(|w| {
        w.read_with(cx, |ws, cx| {
            !confirmed.contains(&ws.editor.entity_id()) && ws.has_unsaved_buffers(cx)
        })
        .unwrap_or(false)
    });
    if let Some(pending) = pending {
        pending
            .update(cx, |ws, window, cx| {
                window.activate_window();
                ws.editor
                    .update(cx, |editor, cx| editor.prompt_quit_save(relaunch, cx));
            })
            .ok();
        return;
    }
    // A save still writing in the background has already marked its buffer
    // clean: wait for it to land before tearing the app down. (The wait
    // reports through a window; with none left there's nothing to wait for.)
    if crate::save::in_flight() > 0
        && let Some(anchor) = windows.first().copied()
    {
        wait_for_saves(anchor, cx);
        return;
    }
    if relaunch && let Err(err) = crate::update::relaunch() {
        // Stay running rather than quitting into nothing.
        cx.set_global(QuitFlow::default());
        if let Some(w) = windows.first() {
            w.update(cx, |ws, _, cx| {
                ws.log_message(format!("Relaunch failed: {err}"), cx)
            })
            .ok();
        }
        return;
    }
    for w in &windows {
        w.update(cx, |ws, _, cx| {
            ws.editor.update(cx, |editor, cx| editor.prepare_quit(cx))
        })
        .ok();
    }
    cx.set_global(QuitFlow::default());
    cx.quit();
}

/// How long a quit waits for background saves before giving up on quitting.
const SAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Carry the quit on once every background save has landed. If one fails (its
/// buffer is modified again) or they don't finish within [`SAVE_WAIT`], call
/// the quit off instead, so no edit is lost to a save the user thought was
/// done.
fn wait_for_saves(anchor: gpui::WindowHandle<Workspace>, cx: &mut App) {
    let failures = crate::save::failures();
    anchor
        .update(cx, |ws, _, cx| {
            ws.log_message(
                "Waiting for saves to finish before quitting…".to_string(),
                cx,
            )
        })
        .ok();
    cx.spawn(async move |cx| {
        let deadline = std::time::Instant::now() + SAVE_WAIT;
        while crate::save::in_flight() > 0 && std::time::Instant::now() < deadline {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(20))
                .await;
        }
        // Through a window handle: it fails (rather than panicking) if the
        // app is already gone.
        anchor
            .update(cx, |ws, _, cx| {
                let failed = crate::save::failures() != failures;
                if failed || crate::save::in_flight() > 0 {
                    cx.set_global(QuitFlow::default());
                    let why = if failed {
                        "a save failed"
                    } else {
                        "a save is still being written"
                    };
                    ws.log_message(format!("Quit cancelled: {why}"), cx);
                } else {
                    continue_app_quit(cx);
                }
            })
            .ok();
    })
    .detach();
}
