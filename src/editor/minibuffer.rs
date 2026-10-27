//! Minibuffer prompts, completion, and history for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use std::path::Path;

use gpui::Context;

use super::*;

/// Whether a yes/no answer is affirmative (`yes` or `y`).
pub(super) fn is_yes(answer: &str) -> bool {
    matches!(answer, "yes" | "y")
}

/// The shared minibuffer input history (Emacs minibuffer history): entries
/// most recent last, recalled with Up/Down or `M-p`/`M-n` while a read is open.
#[derive(Default)]
pub(super) struct MinibufferHistory {
    entries: Vec<String>,
    /// Position while browsing: `None` = editing a fresh input; `Some(i)` =
    /// showing `entries[len-1-i]`. `stash` holds the fresh input so returning
    /// past the newest entry restores it.
    pos: Option<usize>,
    stash: String,
}

impl MinibufferHistory {
    /// The entries, oldest first.
    pub(super) fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Replace the entries wholesale (savehist's restore), ending any browse.
    pub(super) fn replace(&mut self, entries: Vec<String>) {
        self.entries = entries;
        self.pos = None;
    }

    /// Record a submitted input (newest last, skipping blanks and consecutive
    /// duplicates).
    pub(super) fn push(&mut self, input: &str) {
        if input.is_empty() || self.entries.last().map(String::as_str) == Some(input) {
            return;
        }
        self.entries.push(input.to_string());
    }

    /// Start a fresh read: not browsing, nothing stashed.
    fn reset_browse(&mut self) {
        self.pos = None;
        self.stash.clear();
    }

    /// Step through history from a read showing `current`: `delta < 0` recalls
    /// older entries, `delta > 0` newer ones; stepping past the newest restores
    /// the stashed in-progress input. `None` when there's nowhere to go.
    fn step(&mut self, delta: isize, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let len = self.entries.len();
        let next = if delta < 0 {
            // Older. Entering history stashes the current (fresh) input.
            match self.pos {
                None => {
                    self.stash = current.to_string();
                    Some(0)
                }
                Some(i) => Some((i + 1).min(len - 1)),
            }
        } else {
            // Newer.
            match self.pos {
                None => return None,
                Some(0) => None,
                Some(i) => Some(i - 1),
            }
        };
        self.pos = next;
        Some(match next {
            Some(i) => self.entries[len - 1 - i].clone(),
            None => std::mem::take(&mut self.stash),
        })
    }
}

impl Minibuffer {
    fn new(prompt: &str, purpose: MinibufferPurpose, initial: &str) -> Self {
        Minibuffer {
            prompt: prompt.to_string(),
            input: initial.to_string(),
            cursor: initial.len(),
            message: String::new(),
            purpose,
            sticky: false,
            secret: None,
            on_cancel: None,
        }
    }

    /// Replace the input, leaving point at its end.
    pub(super) fn set_input(&mut self, text: String) {
        self.cursor = text.len();
        self.input = text;
    }

    /// Insert `text` at point.
    fn insert(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.message.clear();
    }

    /// Delete the grapheme before point. Returns whether anything changed.
    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let prev = prev_grapheme(&self.input, self.cursor);
        self.input.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.message.clear();
        true
    }

    /// Delete the grapheme after point. Returns whether anything changed.
    fn delete(&mut self) -> bool {
        let next = next_grapheme(&self.input, self.cursor);
        if next <= self.cursor {
            return false;
        }
        self.input.replace_range(self.cursor..next, "");
        self.message.clear();
        true
    }

    /// Move point to byte `to` (clamped to the input).
    fn move_to(&mut self, to: usize) {
        self.cursor = to.min(self.input.len());
    }

