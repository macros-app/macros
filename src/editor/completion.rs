//! Completion popup (corfu-style) methods for [`Editor`].

use super::*;

/// Which text an async completion request was computed against: the active
/// buffer's name plus its edit tick and length at request time. A reply whose
/// stamp no longer matches addresses text that has since changed (or another
/// buffer), so its byte range can't be trusted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct CompletionStamp {
    buffer: String,
    tick: u64,
    len: usize,
}

impl Editor {
    fn completion_stamp_now(&self) -> CompletionStamp {
        CompletionStamp {
            buffer: self.active.name.clone(),
            tick: self.active.buffer.tick(),
            len: self.active.buffer.len(),
        }
    }

    /// Record the active buffer's state as an async completion request (CAPF,
    /// LSP, ANVIL) goes out; checked by [`Self::completion_reply_range`].
    pub(super) fn stamp_completion_request(&mut self) {
        self.completion_stamp = Some(self.completion_stamp_now());
    }

    /// Validate an async completion reply's `start..end`: `None` (and the
    /// auto-trigger flag released, so auto-completion re-arms) when the buffer
    /// switched or changed since the request; otherwise the range clamped and
    /// snapped to char boundaries, so slicing it can never panic.
    pub(super) fn completion_reply_range(
        &mut self,
        start: usize,
        end: usize,
    ) -> Option<Range<usize>> {
        let fresh = self.completion_stamp.take() == Some(self.completion_stamp_now());
        if !fresh {
            self.completion_auto_pending = false;
            return None;
        }
        Some(self.active.buffer.clamp_range(start..end))
    }

