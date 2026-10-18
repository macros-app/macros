use std::collections::HashSet;

use gpui::{App, Context, Entity, SharedString, Window, div, prelude::*, px, relative};

use crate::editor::{Editor, TitlebarDropdownView};
use crate::theme::Theme;

mod chrome;
mod quit;
mod update;

use chrome::*;
pub use quit::*;
use update::*;

/// Height of one Helm row (candidate or source header) at the panel's 13px text.
const HELM_ROW_HEIGHT: f32 = 21.;
/// Height of the Helm prompt + pattern row above the candidates.
const HELM_PROMPT_HEIGHT: f32 = 29.;

/// The echo-area row beneath the minibuffer/mode line: a full-width line that
/// wraps long messages (LSP diagnostics, completion hints) instead of clipping
/// them at the window edge, capped at a few lines so it can't swallow the
/// editor. The full history remains in `*Messages*`.
fn echo_row(text: SharedString, color: gpui::Hsla, bg: gpui::Hsla) -> impl IntoElement {
    // gpui shapes each child with `shape_line`, which panics (aborting the app)
    // on an embedded newline. So instead of collapsing the message to one line,
    // split it into physical lines and give each its OWN row — multi-line
    // messages (e.g. `email-status`) stack vertically, and no child ever holds a
    // newline. Single-line messages (the common case) still render as one row.
    // `max_h` + `overflow_hidden` cap the growth at a few lines.
    let mut col = div()
        .w_full()
        .px_2()
        .py(px(2.))
        .max_h(px(160.))
        .overflow_hidden()
        .bg(bg)
        .text_color(color)
        .text_size(px(13.))
        .flex()
        .flex_col();
    let cleaned = text.replace('\r', "");
    for line in cleaned.split('\n') {
        // An empty line needs a non-empty child, or the row collapses to zero
        // height and the blank-line spacing is lost.
        let line = if line.is_empty() { " " } else { line };
        col = col.child(div().child(SharedString::from(line.to_string())));
    }
    col
}

/// The theme colors the bottom-region renderers share. Resolved once per frame
/// in `render` and threaded into each helper so every region uses identical
/// colors (and we read the `Theme` global only once).
#[derive(Clone, Copy)]
struct Colors {
    background: gpui::Hsla,
    mode_line_bg: gpui::Hsla,
    mode_line_fg: gpui::Hsla,
    prompt_color: gpui::Hsla,
    cursor_color: gpui::Hsla,
    dim: gpui::Hsla,
}

/// The top-level frame: an editor filling the window above a status line. This
/// is the root view GPUI renders, analogous to an Emacs frame containing one
/// window and the mode line.
pub struct Workspace {
    editor: Entity<Editor>,
    /// Set by the launch-time update check; rendered as a pill on the right of
    /// the title bar that the user clicks to download, then restart.
    update_status: Option<UpdateStatus>,
    /// Linux client decorations: the left button went down on the title bar
    /// and hasn't moved yet. The window move starts on the first drag motion
    /// rather than the press, since handing the pointer to the compositor
    /// swallows the release — and with it, double-click-to-maximize.
    title_drag_pending: std::rc::Rc<std::cell::Cell<bool>>,
}

impl Workspace {
    pub fn new(editor: Entity<Editor>, cx: &mut Context<Self>) -> Self {
        // Re-render the frame (status/mode line, minibuffer) whenever the editor
        // changes, so it reflects status messages, mode state, and theme edits.
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        Self {
            editor,
            update_status: None,
            title_drag_pending: Default::default(),
        }
    }