    /// TAB completion of the token after `head` (the kept part of the input;
    /// the token currently typed is `token_len` bytes) against `candidates`: no
    /// match, a sole completion, or extend to the longest common prefix and
    /// list the candidates so the user can see what to disambiguate.
    fn apply_completion(&mut self, head: &str, token_len: usize, candidates: &[String]) {
        match candidates {
            [] => self.message = "[No match]".into(),
            [sole] => {
                self.set_input(format!("{head}{sole}"));
                self.message = "[Sole completion]".into();
            }
            _ => {
                let lcp = longest_common_prefix(candidates);
                if lcp.len() > token_len {
                    self.set_input(format!("{head}{lcp}"));
                }
                self.message = format_candidates(candidates);
            }
        }
    }
}

impl Editor {
    /// A render snapshot of the active minibuffer, split at the cursor.
    pub fn minibuffer_view(&self) -> Option<MinibufferView> {
        self.minibuffer.as_ref().map(|mb| {
            let (before, after) = mb.input.split_at(mb.cursor);
            // A password read shows one bullet per char — the real text never
            // reaches the render path.
            let shown = |s: &str| {
                if mb.secret.is_some() {
                    "•".repeat(s.chars().count())
                } else {
                    s.to_string()
                }
            };
            MinibufferView {
                prompt: mb.prompt.clone(),
                before: shown(before),
                after: shown(after),
                message: mb.message.clone(),
            }
        })
    }

    pub(super) fn open_minibuffer(
        &mut self,
        prompt: &str,
        purpose: MinibufferPurpose,
        initial: &str,
        cx: &mut Context<Self>,
    ) {
        // A non-isearch read replacing an isearch one invalidates the isearch
        // transient state (it is only meaningful while its own read is up).
        if !matches!(purpose, MinibufferPurpose::Isearch) {
            self.isearch = None;
        }
        // A read displaced by this one ends without a submit: tell its caller.
        if let Some(old) = self.minibuffer.take() {
            self.mb_cancelled(old);
        }
        // The in-buffer completion popup is consulted before the minibuffer
        // for Enter/Tab/arrows; left open, it would eat the read's keys.
        self.completion_popup = None;
        self.minibuffer = Some(Minibuffer::new(prompt, purpose, initial));
        self.minibuffer_history.reset_browse();
        self.mb_dynamic = None; // set afterward by a dynamic completing-read
        cx.notify();
    }

    /// For a dynamic `completing-read`, re-dispatch the candidate fn with the
    /// current input on the engine thread (results arrive via `helm_set_candidates`'
    /// sibling routing in the `Candidates` outcome). No-op otherwise.
    pub(super) fn mb_dynamic_requery(&mut self) {
        let Some((function, _)) = self.mb_dynamic.clone() else {
            return;
        };
        let Some(input) = self.minibuffer.as_ref().map(|mb| mb.input.clone()) else {
            return;
        };
        self.helm_query_counter += 1;
        let id = self.helm_query_counter;
        self.mb_dynamic = Some((function.clone(), id));
        // Sentinel source id `usize::MAX` distinguishes minibuffer queries from
        // Helm's (which use real source indices) in the `Candidates` outcome.
        self.script
            .dispatch_candidates(usize::MAX, id, &function, &input);
    }

    /// Install dynamic `completing-read` candidates from the engine, if they're
    /// still current (the query id matches the latest dispatch).
    pub(super) fn mb_set_dynamic_candidates(
        &mut self,
        query_id: u64,
        candidates: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if self.mb_dynamic.as_ref().map(|(_, id)| *id) == Some(query_id) {
            self.completing_candidates = candidates;
            cx.notify();
        }
    }

    /// Browse minibuffer history (see [`MinibufferHistory::step`]).
    pub(super) fn mb_history(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.mb_is_secret() {
            return;
        }
        let Some(mb) = self.minibuffer.as_mut() else {
            return;
        };
        if let Some(text) = self.minibuffer_history.step(delta, &mb.input) {
            mb.set_input(text);
            cx.notify();
        }
    }

    // --- Buffers / files --------------------------------------------------

