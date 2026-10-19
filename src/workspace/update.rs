//! The title-bar update pill: its click-driven state machine and rendering.

use super::*;

/// The lifecycle of the title-bar update pill, advanced by the user's clicks.
/// The launch-time check only ever sets [`UpdatePhase::Available`] — nothing is
/// downloaded until the user asks for it.
#[derive(Clone)]
pub(super) enum UpdatePhase {
    /// A newer release exists. Click to start downloading it.
    Available,
    /// The artifact is downloading; `percent` is 0–100.
    Downloading { percent: u8 },
    /// The new build is installed (or, on Windows, staged to install on exit).
    /// Click to relaunch into it.
    Ready,
}

/// The state behind the title-bar update pill: the resolved release plus the
/// phase its click cycles through.
#[derive(Clone)]
pub(super) struct UpdateStatus {
    /// The newer release found by the check, carrying the download URLs.
    available: crate::update::UpdateAvailable,
    phase: UpdatePhase,
}

impl Workspace {
    /// Announce that a newer release is available. Called by the launch-time
    /// check once it resolves an update; nothing is downloaded yet — the pill
    /// starts in [`UpdatePhase::Available`] and the user clicks to download.
    pub fn show_update_available(
        &mut self,
        available: crate::update::UpdateAvailable,
        cx: &mut Context<Self>,
    ) {
        self.update_status = Some(UpdateStatus {
            available,
            phase: UpdatePhase::Available,
        });
        cx.notify();
    }

    /// Advance the pill to a new phase and repaint.
    pub(super) fn set_update_phase(&mut self, phase: UpdatePhase, cx: &mut Context<Self>) {
        if let Some(status) = self.update_status.as_mut() {
            status.phase = phase;
            cx.notify();
        }
    }

    /// Handle a click on the update pill, dispatching on its current phase:
    /// `Available` starts the download,
    /// `Downloading` is inert, and `Ready` relaunches into the staged build.
    pub(super) fn on_update_pill_click(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.update_status.clone() else {
            return;
        };
        match status.phase {
            UpdatePhase::Available => self.start_update_download(status.available, cx),
            // A download is in flight; ignore clicks until it resolves.
            UpdatePhase::Downloading { .. } => {}
            // Same unsaved-buffer prompt as any other quit; the relaunch helper
            // is only spawned once the quit is confirmed.
            UpdatePhase::Ready => {
                self.editor
                    .update(cx, |editor, cx| editor.confirm_quit_and_relaunch(cx));
            }
        }
    }

    /// Download + install the update in the background, streaming the
    /// percent-complete back to the pill, then flip it to [`UpdatePhase::Ready`]
    /// (or back to `Available` so the user can retry on failure).
    pub(super) fn start_update_download(
        &mut self,
        available: crate::update::UpdateAvailable,
        cx: &mut Context<Self>,
    ) {
        use futures::StreamExt;

        self.set_update_phase(UpdatePhase::Downloading { percent: 0 }, cx);
        self.log_message(format!("Downloading Macros {}…", available.version), cx);

        cx.spawn(async move |this, cx| {
            // The blocking install runs on the background executor and reports
            // progress through an unbounded channel; the sender drops when the
            // install returns, ending the loop below.
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<u8>();
            let install = cx.background_executor().spawn(async move {
                crate::update::install(&available, move |pct| {
                    let _ = tx.unbounded_send(pct);
                })
            });

            while let Some(percent) = rx.next().await {
                this.update(cx, |ws, cx| {
                    ws.set_update_phase(UpdatePhase::Downloading { percent }, cx)
                })
                .ok();
            }

            let outcome = install.await;
            this.update(cx, |ws, cx| match outcome {
                Ok(crate::update::InstallOutcome::Staged) => {
                    ws.log_message("Update downloaded — restart Macros to finish.", cx);
                    ws.set_update_phase(UpdatePhase::Ready, cx);
                }
                // A build the updater can't verify or own (unsigned dev build,
                // a binary not from our package): the download page was opened
                // instead of auto-installing.
                Ok(crate::update::InstallOutcome::OpenedDownloadPage) => {
                    ws.log_message(
                        "This build can't update itself — opened the download page instead.",
                        cx,
                    );
                    ws.set_update_phase(UpdatePhase::Available, cx);
                }
                Err(err) => {
                    ws.log_message(format!("Update download failed: {err}"), cx);
                    ws.set_update_phase(UpdatePhase::Available, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The update pill that sits on the right of the title bar. `None` when no
    /// update has been found. Its label and interactivity track the phase:
    /// available (click to download), downloading (a live percent ticker, inert),
    /// and ready (click to restart). The download/restart click is wired to
    /// [`on_update_pill_click`]; a trailing `✕` dismisses the pill for this run
    /// (the launch check resurfaces it on the next start while the update is
    /// still pending).
    pub(super) fn render_update_pill(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let status = self.update_status.clone()?;
        let theme = cx.global::<Theme>();
        let foreground = theme.foreground();
        let dim = theme.color("dim", foreground);
        let accent = theme.color("minibuffer-prompt", foreground);
        let panel_bg = theme.color("mode-line-bg", theme.background());

        // The label, plus whether a click does anything in this phase.
        let (label, interactive): (SharedString, bool) = match &status.phase {
            UpdatePhase::Available => ("Update available — click to download".into(), true),
            UpdatePhase::Downloading { percent } => {
                (format!("Downloading… {percent}%").into(), false)
            }
            UpdatePhase::Ready => ("Restart to update".into(), true),
        };
        // Ready stands out (accent border); downloading is muted.
        let border = if matches!(status.phase, UpdatePhase::Ready) {
            accent
        } else {
            dim
        };

        // The label is its own click target (download / restart) so the trailing
        // ✕ can dismiss independently of it.
        let mut label_el = div().child(label);
        if interactive {
            label_el = label_el
                .cursor(gpui::CursorStyle::PointingHand)
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _window, cx| {
                        this.on_update_pill_click(cx);
                        cx.stop_propagation();
                    }),
                );
        }

        // The dismiss ✕: hides the pill for this run. The launch check resurfaces
        // it next time the app starts and finds the update still pending.
        let close = div()
            .pl_1()
            .cursor(gpui::CursorStyle::PointingHand)
            .text_color(dim)
            .child(SharedString::from("✕"))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.update_status = None;
                    cx.notify();
                    cx.stop_propagation();
                }),
            );

        let pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(1.))
            .bg(panel_bg)
            .border_1()
            .border_color(border)
            .rounded_md()
            .text_size(px(11.))
            .text_color(if interactive { foreground } else { dim })
            .child(label_el)
            .child(close);

        // Anchored to the right of the title bar, vertically centered. It sits
        // above the draggable title layer; `occlude` blocks the drag hitbox
        // behind it (a plain hitbox wouldn't — the hit test only stops on a
        // blocking one), so clicking the pill never starts a window drag.
        Some(
            div()
                .occlude()
                .absolute()
                .right(px(8.) + CAPTION_BUTTONS_WIDTH)
                .top_0()
                .h_full()
                .flex()
                .items_center()
                .child(pill)
                .into_any_element(),
        )
    }
}