    /// The candidate set filtered by the live prefix `buffer[anchor..point]`.
    /// Point BEFORE the anchor means the completed word is gone (deleted via a
    /// path that never refiltered — evil `C-w`, a Scheme edit): nothing matches,
    /// rather than an empty prefix matching everything.
    pub(super) fn popup_filtered(&self) -> Vec<CompletionItem> {
        let Some(popup) = self.completion_popup.as_ref() else {
            return Vec::new();
        };
        let point = self.point();
        if point < popup.anchor {
            return Vec::new();
        }
        let prefix = self.active.buffer.slice_to_string(popup.anchor..point);
        popup
            .items
            .iter()
            .filter(|c| c.label.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// Render snapshot for the element: visible rows (label + detail + kind) and
    /// the selected one.
    pub fn completion_view(&self) -> Option<CompletionView> {
        let popup = self.completion_popup.as_ref()?;
        let items = self.popup_filtered();
        if items.is_empty() {
            return None;
        }
        let selected = popup.selected.min(items.len() - 1);
        let doc = items.get(selected).and_then(|i| i.documentation.clone());
        let rows = items
            .into_iter()
            .map(|item| CompletionRow {
                label: item.label,
                detail: item.detail,
                kind: item.kind,
            })
            .collect();
        Some(CompletionView {
            items: rows,
            selected,
            doc,
        })
    }

    pub(super) fn completion_next(&mut self, cx: &mut Context<Self>) {
        self.completion_cycle(Direction::Forward, cx);
    }

    pub(super) fn completion_previous(&mut self, cx: &mut Context<Self>) {
        self.completion_cycle(Direction::Backward, cx);
    }

    fn completion_cycle(&mut self, dir: Direction, cx: &mut Context<Self>) {
        let count = self.popup_filtered().len();
        if let Some(popup) = self.completion_popup.as_mut() {
            popup.selected = wrap_index(popup.selected, count, dir);
        }
        self.completion_resolve_selected();
        cx.notify();
    }

    pub(super) fn completion_accept(&mut self, cx: &mut Context<Self>) {
        let filtered = self.popup_filtered();
        let Some(popup) = self.completion_popup.take() else {
            return;
        };
        if let Some(choice) = filtered
            .get(popup.selected.min(filtered.len().saturating_sub(1)))
            .cloned()
        {
            let point = self.point();
            // Point can sit BEFORE the anchor when an edit moved it back without
            // the popup having refiltered-closed first (e.g. a fast backspace then
            // Enter/Tab, or a CAPF that reported an anchor ahead of point). The
            // `anchor..point` prefix range would then invert and abort the editor in
            // `byte_slice`. There's nothing to replace — drop the accept.
            if point < popup.anchor {
                cx.notify();
                return;
            }
            if choice.additional_edits.is_empty() {
                let cursor = self.edit(popup.anchor..point, &choice.insert_text);
                self.collapse_to(cursor);
            } else {
                self.accept_completion_with_edits(popup.anchor, point, &choice, cx);
            }
        }
        cx.notify();
    }

    /// A mouse click on filtered-candidate row `index`: select it and accept.
    pub(super) fn completion_click(&mut self, index: usize, cx: &mut Context<Self>) {
        let count = self.popup_filtered().len();
        if index >= count {
            return;
        }
        if let Some(popup) = self.completion_popup.as_mut() {
            popup.selected = index;
        }
        self.completion_accept(cx);
    }

    pub(super) fn completion_abort(&mut self, cx: &mut Context<Self>) {
        self.completion_popup = None;
        cx.notify();
    }

    /// After an edit while the popup is open: close it if the prefix no longer
    /// matches anything, point moved before the anchor, or the prefix was
    /// erased entirely; otherwise reset the selection to the top.
    ///
    /// The erased-prefix rule matches other editors: backspacing away the word
    /// that opened the popup dismisses it instead of leaving an
    /// everything-matches menu up. The exception is a popup anchored right
    /// after a trigger char (`foo.`), where an empty prefix legitimately means
    /// "all members" — erasing back to the dot keeps the full list.
    pub(super) fn popup_refilter(&mut self, cx: &mut Context<Self>) {
        let Some(anchor) = self.completion_popup.as_ref().map(|p| p.anchor) else {
            return;
        };
        let erased = self.point() == anchor && {
            let triggers = cx
                .global::<crate::settings::Settings>()
                .completion_trigger_chars(self.active_language().as_deref());
            let before = anchor > 0 && {
                let prev = self.active.buffer.prev_grapheme(anchor);
                self.active
                    .buffer
                    .slice_to_string(prev..anchor)
                    .chars()
                    .next_back()
                    .is_some_and(|c| self.lsp_trigger_char(c, &triggers))
            };
            !before
        };
        if self.point() < anchor || erased || self.popup_filtered().is_empty() {
            self.completion_popup = None;
        } else if let Some(popup) = self.completion_popup.as_mut() {
            popup.selected = 0;
        }
        cx.notify();
    }

    /// If the popup is open, run the `completion`-keymap binding for `key`, or
    /// the given default command if unbound. Returns whether it was handled.
    pub(super) fn popup_special_key(
        &mut self,
        key: &str,
        default: EditorCommand,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.completion_popup.is_none() {
            return false;
        }
        match self
            .keymap
            .resolve_mode("completion", key)
            .map(str::to_string)
        {
            Some(expr) => self.run_script(&expr, cx),
            None => self.apply_command(default, cx),
        }
        true
    }

    /// If a ghost suggestion is visible, run the `ghost`-keymap binding for
    /// `key` (Scheme binds TAB → `ghost-text-accept`). Returns whether the key
    /// was handled. Unbound keys fall through, so the ghost never swallows
    /// ordinary typing; like the completion popup this is consulted from the
    /// action paths (`act_tab`, `act_newline`), since those are gpui action
    /// keys that never reach `on_key_down`.
    pub(super) fn ghost_special_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        // Never in evil Normal/Visual: a suggestion shown late (after Esc)
        // must not turn TAB (vim's C-i) into an insert.
        if self.ghost_at().is_none() || self.evil_intercepts() {
            return false;
        }
        match self.keymap.resolve_mode("ghost", key).map(str::to_string) {
            Some(expr) => {
                self.run_script(&expr, cx);
                true
            }
            None => false,
        }
    }

    /// Whether a language server is ready and tracking the active buffer.
    pub(super) fn has_ready_lsp(&self) -> bool {
        let (Some(language), Some(uri)) = (self.active_language(), self.active_uri()) else {
            return false;
        };
        self.servers
            .get(&language)
            .is_some_and(|s| s.ready && s.open.contains(&uri))
    }

    /// Whether `c` is a completion trigger character (e.g. `.` for member access)
    /// for the active buffer — either advertised by its ready language server or
    /// configured via `completion-trigger-chars` (`extra`, resolved by the caller
    /// from settings). Trigger characters open completion with no word prefix — the
    /// `Module.member` case. The `extra` fallback covers servers that under-
    /// advertise (ocamllsp lists no trigger characters, so `.` never fires without it).
    pub(super) fn lsp_trigger_char(&self, c: char, extra: &str) -> bool {
        let (Some(language), Some(uri)) = (self.active_language(), self.active_uri()) else {
            return false;
        };
        self.servers.get(&language).is_some_and(|s| {
            s.ready
                && s.open.contains(&uri)
                && (extra.contains(c)
                    || s.caps
                        .completion_triggers
                        .iter()
                        .any(|t| t.starts_with(c) && t.chars().count() == 1))
        })
    }

    /// After a self-insert, open the popup automatically if `completion-auto` is
    /// on, a word prefix of sufficient length precedes point, and a server is
    /// ready (corfu's auto behavior). One request per session: once the popup is
    /// open, further typing filters client-side via `popup_refilter`.
    pub(super) fn maybe_auto_complete(&mut self, typed: &str, cx: &mut Context<Self>) {
        if self.completion_auto_pending {
            return;
        }
        let settings = cx.global::<crate::settings::Settings>();
        if !settings.completion_auto() {
            return;
        }
        let min_prefix = settings.completion_auto_prefix();
        let capf_enabled = settings.completion_capf();
        let triggers = settings.completion_trigger_chars(self.active_language().as_deref());
        let last = typed.chars().last();
        // A trigger character (e.g. `.`) opens completion immediately with no word
        // prefix — the `Module.member` case. Otherwise only trigger right after a
        // word character, once a prefix of sufficient length precedes point.
        if !last.is_some_and(|c| self.lsp_trigger_char(c, &triggers)) {
            if !last.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return;
            }
            let point = self.point();
            let prefix_start = self.completion_prefix_start(point);
            if point.saturating_sub(prefix_start) < min_prefix.max(1) {
                return;
            }
        }
        // Fire when a language server can complete OR a Scheme CAPF can (argument
        // completion like `(load-theme 'mac…)`, dabbrev in plain-text/scratch
        // buffers). CAPFs run on the engine thread, so even a buffer-scanning CAPF
        // never blocks the UI. Without either source there's nothing to complete.
        let can_capf = capf_enabled && !self.capf_functions.is_empty();
        if !self.has_ready_lsp() && !can_capf {
            return;
        }
        self.completion_auto_pending = true;
        // Route through the full completion-at-point path so CAPFs get a chance
        // before the language server; a CAPF that declines falls back to LSP.
        self.completion_at_point(cx);
    }
}