    /// The active buffer's directory as a minibuffer-ready prefix (trailing `/`),
    /// for file-name prompts. Falls back to the process cwd.
    pub(super) fn default_directory_input(&self) -> String {
        // The fallback is the startup cwd (the editor never `chdir`s), avoiding
        // a `current_dir` syscall — see `process_cwd`'s doc.
        let dir = self
            .active
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from(&self.process_cwd));
        let mut initial = dir.to_string_lossy().into_owned();
        if !initial.ends_with('/') {
            initial.push('/');
        }
        initial
    }

    pub(super) fn find_file_prompt(&mut self, cx: &mut Context<Self>) {
        let initial = self.default_directory_input();
        self.open_minibuffer("Find file: ", MinibufferPurpose::FindFile, &initial, cx);
    }

    pub(super) fn mb_insert(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(mb) = self.minibuffer.as_mut() {
            mb.insert(text);
            cx.notify();
        }
        if self.is_isearch() {
            self.isearch_update(cx);
        }
        self.mb_dynamic_requery();
    }

    /// Paste into the minibuffer: cmd-v inserts the system clipboard; C-y
    /// (`yank`) the kill ring's top, synced from the clipboard first as the
    /// buffer `yank` does. A SECRET read takes the clipboard directly either
    /// way and never touches the kill ring, which savehist may write to disk.
    /// The read is one line: trailing newlines (common when copying from a
    /// terminal or `pbcopy < file`) are dropped, and only the first line of a
    /// multi-line paste goes in.
    pub(super) fn mb_paste(&mut self, yank: bool, cx: &mut Context<Self>) {
        let text = if yank && !self.mb_is_secret() {
            self.sync_clipboard_into_kill_ring(cx);
            self.kill_ring.last().cloned()
        } else {
            cx.read_from_clipboard().and_then(|item| item.text())
        };
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            if let Some(mb) = self.minibuffer.as_mut() {
                mb.message = if yank {
                    "[Kill ring is empty]"
                } else {
                    "[Clipboard is empty]"
                }
                .into();
            }
            cx.notify();
            return;
        };
        let (line, more) = single_line(&text);
        self.mb_insert(line, cx);
        if more && let Some(mb) = self.minibuffer.as_mut() {
            mb.message = "[Pasted the first line only]".into();
        }
    }

    pub(super) fn mb_backspace(&mut self, cx: &mut Context<Self>) {
        if self.minibuffer.as_mut().is_some_and(Minibuffer::backspace) {
            cx.notify();
        }
        if self.is_isearch() {
            // Shorten the string and re-match from the original start so deleting
            // a char backs up to the earlier (shorter) match.
            if let Some(st) = self.isearch.as_mut() {
                st.from = st.origin;
            }
            self.isearch_update(cx);
        }
        self.mb_dynamic_requery();
    }

    /// Whether the open read is a secret one (`read-passwd`).
    pub(super) fn mb_is_secret(&self) -> bool {
        self.minibuffer
            .as_ref()
            .is_some_and(|mb| mb.secret.is_some())
    }

    /// A read that ended without a submit: run its `on_cancel`, if any. Takes
    /// the read by value so its Secure Input guard drops here too.
    pub(super) fn mb_cancelled(&mut self, mb: Minibuffer) {
        if let Some(on_cancel) = mb.on_cancel {
            self.script.dispatch_action(&on_cancel, Some(""));
        }
    }

    /// Whether an incremental-search minibuffer is currently open.
    pub(super) fn is_isearch(&self) -> bool {
        matches!(
            self.minibuffer.as_ref().map(|mb| &mb.purpose),
            Some(MinibufferPurpose::Isearch)
        )
    }

    pub(super) fn mb_delete(&mut self, cx: &mut Context<Self>) {
        if self.minibuffer.as_mut().is_some_and(Minibuffer::delete) {
            cx.notify();
        }
        self.mb_dynamic_requery();
    }

    pub(super) fn mb_move(&mut self, to: usize, cx: &mut Context<Self>) {
        if let Some(mb) = self.minibuffer.as_mut() {
            mb.move_to(to);
            cx.notify();
        }
    }

    pub(super) fn mb_left(&mut self, cx: &mut Context<Self>) {
        if let Some(mb) = self.minibuffer.as_ref() {
            let to = prev_grapheme(&mb.input, mb.cursor);
            self.mb_move(to, cx);
        }
    }

    pub(super) fn mb_right(&mut self, cx: &mut Context<Self>) {
        if let Some(mb) = self.minibuffer.as_ref() {
            let to = next_grapheme(&mb.input, mb.cursor);
            self.mb_move(to, cx);
        }
    }

    pub(super) fn mb_abort(&mut self, cx: &mut Context<Self>) {
        // A sticky read (e.g. an agent permission prompt) can't be aborted — it
        // must resolve a pending request, so C-g/Escape only nudges the user to
        // answer rather than dropping the read and stranding the requester.
        if let Some(mb) = self.minibuffer.as_mut()
            && mb.sticky
        {
            mb.message = "(answer y or n)".into();
            cx.notify();
            return;
        }
        // Aborting an incremental search returns point to where it began and
        // drops the match highlight (Emacs C-g out of isearch).
        let isearch = self.is_isearch();
        let grammar_confirm = matches!(
            self.minibuffer.as_ref().map(|m| &m.purpose),
            Some(MinibufferPurpose::ConfirmGrammarInstall)
        );
        if let Some(old) = self.minibuffer.take() {
            self.mb_cancelled(old);
        }
        // An operator waiting on its `/` motion is abandoned with the read.
        self.vim.op = None;
        if grammar_confirm {
            // C-g out of the trust prompt declines this grammar and moves on to
            // the next queued one — otherwise the queue would stall here.
            self.resolve_grammar_install_confirm(false, cx);
            return;
        }
        if isearch && let Some(st) = self.isearch.take() {
            self.search_query = None;
            self.move_to(st.origin, cx);
        }
        self.echo("Quit", cx);
        // This read may have displaced a grammar trust prompt: re-ask it.
        self.resume_grammar_prompt(cx);
    }

    /// Completion candidates for the active minibuffer's purpose and input.
    pub(super) fn mb_candidates(&self, input: &str) -> Vec<String> {
        match self.minibuffer.as_ref().map(|mb| &mb.purpose) {
            Some(MinibufferPurpose::ExecuteCommand) => self
                .script
                .command_names()
                .iter()
                .filter(|c| c.starts_with(input))
                .cloned()
                .collect(),
            Some(MinibufferPurpose::SwitchBuffer) => self
                .buffer_names()
                .into_iter()
                .filter(|n| n.starts_with(input))
                .collect(),
            Some(MinibufferPurpose::FindFile) => file_completions(input),
            Some(
                MinibufferPurpose::Confirm(_)
                | MinibufferPurpose::QuitSave { .. }
                | MinibufferPurpose::ConfirmGrammarInstall
                | MinibufferPurpose::SchemeYesNo(_),
            ) => ["yes", "no"]
                .iter()
                .map(|s| s.to_string())
                .filter(|c| c.starts_with(input))
                .collect(),
            Some(MinibufferPurpose::CompleteAtPoint | MinibufferPurpose::SchemeCompleting(_)) => {
                self.completing_candidates
                    .iter()
                    .filter(|c| c.starts_with(input))
                    .cloned()
                    .collect()
            }
            // Grid values and column names: any part, ignoring case.
            Some(MinibufferPurpose::GridComplete(..) | MinibufferPurpose::GridColumn) => {
                let needle = input.to_lowercase();
                self.completing_candidates
                    .iter()
                    .filter(|c| c.to_lowercase().contains(&needle))
                    .cloned()
                    .collect()
            }
            // Dynamic: the Scheme fn already filtered for the pattern — show as-is.
            Some(MinibufferPurpose::SchemeCompletingDynamic(_)) => {
                self.completing_candidates.clone()
            }
            Some(MinibufferPurpose::SchemeReadFile(_)) => file_completions(input),

            // Free-form reads (regex search, ex command, read-string) have no
            // completions.
            Some(
                MinibufferPurpose::Search(_)
                | MinibufferPurpose::OperatorSearch(_)
                | MinibufferPurpose::Isearch
                | MinibufferPurpose::ExCommand
                | MinibufferPurpose::GridGoto
                | MinibufferPurpose::GridSearch(_)
                | MinibufferPurpose::GridSelectMatching
                | MinibufferPurpose::GridSaveAsXlsx
                | MinibufferPurpose::GridFilter(_)
                | MinibufferPurpose::EvalExpression
                | MinibufferPurpose::SchemeCallback(_),
            ) => Vec::new(),
            None => Vec::new(),
        }
    }

    /// `TAB`: complete the input against the candidate set for this read.
    pub(super) fn mb_complete(&mut self, cx: &mut Context<Self>) {
        let Some((input, eval)) = self.minibuffer.as_ref().map(|mb| {
            let eval = matches!(mb.purpose, MinibufferPurpose::EvalExpression);
            (mb.input.clone(), eval)
        }) else {
            return;
        };
        // The eval read holds a whole expression, so TAB completes the trailing
        // Scheme symbol token (e.g. `(set-o` → `(set-option`) rather than the
        // whole string — a dedicated, splice-in-place path.
        if eval {
            self.mb_complete_symbol(&input, cx);
            return;
        }
        let matches = self.mb_candidates(&input);
        let mb = self.minibuffer.as_mut().unwrap();
        mb.apply_completion("", input.len(), &matches);
        cx.notify();
    }

    /// All Scheme symbols worth completing in the eval minibuffer: the
    /// Rust-registered builtins and interactive commands, plus every top-level
    /// define in the bundled Scheme library. Sorted and de-duplicated.
    pub(super) fn scheme_symbol_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.script.global_names().to_vec();
        names.extend(crate::runtime::defined_names());
        names.sort();
        names.dedup();
        names
    }

    /// `TAB` in the eval minibuffer: complete the trailing symbol token against
    /// [`Self::scheme_symbol_names`], splicing the result in place of just that
    /// token so the rest of the expression (open parens, earlier args) is kept.
    pub(super) fn mb_complete_symbol(&mut self, input: &str, cx: &mut Context<Self>) {
        let (start, token) = scheme_symbol_token(input);
        let candidates: Vec<String> = if token.is_empty() {
            Vec::new()
        } else {
            self.scheme_symbol_names()
                .into_iter()
                .filter(|s| s.starts_with(token))
                .collect()
        };
        let mb = self.minibuffer.as_mut().unwrap();
        mb.apply_completion(&input[..start], token.len(), &candidates);
        cx.notify();
    }

    /// `RET`: finish the read and act on the input.
    pub(super) fn mb_submit(&mut self, cx: &mut Context<Self>) {
        let Some(mb) = self.minibuffer.take() else {
            return;
        };
        cx.notify();
        // A secret is passed through verbatim (leading/trailing spaces can be
        // part of a password), kept out of history, and dispatched so a
        // failing callback can't echo it. `mb` (and its Secure Input guard)
        // drops on return.
        if mb.secret.is_some() {
            if let MinibufferPurpose::SchemeCallback(callback) = &mb.purpose {
                self.publish_point();
                self.script.dispatch_secret_action(callback, &mb.input);
            }
            self.resume_grammar_prompt(cx);
            return;
        }
        let input = mb.input.trim().to_string();
        // Record text-entry reads in the shared history (not yes/no prompts).
        if !matches!(
            mb.purpose,
            MinibufferPurpose::Confirm(_)
                | MinibufferPurpose::QuitSave { .. }
                | MinibufferPurpose::SchemeYesNo(_)
                | MinibufferPurpose::ConfirmGrammarInstall
        ) {
            self.minibuffer_history.push(&input);
        }
        match mb.purpose {
            MinibufferPurpose::ExecuteCommand => {
                if input.is_empty() {
                    self.echo("", cx);
                } else if self.script.command_names().iter().any(|c| c == &input) {
                    self.note_command(&input);
                    self.run_script(&format!("({input})"), cx);
                } else {
                    self.echo(format!("[No match: {input}]"), cx);
                }
            }
            MinibufferPurpose::FindFile => {
                if !input.is_empty() {
                    self.find_file(&input, cx);
                }
            }
            MinibufferPurpose::SwitchBuffer => {
                if !input.is_empty() {
                    self.switch_to_buffer(&input, cx);
                }
            }
            MinibufferPurpose::Confirm(action) => {
                if is_yes(&input) {
                    self.perform(action, cx);
                } else {
                    self.echo("Cancelled", cx);
                }
            }
            MinibufferPurpose::QuitSave { relaunch } => match input.as_str() {
                "yes" | "y" => {
                    // A failed write aborts the quit — the error echo would never
                    // be seen otherwise, and the unsaved buffer would be lost.
                    if self.save_all_modified(true, cx) {
                        self.finish_quit(relaunch, cx);
                    }
                }
                "no" | "n" => self.finish_quit(relaunch, cx),
                _ => self.echo("Cancelled", cx),
            },
            MinibufferPurpose::CompleteAtPoint => {
                if let Some(range) = self.completion_range.take()
                    && !input.is_empty()
                {
                    let cursor = self.edit(range, &input);
                    self.collapse_to(cursor);
                    cx.notify();
                }
            }
            MinibufferPurpose::Search(dir) => {
                if !input.is_empty() {
                    self.note_search(&input);
                    self.search_query = Some(input);
                }
                self.evil_search_forward = dir.is_forward();
                self.do_search(dir.is_forward(), cx);
            }
            MinibufferPurpose::Isearch => {
                // RET ends the search, leaving point at the current match. Remember
                // the string so a later empty C-s recalls it; keep `search_query`
                // set so the matches stay highlighted (Emacs lazy-highlight).
                if !input.is_empty() {
                    self.note_search(&input);
                    self.last_isearch = Some(input);
                } else if let Some(prev) = self.last_isearch.clone() {
                    // RET on an empty isearch with a prior string: adopt it.
                    self.search_query = Some(self.isearch_pattern(&prev));
                }
                // Emacs leaves the mark where the search began.
                if let Some(origin) = self.isearch.take().map(|st| st.origin)
                    && origin != self.point()
                {
                    self.push_mark_quietly(origin);
                }
            }
            MinibufferPurpose::OperatorSearch(dir) => {
                self.evil_operator_search_submit(dir.is_forward(), &input, cx)
            }
            MinibufferPurpose::ExCommand => self.run_ex_command(&input, cx),
            MinibufferPurpose::GridGoto => {
                if !input.is_empty() {
                    self.grid_goto_address(&input, cx);
                }
            }
            MinibufferPurpose::GridSaveAsXlsx => {
                if !input.is_empty() {
                    self.grid_save_as_xlsx(&input, cx);
                }
            }
            MinibufferPurpose::GridSearch(dir) => {
                if !input.is_empty() {
                    self.grid_search(&input, dir.is_forward(), cx);
                }
            }
            MinibufferPurpose::GridSelectMatching => {
                if !input.is_empty() {
                    self.grid_select_matching(&input, cx);
                }
            }
            MinibufferPurpose::GridFilter(col) => self.grid_filter_submit(col, &input, cx),
            MinibufferPurpose::GridComplete(r, c) => self.grid_complete_submit(r, c, &input, cx),
            MinibufferPurpose::GridColumn => {
                if !input.is_empty() {
                    self.grid_jump_submit(&input, cx);
                }
            }
            MinibufferPurpose::EvalExpression => {
                if !input.is_empty() {
                    self.eval_interactive(&input, cx); // echoes `=> value`
                }
            }
            MinibufferPurpose::SchemeCallback(callback)
            | MinibufferPurpose::SchemeCompleting(callback)
            | MinibufferPurpose::SchemeCompletingDynamic(callback) => {
                // Pass the entered text to the Scheme callback.
                self.publish_point();
                self.script.dispatch_action(&callback, Some(&input));
            }
            MinibufferPurpose::SchemeReadFile(callback) => {
                self.publish_point();
                self.script
                    .dispatch_action(&callback, Some(&expand_path(&input)));
            }
            MinibufferPurpose::SchemeYesNo(callback) => {
                let answer = if is_yes(&input) { "yes" } else { "no" };
                self.publish_point();
                self.script.dispatch_action(&callback, Some(answer));
            }
            MinibufferPurpose::ConfirmGrammarInstall => {
                let approved = is_yes(&input);
                self.resolve_grammar_install_confirm(approved, cx);
            }
        }
        // This read may have displaced a grammar trust prompt: re-ask it.
        self.resume_grammar_prompt(cx);
    }

    pub(super) fn open_confirm(
        &mut self,
        prompt: &str,
        action: PendingAction,
        cx: &mut Context<Self>,
    ) {
        let prompt = format!("{prompt} (yes or no) ");
        self.open_minibuffer(&prompt, MinibufferPurpose::Confirm(action), "", cx);
    }

    pub(super) fn perform(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::KillBuffer(name) => self.kill_buffer_named(&name, cx),
            PendingAction::KillBufferAndWindow(name) => {
                // Only close the window if it still shows the buffer.
                let was_active = name == self.active.name;
                self.kill_buffer_named(&name, cx);
                if was_active {
                    self.delete_window(cx);
                }
            }
            PendingAction::OpenLargeGrid(path) => {
                let path = std::path::PathBuf::from(path);
                let identity = super::buffers::file_identity(&path);
                if self.pending_opens.insert(identity.clone()) {
                    let name = path
                        .file_name()
                        .map_or_else(String::new, |f| f.to_string_lossy().into_owned());
                    self.echo(format!("Loading {name}…"), cx);
                    self.read_and_open(name, path, identity, true, cx);
                }
            }
            PendingAction::WriteFile { buffer, path } => {
                if buffer == self.active.name {
                    self.write_file(&path, true, cx);
                } else {
                    self.echo(format!("Not written: {buffer} is no longer current"), cx);
                }
            }
            PendingAction::SaveOverChanged(name) => {
                if name == self.active.name {
                    self.save_buffer_ex(false, cx);
                } else {
                    self.echo(format!("Not saved: {name} is no longer current"), cx);
                }
            }
        }
    }

    // --- kill-buffer / quit ----------------------------------------------
}

/// The first line of `text` minus its line ending, and whether non-empty text
/// followed it (a trailing newline alone doesn't count).
fn single_line(text: &str) -> (&str, bool) {
    let trimmed = text.trim_end_matches(['\r', '\n']);
    match trimmed.find(['\r', '\n']) {
        Some(i) => (&trimmed[..i], true),
        None => (trimmed, false),
    }
}

#[cfg(test)]
mod paste_tests {
    use super::single_line;

    #[test]
    fn single_line_shapes() {
        assert_eq!(single_line("s3cret\n"), ("s3cret", false));
        assert_eq!(single_line("s3cret\r\n\n"), ("s3cret", false));
        assert_eq!(single_line(" sp ace "), (" sp ace ", false));
        assert_eq!(single_line("one\ntwo\n"), ("one", true));
    }
}