    /// Log a status message to the editor's echo area and `*Messages*` buffer.
    /// Used by the launch-time update flow so its progress (checking, downloading)
    /// is visible in `*Messages*` like the interactive `check-for-updates` command.
    pub fn log_message(&self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.message(message, cx));
    }

    /// Move keyboard focus to the editor. Called once after the window opens.
    pub fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.editor.read(cx).focus_handle.clone();
        window.focus(&handle);
    }

    /// Run a Steel snippet against the editor synchronously (startup config:
    /// keymaps, theme, options must apply in order before the UI is driven).
    pub fn eval(&self, source: &str, cx: &mut Context<Self>) {
        let source = source.to_string();
        self.editor.update(cx, |editor, cx| {
            editor.run_script_blocking(&source, cx);
        });
    }

    /// Run the user's config (`init.scm`) like [`Self::eval`], but interrupt it
    /// after `timeout`, so an infinite loop in it can't hang startup.
    pub fn eval_user_config(
        &self,
        source: &str,
        timeout: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let source = source.to_string();
        self.editor.update(cx, |editor, cx| {
            editor.run_script_blocking_within(&source, timeout, cx);
        });
    }

    /// Signal that startup config (bundled scripts + `init.scm`) is fully
    /// applied, so the editor stops swallowing plain keystrokes (which it does
    /// until now so a key pressed before `(evil-mode)` runs can't self-insert).
    pub fn mark_config_loaded(&self, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.mark_config_loaded(cx));
    }

    /// Run `after-init-hook` once startup + user config have loaded — where
    /// session restore (desktop / savehist, scheme/session.scm) runs.
    pub fn run_after_init(&self, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.run_after_init(cx));
    }

    /// Run the initial buffer's mode hooks, after the startup + user scripts have
    /// registered them (this is what brings up the Steel LSP for `*scratch*`).
    pub fn run_startup_hooks(&self, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.run_mode_hooks_once(cx));
    }

    /// An OS window-close request: defer to the editor, which cancels the close
    /// and prompts to save when buffers are modified (see `Editor::confirm_quit`).
    pub fn confirm_quit(&self, cx: &mut Context<Self>) -> bool {
        self.editor.update(cx, |editor, cx| editor.confirm_quit(cx))
    }

    /// The OS close button on this window while other windows stay open and
    /// this one has unsaved buffers: prompt, then close only this window.
    fn confirm_close_window(&self, cx: &mut Context<Self>) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.confirm_close_window(cx))
    }

    /// Whether any of this window's buffers have unsaved changes. Lets the
    /// window-close handler drop a clean secondary window outright while still
    /// routing a dirty one through the save prompt.
    pub fn has_unsaved_buffers(&self, cx: &App) -> bool {
        self.editor.read(cx).any_modified()
    }

    /// A request to close this window — the OS close button, or our own on
    /// Linux. Returns whether the window may close right now. Unsaved buffers
    /// get the same save/cancel prompt as C-x C-c. The last window closing
    /// quits the app (`confirm_quit` — prompting across every window, then
    /// `cx.quit()`). With another window still open, only this window closes:
    /// straight away when clean, or after the save/discard prompt when dirty —
    /// never quitting the app and discarding the other windows' edits.
    pub fn should_close_window(&self, cx: &mut Context<Self>) -> bool {
        if workspace_window_count(cx) <= 1 {
            return self.confirm_quit(cx);
        }
        if !self.has_unsaved_buffers(cx) {
            return true;
        }
        self.confirm_close_window(cx)
    }

    /// The transient popup (magit-style command menu): a titled grid of
    /// key/label rows laid out as columns. Each row is clickable and dispatches
    /// the bound command via `Editor::transient_click`.
    fn render_transient(
        &self,
        transient: crate::editor::TransientView,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.global::<Theme>();
        let panel_bg = theme.color("completion-bg", colors.background);
        let key_color = theme.color("magit-key", colors.prompt_color);
        let foreground = theme.foreground();
        let dim = colors.dim;
        // Build each column as a flex-col; lay the columns out in a row that
        // WRAPS — on a narrow window the columns flow onto additional rows
        // instead of running off the right edge. `gap_4` spaces both the
        // columns and the wrapped rows.
        let mut columns_row = div().flex().flex_row().flex_wrap().gap_4().px_2();
        for column in transient.columns {
            let mut col = div().flex().flex_col();
            if let Some(title) = column.title {
                col = col.child(div().text_color(dim).child(SharedString::from(title)));
            }
            for row in column.rows {
                let command = row.command.clone();
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .cursor(gpui::CursorStyle::PointingHand)
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _ev, _window, cx| {
                                let command = command.clone();
                                this.editor
                                    .update(cx, |editor, cx| editor.transient_click(&command, cx));
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .text_color(key_color)
                                .min_w(px(48.))
                                .child(SharedString::from(row.key)),
                        )
                        .child(SharedString::from(row.label)),
                );
            }
            columns_row = columns_row.child(col);
        }
        // An empty title means no header row: the popup is just its columns.
        let header = (!transient.title.is_empty()).then(|| {
            div().text_color(dim).child(SharedString::from(format!(
                "{} — transient",
                transient.title
            )))
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .bg(panel_bg)
            .text_color(foreground)
            .text_size(px(13.))
            .px_2()
            .py_1()
            .children(header)
            .child(columns_row)
            .into_any_element()
    }

    /// How many rows (candidates and source headers) fit in the Helm panel for
    /// a window of `window_height`, so the list fills the panel.
    fn helm_max_lines(window_height: f32, settings: &crate::settings::Settings) -> usize {
        let panel = window_height * settings.helm_height()
            - settings.helm_bevel_top()
            - settings.helm_bevel_bottom()
            - HELM_PROMPT_HEIGHT;
        (panel / HELM_ROW_HEIGHT).floor().max(0.) as usize
    }

    /// The Helm selection panel: a beveled, fixed-height box with a prompt +
    /// live filter pattern on top, candidate rows beneath, and an optional
    /// preview pane on the right. Rows are clickable and run the default action.
    fn render_helm(
        &self,
        helm: crate::editor::HelmView,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.global::<Theme>();
        let background = colors.background;
        let foreground = theme.foreground();
        let prompt_color = colors.prompt_color;
        let cursor_color = colors.cursor_color;
        let dim = colors.dim;
        let settings = cx.global::<crate::settings::Settings>();
        let helm_height = settings.helm_height();
        let helm_bevel_top = settings.helm_bevel_top();
        let helm_bevel_left = settings.helm_bevel_left();
        let helm_bevel_right = settings.helm_bevel_right();
        let helm_bevel_bottom = settings.helm_bevel_bottom();
        let panel_bg = theme.color("completion-bg", background);
        let selected_bg = theme.color("completion-selected-bg", theme.color("region", background));
        let selected_fg = theme.color("completion-selected-fg", theme.foreground());
        // Bevel: a light top/left edge and a dark bottom/right edge give the
        // panel a raised look. Set both faces to the panel bg for a flat one.
        let bevel_light = theme.color("helm-bevel-light", panel_bg);
        let bevel_shadow = theme.color("helm-bevel-shadow", panel_bg);
        // Optional preview pane (right half) for the current selection.
        let preview_pane = helm.preview.map(|text| {
            div()
                .flex_1()
                .px_2()
                .py_1()
                .text_size(px(13.))
                .text_color(dim)
                .bg(panel_bg)
                .child(SharedString::from(text))
        });

        let mut content = div()
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(panel_bg)
            .text_color(foreground)
            .text_size(px(13.))
            // Prompt + live filter pattern.
            .child(
                div()
                    .h(px(HELM_PROMPT_HEIGHT))
                    .flex_none()
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .text_color(prompt_color)
                            .child(SharedString::from(helm.prompt)),
                    )
                    .child(SharedString::from(helm.pattern))
                    .child(div().w(px(2.)).h(px(15.)).bg(cursor_color)),
            );
        for row in helm.rows {
            // A leading "* " marks multi-selected candidates (C-SPC).
            let prefix = if row.marked { "* " } else { "  " };
            let text: SharedString = format!("{prefix}{}", row.text).into();
            // A marginalia-style annotation, pushed to the right and dimmed.
            let annotation = row.annotation.clone();
            let with_annotation = |line: gpui::Div| match &annotation {
                Some(ann) if !ann.is_empty() => line
                    .flex()
                    .flex_row()
                    .justify_between()
                    .child(text.clone())
                    .child(
                        div()
                            .pl_4()
                            .text_color(dim)
                            .child(SharedString::from(ann.clone())),
                    ),
                _ => line.child(text.clone()),
            };
            let line = if row.header {
                div()
                    .px_2()
                    .text_color(dim)
                    .child(SharedString::from(format!("— {} —", row.text)))
            } else if row.selected {
                with_annotation(
                    div()
                        .px_4()
                        .w_full()
                        .bg(selected_bg)
                        .text_color(selected_fg),
                )
            } else if row.marked {
                with_annotation(div().px_4().w_full().text_color(prompt_color))
            } else {
                with_annotation(div().px_4().w_full())
            };
            // Candidate rows are clickable: select and run the default action.
            let line = match row.index {
                Some(index) => line.cursor(gpui::CursorStyle::PointingHand).on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _window, cx| {
                        this.editor
                            .update(cx, |editor, cx| editor.helm_click(index, cx));
                        cx.stop_propagation();
                    }),
                ),
                None => line,
            };
            content = content.child(line.h(px(HELM_ROW_HEIGHT)).flex_none());
        }
        // Beveled frame: light top/left edges, dark bottom/right edges, each a
        // configurable-width colored strip. The panel takes `helm-height` of
        // the window; the editor (flex) fills the rest.
        div()
            .w_full()
            .h(relative(helm_height))
            .flex()
            .flex_col()
            // Fill the whole box so empty space below the rows is panel bg.
            .bg(panel_bg)
            .child(div().w_full().h(px(helm_bevel_top)).bg(bevel_light))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .child(div().w(px(helm_bevel_left)).bg(bevel_light))
                    .child(content)
                    .children(preview_pane)
                    .child(div().w(px(helm_bevel_right)).bg(bevel_shadow))
                    // The content row fills the panel; its bg shows under rows.
                    .bg(panel_bg),
            )
            .child(div().w_full().h(px(helm_bevel_bottom)).bg(bevel_shadow))
            .into_any_element()
    }

    /// The minibuffer: a fixed-height input row (prompt + split input + caret),
    /// with any trailing message wrapped onto its own echo row below so long
    /// text (e.g. completion hints) is readable instead of clipped.
    fn render_minibuffer(
        &self,
        mb: crate::editor::MinibufferView,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.global::<Theme>();
        let background = colors.background;
        let foreground = theme.foreground();
        let prompt_color = colors.prompt_color;
        let cursor_color = colors.cursor_color;
        let dim = colors.dim;
        // The input line (prompt + split input + caret) stays one fixed row;
        // any trailing message wraps onto its own echo row below it so long
        // text (e.g. completion hints) is readable instead of clipped.
        let input = div()
            .h(px(24.))
            .w_full()
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .child(div().text_color(prompt_color).child(mb.prompt))
            .child(mb.before)
            // A thin caret between the two halves of the input.
            .child(div().w(px(2.)).h(px(15.)).bg(cursor_color))
            .child(mb.after);
        let mut col = div()
            .w_full()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .text_size(px(13.))
            .child(input);
        if !mb.message.is_empty() {
            col = col.child(echo_row(mb.message.into(), dim, background));
        }
        col.into_any_element()
    }

    /// The default bottom region: a fixed-height mode line carrying the
    /// buffer/mode/diagnostic summary, with the echo message (e.g. the LSP
    /// diagnostic at point) on its own wrapping row below it.
    fn render_status_line(&self, colors: Colors, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.global::<Theme>();
        let foreground = theme.foreground();
        let editor = self.editor.read(cx);
        // The mode-line content is defined in Scheme (`scheme/modeline.scm`) as an
        // ordered list of segments; the editor resolves it to `(text, face)` spans
        // against live state. We paint each span as its own colored child so a
        // segment can carry its own face (e.g. the git slot), falling back to the
        // default mode-line foreground.
        let segments = editor.mode_line_segments();
        let message = editor.status.clone();
        // Mode line: fixed-height strip carrying buffer/mode/diagnostic summary.
        // The echo message (e.g. the LSP diagnostic at point) lives on its own
        // wrapping row below, Emacs-style, so it no longer fights the mode line
        // for space or bleeds off the window's right edge.
        let mut mode_line = div()
            .h(px(24.))
            .w_full()
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .bg(colors.mode_line_bg)
            .text_color(colors.mode_line_fg)
            .text_size(px(13.));
        for (text, face) in segments {
            let color = face
                .as_ref()
                .map(|f| theme.color(f, colors.mode_line_fg))
                .unwrap_or(colors.mode_line_fg);
            mode_line = mode_line.child(div().text_color(color).child(text));
        }
        // The echo row is always present so the layout never shifts when a
        // pending-chord hint (or any status message) appears and clears. A live
        // status message wins; otherwise we fall back to the evil mode name
        // (Normal/Visual/Insert...) so the bar doubles as the modal indicator.
        // With evil disabled and nothing pending we feed it a single space,
        // which reserves exactly one line of height instead of collapsing.
        let echo_text = if !message.is_empty() {
            message
        } else if let Some(name) = editor.evil_mode_name() {
            SharedString::from(name)
        } else {
            SharedString::from(" ")
        };
        let col = div()
            .w_full()
            .flex()
            .flex_col()
            .child(mode_line)
            .child(echo_row(echo_text, foreground, colors.background));
        col.into_any_element()
    }

    /// One title-bar dropdown trigger: the active item's name (else the
    /// dropdown's own) plus a chevron. Clicking toggles its menu (see
    /// [`Self::render_titlebar_dropdown_menu`]). The triggers sit in a row just
    /// right of the macOS traffic lights; like the update pill, the row floats
    /// above the draggable title layer and `occlude`s so a click never starts
    /// a window drag.
    fn render_titlebar_dropdown_trigger(
        &self,
        index: usize,
        dropdown: &TitlebarDropdownView,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = cx.global::<Theme>();
        let dim = theme.color("dim", theme.foreground());
        let mut hover_bg = dim;
        hover_bg.a = 0.15;
        let key = dropdown.key.clone();
        let mut trigger = div()
            .id(("titlebar-dropdown-trigger", index))
            .hover(move |style| style.bg(hover_bg))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(1.))
            .rounded_md()
            .text_size(px(12.))
            .cursor(gpui::CursorStyle::PointingHand)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    this.editor
                        .update(cx, |editor, cx| editor.titlebar_dropdown_toggle(&key, cx));
                    cx.stop_propagation();
                }),
            )
            .child(dropdown.trigger.clone())
            .child(div().text_color(dim).child(SharedString::from("▾")));
        if dropdown.open {
            trigger = trigger.bg(hover_bg);
        }
        trigger
    }

    /// The full-window transparent backdrop behind an unfolded title-bar
    /// menu: folds it on any stray click. Rendered as the workspace root's
    /// last child so it covers the editor area.
    fn render_titlebar_dropdown_backdrop(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .occlude()
            .absolute()
            .inset_0()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.editor
                        .update(cx, |editor, cx| editor.titlebar_dropdown_close(cx));
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    /// An unfolded title-bar menu: the search field over the item panel,
    /// anchored beneath its trigger and `deferred` so it paints above the
    /// editor area and the backdrop.
    fn render_titlebar_dropdown_menu(
        &self,
        dropdown: &TitlebarDropdownView,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.global::<Theme>();
        let foreground = theme.foreground();
        // Menu chrome: a hairline border and a soft hover pill (both derived
        // from the theme's dim at reduced alpha) over the mode-line panel color.
        let mut hairline = colors.dim;
        hairline.a *= 0.4;
        let mut hover_bg = colors.dim;
        hover_bg.a = 0.15;

        // The search field: typed keys route here while the menu is open (the
        // key layer + IME, like Helm's filter). Rendered like the minibuffer —
        // the text split at the caret with a thin caret quad between the
        // halves. Each character is its own child so a click can land the
        // caret on it; the flex filler after the text catches clicks past the
        // end. Placeholder (caret first) when empty.
        // The caret: cursor color softened (a full-strength block reads too
        // heavy at this size) and blinking in step with the editor cursor.
        // When blinked off it keeps its 2px slot, just unpainted, so the text
        // doesn't jiggle.
        let mut caret_color = colors.cursor_color;
        caret_color.a *= 0.6;
        let caret_on = self.editor.read(cx).cursor_blink_visible();
        let caret = move || {
            let slot = div().flex_none().w(px(2.)).h(px(14.));
            if caret_on { slot.bg(caret_color) } else { slot }
        };
        let filter_chars: Vec<char> = dropdown.filter.chars().collect();
        let mut search_row = div()
            .px_3()
            .py(px(6.))
            .border_b_1()
            .border_color(hairline)
            .flex()
            .flex_row()
            .items_center()
            .cursor(gpui::CursorStyle::IBeam)
            .text_color(foreground);
        if filter_chars.is_empty() {
            search_row = search_row.child(caret()).child(
                div()
                    .text_color(colors.dim)
                    .child(dropdown.placeholder.clone()),
            );
        } else {
            for (char_index, ch) in filter_chars.iter().enumerate() {
                if char_index == dropdown.cursor {
                    search_row = search_row.child(caret());
                }
                let label: SharedString = ch.to_string().into();
                search_row = search_row.child(
                    div()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _ev, _window, cx| {
                                this.editor.update(cx, |editor, cx| {
                                    editor.titlebar_dropdown_cursor_set(char_index, cx)
                                });
                                cx.stop_propagation();
                            }),
                        )
                        .child(label),
                );
            }
            if dropdown.cursor >= filter_chars.len() {
                search_row = search_row.child(caret());
            }
            // Clicks in the blank space right of the text put the caret at the
            // end (the row itself is taller than this filler; height keeps it
            // clickable inside the centered flex row).
            search_row = search_row.child(div().flex_grow().h(px(16.)).on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.editor.update(cx, |editor, cx| {
                        editor.titlebar_dropdown_cursor_set(usize::MAX, cx)
                    });
                    cx.stop_propagation();
                }),
            ));
        }

        // Dim section label over the row list, per the reference design.
        let section_label = div()
            .px_3()
            .pt(px(6.))
            .pb(px(2.))
            .text_size(px(10.))
            .text_color(colors.dim)
            .child(dropdown.section.clone());

        let mut panel = div()
            .flex()
            .flex_col()
            .pb_1()
            .min_w(px(220.))
            // Laid out inside the trigger's narrow slot: without nowrap, text
            // measures as wrapped (tall) but paints on one line, leaving gaps.
            .whitespace_nowrap()
            .bg(colors.mode_line_bg)
            .border_1()
            .border_color(hairline)
            .rounded_lg()
            .shadow_md()
            .overflow_hidden()
            .text_size(px(12.))
            .text_color(foreground)
            .child(search_row)
            .child(section_label);
        for (row_index, (item_index, item)) in dropdown.items.iter().enumerate() {
            let active = !dropdown.active.is_empty() && *item == dropdown.active;
            let item_index = *item_index;
            let key = dropdown.key.clone();
            // Rows are inset (mx_1 + rounded) so the highlight reads as a pill
            // inside the panel; the keyboard-selected row is filled the same
            // way a hover is. The id makes the row stateful so gpui tracks its
            // hover state across frames.
            let mut row = div()
                .id(row_index)
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .mx_1()
                .px_1p5()
                .py(px(2.))
                .rounded_md()
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(hover_bg))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _ev, _window, cx| {
                        this.editor.update(cx, |editor, cx| {
                            editor.titlebar_dropdown_click(&key, item_index, cx)
                        });
                        cx.stop_propagation();
                    }),
                )
                .child(item.clone());
            if Some(row_index) == dropdown.selected {
                row = row.bg(hover_bg);
            }
            if active {
                row = row.child(
                    div()
                        .text_color(colors.prompt_color)
                        .text_size(px(11.))
                        .child(SharedString::from("✓")),
                );
            }
            panel = panel.child(row);
        }
        if dropdown.items.is_empty() {
            panel = panel.child(
                div()
                    .px_3()
                    .py(px(3.))
                    .text_color(colors.dim)
                    .child(SharedString::from("No matches")),
            );
        }
        gpui::deferred(
            div()
                .occlude()
                .absolute()
                .top(px(30.))
                .left_0()
                .child(panel),
        )
        .with_priority(1)
        .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_family = cx
            .global::<crate::settings::Settings>()
            .font_family()
            .to_string();
        let theme = cx.global::<Theme>();
        let background = theme.background();
        let colors = Colors {
            background,
            mode_line_bg: theme.color("mode-line-bg", theme.background()),
            mode_line_fg: theme.color("mode-line-fg", theme.foreground()),
            prompt_color: theme.color("minibuffer-prompt", theme.foreground()),
            cursor_color: theme.color("cursor", theme.foreground()),
            dim: theme.color("dim", theme.foreground()),
        };

        // Pull the active bottom-region view (owned) before building it, so no
        // `&Editor` borrow is held across the `cx.listener` calls in the helpers.
        let transient = self.editor.read(cx).transient_view();
        let helm_lines = Self::helm_max_lines(
            f32::from(window.viewport_size().height),
            cx.global::<crate::settings::Settings>(),
        );
        let helm = self.editor.read(cx).helm_view(helm_lines);
        let minibuffer = self.editor.read(cx).minibuffer_view();
        let titlebar_dropdowns = self.editor.read(cx).titlebar_dropdown_views();

        // The bottom region is, in priority order: a transient popup, the Helm
        // selection panel, the minibuffer, or the status / mode line.
        let bottom = if let Some(transient) = transient {
            self.render_transient(transient, colors, cx)
        } else if let Some(helm) = helm {
            self.render_helm(helm, colors, cx)
        } else if let Some(mb) = minibuffer {
            self.render_minibuffer(mb, colors, cx)
        } else {
            self.render_status_line(colors, cx)
        };

        // Linux only: `Client` means we draw the window decorations (title
        // bar, caption buttons, resize border); `Server` means the compositor
        // fell back to drawing its own title bar. Always `Server` elsewhere.
        let client_tiling = match window.window_decorations() {
            gpui::Decorations::Client { tiling } => Some(tiling),
            gpui::Decorations::Server => None,
        };
        let system_title_bar =
            cfg!(any(target_os = "linux", target_os = "freebsd")) && client_tiling.is_none();

        // Custom title-bar strip. The system title bar is transparent (see
        // `main.rs`), so we paint this ourselves in the theme's `title-bar-bg`
        // and leave room on the left for the macOS traffic lights. The centered
        // title lives on a draggable layer; the update pill (if any) floats on
        // the right, above and outside that drag area so it stays clickable.
        // Under a Linux system title bar the strip carries only the pill and
        // perspectives dropdown, and is dropped when it has neither.
        let theme = cx.global::<Theme>();
        let title_bar_bg = theme.title_bar_bg();
        let title_bar_fg = theme.title_bar_fg();
        let title: SharedString = format!("Macros — {}", self.editor.read(cx).buffer_name()).into();
        let update_pill = self.render_update_pill(cx);
        let caption = if cfg!(target_os = "windows") {
            Some((
                CAPTION_BUTTONS_WIDTH,
                caption_buttons(window, title_bar_fg).into_any_element(),
            ))
        } else if client_tiling.is_some() {
            Some(linux_caption_buttons(window, title_bar_fg, cx))
        } else {
            None
        };
        // Keep the title centered, clear of whatever sits at either edge.
        let title_inset = px(72.).max(caption.as_ref().map_or(px(0.), |(w, _)| *w));
        let mut drag_layer = div()
            .absolute()
            .inset_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .pl(title_inset)
            .pr(title_inset)
            .window_control_area(gpui::WindowControlArea::Drag);
        if !system_title_bar {
            drag_layer = drag_layer.child(title);
        }
        if client_tiling.is_some() {
            // Linux has no hit-testing for `WindowControlArea::Drag`, so drive
            // the compositor's move / maximize / window menu from mouse events.
            let pending = self.title_drag_pending.clone();
            // The motion that starts the move is watched window-wide: a quick
            // flick can leave the title bar before the first move event lands.
            let watch_drag = gpui::canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let on_move = pending.clone();
                    window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, window, _| {
                        if phase == gpui::DispatchPhase::Capture && on_move.get() {
                            on_move.set(false);
                            if e.dragging() {
                                window.start_window_move();
                            }
                        }
                    });
                    let on_up = pending.clone();
                    window.on_mouse_event(move |_: &gpui::MouseUpEvent, _, _, _| on_up.set(false));
                },
            )
            .absolute()
            .size_full();
            let pending = self.title_drag_pending.clone();
            drag_layer = drag_layer
                .child(watch_drag)
                .on_mouse_down(gpui::MouseButton::Left, move |e, window, _| {
                    pending.set(e.click_count < 2);
                    if e.click_count >= 2 {
                        window.zoom_window();
                    }
                })
                .on_mouse_down(gpui::MouseButton::Right, |e, window, _| {
                    window.show_window_menu(e.position)
                });
        }
        let mut title_bar = div()
            .w_full()
            .h(px(28.))
            .flex_none()
            .relative()
            .bg(title_bar_bg)
            .text_color(title_bar_fg)
            .text_size(px(13.))
            .child(drag_layer);
        let title_bar_has_content = update_pill.is_some() || !titlebar_dropdowns.is_empty();
        if let Some(pill) = update_pill {
            title_bar = title_bar.child(pill);
        }
        if let Some((_, buttons)) = caption {
            title_bar = title_bar.child(buttons);
        }
        // The dropdown triggers float left of the centered title, just past
        // the traffic lights (the same layering as the update pill); an open
        // menu hangs beneath its own trigger.
        if !titlebar_dropdowns.is_empty() {
            let mut row = div()
                .occlude()
                .absolute()
                .left(TITLE_LEFT_INSET + px(8.))
                .top_0()
                .h_full()
                .flex()
                .flex_row()
                .items_center()
                .gap_1();
            for (index, dropdown) in titlebar_dropdowns.iter().enumerate() {
                let mut slot = div()
                    .relative()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(self.render_titlebar_dropdown_trigger(index, dropdown, cx));
                if dropdown.open {
                    slot = slot.child(self.render_titlebar_dropdown_menu(dropdown, colors, cx));
                }
                row = row.child(slot);
            }
            title_bar = title_bar.child(row);
        }

        // The editor fills the central region.
        let editor_area = div().flex_1().relative().child(self.editor.clone());

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .bg(background)
            .font_family(font_family)
            .when(!system_title_bar || title_bar_has_content, |root| {
                root.child(title_bar)
            })
            .child(editor_area)
            .child(bottom);
        // Behind an unfolded title-bar menu (itself deferred above
        // everything), a backdrop folds it on a stray click.
        if titlebar_dropdowns.iter().any(|d| d.open) {
            root = root.child(self.render_titlebar_dropdown_backdrop(cx));
        }
        match client_tiling {
            Some(tiling) => client_frame(root, tiling, colors.dim.opacity(0.4), window),
            None => root.into_any_element(),
        }
    }
}
