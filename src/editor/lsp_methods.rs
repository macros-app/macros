//! LSP request/response methods for [`Editor`] (extracted from the editor
//! module for size). These extend the same `Editor` type; cross-module
//! entry points (called from `mod.rs`'s command dispatch) are `pub(super)`.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use gpui::Context;
use serde_json::{Value, json};

use super::*;
use crate::jsonrpc::Message;
use crate::lsp::{CallDirection, ParamsKind};

/// The final path component of `path`, falling back to the whole string when
/// there is no `/` (matches the bare `rsplit('/').next().unwrap_or(path)` idiom).
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether a `set-lsp` command string selects the in-process Steel/Scheme
/// server (`"builtin"` or `"builtin:<variant>"`) rather than an external
/// shell command.
fn is_builtin_lsp(command: &str) -> bool {
    command == "builtin" || command.starts_with("builtin:")
}

/// A configured server (from `set-lsp`): its launch command and the env
/// overrides captured when it was configured.
#[derive(Clone)]
pub(super) struct LspCommand {
    pub(super) command: String,
    pub(super) env: crate::process::EnvOverrides,
}

/// One pending `textDocument/didChange`: the server's language plus the
/// pre-edit bounds of the range being replaced.
pub(super) struct LspChange {
    pub(super) language: String,
    pub(super) start: lsp::Position,
    pub(super) end: lsp::Position,
}

/// Cap on the `*lsp-log*` scrollback ring (captured server stderr).
pub(super) const LSP_LOG_CAP: usize = 2000;

/// Cap on the xref jump-back ring (`pop-tag-mark` locations).
const XREF_MARKER_RING_CAP: usize = 64;

impl Editor {
    /// Start a language server for the active buffer (if one is configured for
    /// its file type and isn't already running), and `didOpen` the buffer.
    pub(super) fn maybe_start_lsp(&mut self, cx: &mut Context<Self>) {
        if self.active_is_remote() {
            return; // a local server can't see a file on another host
        }
        let Some(language) = self.active_language() else {
            return;
        };

        // Already have a server for this language: just open the buffer in it.
        // A server whose process died is replaced by a fresh spawn below.
        match self.servers.get(&language) {
            Some(server) if !server.exited => {
                self.lsp_open_active(&language);
                return;
            }
            Some(_) => {
                self.servers.remove(&language);
            }
            None => {}
        }

        let Some(LspCommand { command, env }) = self.lsp_commands.get(&language).cloned() else {
            return; // no server configured for this language
        };

        // `"builtin"` selects the in-process Steel/Scheme server; anything else
        // is a shell command for an external server (the usual case).
        let started = if is_builtin_lsp(&command) {
            // Only the editor-aware `macros-scheme` server gets the editor API
            // seeded; the plain `scheme` server stays a vanilla interpreter.
            let seed = language == "macros-scheme";
            let globals: &[String] = if seed {
                self.script.global_names()
            } else {
                &[]
            };
            let (server, rx) = SteelLsp::start(seed, globals);
            Ok((Box::new(server) as Box<dyn LanguageServer>, rx))
        } else {
            let root = self
                .active
                .path
                .as_ref()
                .and_then(|p| p.parent())
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            LspClient::start(&command, &root, &env)
                .map(|(c, rx)| (Box::new(c) as Box<dyn LanguageServer>, rx))
        };
        let (client, mut rx) = match started {
            Ok(v) => v,
            Err(reason) => {
                // Record the cause in *lsp-log* (persistent) as well as the echo
                // area (transient) — a failed spawn is otherwise invisible.
                let line = format!("[{language}] could not start `{command}`: {reason}");
                self.lsp_log.push(line.clone());
                self.append_to_log_buffer("*lsp-log*", &line);
                self.echo(format!("Could not start {command}: {reason}"), cx);
                return;
            }
        };

        // Forward this server's messages to the editor, tagged with its language.
        let lang = language.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                let exited = message["method"] == lsp::EXITED_METHOD;
                if this
                    .update(cx, |editor, cx| {
                        editor.on_lsp_message(lang.clone(), message, cx)
                    })
                    .is_err()
                    || exited
                {
                    break; // editor dropped, or the server is gone
                }
            }
        });

        self.servers.insert(
            language.clone(),
            LspServer {
                client,
                ready: false,
                exited: false,
                caps: lsp::ServerCapabilities::default(),
                open: HashSet::new(),
                versions: HashMap::new(),
                pending_completion: None,
                pending: HashMap::new(),
                semantic_state: HashMap::new(),
                _task: task,
            },
        );
        self.echo(format!("Started {command} ({language})"), cx);
        // For external servers, optionally pop `*lsp-log*` in a split so the
        // startup stderr is visible while editing (only on an actual spawn).
        let external = !is_builtin_lsp(&command);
        if external && cx.global::<crate::settings::Settings>().lsp_log_on_start() {
            self.open_lsp_log_split(cx);
        }
    }

    /// `didOpen` the active buffer into its language's server, if ready & unseen.
    pub(super) fn lsp_open_active(&mut self, language: &str) {
        if self.active_is_remote() {
            return;
        }
        let Some(uri) = self.active_uri() else {
            return;
        };
        let text = self.active.buffer.full_text();
        // The server is keyed by `language` (e.g. "typescript"), but the wire
        // `languageId` must reflect the actual file kind (e.g. "typescriptreact"
        // for .tsx) so the server enables JSX parsing.
        let language_id = self.active_language_id(language);
        if let Some(server) = self.servers.get_mut(language) {
            if !server.ready || server.open.contains(&uri) {
                return;
            }
            server.client.did_open(&uri, &language_id, 1, &text);
            server.open.insert(uri.clone());
            server.versions.insert(uri, 1);
        }
    }

    /// The `(language, start, end)` an about-to-happen replacement of
    /// `[start_byte, end_byte)` should be reported to `lsp_did_change` with, or
    /// `None` when the buffer has no language server language. MUST be called
    /// *before* the rope is mutated — the positions are the pre-edit ones.
    /// Shared by `edit()` and the undo/redo paths, which mutate the rope
    /// directly.
    pub(super) fn lsp_change_positions(
        &self,
        start_byte: usize,
        end_byte: usize,
    ) -> Option<LspChange> {
        let language = self.active_language()?;
        Some(LspChange {
            language,
            start: self.active.buffer.byte_to_position(start_byte).into(),
            end: self.active.buffer.byte_to_position(end_byte).into(),
        })
    }

    /// Notify the active buffer's server of an edit: `change` (from
    /// [`lsp_change_positions`](Self::lsp_change_positions)) was replaced by
    /// `text`.
    pub(super) fn lsp_did_change(&mut self, change: LspChange, text: &str) {
        let LspChange {
            language,
            start,
            end,
        } = change;
        let language = language.as_str();
        let Some(uri) = self.active_uri() else {
            return;
        };
        let Some((sync_kind, open)) = self
            .servers
            .get(language)
            .map(|s| (s.caps.sync_kind, s.open.contains(&uri)))
        else {
            return;
        };
        if !open {
            return;
        }
        match sync_kind {
            // Full-document sync: coalesce. Mark the active doc dirty and wake the
            // debounce task instead of serializing + shipping the whole buffer on
            // every keystroke. `flush_lsp_fullsync` runs before any outbound
            // request and before a buffer swap, so the server never acts on a
            // stale document.
            lsp::SyncKind::Full => {
                self.lsp_fullsync_dirty = Some((language.to_string(), uri));
                let _ = self.lsp_flush_tx.unbounded_send(());
            }
            // Incremental sync: send just the changed range, per edit.
            lsp::SyncKind::Incremental => {
                if let Some(server) = self.servers.get_mut(language) {
                    let version = server.versions.entry(uri.clone()).or_insert(1);
                    *version += 1;
                    let version = *version;
                    server
                        .client
                        .did_change_incremental(&uri, version, start, end, text);
                }
            }
            lsp::SyncKind::None => {}
        }
    }

    /// Send the pending full-sync document to its server as one `did_change`.
    /// A cheap no-op when nothing is dirty. Needs no `cx`: it reads the active
    /// buffer and emits an LSP notification. Callers must ensure `self.active.buffer` is
    /// still the dirty document — true everywhere except mid-buffer-swap, which
    /// is why `snapshot_active` flushes before it takes the buffer.
    pub(super) fn flush_lsp_fullsync(&mut self) {
        let Some((language, uri)) = self.lsp_fullsync_dirty.take() else {
            return;
        };
        // Belt and braces: never ship one buffer's text under another's URI.
        if self.active_uri().as_deref() != Some(uri.as_str()) {
            return;
        }
        let full = self.active.buffer.full_text();
        if let Some(server) = self.servers.get_mut(&language) {
            if !server.open.contains(&uri) {
                return;
            }
            let version = server.versions.entry(uri.clone()).or_insert(1);
            *version += 1;
            let version = *version;
            server.client.did_change(&uri, version, &full);
        }
    }

    pub(super) fn on_lsp_message(
        &mut self,
        language: String,
        message: Value,
        cx: &mut Context<Self>,
    ) {
        let (id, result) = match Message::from(message) {
            Message::Request { id, method, params } => {
                self.on_lsp_server_request(&language, id, &method, &params, cx);
                return;
            }
            Message::Notification { method, params } => {
                self.on_lsp_notification(&language, &method, &params, cx);
                return;
            }
            // Responses to our requests.
            Message::Response { id, result, .. } => (id, result),
            Message::Invalid => return,
        };
        let is_init = self.servers.get(&language).is_some_and(|s| !s.ready);
        if is_init {
            if let Some(server) = self.servers.get_mut(&language) {
                server.ready = true;
                server.caps = lsp::ServerCapabilities::from_initialize(&result);
                server.client.initialized();
            }
            // Open the buffer that triggered the server (if it's this language).
            if self.active_language().as_deref() == Some(&language) {
                self.lsp_open_active(&language);
            }
            cx.notify();
            return;
        }
        let Some(id) = id.as_i64() else {
            return; // all our request ids are numbers
        };

        // A completion response?
        let completion =
            self.servers
                .get_mut(&language)
                .and_then(|server| match &server.pending_completion {
                    Some(pending) if pending.id == id => server.pending_completion.take(),
                    _ => None,
                });
        if let Some(PendingCompletion { range, .. }) = completion {
            // Typing (or a buffer switch) since the request makes the reply's
            // range stale; drop it rather than slice text that moved.
            let Some(range) = self.completion_reply_range(range.start, range.end) else {
                return;
            };
            let items = lsp::parse_completion(&result);
            self.show_completion(items, range, cx);
            return;
        }

        // A navigation / rename / symbols / … response?
        let pending = self
            .servers
            .get_mut(&language)
            .and_then(|server| server.pending.remove(&id));
        if let Some((kind, target)) = pending {
            self.handle_lsp_response(&language, kind, target, &result, cx);
        }
    }

    /// A server notification (including the reader thread's synthetic
    /// `$/stderr` and exit ones).
    fn on_lsp_notification(
        &mut self,
        language: &str,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) {
        match method {
            lsp::EXITED_METHOD => self.on_lsp_server_exit(language, cx),
            "$/progress" => self.on_lsp_progress(language, params),
            "textDocument/publishDiagnostics" => {
                let (uri, diagnostics) = lsp::parse_diagnostics(params);
                // Diagnostics are keyed only by URI, and each server runs its own
                // async task — so after a language switch (e.g. `scheme` →
                // `macros-scheme` via `macros-steel-mode`) the OLD server's
                // late-arriving publish could clobber the new one's. For the active
                // buffer, accept diagnostics only from its CURRENT language server;
                // a stale server's are dropped. (Background buffers — a different
                // URI — are unaffected.)
                let stale_for_active = self.active_uri().as_deref() == Some(&uri)
                    && self.active_language().as_deref() != Some(language);
                if !stale_for_active {
                    self.diagnostics.insert(uri, diagnostics);
                    cx.notify();
                }
            }
            "$/stderr" => {
                // Server stderr, captured into `*lsp-log*` (tagged by language)
                // and appended live to the buffer if it's open.
                if let Some(line) = params["line"].as_str() {
                    let line = format!("[{language}] {line}");
                    self.lsp_log.push(line.clone());
                    self.append_to_log_buffer("*lsp-log*", &line);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// Answer a server→client request. `workspace/applyEdit` edits buffers, so
    /// it's handled here; everything else gets the stateless default reply
    /// (`MethodNotFound` for methods we don't implement).
    fn on_lsp_server_request(
        &mut self,
        language: &str,
        id: Value,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) {
        if method == "workspace/applyEdit" {
            let edit = lsp::parse_workspace_edit(&params["edit"]);
            let failed = self.apply_workspace_edit(edit, cx);
            if let Some(server) = self.servers.get_mut(language) {
                let result = if failed.is_empty() {
                    json!({ "applied": true })
                } else {
                    json!({ "applied": false, "failureReason": failed.join("; ") })
                };
                server.client.respond(id, result);
            }
            return;
        }
        let reply = lsp::default_server_reply(method, params);
        if let Some(server) = self.servers.get_mut(language) {
            match reply {
                Ok(result) => server.client.respond(id, result),
                Err((code, message)) => server.client.respond_error(id, code, &message),
            }
        }
    }

    /// A `$/progress` notification → the `lsp-progress` mode-line slot. Shows
    /// the most recently begun still-running task; cleared when all end.
    fn on_lsp_progress(&mut self, language: &str, params: &Value) {
        let key = format!("{language}:{}", params["token"]);
        let value = &params["value"];
        match value["kind"].as_str() {
            Some("begin") => {
                let title = value["title"].as_str().unwrap_or_default().to_string();
                let text = lsp::progress_text(language, &title, value);
                self.lsp_progress.retain(|(k, ..)| *k != key);
                self.lsp_progress.push((key, title, text));
            }
            Some("report") => {
                if let Some(entry) = self.lsp_progress.iter_mut().find(|(k, ..)| *k == key) {
                    entry.2 = lsp::progress_text(language, &entry.1, value);
                }
            }
            Some("end") => self.lsp_progress.retain(|(k, ..)| *k != key),
            _ => return,
        }
        self.show_lsp_progress();
    }

    /// Mirror the newest in-flight progress report into its mode-line slot.
    fn show_lsp_progress(&mut self) {
        let shown = self
            .lsp_progress
            .last()
            .map(|(.., text)| text.clone())
            .unwrap_or_default();
        self.set_mode_line_slot("lsp-progress".into(), shown);
    }

    /// The server's process is gone (its stdout closed). Fail everything in
    /// flight — a format-on-save waiting on it saves unformatted right away —
    /// and mark the entry exited so the next `maybe_start_lsp` (or
    /// `lsp-restart`) spawns a fresh one.
    fn on_lsp_server_exit(&mut self, language: &str, cx: &mut Context<Self>) {
        if self.servers.get(language).is_none_or(|s| s.exited) {
            return;
        }
        self.lsp_abandon_server(language, cx);
        if let Some(server) = self.servers.get_mut(language) {
            server.exited = true;
        }
        let line = format!("[{language}] server exited");
        self.lsp_log.push(line.clone());
        self.append_to_log_buffer("*lsp-log*", &line);
        self.echo(
            format!("Language server for {language} exited — M-x lsp-restart to restart it"),
            cx,
        );
        cx.notify();
    }

    /// Drop all per-document and in-flight state of `language`'s server (it's
    /// exiting or being replaced): not ready, nothing open, every pending
    /// request failed. Saves waiting on its formatting proceed unformatted.
    fn lsp_abandon_server(&mut self, language: &str, cx: &mut Context<Self>) {
        let Some(server) = self.servers.get_mut(language) else {
            return;
        };
        server.ready = false;
        let open: Vec<String> = server.open.drain().collect();
        server.versions.clear();
        server.semantic_state.clear();
        let had_completion = server.pending_completion.take().is_some();
        let pending = std::mem::take(&mut server.pending);
        if had_completion {
            self.completion_auto_pending = false;
        }
        for uri in &open {
            self.diagnostics.remove(uri);
            if self
                .lsp_fullsync_dirty
                .as_ref()
                .is_some_and(|(l, u)| l == language && u == uri)
            {
                self.lsp_fullsync_dirty = None;
            }
        }
        let prefix = format!("{language}:");
        self.lsp_progress.retain(|(k, ..)| !k.starts_with(&prefix));
        self.show_lsp_progress();
        for (kind, target) in pending.into_values() {
            if let PendingRequest::Format { save: Some(token) } = kind {
                self.finish_format_on_save(&target.uri, token, cx);
            }
        }
    }

    /// `M-x lsp-restart`: kill the active buffer's language server (if any)
    /// and spawn a fresh one. Buffers re-`didOpen` into it as they're shown.
    pub(super) fn lsp_restart(&mut self, cx: &mut Context<Self>) {
        let Some(language) = self.active_language() else {
            self.echo(NO_LSP_FOR_BUFFER, cx);
            return;
        };
        if !self.lsp_commands.contains_key(&language) {
            self.echo(format!("No language server configured for {language}"), cx);
            return;
        }
        if self.servers.contains_key(&language) {
            self.lsp_abandon_server(&language, cx);
            // Dropping the entry kills the process and its reader task.
            self.servers.remove(&language);
        }
        self.maybe_start_lsp(cx);
    }

    /// Complete a format-on-save: if `token` is still the pending save for
    /// `uri` (not already completed by the response/timeout), write that
    /// document. Returns whether it was still pending.
    pub(super) fn finish_format_on_save(
        &mut self,
        uri: &str,
        token: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.formatting_for_save.get(uri) != Some(&token) {
            return false;
        }
        self.formatting_for_save.remove(uri);
        self.write_document_to_disk(uri, cx);
        true
    }

    /// Write the document visiting `uri` — active (async, hooks) or inactive
    /// (synchronous, like `save-some-buffers`). No-op if it was killed.
    pub(super) fn write_document_to_disk(&mut self, uri: &str, cx: &mut Context<Self>) {
        match self.document_for_uri(uri) {
            Some(None) => self.write_active_to_disk(cx),
            Some(Some(idx)) => {
                let doc = &mut self.inactive[idx];
                let Some(path) = doc.path.clone() else {
                    return;
                };
                let name = doc.name.clone();
                match doc
                    .save_bytes()
                    .and_then(|b| crate::save::write_now(&path, &b))
                {
                    Ok(()) => {
                        doc.modified = false;
                        self.echo(format!("Wrote {name}"), cx);
                        self.lsp_did_save(uri);
                        self.rewatch_file(&path, cx);
                    }
                    Err(err) => self.echo(format!("Save of {name} failed: {err}"), cx),
                }
            }
            None => {}
        }
    }

    /// Where the document visiting `uri` lives: `Some(None)` = the active
    /// buffer, `Some(Some(i))` = `inactive[i]`, `None` = not open (killed).
    pub(super) fn document_for_uri(&self, uri: &str) -> Option<Option<usize>> {
        if document_uri(&self.active) == uri {
            return Some(None);
        }
        self.inactive
            .iter()
            .position(|d| document_uri(d) == uri)
            .map(Some)
    }

    /// Whether `target`'s document is the active buffer with its text unchanged
    /// since the request went out — the precondition for applying a response
    /// that decorates or prompts in the active buffer.
    fn target_is_active_and_current(&self, target: &RequestTarget) -> bool {
        document_uri(&self.active) == target.uri && self.active.buffer.tick() == target.tick
    }

    /// `textDocument/didSave` for `uri` to every server that has it open and
    /// asked for save notifications (with the text when it asked for that).
    pub(super) fn lsp_did_save(&mut self, uri: &str) {
        let wants_text = self
            .servers
            .values()
            .any(|s| s.caps.save == Some(true) && s.open.contains(uri));
        let text = if wants_text {
            match self.document_for_uri(uri) {
                Some(None) => Some(self.active.buffer.full_text()),
                Some(Some(i)) => Some(self.inactive[i].buffer.full_text()),
                None => None,
            }
        } else {
            None
        };
        for server in self.servers.values_mut() {
            if let Some(include_text) = server.caps.save
                && server.ready
                && server.open.contains(uri)
            {
                let text = text.as_deref().filter(|_| include_text);
                server.client.did_save(uri, text);
            }
        }
    }

    /// Tell every server that has `uri` open that its whole text is now
    /// `text` — for edits made to a buffer that isn't active (workspace edits,
    /// formatting, auto-revert), which bypass the active-buffer `didChange`
    /// path. A full-text change is valid for incremental servers too.
    pub(super) fn lsp_sync_document(&mut self, uri: &str, text: &str) {
        for server in self.servers.values_mut() {
            if server.caps.sync_kind == lsp::SyncKind::None || !server.open.contains(uri) {
                continue;
            }
            let version = server.versions.entry(uri.to_string()).or_insert(1);
            *version += 1;
            let version = *version;
            server.client.did_change(uri, version, text);
        }
    }

    /// The buffer visiting `uri` is being killed: `didClose` it in every
    /// server that has it open and forget its per-document state, so reopening
    /// the file later sends a fresh `didOpen`. Any pending full-sync for it is
    /// discarded (the replacement buffer's text must never go out under this
    /// URI), as is a format-on-save waiting on it.
    pub(super) fn lsp_close_document(&mut self, uri: &str) {
        if self
            .lsp_fullsync_dirty
            .as_ref()
            .is_some_and(|(_, u)| u == uri)
        {
            self.lsp_fullsync_dirty = None;
        }
        for server in self.servers.values_mut() {
            if server.open.remove(uri) {
                server.client.did_close(uri);
            }
            server.versions.remove(uri);
            server.semantic_state.remove(uri);
        }
        self.diagnostics.remove(uri);
        self.formatting_for_save.remove(uri);
    }

    /// Dispatch a finished LSP request's `result` to the right handler.
    /// `target` is the document the request was issued against.
    pub(super) fn handle_lsp_response(
        &mut self,
        language: &str,
        kind: PendingRequest,
        target: RequestTarget,
        result: &Value,
        cx: &mut Context<Self>,
    ) {
        use PendingRequest::*;
        match kind {
            // Scheme callbacks act on the current buffer (overlays, inlay
            // suffixes, point): after a switch they'd hit the wrong one.
            Script(_) if document_uri(&self.active) != target.uri => {}
            Script(callback) => {
                // Deliver the raw result JSON to the Steel callback; Scheme
                // parses it with `string->jsexpr` and renders however it likes.
                let json = result.to_string();
                self.script.dispatch_action(&callback, Some(&json));
            }
            Goto(what) => {
                let locations = lsp::parse_locations(result);
                self.goto_locations(what, locations, cx);
            }
            References => {
                let locations = lsp::parse_locations(result);
                self.goto_locations("references", locations, cx);
            }
            DocumentSymbols | WorkspaceSymbols => {
                let symbols = lsp::parse_symbols(result, &target.uri);
                self.show_symbols(symbols, cx);
            }
            Hover => match lsp::parse_hover(result) {
                Some(text) => self.show_hover(text, cx),
                None => self.echo("No documentation at point", cx),
            },
            SignatureHelp => match lsp::parse_signature_help(result) {
                Some(text) => self.echo(text, cx),
                None => self.echo("No signature help", cx),
            },
            Rename => {
                // Positions in the edit are relative to the text the rename was
                // computed against; if the buffer moved on, they'd land wrong.
                if self.target_tick(&target) != Some(target.tick) {
                    self.echo("Buffer changed during rename; not applied", cx);
                    return;
                }
                let edit = lsp::parse_workspace_edit(result);
                self.apply_workspace_edit(edit, cx);
            }
            Format { save } => {
                let edits = lsp::parse_text_edits(result);
                self.apply_format_response(edits, &target, save, cx);
            }
            // A code action's edits are positions in the text it was computed
            // for: once the buffer moves on (typing while the request or its
            // resolve was in flight) they'd land in the wrong place.
            CodeAction | ResolveCodeAction(_) if self.target_tick(&target) != Some(target.tick) => {
                self.echo("Buffer changed; code action not applied", cx);
            }
            CodeAction => {
                let actions = lsp::parse_code_actions(result);
                self.show_code_actions(actions, cx);
            }
            ResolveCodeAction(original) => {
                // Resolved once: never round-trip it again. A null/error result
                // (nothing to add) applies the original as it stood.
                let mut action = lsp::parse_code_actions(&json!([result]))
                    .into_iter()
                    .next()
                    .unwrap_or(*original);
                action.resolve = None;
                self.apply_code_action(action, cx);
            }
            SemanticTokens { callback, kind } => {
                // Tokens only paint the active buffer, and only while its text
                // is the text they were computed for. The cache below is
                // updated either way: it mirrors the server's resultId state.
                let fresh = self.target_is_active_and_current(&target);
                // The answering server, not the active buffer's — the active
                // buffer may belong to another language by now.
                let uri = target.uri.clone();
                let Some(server) = self.servers.get_mut(language) else {
                    return;
                };
                // Resolve the packed token data per request kind, caching it (with
                // the resultId) so a later delta can splice onto it.
                let data = match kind {
                    SemanticKind::Range => lsp::semantic_data(result),
                    SemanticKind::Full => {
                        let data = lsp::semantic_data(result);
                        server.semantic_state.insert(
                            uri.clone(),
                            SemanticCache {
                                result_id: lsp::semantic_result_id(result),
                                data: data.clone(),
                            },
                        );
                        data
                    }
                    SemanticKind::Delta => {
                        let prev = server
                            .semantic_state
                            .get(&uri)
                            .map(|cache| cache.data.clone())
                            .unwrap_or_default();
                        let merged = lsp::apply_semantic_delta(&prev, result);
                        server.semantic_state.insert(
                            uri.clone(),
                            SemanticCache {
                                result_id: lsp::semantic_result_id(result),
                                data: merged.clone(),
                            },
                        );
                        merged
                    }
                };
                if !fresh {
                    return;
                }
                let tokens = lsp::decode_semantic_data(&data, &server.caps.semantic_legend);
                let records: Vec<String> = tokens
                    .iter()
                    .map(|(line, col, len, ty)| format!("{line}\t{col}\t{len}\t{ty}"))
                    .collect();
                self.script
                    .dispatch_action(&callback, Some(&records.join("\n")));
            }
            // The prompt (and the placeholder's range) belong to the buffer the
            // rename started in; after a switch or an edit, drop it quietly.
            PrepareRename if !self.target_is_active_and_current(&target) => {}
            PrepareRename => match lsp::parse_prepare_rename(result) {
                None => self.echo("Cannot rename this symbol", cx),
                Some(prep) => {
                    let placeholder = prep.placeholder.unwrap_or_else(|| match prep.range {
                        Some((s, e)) => {
                            let sb = self.active.buffer.position_to_byte(s.line, s.character);
                            let eb = self.active.buffer.position_to_byte(e.line, e.character);
                            // A server-sent range may be inverted; slicing one
                            // trips `slice_to_string`'s debug assertion.
                            self.active.buffer.slice_to_string(sb.min(eb)..sb.max(eb))
                        }
                        None => self.identifier_at_point(),
                    });
                    self.open_minibuffer(
                        "Rename to: ",
                        MinibufferPurpose::SchemeCallback("lsp-rename-to".into()),
                        &placeholder,
                        cx,
                    );
                }
            },
            ResolveCompletion(label) => {
                if let Some(popup) = self.completion_popup.as_mut() {
                    for item in popup.items.iter_mut().filter(|i| i.label == label) {
                        lsp::merge_resolved_completion(item, result);
                    }
                    cx.notify();
                }
            }
            // Lenses are line-keyed decorations of the requesting buffer: stale
            // (switched away / edited since) ones are dropped, not misplaced.
            CodeLens | ResolveCodeLens(..) if !self.target_is_active_and_current(&target) => {}
            CodeLens => self.render_code_lenses(result, cx),
            ResolveCodeLens(line, character) => {
                if let Some(title) = lsp::parse_code_lens_title(result) {
                    self.set_code_lens(line, character, &title, cx);
                }
            }
            CallHierarchyPrepare(direction) => self.lsp_call_hierarchy_calls(direction, result, cx),
            CallHierarchyCalls(direction) => {
                let calls = lsp::parse_call_hierarchy(result, direction);
                if calls.is_empty() {
                    self.echo("No calls found", cx);
                    return;
                }
                let mut rows = Vec::new();
                let mut targets = Vec::new();
                for (name, loc) in &calls {
                    let short = lsp::uri_to_path(&loc.uri);
                    let short = basename(&short);
                    rows.push(format!("{name}  —  {short}:{}", loc.start.line + 1));
                    targets.push((loc.uri.clone(), loc.start.line, loc.start.character));
                }
                self.xref_targets = targets;
                let text = format!(
                    "{} calls ({})\n\n{}",
                    direction.label(),
                    calls.len(),
                    rows.join("\n")
                );
                self.render_buffer(
                    "*lsp-xref*".into(),
                    "lsp-xref-mode".into(),
                    text,
                    Vec::new(),
                    cx,
                );
            }
        }
    }

    /// Send a semantic-tokens request (`method`/`params` built by the caller) and
    /// route the result to `callback` as `"line\tcol\tlength\ttype"` records.
    pub(super) fn semantic_send(
        &mut self,
        method: &str,
        params: Value,
        kind: SemanticKind,
        callback: String,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_dispatch(
            method,
            params,
            PendingRequest::SemanticTokens { callback, kind },
        ) {
            self.echo(NO_LSP_FOR_BUFFER, cx);
        }
    }

    /// `semanticTokens/full` — whole document; caches data for later deltas.
    pub(super) fn lsp_semantic_tokens(&mut self, callback: String, cx: &mut Context<Self>) {
        let Some(uri) = self.active_uri() else { return };
        let params = json!({ "textDocument": { "uri": uri } });
        self.semantic_send(
            "textDocument/semanticTokens/full",
            params,
            SemanticKind::Full,
            callback,
            cx,
        );
    }

    /// `semanticTokens/range` — only a window around point (cheap on big files).
    pub(super) fn lsp_semantic_tokens_range(&mut self, callback: String, cx: &mut Context<Self>) {
        let Some(uri) = self.active_uri() else { return };
        let point_line = self.active.buffer.line_index_at_byte(self.point());
        let last = self.active.buffer.line_count().saturating_sub(1);
        let start = point_line.saturating_sub(200);
        let end = (point_line + 200).min(last);
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": start, "character": 0 },
                "end": { "line": end, "character": 0 }
            }
        });
        self.semantic_send(
            "textDocument/semanticTokens/range",
            params,
            SemanticKind::Range,
            callback,
            cx,
        );
    }

    /// `semanticTokens/full/delta` when we have a cached resultId, else a full
    /// request — the efficient live-refresh path.
    pub(super) fn lsp_semantic_tokens_delta(&mut self, callback: String, cx: &mut Context<Self>) {
        let Some(uri) = self.active_uri() else { return };
        let prev_id = self
            .active_language()
            .and_then(|l| self.servers.get(&l))
            .and_then(|s| s.semantic_state.get(&uri))
            .and_then(|cache| cache.result_id.clone());
        match prev_id {
            Some(id) => {
                let params = json!({
                    "textDocument": { "uri": uri },
                    "previousResultId": id
                });
                self.semantic_send(
                    "textDocument/semanticTokens/full/delta",
                    params,
                    SemanticKind::Delta,
                    callback,
                    cx,
                );
            }
            None => self.lsp_semantic_tokens(callback, cx),
        }
    }

    /// The identifier (word of `[A-Za-z0-9_]`) surrounding point, for rename
    /// prefill when the server gives no placeholder.
    pub(super) fn identifier_at_point(&self) -> String {
        let text = self.active.buffer.full_text();
        let point = self.point().min(text.len());
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        let mut start = point;
        for (i, c) in text[..point].char_indices().rev() {
            if is_ident(c) {
                start = i;
            } else {
                break;
            }
        }
        let mut end = point;
        for (i, c) in text[point..].char_indices() {
            if is_ident(c) {
                end = point + i + c.len_utf8();
            } else {
                break;
            }
        }
        text[start..end].to_string()
    }

    /// `lsp-rename`: validate the position with `prepareRename` (when supported),
    /// then open the rename prompt prefilled with the symbol; otherwise prompt
    /// directly.
    pub(super) fn lsp_prepare_rename(&mut self, cx: &mut Context<Self>) {
        let supports = self
            .active_language()
            .and_then(|l| self.servers.get(&l))
            .is_some_and(|s| s.caps.prepare_rename);
        if supports {
            self.lsp_send(
                "textDocument/prepareRename",
                PendingRequest::PrepareRename,
                Self::text_doc_position,
                cx,
            );
        } else if self.active_lsp_ready() {
            let placeholder = self.identifier_at_point();
            self.open_minibuffer(
                "Rename to: ",
                MinibufferPurpose::SchemeCallback("lsp-rename-to".into()),
                &placeholder,
                cx,
            );
        } else {
            self.echo(NO_LSP_FOR_BUFFER, cx);
        }
    }

    /// `lsp-code-lens`: request lenses; resolved titles render as virtual text.
    pub(super) fn lsp_code_lens(&mut self, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/codeLens",
            PendingRequest::CodeLens,
            |uri, _, _| json!({ "textDocument": { "uri": uri } }),
            cx,
        );
    }

    /// Render code lenses: those with a command show immediately; the rest are
    /// resolved via `codeLens/resolve` (the lens echoed back as a `Value`).
    pub(super) fn render_code_lenses(&mut self, result: &Value, cx: &mut Context<Self>) {
        for lens in lsp::parse_code_lenses(result) {
            match lens.title {
                Some(title) => self.set_code_lens(lens.line, lens.character, &title, cx),
                None => {
                    let kind = PendingRequest::ResolveCodeLens(lens.line, lens.character);
                    self.lsp_dispatch("codeLens/resolve", lens.data, kind);
                }
            }
        }
    }

    /// Show a code lens as end-of-line virtual text.
    pub(super) fn set_code_lens(
        &mut self,
        line: usize,
        _character: usize,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        self.active.overlays.suffixes.insert(
            line,
            (format!("  » {title}").into(), "lsp-code-lens".into()),
        );
        cx.notify();
    }

    /// Resolve the currently-selected completion item (documentation +
    /// auto-import edits) if it hasn't been resolved yet.
    pub(super) fn completion_resolve_selected(&mut self) {
        let filtered = self.popup_filtered();
        let Some(popup) = self.completion_popup.as_ref() else {
            return;
        };
        let Some(item) = filtered.get(popup.selected.min(filtered.len().saturating_sub(1))) else {
            return;
        };
        if item.resolved {
            return;
        }
        let label = item.label.clone();
        let data = item.data.clone();
        self.lsp_dispatch(
            "completionItem/resolve",
            data,
            PendingRequest::ResolveCompletion(label),
        );
    }

    /// Accept a completion that carries `additionalTextEdits` (e.g. an
    /// auto-import): apply them together with the insertion in one pass so byte
    /// offsets stay consistent, then place the cursor after the inserted text.
    pub(super) fn accept_completion_with_edits(
        &mut self,
        anchor: usize,
        point: usize,
        choice: &CompletionItem,
        cx: &mut Context<Self>,
    ) {
        let full = self.active.buffer.full_text();
        let main = lsp::TextEdit {
            start: self.active.buffer.byte_to_position(anchor).into(),
            end: self.active.buffer.byte_to_position(point).into(),
            new_text: choice.insert_text.clone(),
        };
        let mut edits = choice.additional_edits.clone();
        edits.push(main);
        let new_text = lsp::apply_edits_to_text(&full, &edits);
        // Cursor lands at the end of the inserted completion, shifted by any
        // edits that fall entirely before the anchor (e.g. an import at the top).
        let mut shift: isize = 0;
        for e in &choice.additional_edits {
            let s = self
                .active
                .buffer
                .position_to_byte(e.start.line, e.start.character);
            let en = self
                .active
                .buffer
                .position_to_byte(e.end.line, e.end.character);
            if en <= anchor {
                shift += e.new_text.len() as isize - (en as isize - s as isize);
            }
        }
        let cursor =
            ((anchor as isize + shift) as usize + choice.insert_text.len()).min(new_text.len());
        self.replace_whole_buffer(&new_text, cursor, cx);
        cx.notify();
    }

    /// `prepareCallHierarchy` at point (incoming/outgoing call hierarchy).
    pub(super) fn lsp_call_hierarchy(&mut self, direction: CallDirection, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/prepareCallHierarchy",
            PendingRequest::CallHierarchyPrepare(direction),
            Self::text_doc_position,
            cx,
        );
    }

    /// Second leg: send `callHierarchy/incoming|outgoingCalls` with the prepared
    /// item echoed back verbatim (a `serde_json::Value`, so integer fields stay
    /// integers — unlike a Scheme round-trip).
    pub(super) fn lsp_call_hierarchy_calls(
        &mut self,
        direction: CallDirection,
        prepared: &Value,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = prepared.as_array().and_then(|a| a.first()).cloned() else {
            self.echo("No call hierarchy at point", cx);
            return;
        };
        let kind = PendingRequest::CallHierarchyCalls(direction);
        self.lsp_dispatch(direction.method(), json!({ "item": item }), kind);
    }

    // --- LSP navigation / rename / symbols / formatting ---------------------

    /// Send `params` for `method` to the active buffer's ready+open language
    /// server, recording `kind` so the response routes back. Returns whether it
    /// was sent (no echo — callers decide whether to report failure).
    pub(super) fn lsp_dispatch(
        &mut self,
        method: &str,
        params: Value,
        kind: PendingRequest,
    ) -> bool {
        // The server must have every edit before it answers this request.
        self.flush_lsp_fullsync();
        let (Some(language), Some(uri)) = (self.active_language(), self.active_uri()) else {
            return false;
        };
        let target = RequestTarget {
            uri: uri.clone(),
            tick: self.active.buffer.tick(),
        };
        let Some(server) = self
            .servers
            .get_mut(&language)
            .filter(|s| s.ready && s.open.contains(&uri))
        else {
            return false;
        };
        // A new semantic-tokens request for a document supersedes any still in
        // flight for it (they'd paint the same buffer, staler): cancel them.
        if matches!(kind, PendingRequest::SemanticTokens { .. }) {
            let superseded: Vec<i64> = server
                .pending
                .iter()
                .filter(|(_, (k, t))| {
                    matches!(k, PendingRequest::SemanticTokens { .. }) && t.uri == uri
                })
                .map(|(id, _)| *id)
                .collect();
            for id in superseded {
                server.pending.remove(&id);
                server.client.cancel(id);
            }
        }
        match server.client.request(method, params) {
            Some(id) => {
                server.pending.insert(id, (kind, target));
                true
            }
            None => false,
        }
    }

    /// The current edit tick of `target`'s document, or `None` once it's
    /// been killed — compared against `target.tick` to detect stale results.
    fn target_tick(&self, target: &RequestTarget) -> Option<u64> {
        Some(match self.document_for_uri(&target.uri)? {
            None => self.active.buffer.tick(),
            Some(i) => self.inactive[i].buffer.tick(),
        })
    }

    /// Like [`lsp_dispatch`](Self::lsp_dispatch) but builds position params from
    /// point via `build` and echoes on failure. The common entry point.
    pub(super) fn lsp_send(
        &mut self,
        method: &str,
        kind: PendingRequest,
        build: impl FnOnce(&str, usize, usize) -> Value,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(uri) = self.active_uri() else {
            self.echo(NO_LSP_FOR_BUFFER, cx);
            return false;
        };
        let (line, character) = self.active.buffer.byte_to_position(self.point());
        let params = build(&uri, line, character);
        let sent = self.lsp_dispatch(method, params, kind);
        if !sent {
            self.echo(NO_LSP_FOR_BUFFER, cx);
        }
        sent
    }

    pub(super) fn text_doc_position(uri: &str, line: usize, character: usize) -> Value {
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        })
    }

    /// `M-.` / go-to-definition (and the declaration/type/impl variants).
    pub(super) fn lsp_goto(
        &mut self,
        method: &'static str,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.lsp_send(
            method,
            PendingRequest::Goto(what),
            Self::text_doc_position,
            cx,
        );
    }

    /// `M-?` / find references (includes the declaration).
    pub(super) fn lsp_references(&mut self, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/references",
            PendingRequest::References,
            |uri, line, character| {
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": { "includeDeclaration": true }
                })
            },
            cx,
        );
    }

    pub(super) fn lsp_hover(&mut self, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/hover",
            PendingRequest::Hover,
            Self::text_doc_position,
            cx,
        );
    }

    pub(super) fn lsp_signature_help(&mut self, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/signatureHelp",
            PendingRequest::SignatureHelp,
            Self::text_doc_position,
            cx,
        );
    }

    pub(super) fn lsp_document_symbols(&mut self, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/documentSymbol",
            PendingRequest::DocumentSymbols,
            |uri, _, _| json!({ "textDocument": { "uri": uri } }),
            cx,
        );
    }

    /// `workspace/symbol` for a query read from the minibuffer.
    pub(super) fn lsp_workspace_symbol(&mut self, query: String, cx: &mut Context<Self>) {
        let params = json!({ "query": query });
        if !self.lsp_dispatch("workspace/symbol", params, PendingRequest::WorkspaceSymbols) {
            self.echo(NO_LSP_FOR_BUFFER, cx);
        }
    }

    /// Rename the symbol at point to `new_name` across the workspace.
    pub(super) fn lsp_rename(&mut self, new_name: String, cx: &mut Context<Self>) {
        self.lsp_send(
            "textDocument/rename",
            PendingRequest::Rename,
            move |uri, line, character| {
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "newName": new_name
                })
            },
            cx,
        );
    }

    /// Format the active buffer (`textDocument/formatting`).
    pub(super) fn lsp_format(&mut self, cx: &mut Context<Self>) {
        self.lsp_format_request(None, cx);
    }

    /// Send `textDocument/formatting` for the active buffer; `save` carries
    /// the format-on-save token. Returns whether the request went out.
    pub(super) fn lsp_format_request(&mut self, save: Option<u64>, cx: &mut Context<Self>) -> bool {
        let tab_width = cx.global::<crate::settings::Settings>().tab_width();
        self.lsp_send(
            "textDocument/formatting",
            PendingRequest::Format { save },
            move |uri, _, _| {
                json!({
                    "textDocument": { "uri": uri },
                    "options": { "tabSize": tab_width, "insertSpaces": true }
                })
            },
            cx,
        )
    }

    /// Request code actions for the selection (or the point's line), passing any
    /// diagnostics in range as context.
    pub(super) fn lsp_code_action(&mut self, cx: &mut Context<Self>) {
        let (start, end) = if self.active.mark_active {
            (
                self.active.selected_range.start,
                self.active.selected_range.end,
            )
        } else {
            let line = self.active.buffer.line_range_at(self.point());
            (line.start, line.end)
        };
        let start_pos = self.active.buffer.byte_to_position(start);
        let end_pos = self.active.buffer.byte_to_position(end);
        // The overlapping diagnostics exactly as the server published them —
        // servers match quick fixes on `code`/`source`/`data`, which a
        // re-serialized range+message would drop.
        let buffer = &self.active.buffer;
        let context_diags: Vec<Value> = self
            .active_uri()
            .and_then(|uri| self.diagnostics.get(&uri))
            .into_iter()
            .flatten()
            .filter(|d| {
                let s = buffer.position_to_byte(d.start.line, d.start.character);
                let e = buffer.position_to_byte(d.end.line, d.end.character).max(s);
                s < end && e > start || s == start
            })
            .map(|d| d.raw.clone())
            .collect();
        self.lsp_send(
            "textDocument/codeAction",
            PendingRequest::CodeAction,
            move |uri, _, _| {
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": start_pos.0, "character": start_pos.1 },
                        "end": { "line": end_pos.0, "character": end_pos.1 }
                    },
                    "context": { "diagnostics": context_diags }
                })
            },
            cx,
        );
    }

    // --- result handlers ----------------------------------------------------

    /// Go to a single location, or offer multiple in a Helm picker (with a
    /// source preview pane) — RET jumps to the chosen one.
    pub(super) fn goto_locations(
        &mut self,
        what: &str,
        locations: Vec<lsp::Location>,
        cx: &mut Context<Self>,
    ) {
        match locations.len() {
            0 => self.echo(format!("No {what} found"), cx),
            1 => {
                let loc = &locations[0];
                self.goto_file_position(loc.uri.clone(), loc.start.line, loc.start.character, cx);
            }
            _ => self.helm_locations(what, locations, cx),
        }
    }

    /// Open a Helm picker over `locations` (parallel candidate rows
    /// `file:line:col: source`), jumping to the chosen one on RET. The preview
    /// pane shows the source around the selected location.
    fn helm_locations(
        &mut self,
        what: &str,
        locations: Vec<lsp::Location>,
        cx: &mut Context<Self>,
    ) {
        let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();
        let mut candidates = Vec::new();
        for loc in &locations {
            let path = lsp::uri_to_path(&loc.uri);
            let lines = file_cache.entry(path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            });
            let short = basename(&path);
            let text = lines.get(loc.start.line).map_or("", |s| s.trim());
            candidates.push(format!(
                "{short}:{}:{}: {text}",
                loc.start.line + 1,
                loc.start.character + 1
            ));
        }
        let source = HelmSource {
            name: format!("{what} ({})", locations.len()),
            candidates,
            annotations: HashMap::new(),
            dynamic: None,
            raw: false,
            action: HelmAction::GotoLocation(locations),
            preview: None,
        };
        self.helm_open(format!("{what}: "), vec![source], String::new(), cx);
    }

    /// The source-context preview for an LSP location: a windowed slice of the
    /// file with the target line marked, shown in the Helm preview pane.
    pub(super) fn location_preview(&self, loc: &lsp::Location) -> String {
        let path = lsp::uri_to_path(&loc.uri);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let target = loc.start.line;
        const CONTEXT: usize = 8;
        let start = target.saturating_sub(CONTEXT);
        let end = (target + CONTEXT + 1).min(lines.len());
        let short = basename(&path);
        let mut out = vec![format!("{short}:{}", target + 1), String::new()];
        for (i, line) in lines.iter().enumerate().take(end).skip(start) {
            let marker = if i == target { ">" } else { " " };
            out.push(format!("{marker} {:>4}  {line}", i + 1));
        }
        out.join("\n")
    }

    /// Open `uri` (or stay if it's the active buffer) and move point to the LSP
    /// position. Cross-file jumps read the file synchronously (source files are
    /// small) so point can land precisely once the buffer is installed.
    pub(super) fn goto_file_position(
        &mut self,
        uri: String,
        line: usize,
        character: usize,
        cx: &mut Context<Self>,
    ) {
        // Remember where we are so `xref-pop-marker` can return here.
        self.xref_push_marker();
        let path = PathBuf::from(lsp::uri_to_path(&uri));
        let same = self.active.path.as_deref() == Some(path.as_path());
        if !same && !self.visit_path(&path, || uri.clone(), cx) {
            return; // unreadable target — stay put (the error was echoed)
        }
        let offset = self.active.buffer.position_to_byte(line, character);
        self.move_to(offset, cx);
    }

    /// Make `path` the active buffer when it isn't already: prefer an already-open
    /// inactive buffer (just activate it), otherwise read the file from disk and
    /// open it. `fallback_name` supplies the buffer name when `path` has no file
    /// component. The caller is responsible for the "is it already active?" check
    /// and for positioning point afterward. Returns whether the buffer was
    /// switched — an unreadable file (permissions, non-UTF-8, …) is reported and
    /// leaves the editor unchanged, so a later save can't truncate it.
    fn visit_path(
        &mut self,
        path: &Path,
        fallback_name: impl FnOnce() -> String,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(index) = self
            .inactive
            .iter()
            .position(|d| d.path.as_deref() == Some(path))
        {
            self.activate(index, true, cx);
            return true;
        }
        let stamp = crate::save::stamp(path);
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // Missing file → visit it as a fresh (empty) buffer, matching
            // `find_file`'s new-file flow.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                self.echo(format!("Could not open {}: {err}", path.display()), cx);
                return false;
            }
        };
        let name = path
            .file_name()
            .map_or_else(fallback_name, |f| f.to_string_lossy().into_owned());
        if !self.is_visiting(&super::buffers::file_identity(path)) {
            crate::save::note_synced(path, stamp, contents.as_bytes());
        }
        self.open_file(name, path.to_path_buf(), contents, cx);
        true
    }

    // --- generic Scheme bridge + jump-back stack ----------------------------

    /// Push the current location onto the jump-back ring (`pop-tag-mark`).
    pub(super) fn xref_push_marker(&mut self) {
        self.xref_marker_stack
            .push((self.active.path.clone(), self.point()));
        // Bound the ring so it can't grow without limit.
        if self.xref_marker_stack.len() > XREF_MARKER_RING_CAP {
            self.xref_marker_stack.remove(0);
        }
    }

    /// Pop the jump-back ring and return there (Emacs `pop-tag-mark`, vim C-o).
    pub(super) fn xref_pop_marker(&mut self, cx: &mut Context<Self>) {
        let Some((path, offset)) = self.xref_marker_stack.pop() else {
            self.echo("No further jump-back markers", cx);
            return;
        };
        if let Some(path) = path
            && self.active.path.as_deref() != Some(path.as_path())
            && !self.visit_path(&path, String::default, cx)
        {
            return; // unreadable target — stay put (the error was echoed)
        }
        let offset = offset.min(self.active.buffer.len());
        self.move_to(offset, cx);
    }

    /// The active buffer's LSP language id + URI, or `None` (with an echo) when
    /// there's no server-eligible buffer.
    pub(super) fn lsp_language_uri(&self) -> Option<(String, String)> {
        Some((self.active_language()?, self.active_uri()?))
    }

    /// Base request params for `kind`, with the active buffer's URI/position/
    /// range filled in. `extra` (a JSON object) is merged over the base; for
    /// [`ParamsKind::Raw`] the base is empty and `extra` is used verbatim.
    pub(super) fn lsp_build_params(&self, kind: ParamsKind, extra: Value, uri: &str) -> Value {
        let buffer = &self.active.buffer;
        let range_json = |start: usize, end: usize| {
            let (sl, sc) = buffer.byte_to_position(start);
            let (el, ec) = buffer.byte_to_position(end);
            json!({ "start": { "line": sl, "character": sc }, "end": { "line": el, "character": ec } })
        };
        let position = || {
            let (line, character) = buffer.byte_to_position(self.point());
            json!({ "line": line, "character": character })
        };
        let mut params = match kind {
            ParamsKind::Document => json!({ "textDocument": { "uri": uri } }),
            ParamsKind::Range => json!({
                "textDocument": { "uri": uri },
                "range": range_json(0, buffer.len())
            }),
            ParamsKind::Region => {
                let region = if self.active.mark_active {
                    self.active.selected_range.clone()
                } else {
                    buffer.line_range_at(self.point())
                };
                json!({
                    "textDocument": { "uri": uri },
                    "range": range_json(region.start, region.end),
                    "options": { "tabSize": 4, "insertSpaces": true }
                })
            }
            ParamsKind::Positions => {
                json!({ "textDocument": { "uri": uri }, "positions": [position()] })
            }
            ParamsKind::Raw => json!({}),
            ParamsKind::Position => {
                json!({ "textDocument": { "uri": uri }, "position": position() })
            }
        };
        match (params.as_object_mut(), extra) {
            (Some(base), Value::Object(ext)) => base.extend(ext),
            (_, extra) if kind == ParamsKind::Raw => params = extra,
            _ => {}
        }
        params
    }

    /// The generic `(lsp-request method kind params callback)` bridge: send any
    /// request and deliver its result JSON to the Steel `callback`.
    pub(super) fn lsp_script_request(
        &mut self,
        method: String,
        kind: ParamsKind,
        params_json: String,
        callback: String,
        cx: &mut Context<Self>,
    ) {
        let Some((_, uri)) = self.lsp_language_uri() else {
            self.echo(NO_LSP_FOR_BUFFER, cx);
            return;
        };
        let extra: Value = serde_json::from_str(&params_json).unwrap_or(Value::Null);
        let params = self.lsp_build_params(kind, extra, &uri);
        if !self.lsp_dispatch(&method, params, PendingRequest::Script(callback)) {
            self.echo(NO_LSP_FOR_BUFFER, cx);
        }
    }

    /// `(lsp-apply-workspace-edit json)`: apply a `WorkspaceEdit` (from a Scheme
    /// handler) across files.
    pub(super) fn lsp_apply_workspace_edit_json(
        &mut self,
        json_text: String,
        cx: &mut Context<Self>,
    ) {
        let value: Value = serde_json::from_str(&json_text).unwrap_or(Value::Null);
        let edit = lsp::parse_workspace_edit(&value);
        self.apply_workspace_edit(edit, cx);
    }

    /// `(lsp-apply-text-edits json)`: apply `TextEdit[]` to the active buffer.
    pub(super) fn lsp_apply_text_edits_json(&mut self, json_text: String, cx: &mut Context<Self>) {
        let value: Value = serde_json::from_str(&json_text).unwrap_or(Value::Null);
        let edits = lsp::parse_text_edits(&value);
        self.apply_format_edits(edits, cx);
    }

    /// Render document/workspace symbols into `*lsp-symbols*` (RET visits).
    pub(super) fn show_symbols(&mut self, symbols: Vec<lsp::SymbolInfo>, cx: &mut Context<Self>) {
        if symbols.is_empty() {
            self.echo("No symbols", cx);
            return;
        }
        let mut rows = Vec::new();
        let mut targets = Vec::new();
        for sym in &symbols {
            let indent = "  ".repeat(sym.depth);
            let kind = lsp::symbol_kind_name(sym.kind);
            let container = if sym.container.is_empty() {
                String::new()
            } else {
                format!("  ({})", sym.container)
            };
            rows.push(format!("{indent}{kind}  {}{container}", sym.name));
            targets.push((sym.uri.clone(), sym.start.line, sym.start.character));
        }
        self.xref_targets = targets;
        let text = format!("Symbols ({})\n\n{}", symbols.len(), rows.join("\n"));
        self.render_buffer(
            "*lsp-symbols*".into(),
            "lsp-xref-mode".into(),
            text,
            Vec::new(),
            cx,
        );
    }

    /// RET in an `*lsp-xref*` / `*lsp-symbols*` buffer: visit row N's target.
    /// The first two rows are the header + blank line, so row 0 maps to target 0.
    pub(super) fn lsp_xref_visit(&mut self, cx: &mut Context<Self>) {
        let row = self.active.buffer.line_index_at_byte(self.point());
        let Some(index) = row.checked_sub(2) else {
            return; // header / blank line
        };
        if let Some((uri, line, character)) = self.xref_targets.get(index).cloned() {
            self.goto_file_position(uri, line, character, cx);
        }
    }

    /// Show hover documentation in the echo area (minibuffer), Emacs eldoc-style.
    /// Multi-line docs are collapsed to a single line: markdown code fences and
    /// blank lines are dropped and the rest joined, so the signature and summary
    /// stay readable on one line.
    pub(super) fn show_hover(&mut self, text: String, cx: &mut Context<Self>) {
        let line = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("```"))
            .collect::<Vec<_>>()
            .join("  ");
        if line.is_empty() {
            self.echo("No documentation at point", cx);
        } else {
            self.echo(line, cx);
        }
    }

    /// `(lsp-apply-text-edits json)`'s worker: apply `edits` to the active
    /// buffer (whole-buffer replace, one undo step), keeping point clamped.
    pub(super) fn apply_format_edits(&mut self, edits: Vec<lsp::TextEdit>, cx: &mut Context<Self>) {
        self.apply_format_edits_active(&edits, false, cx);
    }

    fn apply_format_edits_active(
        &mut self,
        edits: &[lsp::TextEdit],
        quiet: bool,
        cx: &mut Context<Self>,
    ) {
        let current = self.active.buffer.full_text();
        let formatted = lsp::apply_edits_to_text(&current, edits);
        if edits.is_empty() || formatted == current {
            if !quiet {
                self.echo("No formatting changes", cx);
            }
        } else {
            let point = self.point();
            self.replace_whole_buffer(&formatted, point, cx);
            if !quiet {
                self.echo("Formatted buffer", cx);
            }
        }
    }

    /// A `textDocument/formatting` response: apply it to the document it was
    /// requested for — active or not — unless that document changed since
    /// (the edits' positions would be stale) or was killed. For format-on-save
    /// (`save` = its token) the formatted document is then written, unless
    /// the timeout already saved it unformatted.
    fn apply_format_response(
        &mut self,
        edits: Vec<lsp::TextEdit>,
        target: &RequestTarget,
        save: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if let Some(token) = save
            && self.formatting_for_save.get(&target.uri) != Some(&token)
        {
            return; // timed out (saved unformatted) or the buffer was killed
        }
        match self.target_tick(target) {
            Some(tick) if tick == target.tick => match self.document_for_uri(&target.uri) {
                Some(None) => self.apply_format_edits_active(&edits, save.is_some(), cx),
                Some(Some(idx)) => {
                    let current = self.inactive[idx].buffer.full_text();
                    let formatted = lsp::apply_edits_to_text(&current, &edits);
                    if formatted != current {
                        self.replace_inactive_text(idx, &formatted, true);
                    }
                }
                None => {}
            },
            Some(_) if save.is_some() => {
                self.echo("Buffer changed while formatting; saving unformatted", cx)
            }
            Some(_) => self.echo("Buffer changed while formatting; edits discarded", cx),
            None => {}
        }
        if let Some(token) = save {
            self.finish_format_on_save(&target.uri, token, cx);
        }
    }

    /// Apply a workspace edit (rename / code action / `workspace/applyEdit`):
    /// the active buffer in place (undoable), other files on disk (and
    /// refreshed if open). Returns the per-file failures (empty = all applied).
    pub(super) fn apply_workspace_edit(
        &mut self,
        edit: lsp::WorkspaceEdit,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        if edit.changes.is_empty() {
            self.echo("Nothing to change", cx);
            return Vec::new();
        }
        let mut files = 0;
        let mut total = 0;
        // Files whose disk write (or read) failed — reported in the echo so a
        // partially-applied rename is never silently presented as a success.
        let mut failed: Vec<String> = Vec::new();
        for (uri, edits) in &edit.changes {
            let path = PathBuf::from(lsp::uri_to_path(uri));
            total += edits.len();
            // Match open buffers by canonical identity (as `find_file` does): a
            // buffer opened via a symlink / relative path / `/tmp` alias must be
            // edited in place, not bypassed by a disk write it would then clobber.
            let identity = super::buffers::file_identity(&path);
            let visits = |d: &Document| {
                d.path
                    .as_deref()
                    .is_some_and(|p| super::buffers::file_identity(p) == identity)
            };
            if visits(&self.active) {
                // Active buffer: edit in place so it's undoable + reparsed.
                let current = self.active.buffer.full_text();
                let updated = lsp::apply_edits_to_text(&current, edits);
                if updated != current {
                    let point = self.point();
                    self.replace_whole_buffer(&updated, point, cx);
                }
            } else if let Some(idx) = self.inactive.iter().position(visits) {
                // Open but not active: rewrite its buffer (undoably, and synced
                // to its servers — they'd otherwise keep the pre-edit text) and
                // persist to disk.
                let current = self.inactive[idx].buffer.full_text();
                let updated = lsp::apply_edits_to_text(&current, edits);
                if updated != current {
                    self.replace_inactive_text(idx, &updated, true);
                }
                // Write/re-watch via the buffer's own spelling of the path — the
                // file watch is keyed by it.
                let path = self.inactive[idx].path.clone().unwrap_or(path);
                match crate::save::write_now(&path, updated.as_bytes()) {
                    // Only a successful write makes the buffer clean; on failure
                    // it keeps its (edited) unsaved state so nothing is lost.
                    Ok(()) => {
                        self.inactive[idx].modified = false;
                        let doc_uri = document_uri(&self.inactive[idx]);
                        self.lsp_did_save(&doc_uri);
                        self.rewatch_file(&path, cx);
                    }
                    Err(err) => {
                        self.inactive[idx].modified = true;
                        failed.push(format!("{}: {err}", path.display()));
                    }
                }
            } else {
                // Not open: apply on disk. An unreadable existing file must NOT
                // be treated as empty — the edits would then overwrite it.
                let current = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(err) => {
                        failed.push(format!("{}: {err}", path.display()));
                        continue;
                    }
                };
                let updated = lsp::apply_edits_to_text(&current, edits);
                if let Err(err) = crate::save::write_now(&path, updated.as_bytes()) {
                    failed.push(format!("{}: {err}", path.display()));
                    continue;
                }
            }
            files += 1;
        }
        let mut message = format!("Applied {total} edit(s) across {files} file(s)");
        if !failed.is_empty() {
            message.push_str(&format!(
                "; {} file(s) failed: {}",
                failed.len(),
                failed.join("; ")
            ));
        }
        self.echo(message, cx);
        failed
    }

    /// List code actions in `*lsp-actions*` (RET applies). Edit-only actions
    /// apply immediately; command actions run via `workspace/executeCommand`.
    pub(super) fn show_code_actions(
        &mut self,
        actions: Vec<lsp::CodeAction>,
        cx: &mut Context<Self>,
    ) {
        if actions.is_empty() {
            self.echo("No code actions available", cx);
            return;
        }
        let candidates: Vec<String> = actions
            .iter()
            .enumerate()
            .map(|(i, a)| format!("{}. {}", i + 1, a.title))
            .collect();
        // Kept in sync for the buffer-based RET path (`lsp-apply-code-action`),
        // which some keymaps may still reach.
        self.code_actions = actions.clone();
        self.code_actions_source = Some(self.active.name.clone());
        let source = HelmSource {
            name: format!("Code actions ({})", actions.len()),
            candidates,
            annotations: HashMap::new(),
            dynamic: None,
            raw: false,
            action: HelmAction::ApplyCodeAction(actions),
            preview: None,
        };
        self.helm_open("Code action: ".into(), vec![source], String::new(), cx);
    }

    /// A preview of what a code action will do: its title plus a per-file summary
    /// of the edits (and any server command), shown in the Helm preview pane.
    pub(super) fn code_action_preview(action: &lsp::CodeAction) -> String {
        let mut out = vec![action.title.clone(), String::new()];
        match &action.edit {
            Some(edit) if !edit.changes.is_empty() => {
                for (uri, edits) in &edit.changes {
                    let path = lsp::uri_to_path(uri);
                    let short = basename(&path);
                    out.push(format!("{short} — {} edit(s)", edits.len()));
                    for e in edits {
                        let new = e.new_text.replace('\n', "\\n");
                        let new = if new.chars().count() > 60 {
                            format!("{}…", new.chars().take(60).collect::<String>())
                        } else {
                            new
                        };
                        out.push(format!("  {}: {new}", e.start.line + 1));
                    }
                }
            }
            _ => {}
        }
        if let Some((command, _)) = &action.command {
            out.push(format!("Command: {command}"));
        }
        if out.len() == 2 {
            out.push("(no preview available)".to_string());
        }
        out.join("\n")
    }

    /// RET in `*lsp-actions*`: apply the action on the current row.
    pub(super) fn lsp_apply_code_action(&mut self, cx: &mut Context<Self>) {
        let row = self.active.buffer.line_index_at_byte(self.point());
        let Some(index) = row.checked_sub(2) else {
            return;
        };
        let Some(action) = self.code_actions.get(index).cloned() else {
            return;
        };
        // Return to the source buffer before applying, so edits land there (the
        // `*lsp-actions*` buffer is active when this runs): the one the actions
        // were requested in, else the most recently used file buffer.
        let source = self.code_actions_source.clone();
        if source.as_deref() != Some(self.active.name.as_str()) {
            let idx = source
                .and_then(|name| self.inactive.iter().position(|d| d.name == name))
                .or_else(|| self.inactive.iter().rposition(|d| d.special.is_none()));
            if let Some(idx) = idx {
                self.activate(idx, true, cx);
            }
        }
        self.apply_code_action(action, cx);
    }

    /// Apply a resolved code action to the current buffer: its workspace edit
    /// and/or the server `command`. The caller must have the source buffer
    /// active.
    pub(super) fn apply_code_action(&mut self, action: lsp::CodeAction, cx: &mut Context<Self>) {
        // A literal without its edit may be lazily filled in: when the server
        // supports it, resolve first (the response re-enters here with
        // `resolve` cleared); otherwise apply it as it stands.
        let can_resolve = self
            .active_language()
            .and_then(|l| self.servers.get(&l))
            .is_some_and(|s| s.caps.code_action_resolve);
        if can_resolve && let Some(raw) = action.resolve.clone() {
            let mut original = action.clone();
            original.resolve = None;
            let kind = PendingRequest::ResolveCodeAction(Box::new(original));
            if self.lsp_dispatch("codeAction/resolve", raw, kind) {
                return;
            }
        }
        let had_edit = action.edit.is_some();
        if let Some(edit) = action.edit {
            self.apply_workspace_edit(edit, cx);
        }
        if let Some((command, arguments)) = action.command {
            self.lsp_execute_command(command, arguments, cx);
        } else if !had_edit {
            self.echo("Code action had no edit", cx);
        }
    }

    /// Send `workspace/executeCommand` (a code action's server command). No
    /// response is tracked; the server applies its effect via diagnostics/edits.
    pub(super) fn lsp_execute_command(
        &mut self,
        command: String,
        arguments: Value,
        cx: &mut Context<Self>,
    ) {
        let Some(language) = self.active_language() else {
            return;
        };
        let params = json!({ "command": command, "arguments": arguments });
        if let Some(server) = self.servers.get_mut(&language)
            && server.ready
        {
            server.client.request("workspace/executeCommand", params);
            self.echo("Ran code action", cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Entity, TestAppContext};

    use super::*;
    use crate::lsp::{LanguageServer, Position, TextEdit, WorkspaceEdit};

    /// What a [`FakeServer`] was sent.
    #[derive(Default)]
    struct Sent {
        requests: Vec<(String, Value)>,
        responses: Vec<(Value, Value)>,
        changes: Vec<(String, String)>,
        saves: Vec<String>,
    }

    /// A language server that records what it's sent and numbers its
    /// requests from 1.
    struct FakeServer {
        sent: Rc<RefCell<Sent>>,
        next_id: i64,
    }

    impl LanguageServer for FakeServer {
        fn did_open(&mut self, _uri: &str, _language_id: &str, _version: i32, _text: &str) {}

        fn did_change(&mut self, uri: &str, _version: i32, text: &str) {
            self.sent
                .borrow_mut()
                .changes
                .push((uri.to_string(), text.to_string()));
        }

        fn completion(&mut self, _uri: &str, _line: usize, _character: usize) -> i64 {
            0
        }

        fn request(&mut self, method: &str, params: Value) -> Option<i64> {
            self.next_id += 1;
            self.sent
                .borrow_mut()
                .requests
                .push((method.to_string(), params));
            Some(self.next_id)
        }

        fn respond(&mut self, id: Value, result: Value) {
            self.sent.borrow_mut().responses.push((id, result));
        }

        fn did_close(&mut self, _uri: &str) {}

        fn did_save(&mut self, uri: &str, _text: Option<&str>) {
            self.sent.borrow_mut().saves.push(uri.to_string());
        }
    }

    fn setup(cx: &mut TestAppContext) -> Entity<Editor> {
        crate::editor::test_context(cx);
        let editor = cx.new(Editor::new);
        editor.update(cx, |e, _| {
            e.extensions.insert("rs".into(), "rust".into());
        });
        editor
    }

    /// A ready `rust` server with `open` already opened.
    fn install_server(
        e: &mut Editor,
        open: &[String],
        caps: lsp::ServerCapabilities,
    ) -> Rc<RefCell<Sent>> {
        let sent = Rc::new(RefCell::new(Sent::default()));
        e.servers.insert(
            "rust".into(),
            LspServer {
                client: Box::new(FakeServer {
                    sent: sent.clone(),
                    next_id: 0,
                }),
                ready: true,
                caps,
                exited: false,
                open: open.iter().cloned().collect(),
                versions: HashMap::new(),
                pending_completion: None,
                pending: HashMap::new(),
                semantic_state: HashMap::new(),
                _task: Task::ready(()),
            },
        );
        sent
    }

    /// Make `doc` the active buffer, keeping the previous one open.
    fn open_active(e: &mut Editor, name: &str, path: Option<PathBuf>, text: &str) {
        let current = e.snapshot_active();
        e.inactive.push(current);
        e.install_active(make_document(name.into(), path, text));
    }

    /// A fresh, empty directory for one test's files (canonical, so URIs
    /// built from it match the buffers' own).
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("macros-lsp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn edit(sl: usize, sc: usize, el: usize, ec: usize, text: &str) -> TextEdit {
        TextEdit {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
            new_text: text.into(),
        }
    }

    fn status(e: &Editor) -> String {
        e.status.to_string()
    }

    fn active_target(e: &Editor) -> RequestTarget {
        RequestTarget {
            uri: document_uri(&e.active),
            tick: e.active.buffer.tick(),
        }
    }

    #[test]
    fn basename_and_builtin_server_names() {
        assert_eq!(basename("/a/b/c.rs"), "c.rs");
        assert_eq!(basename("c.rs"), "c.rs");
        assert_eq!(basename("/a/b/"), "");
        assert!(is_builtin_lsp("builtin"));
        assert!(is_builtin_lsp("builtin:macros"));
        assert!(!is_builtin_lsp("builtins"));
        assert!(!is_builtin_lsp("rust-analyzer"));
    }

    #[gpui::test]
    fn workspace_edit_spans_active_open_unopened_and_new_files(cx: &mut TestAppContext) {
        let dir = temp_dir("multi");
        let active = dir.join("active.rs");
        let open = dir.join("open.rs");
        let closed = dir.join("closed.rs");
        let new = dir.join("new.rs");
        std::fs::write(&active, "aaa bbb ccc\n").unwrap();
        std::fs::write(&open, "fn old() {}\n").unwrap();
        std::fs::write(&closed, "one\ntwo\n").unwrap();
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            open_active(e, "open.rs", Some(open.clone()), "fn old() {}\n");
            open_active(e, "active.rs", Some(active.clone()), "aaa bbb ccc\n");
            let edit = WorkspaceEdit {
                changes: vec![
                    (
                        lsp::path_to_uri(&active),
                        // Out of order, and lengths change: each position is
                        // in the original text, so they mustn't shift.
                        vec![edit(0, 8, 0, 11, "zzzzz"), edit(0, 0, 0, 3, "x")],
                    ),
                    (lsp::path_to_uri(&open), vec![edit(0, 3, 0, 6, "renamed")]),
                    (
                        lsp::path_to_uri(&closed),
                        vec![edit(1, 0, 1, 3, "TWO"), edit(0, 0, 0, 0, "zero\n")],
                    ),
                    (lsp::path_to_uri(&new), vec![edit(0, 0, 0, 0, "fresh\n")]),
                ],
            };
            let failed = e.apply_workspace_edit(edit, cx);
            assert!(failed.is_empty(), "{failed:?}");
            assert_eq!(e.active.buffer.full_text(), "x bbb zzzzz\n");
            // The active buffer is edited in place, not written.
            assert_eq!(std::fs::read_to_string(&active).unwrap(), "aaa bbb ccc\n");
            // An open, inactive buffer is edited and saved.
            let idx = e.inactive.iter().position(|d| d.name == "open.rs").unwrap();
            assert_eq!(e.inactive[idx].buffer.full_text(), "fn renamed() {}\n");
            assert!(!e.inactive[idx].modified);
            assert_eq!(std::fs::read_to_string(&open).unwrap(), "fn renamed() {}\n");
            // Files that aren't open are edited on disk; a missing one is
            // created.
            assert_eq!(
                std::fs::read_to_string(&closed).unwrap(),
                "zero\none\nTWO\n"
            );
            assert_eq!(std::fs::read_to_string(&new).unwrap(), "fresh\n");
            assert_eq!(status(e), "Applied 6 edit(s) across 4 file(s)");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn workspace_edit_positions_are_utf16(cx: &mut TestAppContext) {
        let dir = temp_dir("utf16");
        let path = dir.join("u.rs");
        let text = "let s = \"é😀x\";\nlet t = 1;\n";
        std::fs::write(&path, text).unwrap();
        let closed = dir.join("closed.rs");
        std::fs::write(&closed, "// 😀😀 end\n").unwrap();
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            open_active(e, "u.rs", Some(path.clone()), text);
            // `let s = "` is 9 units, é one, 😀 two: `x` is at 12.
            let edit = WorkspaceEdit {
                changes: vec![
                    (lsp::path_to_uri(&path), vec![edit(0, 12, 0, 13, "y")]),
                    // `// ` 3 + two 😀 at 2 units each: ` end` starts at 7.
                    (lsp::path_to_uri(&closed), vec![edit(0, 7, 0, 11, "!")]),
                ],
            };
            assert!(e.apply_workspace_edit(edit, cx).is_empty());
            assert_eq!(
                e.active.buffer.full_text(),
                "let s = \"é😀y\";\nlet t = 1;\n"
            );
            assert_eq!(std::fs::read_to_string(&closed).unwrap(), "// 😀😀!\n");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn workspace_edit_reports_files_it_cannot_read(cx: &mut TestAppContext) {
        let dir = temp_dir("fail");
        // A directory reads as an error other than "not found": it must be
        // reported, never overwritten as if empty.
        let not_a_file = dir.join("sub");
        std::fs::create_dir_all(&not_a_file).unwrap();
        let ok = dir.join("ok.rs");
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            let edit = WorkspaceEdit {
                changes: vec![
                    (lsp::path_to_uri(&not_a_file), vec![edit(0, 0, 0, 0, "x")]),
                    (lsp::path_to_uri(&ok), vec![edit(0, 0, 0, 0, "ok\n")]),
                ],
            };
            let failed = e.apply_workspace_edit(edit, cx);
            assert_eq!(failed.len(), 1, "{failed:?}");
            assert!(failed[0].contains("sub"), "{failed:?}");
            assert!(not_a_file.is_dir());
            assert_eq!(std::fs::read_to_string(&ok).unwrap(), "ok\n");
            let msg = status(e);
            assert!(
                msg.starts_with("Applied 2 edit(s) across 1 file(s); 1 file(s) failed: "),
                "{msg}"
            );

            e.apply_workspace_edit(WorkspaceEdit { changes: vec![] }, cx);
            assert_eq!(status(e), "Nothing to change");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn server_apply_edit_requests_are_answered(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            open_active(e, "x.rs", Some(PathBuf::from("/nonexistent/x.rs")), "a\n");
            let sent = install_server(e, &[], Default::default());
            let uri = document_uri(&e.active);
            e.on_lsp_message(
                "rust".into(),
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "workspace/applyEdit",
                    "params": { "edit": { "changes": { uri: [
                        { "range": { "start": { "line": 0, "character": 0 },
                                     "end": { "line": 0, "character": 1 } },
                          "newText": "b" }
                    ] } } }
                }),
                cx,
            );
            assert_eq!(e.active.buffer.full_text(), "b\n");
            assert_eq!(
                sent.borrow().responses,
                [(json!(7), json!({ "applied": true }))]
            );
            // Unknown server requests get the default reply, not an edit.
            e.on_lsp_message(
                "rust".into(),
                json!({ "jsonrpc": "2.0", "id": 8, "method": "workspace/configuration",
                        "params": { "items": [{}, {}] } }),
                cx,
            );
            assert_eq!(sent.borrow().responses.len(), 2);
            assert_eq!(sent.borrow().responses[1].0, json!(8));
        });
    }

    #[gpui::test]
    fn format_responses_apply_to_their_own_document(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            open_active(e, "a.rs", None, "fn  a(){}\n");
            let a = active_target(e);
            let format = PendingRequest::Format { save: None };
            let result = json!([{ "range": { "start": { "line": 0, "character": 2 },
                                             "end": { "line": 0, "character": 4 } },
                                  "newText": " " }]);
            e.handle_lsp_response("rust", format.clone(), a.clone(), &result, cx);
            assert_eq!(e.active.buffer.full_text(), "fn a(){}\n");
            assert_eq!(status(e), "Formatted buffer");

            // Nothing to change.
            let a = active_target(e);
            e.handle_lsp_response("rust", format.clone(), a, &json!([]), cx);
            assert_eq!(status(e), "No formatting changes");

            // Edited since the request: discarded.
            let stale = active_target(e);
            let point = e.point();
            e.replace_whole_buffer("fn  a(){} // edited\n", point, cx);
            e.handle_lsp_response("rust", format.clone(), stale, &result, cx);
            assert_eq!(e.active.buffer.full_text(), "fn  a(){} // edited\n");
            assert_eq!(
                status(e),
                "Buffer changed while formatting; edits discarded"
            );

            // A document no longer active is still formatted, in place.
            let a = active_target(e);
            open_active(e, "b.rs", None, "other\n");
            e.handle_lsp_response("rust", format.clone(), a, &result, cx);
            let idx = e.inactive.iter().position(|d| d.name == "a.rs").unwrap();
            assert_eq!(e.inactive[idx].buffer.full_text(), "fn a(){} // edited\n");
            assert_eq!(e.active.buffer.full_text(), "other\n");

            // A killed document: nothing happens.
            let gone = RequestTarget {
                uri: "untitled:gone.rs".into(),
                tick: 0,
            };
            e.handle_lsp_response("rust", format, gone, &result, cx);
            assert_eq!(e.active.buffer.full_text(), "other\n");
        });
    }

    #[gpui::test]
    fn format_on_save_responses_after_a_timeout_are_ignored(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            open_active(e, "a.rs", None, "x  = 1\n");
            let target = active_target(e);
            let result = json!([{ "range": { "start": { "line": 0, "character": 1 },
                                             "end": { "line": 0, "character": 3 } },
                                  "newText": " " }]);
            // No `formatting_for_save` entry: the save already went ahead.
            e.handle_lsp_response(
                "rust",
                PendingRequest::Format { save: Some(3) },
                target,
                &result,
                cx,
            );
            assert_eq!(e.active.buffer.full_text(), "x  = 1\n");
        });
    }

    #[gpui::test]
    fn apply_format_edits_replaces_the_active_buffer(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            open_active(e, "a.rs", None, "b\na\n");
            e.apply_format_edits(vec![edit(1, 0, 1, 1, "B"), edit(0, 0, 0, 1, "A")], cx);
            assert_eq!(e.active.buffer.full_text(), "A\nB\n");
            assert_eq!(status(e), "Formatted buffer");
            e.apply_format_edits(vec![], cx);
            assert_eq!(status(e), "No formatting changes");
        });
    }

    #[gpui::test]
    fn rename_responses_need_unchanged_text(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            let dir = temp_dir("rename");
            let path = dir.join("r.rs");
            open_active(e, "r.rs", Some(path.clone()), "let foo = foo;\n");
            let uri = lsp::path_to_uri(&path);
            let result = json!({ "changes": { uri.clone(): [
                { "range": { "start": { "line": 0, "character": 4 },
                             "end": { "line": 0, "character": 7 } }, "newText": "bar" },
                { "range": { "start": { "line": 0, "character": 10 },
                             "end": { "line": 0, "character": 13 } }, "newText": "bar" }
            ] } });
            let stale = active_target(e);
            let point = e.point();
            e.replace_whole_buffer("let foo = foo; \n", point, cx);
            e.handle_lsp_response("rust", PendingRequest::Rename, stale, &result, cx);
            assert_eq!(status(e), "Buffer changed during rename; not applied");
            assert_eq!(e.active.buffer.full_text(), "let foo = foo; \n");

            let fresh = active_target(e);
            e.handle_lsp_response("rust", PendingRequest::Rename, fresh, &result, cx);
            assert_eq!(e.active.buffer.full_text(), "let bar = bar; \n");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[gpui::test]
    fn simple_responses_echo_their_result(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            let t = active_target(e);
            for (kind, result, want) in [
                (
                    PendingRequest::Hover,
                    json!(null),
                    "No documentation at point",
                ),
                (
                    PendingRequest::SignatureHelp,
                    json!(null),
                    "No signature help",
                ),
                (
                    PendingRequest::SignatureHelp,
                    json!({ "signatures": [{ "label": "fn f(x: i32)" }] }),
                    "fn f(x: i32)",
                ),
                (
                    PendingRequest::CodeAction,
                    json!([]),
                    "No code actions available",
                ),
                (
                    PendingRequest::CallHierarchyCalls(CallDirection::Incoming),
                    json!([]),
                    "No calls found",
                ),
            ] {
                e.handle_lsp_response("rust", kind.clone(), t.clone(), &result, cx);
                assert!(status(e).contains(want), "{kind:?}: {}", status(e));
            }
            // Code actions computed for older text are dropped.
            let stale = RequestTarget {
                tick: t.tick + 1,
                ..t
            };
            e.handle_lsp_response(
                "rust",
                PendingRequest::CodeAction,
                stale,
                &json!([{ "title": "Fix" }]),
                cx,
            );
            assert_eq!(status(e), "Buffer changed; code action not applied");
        });
    }

    #[gpui::test]
    fn script_responses_reach_the_callback_only_in_their_buffer(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            e.run_script_blocking(
                "(define got \"none\") (define (lsp-test-cb json) (set! got json))",
                cx,
            );
            let other = RequestTarget {
                uri: "untitled:elsewhere".into(),
                tick: 0,
            };
            let kind = PendingRequest::Script("lsp-test-cb".into());
            e.handle_lsp_response("rust", kind.clone(), other, &json!({"a": 1}), cx);
            let t = active_target(e);
            e.handle_lsp_response("rust", kind, t, &json!({"b": 2}), cx);
        });
        cx.run_until_parked();
        let got = ed.read_with(cx, |e, _| e.script.eval("got").unwrap().value);
        assert_eq!(got.as_deref(), Some("\"{\\\"b\\\":2}\""));
    }

    #[gpui::test]
    fn code_actions_apply_edits_run_commands_or_resolve(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            let path = PathBuf::from("/nonexistent/c.rs");
            open_active(e, "c.rs", Some(path.clone()), "let x = 1;\n");
            let uri = lsp::path_to_uri(&path);
            let caps = lsp::ServerCapabilities {
                code_action_resolve: true,
                ..Default::default()
            };
            let sent = install_server(e, std::slice::from_ref(&uri), caps);

            // An edit-only action applies right away.
            e.apply_code_action(
                lsp::CodeAction {
                    title: "Rename".into(),
                    edit: Some(WorkspaceEdit {
                        changes: vec![(uri.clone(), vec![edit(0, 4, 0, 5, "y")])],
                    }),
                    command: None,
                    resolve: None,
                },
                cx,
            );
            assert_eq!(e.active.buffer.full_text(), "let y = 1;\n");

            // Neither edit nor command.
            e.apply_code_action(
                lsp::CodeAction {
                    title: "Nothing".into(),
                    edit: None,
                    command: None,
                    resolve: None,
                },
                cx,
            );
            assert_eq!(status(e), "Code action had no edit");

            // A command runs on the server.
            e.apply_code_action(
                lsp::CodeAction {
                    title: "Run".into(),
                    edit: None,
                    command: Some(("rust.run".into(), json!([1]))),
                    resolve: None,
                },
                cx,
            );
            assert_eq!(status(e), "Ran code action");
            assert_eq!(
                sent.borrow().requests.last().unwrap(),
                &(
                    "workspace/executeCommand".to_string(),
                    json!({ "command": "rust.run", "arguments": [1] })
                )
            );

            // An edit-less literal resolves first; the reply is applied once.
            let raw = json!({ "title": "Lazy", "data": 42 });
            e.apply_code_action(
                lsp::CodeAction {
                    title: "Lazy".into(),
                    edit: None,
                    command: None,
                    resolve: Some(raw.clone()),
                },
                cx,
            );
            let (method, params) = sent.borrow().requests.last().unwrap().clone();
            assert_eq!((method.as_str(), params), ("codeAction/resolve", raw));
            let id = sent.borrow().requests.len() as i64;
            e.on_lsp_message(
                "rust".into(),
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "title": "Lazy",
                    "edit": { "changes": { uri.clone(): [
                        { "range": { "start": { "line": 0, "character": 8 },
                                     "end": { "line": 0, "character": 9 } },
                          "newText": "2" } ] } }
                } }),
                cx,
            );
            assert_eq!(e.active.buffer.full_text(), "let y = 2;\n");
            // Not re-resolved.
            assert_eq!(sent.borrow().requests.len() as i64, id);
            assert!(e.servers["rust"].pending.is_empty());
        });
    }

    #[gpui::test]
    fn diagnostics_are_kept_per_uri_from_the_current_server(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            let path = PathBuf::from("/nonexistent/d.rs");
            open_active(e, "d.rs", Some(path.clone()), "x\n");
            let uri = lsp::path_to_uri(&path);
            let publish = |uri: &str, message: &str| {
                json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": [
                            { "range": { "start": { "line": 0, "character": 0 },
                                         "end": { "line": 0, "character": 1 } },
                              "severity": 2, "message": message } ] } })
            };
            e.on_lsp_message("rust".into(), publish(&uri, "from rust"), cx);
            assert_eq!(e.diagnostics[&uri][0].message, "from rust");
            // Another language's server can't overwrite the active buffer's…
            e.on_lsp_message("scheme".into(), publish(&uri, "stale"), cx);
            assert_eq!(e.diagnostics[&uri][0].message, "from rust");
            // …but may publish for another document.
            let other = lsp::path_to_uri(Path::new("/nonexistent/o.scm"));
            e.on_lsp_message("scheme".into(), publish(&other, "other"), cx);
            assert_eq!(e.diagnostics[&other][0].message, "other");
            // An empty publish clears.
            e.on_lsp_message(
                "rust".into(),
                json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": [] } }),
                cx,
            );
            assert!(e.diagnostics[&uri].is_empty());
        });
    }

    #[gpui::test]
    fn progress_shows_the_newest_running_task(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            let progress = |token: &str, value: Value| {
                json!({ "jsonrpc": "2.0", "method": "$/progress",
                        "params": { "token": token, "value": value } })
            };
            let slot = |e: &Editor| {
                e.mode_line_slots
                    .get("lsp-progress")
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            };
            e.on_lsp_message(
                "rust".into(),
                progress("a", json!({ "kind": "begin", "title": "Indexing" })),
                cx,
            );
            assert_eq!(slot(e), "rust: Indexing");
            e.on_lsp_message(
                "rust".into(),
                progress("b", json!({ "kind": "begin", "title": "Checking" })),
                cx,
            );
            assert_eq!(slot(e), "rust: Checking");
            e.on_lsp_message(
                "rust".into(),
                progress(
                    "a",
                    json!({ "kind": "report", "message": "3/10", "percentage": 30 }),
                ),
                cx,
            );
            // A report updates its own task; the newest is still shown.
            assert_eq!(slot(e), "rust: Checking");
            e.on_lsp_message("rust".into(), progress("b", json!({ "kind": "end" })), cx);
            assert_eq!(slot(e), "rust: Indexing 3/10 30%");
            // An unknown kind changes nothing.
            e.on_lsp_message("rust".into(), progress("a", json!({ "kind": "weird" })), cx);
            assert_eq!(slot(e), "rust: Indexing 3/10 30%");
            e.on_lsp_message("rust".into(), progress("a", json!({ "kind": "end" })), cx);
            assert_eq!(slot(e), "");
        });
    }

    #[gpui::test]
    fn server_stderr_goes_to_the_log(cx: &mut TestAppContext) {
        let ed = setup(cx);
        ed.update(cx, |e, cx| {
            e.on_lsp_message(
                "rust".into(),
                json!({ "jsonrpc": "2.0", "method": "$/stderr", "params": { "line": "boom" } }),
                cx,
            );
            assert_eq!(e.lsp_log.last().map(String::as_str), Some("[rust] boom"));
        });
    }
}
