use std::io::BufReader;
use std::ops::Range;
use std::path::Path;
use std::sync::mpsc::Sender;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jsonrpc::{self, lenient};
use crate::process::{EnvOverrides, ProcessGroup};
use crate::transport;

mod builtin;
pub use builtin::SteelLsp;

/// An LSP diagnostic severity. Declared worst-first so the derived `Ord`
/// makes "worst severity" an explicit `min()` (matching the wire encoding,
/// where 1 = error is the smallest value).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Info = 3,
    Hint = 4,
}

impl Severity {
    /// From the wire integer (1–4). Anything else — including a missing field —
    /// is treated as an error, matching the old `unwrap_or(1)` default.
    pub fn from_wire(n: u64) -> Self {
        match n {
            2 => Severity::Warning,
            3 => Severity::Info,
            4 => Severity::Hint,
            _ => Severity::Error,
        }
    }

    /// The lowercase name Scheme sees (`error`, `warning`, `info`, `hint`).
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        }
    }
}

/// A diagnostic from the language server, already mapped to buffer byte offsets.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub severity: Severity,
    pub message: String,
}

/// How `(lsp-request method kind params callback)` builds a request's base
/// params around the active buffer (its `kind` argument).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamsKind {
    /// `textDocument` + `position` (point) — also any unrecognized kind.
    Position,
    /// `textDocument` alone.
    Document,
    /// `textDocument` + the whole document as `range`.
    Range,
    /// `textDocument` + the region (or point's line) as `range`, plus
    /// formatting `options`.
    Region,
    /// `textDocument` + `positions: [point]`.
    Positions,
    /// No base: the caller's params verbatim.
    Raw,
}

impl ParamsKind {
    /// Parse the Scheme-side kind name.
    pub fn parse(kind: &str) -> Self {
        match kind {
            "document" => ParamsKind::Document,
            "range" => ParamsKind::Range,
            "region" => ParamsKind::Region,
            "positions" => ParamsKind::Positions,
            "raw" => ParamsKind::Raw,
            _ => ParamsKind::Position,
        }
    }
}

/// A minimal LSP client: one server process, JSON-RPC over stdio. A background
/// thread reads framed messages and forwards them as JSON `Value`s over a
/// channel; the editor drives requests/notifications from the main thread.
pub struct LspClient {
    /// The server's process group; killed and reaped on Drop (an `Option`
    /// only so Drop can move it to a reaper thread).
    child: Option<ProcessGroup>,
    /// Outgoing framed messages, drained by a writer thread so the UI never
    /// blocks on a full stdin pipe (e.g. a large `didOpen` to a busy server).
    outgoing: Sender<String>,
    next_id: i64,
    root_uri: String,
}

impl LspClient {
    /// Spawn `command` rooted at `root` with `env` applied, returning the
    /// client and a receiver of incoming messages. `Err(reason)` if the
    /// process can't start — the reason is human-readable and, for a missing
    /// executable, names the program and the `PATH` it was searched in (the
    /// usual GUI-launch failure mode).
    pub fn start(
        command: &str,
        root: &Path,
        env: &EnvOverrides,
    ) -> Result<(Self, UnboundedReceiver<Value>), String> {
        // The command may include arguments, e.g. "typescript-language-server --stdio".
        let mut parts = command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| "empty LSP command".to_string())?;
        // `Command::new` does no shell expansion, so expand a leading `~/` in the
        // program path ourselves — otherwise `~/.opam/…/ocaml_lsp` is looked up
        // as a literal path and fails with NotFound.
        let program = crate::util::expand_tilde(program);
        // Its own process group, so dropping the client takes down whatever a
        // wrapper (`npx`, `sh -c`, `opam exec`) spawned too, not just the
        // wrapper (mirrors `make-process` and DAP adapters).
        let transport::StdioTransport {
            group,
            stdin,
            stdout,
            stderr,
        } = transport::spawn_stdio(&program, parts, root, env).map_err(|e| e.to_string())?;

        // Reader thread: framed messages → channel the editor drains async.
        let (tx, rx) = unbounded();
        // Forward the server's stderr as synthetic `$/stderr` notifications on the
        // same channel, so the editor can surface it in `*lsp-log*` (otherwise a
        // crash/startup failure is invisible).
        let tx_err = tx.clone();
        std::thread::spawn(move || {
            transport::for_each_stderr_line(stderr, |line| {
                let msg = json!({ "method": "$/stderr", "params": { "line": line } });
                tx_err.unbounded_send(msg).is_ok()
            });
        });
        std::thread::spawn(move || read_loop(stdout, tx));

        let mut client = Self {
            child: Some(group),
            outgoing: transport::spawn_writer(stdin),
            next_id: 1,
            root_uri: path_to_uri(root),
        };
        client.send_initialize();
        Ok((client, rx))
    }

    fn send(&mut self, message: &Value) {
        // Non-blocking: hand the framed message to the writer thread. If the
        // channel is gone the server has died; the editor drops the client.
        let _ = self.outgoing.send(transport::frame(&message.to_string()));
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&jsonrpc::notification(method, params));
    }

    fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&jsonrpc::request(id, method, params));
        id
    }

    fn send_initialize(&mut self) {
        let root = self.root_uri.clone();
        self.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root,
                "capabilities": client_capabilities(),
            }),
        );
    }
}

impl LanguageServer for LspClient {
    /// Acknowledge the server's `initialize` response.
    fn initialized(&mut self) {
        self.notify("initialized", json!({}));
    }

    /// The spec's polite exit: servers flush caches and stop their own
    /// children (rust-analyzer's proc-macro server, tsserver, …) on `exit`,
    /// which a group SIGKILL can't do for anything that left the group.
    /// The response to `shutdown` isn't awaited — servers process requests in
    /// order, and the caller's grace period covers both.
    fn begin_shutdown(&mut self) -> Option<ProcessGroup> {
        let group = self.child.take()?;
        self.send_request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        Some(group)
    }

    fn did_open(&mut self, uri: &str, language_id: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                }
            }),
        );
    }

    /// Full-document sync (used when the server only supports `Full`).
    fn did_change(&mut self, uri: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }

    /// Incremental sync: one ranged change. `start`/`end` are pre-edit LSP
    /// positions; `text` is the replacement.
    fn did_change_incremental(
        &mut self,
        uri: &str,
        version: i32,
        start: Position,
        end: Position,
        text: &str,
    ) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ {
                    "range": {
                        "start": { "line": start.line, "character": start.character },
                        "end": { "line": end.line, "character": end.character },
                    },
                    "text": text,
                } ]
            }),
        );
    }

    /// Request completions at a position; returns the request id to match the
    /// response.
    fn completion(&mut self, uri: &str, line: usize, character: usize) -> i64 {
        self.send_request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )
    }

    /// Send an arbitrary request, returning its id (the generic path the editor
    /// uses for navigation/rename/symbols/etc.).
    fn request(&mut self, method: &str, params: Value) -> Option<i64> {
        Some(self.send_request(method, params))
    }

    /// Reply to a server→client request with `result`.
    fn respond(&mut self, id: Value, result: Value) {
        self.send(&jsonrpc::response(id, result));
    }

    /// Reply to a server→client request with a JSON-RPC error.
    fn respond_error(&mut self, id: Value, code: i64, message: &str) {
        self.send(&jsonrpc::error_response(id, code, message));
    }

    fn did_close(&mut self, uri: &str) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
    }

    /// `textDocument/didSave`; `text` is included only when the server asked.
    fn did_save(&mut self, uri: &str, text: Option<&str>) {
        let mut params = json!({ "textDocument": { "uri": uri } });
        if let Some(text) = text {
            params["text"] = json!(text);
        }
        self.notify("textDocument/didSave", params);
    }

    /// `$/cancelRequest` for an in-flight request we no longer want.
    fn cancel(&mut self, id: i64) {
        self.notify("$/cancelRequest", json!({ "id": id }));
    }
}

/// The `ClientCapabilities` we advertise in `initialize`. Everything here must
/// be backed by real handling: `didSave` is sent after each successful save,
/// `workspace/applyEdit` / `workspace/configuration` requests are answered, and
/// code actions arrive as literals (resolved lazily via `codeAction/resolve`
/// when the server leaves out the edit).
pub fn client_capabilities() -> Value {
    json!({
        "workspace": {
            "applyEdit": true,
            "configuration": true,
            "symbol": {},
            "workspaceEdit": { "documentChanges": true },
            "executeCommand": {}
        },
        "textDocument": {
            "synchronization": { "didSave": true, "willSave": false, "willSaveWaitUntil": false },
            "publishDiagnostics": {},
            "completion": {
                "completionItem": {
                    "snippetSupport": false,
                    "documentationFormat": ["plaintext", "markdown"],
                    "resolveSupport": {
                        "properties": ["documentation", "detail", "additionalTextEdits"]
                    }
                }
            },
            "hover": { "contentFormat": ["plaintext", "markdown"] },
            "definition": { "linkSupport": true },
            "declaration": { "linkSupport": true },
            "typeDefinition": { "linkSupport": true },
            "implementation": { "linkSupport": true },
            "references": {},
            "documentHighlight": {},
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "formatting": {},
            "rangeFormatting": {},
            "rename": { "prepareSupport": false },
            "signatureHelp": {},
            "codeAction": {
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "", "quickfix", "refactor", "refactor.extract",
                            "refactor.inline", "refactor.rewrite", "source",
                            "source.organizeImports", "source.fixAll"
                        ]
                    }
                },
                "isPreferredSupport": true,
                "dataSupport": true,
                "resolveSupport": { "properties": ["edit"] }
            },
            "semanticTokens": {
                "requests": { "full": true },
                "tokenTypes": [
                    "namespace", "type", "class", "enum", "interface",
                    "struct", "typeParameter", "parameter", "variable",
                    "property", "enumMember", "event", "function",
                    "method", "macro", "keyword", "modifier", "comment",
                    "string", "number", "regexp", "operator", "decorator"
                ],
                "tokenModifiers": [],
                "formats": ["relative"]
            }
        }
    })
}

/// JSON-RPC `MethodNotFound`, for server→client requests we don't implement.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// The reply to a server→client request that needs no editor state:
/// `Ok(result)` to answer, or `Err((code, message))` for an error response.
/// `workspace/applyEdit` is NOT handled here — it edits buffers, so the editor
/// answers it itself.
pub fn default_server_reply(method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        // One `null` per requested item = "no client-side setting", which every
        // server treats as "use your defaults".
        "workspace/configuration" => {
            let n = params["items"].as_array().map_or(0, Vec::len);
            Ok(Value::Array(vec![Value::Null; n]))
        }
        // Void-result (or "no answer") requests: `null` is the correct reply.
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "window/showMessageRequest"
        | "workspace/workspaceFolders"
        | "workspace/semanticTokens/refresh"
        | "workspace/codeLens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/inlineValue/refresh"
        | "workspace/diagnostic/refresh" => Ok(Value::Null),
        _ => Err((METHOD_NOT_FOUND, format!("unhandled method: {method}"))),
    }
}

/// The mode-line text for a `$/progress` begin/report `value` of a task
/// titled `title` (reports don't repeat the title), e.g.
/// `rust: Indexing 3/10 30%`.
pub fn progress_text(language: &str, title: &str, value: &Value) -> String {
    let mut text = format!("{language}: {title}");
    if let Some(message) = value["message"].as_str().filter(|m| !m.is_empty()) {
        text.push(' ');
        text.push_str(&crate::util::single_line(message));
    }
    if let Some(pct) = value["percentage"].as_u64() {
        text.push_str(&format!(" {pct}%"));
    }
    text
}

/// A diagnostic in LSP coordinates (line, UTF-16 column), before mapping to a
/// specific buffer's byte offsets.
#[derive(Clone, Debug)]
pub struct RawDiagnostic {
    pub start: Position,
    pub end: Position,
    pub severity: Severity,
    pub message: String,
    /// The diagnostic exactly as the server published it (`code`, `source`,
    /// `data`, … included), echoed back verbatim in a `codeAction` request's
    /// context so the server can match its quick fixes to it.
    pub raw: Value,
}

/// How a server wants document changes synced, from its `initialize` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SyncKind {
    /// Sync kind 0: the server wants no `didChange` notifications at all.
    None,
    /// Sync kind 1: resend the whole document on change.
    #[default]
    Full,
    /// Sync kind 2: send only the changed range.
    Incremental,
}

/// `textDocumentSync`: a bare `TextDocumentSyncKind` number or an options
/// object.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireTextDocumentSync {
    /// Kept as a raw number: any number (even an out-of-range one) means
    /// "the pre-`save`-option form".
    Kind(serde_json::Number),
    Options {
        #[serde(default, deserialize_with = "lenient")]
        change: Option<u64>,
        #[serde(default, deserialize_with = "lenient")]
        save: Option<WireSaveOptions>,
    },
}

/// `textDocumentSync.save`: a bool or `{ includeText }`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireSaveOptions {
    Enabled(bool),
    #[serde(rename_all = "camelCase")]
    Options {
        #[serde(default, deserialize_with = "lenient")]
        include_text: Option<bool>,
    },
}

/// `renameProvider`: a bool or `{ prepareProvider }`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireRenameProvider {
    Enabled(#[allow(dead_code)] bool),
    #[serde(rename_all = "camelCase")]
    Options {
        #[serde(default, deserialize_with = "lenient")]
        prepare_provider: Option<bool>,
    },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireCompletionProvider {
    #[serde(default, deserialize_with = "lenient")]
    trigger_characters: Option<Vec<Value>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSemanticTokensProvider {
    #[serde(default, deserialize_with = "lenient")]
    legend: Option<WireSemanticLegend>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSemanticLegend {
    #[serde(default, deserialize_with = "lenient")]
    token_types: Option<Vec<Value>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireCodeActionProvider {
    #[serde(default, deserialize_with = "lenient")]
    resolve_provider: Option<bool>,
}

/// The `capabilities` of an `initialize` result — just the ones we consume.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireCapabilities {
    #[serde(default, deserialize_with = "lenient")]
    text_document_sync: Option<WireTextDocumentSync>,
    #[serde(default, deserialize_with = "lenient")]
    rename_provider: Option<WireRenameProvider>,
    #[serde(default, deserialize_with = "lenient")]
    completion_provider: Option<WireCompletionProvider>,
    #[serde(default, deserialize_with = "lenient")]
    semantic_tokens_provider: Option<WireSemanticTokensProvider>,
    #[serde(default, deserialize_with = "lenient")]
    code_action_provider: Option<WireCodeActionProvider>,
}

/// What a server advertised in its `initialize` result, parsed once when it
/// arrives. The `Default` (also what an empty `capabilities` parses to) is
/// what we assume until then.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ServerCapabilities {
    /// How it wants document changes synced. A missing kind defaults to full
    /// (the safe choice); an out-of-range one means no sync.
    pub sync_kind: SyncKind,
    /// Whether (and how) it wants `textDocument/didSave`: `None` = don't
    /// send, `Some(include_text)` = send. A bare-number `textDocumentSync`
    /// predates the `save` option and gets text-less notifications (matching
    /// vscode-languageclient's resolution).
    pub save: Option<bool>,
    /// `semanticTokensProvider.legend.tokenTypes`: token-type names by id.
    pub semantic_legend: Vec<String>,
    /// `renameProvider.prepareProvider`: gates the `prepareRename` pre-check.
    pub prepare_rename: bool,
    /// `completionProvider.triggerCharacters` — characters that should
    /// auto-open completion even though they aren't identifier characters
    /// (e.g. `.` for member access in OCaml/Rust).
    pub completion_triggers: Vec<String>,
    /// `codeActionProvider.resolveProvider`: edit-less code actions are
    /// completed via `codeAction/resolve` before applying.
    pub code_action_resolve: bool,
}

impl ServerCapabilities {
    /// Parse the capabilities out of an `initialize` result.
    pub fn from_initialize(result: &Value) -> Self {
        WireCapabilities::deserialize(&result["capabilities"])
            .unwrap_or_default()
            .into()
    }
}

impl From<WireCapabilities> for ServerCapabilities {
    fn from(wire: WireCapabilities) -> Self {
        let (change, save) = match wire.text_document_sync {
            Some(WireTextDocumentSync::Kind(n)) => (n.as_u64(), Some(false)),
            Some(WireTextDocumentSync::Options { change, save }) => (
                change,
                match save {
                    Some(WireSaveOptions::Enabled(true)) => Some(false),
                    Some(WireSaveOptions::Options { include_text }) => {
                        Some(include_text == Some(true))
                    }
                    Some(WireSaveOptions::Enabled(false)) | None => None,
                },
            ),
            None => (None, None),
        };
        let sync_kind = match change.unwrap_or(1) {
            1 => SyncKind::Full,
            2 => SyncKind::Incremental,
            _ => SyncKind::None,
        };
        let strings = |values: Option<Vec<Value>>| values.unwrap_or_default().into_iter();
        ServerCapabilities {
            sync_kind,
            save,
            semantic_legend: strings(
                wire.semantic_tokens_provider
                    .and_then(|p| p.legend)
                    .and_then(|l| l.token_types),
            )
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect(),
            prepare_rename: matches!(
                wire.rename_provider,
                Some(WireRenameProvider::Options {
                    prepare_provider: Some(true)
                })
            ),
            completion_triggers: strings(
                wire.completion_provider.and_then(|p| p.trigger_characters),
            )
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
            code_action_resolve: wire.code_action_provider.and_then(|p| p.resolve_provider)
                == Some(true),
        }
    }
}

/// An LSP `Position`: a line and a UTF-16 column. On the wire, missing
/// fields default to 0 (the same lossy behavior as the manual
/// `as_u64().unwrap_or(0)` digs this replaced).
#[derive(serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    #[serde(default)]
    pub line: usize,
    #[serde(default)]
    pub character: usize,
}

impl Position {
    pub const fn new(line: usize, character: usize) -> Self {
        Position { line, character }
    }
}

/// From a [`Buffer::byte_to_position`](crate::buffer::Buffer::byte_to_position)
/// `(line, utf16_col)` pair.
impl From<(usize, usize)> for Position {
    fn from((line, character): (usize, usize)) -> Self {
        Position { line, character }
    }
}

/// An LSP `Range` on the wire. `start`/`end` default so a missing or malformed
/// endpoint becomes `(0, 0)` — matching the old per-field `unwrap_or(0)` digs.
#[derive(serde::Deserialize, Default)]
struct WireRange {
    #[serde(default)]
    start: Position,
    #[serde(default)]
    end: Position,
}

impl From<WireRange> for (Position, Position) {
    fn from(r: WireRange) -> (Position, Position) {
        (r.start, r.end)
    }
}

/// A `Diagnostic` on the wire — just the fields we consume. A missing/malformed
/// field takes its default, matching the old per-field `unwrap_or` digs.
#[derive(serde::Deserialize, Default)]
struct WireDiagnostic {
    #[serde(default)]
    range: WireRange,
    severity: Option<u64>,
    #[serde(default)]
    message: String,
}

/// Parse a `textDocument/publishDiagnostics` params object into `(uri, diags)`.
pub fn parse_diagnostics(params: &Value) -> (String, Vec<RawDiagnostic>) {
    let uri = normalize_uri(params["uri"].as_str().unwrap_or_default());
    let diagnostics = params["diagnostics"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|d| {
                    let wire: WireDiagnostic = WireDiagnostic::deserialize(d).unwrap_or_default();
                    let (start, end) = wire.range.into();
                    RawDiagnostic {
                        start,
                        end,
                        severity: wire.severity.map_or(Severity::Error, Severity::from_wire),
                        message: wire.message,
                        raw: d.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    (uri, diagnostics)
}

/// A completion candidate with the extra metadata servers provide.
#[derive(Clone, Debug)]
pub struct CompletionItem {
    /// What the user sees / filters on.
    pub label: String,
    /// What's inserted on accept (often the same as `label`).
    pub insert_text: String,
    /// Signature / parameters / type, e.g. `(x: i32) -> bool` (from
    /// `labelDetails` or `detail`).
    pub detail: Option<String>,
    /// A short kind tag, e.g. `fn`, `var`, `kw`, `type`.
    pub kind: Option<String>,
    /// Documentation (plain text), filled in by `completionItem/resolve`.
    pub documentation: Option<String>,
    /// Edits applied alongside the insertion on accept — e.g. an auto-added
    /// `use`/import. May arrive in the initial response or via resolve.
    pub additional_edits: Vec<TextEdit>,
    /// The raw item JSON, echoed back verbatim to `completionItem/resolve`
    /// (kept as a `Value` so integer fields stay integers).
    pub data: Value,
    /// Whether this item has been through `completionItem/resolve`.
    pub resolved: bool,
}

/// Extract completion items from a `textDocument/completion` result.
pub fn parse_completion(result: &Value) -> Vec<CompletionItem> {
    // Result is either `CompletionItem[]` or `{ items: CompletionItem[] }`.
    result
        .as_array()
        .or_else(|| result["items"].as_array())
        .map(|arr| arr.iter().filter_map(parse_completion_item).collect())
        .unwrap_or_default()
}

/// A `CompletionItem` on the wire — the fields we consume, typed. Two fields
/// stay raw `Value`s because they're genuinely polymorphic: `documentation`
/// (string-or-`MarkupContent`) and `additionalTextEdits` (parsed by the shared
/// [`parse_text_edits`]). The original JSON is kept separately as
/// [`CompletionItem::data`] to echo back to `completionItem/resolve`.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireCompletionItem {
    label: Option<String>,
    insert_text: Option<String>,
    /// `TextEdit` or `InsertReplaceEdit` — both carry `newText`.
    text_edit: Option<WireNewText>,
    kind: Option<u64>,
    #[serde(default)]
    documentation: Value,
    #[serde(default)]
    additional_text_edits: Value,
    #[serde(default)]
    label_details: WireLabelDetails,
    detail: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireNewText {
    new_text: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct WireLabelDetails {
    detail: Option<String>,
    description: Option<String>,
}

impl WireCompletionItem {
    /// The signature/type annotation, preferring `labelDetails` (params +
    /// return or module) and falling back to `detail`.
    fn signature(&self) -> Option<String> {
        let detail = self.label_details.detail.as_deref().unwrap_or("");
        let description = self.label_details.description.as_deref().unwrap_or("");
        let combined = format!("{detail} {description}");
        let combined = combined.trim();
        if !combined.is_empty() {
            return Some(combined.to_string());
        }
        self.detail.clone().filter(|s| !s.is_empty())
    }
}

fn parse_completion_item(item: &Value) -> Option<CompletionItem> {
    // Completion labels and detail annotations render as a single popup line
    // (`shape_line`, which panics on an embedded newline) — and servers like
    // ocamllsp put multi-line type signatures in `detail`.
    use crate::util::single_line;
    let wire: WireCompletionItem = WireCompletionItem::deserialize(item).ok()?;
    let detail = wire.signature().map(|d| single_line(&d));
    let raw_label = wire.label?;
    // `insert_text` goes into the buffer, so fall back to the *raw* label; the
    // display `label` is collapsed to a single line for the popup.
    // Per the spec, `textEdit` wins over `insertText` when both are present.
    let insert_text = wire
        .text_edit
        .and_then(|e| e.new_text)
        .or(wire.insert_text)
        .unwrap_or_else(|| raw_label.clone());
    let label = single_line(&raw_label);
    let kind = wire.kind.and_then(completion_kind).map(str::to_string);
    Some(CompletionItem {
        label,
        insert_text,
        detail,
        kind,
        documentation: parse_documentation(&wire.documentation),
        additional_edits: parse_text_edits(&wire.additional_text_edits),
        data: item.clone(),
        resolved: false,
    })
}

/// Merge a `completionItem/resolve` result into `item` — servers fill in
/// `documentation` and `additionalTextEdits` (auto-imports) lazily.
pub fn merge_resolved_completion(item: &mut CompletionItem, resolved: &Value) {
    let wire: WireCompletionItem = WireCompletionItem::deserialize(resolved).unwrap_or_default();
    if let Some(doc) = parse_documentation(&wire.documentation) {
        item.documentation = Some(doc);
    }
    let edits = parse_text_edits(&wire.additional_text_edits);
    if !edits.is_empty() {
        item.additional_edits = edits;
    }
    if let Some(detail) = wire.signature() {
        // Same single-line rule as `parse_completion_item`: the popup can't
        // render an embedded newline.
        item.detail = Some(crate::util::single_line(&detail));
    }
    item.resolved = true;
}

/// Plain text of a `documentation` field (a string or a `MarkupContent`).
pub fn parse_documentation(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Object(_) => value["value"].as_str().unwrap_or("").to_string(),
        _ => String::new(),
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The result of `textDocument/prepareRename`: whether the position is
/// renameable, and the server's placeholder/range if given.
#[derive(Clone, Debug, PartialEq)]
pub struct PrepareRename {
    pub placeholder: Option<String>,
    pub range: Option<(Position, Position)>,
}

/// A `prepareRename` result on the wire (`null` fails every variant).
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WirePrepareRename {
    /// `{ defaultBehavior }` — renameable, the client picks the range.
    #[serde(rename_all = "camelCase")]
    Default { default_behavior: bool },
    /// `{ range?, placeholder }`.
    Placeholder {
        placeholder: String,
        range: Option<WireRange>,
    },
    /// A bare `Range`.
    Range {
        start: Position,
        #[serde(default)]
        end: Position,
    },
}

/// Parse a `prepareRename` result: `Range` | `{range, placeholder}` |
/// `{defaultBehavior:true}` | `null`. `None` ⇒ not renameable here.
pub fn parse_prepare_rename(result: &Value) -> Option<PrepareRename> {
    match WirePrepareRename::deserialize(result).ok()? {
        WirePrepareRename::Default { default_behavior } => {
            default_behavior.then_some(PrepareRename {
                placeholder: None,
                range: None,
            })
        }
        WirePrepareRename::Placeholder { placeholder, range } => Some(PrepareRename {
            placeholder: Some(placeholder),
            range: range.map(Into::into),
        }),
        WirePrepareRename::Range { start, end } => Some(PrepareRename {
            placeholder: None,
            range: Some((start, end)),
        }),
    }
}

/// A code lens: where it sits, its title (if the command is already present),
/// and the raw lens (echoed to `codeLens/resolve` when the command is missing).
#[derive(Clone, Debug)]
pub struct CodeLens {
    pub line: usize,
    pub character: usize,
    pub title: Option<String>,
    pub data: Value,
}

/// A `CodeLens` on the wire — position plus the (possibly absent) command title.
#[derive(serde::Deserialize, Default)]
struct WireCodeLens {
    #[serde(default)]
    range: WireRange,
    command: Option<WireCommandTitle>,
}

#[derive(serde::Deserialize, Default)]
struct WireCommandTitle {
    title: Option<String>,
}

/// Parse a `textDocument/codeLens` result.
pub fn parse_code_lenses(result: &Value) -> Vec<CodeLens> {
    result
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|lens| {
                    let wire: WireCodeLens = WireCodeLens::deserialize(lens).unwrap_or_default();
                    CodeLens {
                        line: wire.range.start.line,
                        character: wire.range.start.character,
                        title: wire.command.and_then(|c| c.title),
                        data: lens.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The title of a resolved code lens (`codeLens/resolve` fills in `command`).
pub fn parse_code_lens_title(resolved: &Value) -> Option<String> {
    WireCodeLens::deserialize(resolved).ok()?.command?.title
}

/// Map an LSP `CompletionItemKind` to a short tag.
fn completion_kind(kind: u64) -> Option<&'static str> {
    Some(match kind {
        2 | 3 => "fn",           // Method / Function
        4 => "ctor",             // Constructor
        5 | 10 => "field",       // Field / Property
        6 => "var",              // Variable
        7 | 8 | 22 => "type",    // Class / Interface / Struct
        9 => "mod",              // Module
        14 => "kw",              // Keyword
        21 => "const",           // Constant
        12 | 13 | 20 => "value", // Value / Enum / EnumMember
        _ => return None,
    })
}

/// A source location: a document URI and a `(line, utf16-col)` range. Used for
/// go-to-definition and friends, references, and symbols.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub uri: String,
    pub start: Position,
    pub end: Position,
}

/// One definition/references/etc. target on the wire.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireAnyLocation {
    /// `LocationLink` (link-support responses): `targetUri` plus a
    /// `targetSelectionRange` (the identifier) / `targetRange` (the whole def).
    #[serde(rename_all = "camelCase")]
    Link {
        target_uri: String,
        target_selection_range: Option<WireRange>,
        #[serde(default)]
        target_range: WireRange,
    },
    /// Plain `Location`.
    Plain {
        uri: String,
        #[serde(default)]
        range: WireRange,
    },
}

impl From<WireAnyLocation> for Location {
    fn from(wire: WireAnyLocation) -> Location {
        let (uri, range) = match wire {
            WireAnyLocation::Link {
                target_uri,
                target_selection_range,
                target_range,
            } => (target_uri, target_selection_range.unwrap_or(target_range)),
            WireAnyLocation::Plain { uri, range } => (uri, range),
        };
        let (start, end) = range.into();
        Location { uri, start, end }
    }
}

/// Parse a definition/references/etc. result, which may be `null`, a single
/// `Location`, an array of `Location`, or an array of `LocationLink`. A
/// malformed array entry is skipped.
pub fn parse_locations(result: &Value) -> Vec<Location> {
    let one = |v: &Value| WireAnyLocation::deserialize(v).ok().map(Location::from);
    match result {
        Value::Array(arr) => arr.iter().filter_map(one).collect(),
        v => one(v).into_iter().collect(),
    }
}

/// Hover `contents` on the wire: a string, a `MarkupContent` /
/// `MarkedString` object, or an array of those. Anything else renders empty.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireHoverContents {
    Plain(String),
    Many(Vec<WireHoverContents>),
    /// `MarkupContent { kind, value }` or `MarkedString { language, value }`.
    Markup {
        #[serde(default)]
        value: String,
    },
    Other(serde::de::IgnoredAny),
}

impl WireHoverContents {
    fn render(self) -> String {
        match self {
            WireHoverContents::Plain(s) | WireHoverContents::Markup { value: s } => s,
            WireHoverContents::Many(items) => items
                .into_iter()
                .map(WireHoverContents::render)
                .collect::<Vec<_>>()
                .join("\n"),
            WireHoverContents::Other(_) => String::new(),
        }
    }
}

/// Plain-text rendering of a `Hover` result (`contents` may be a string, a
/// `MarkupContent`, a `MarkedString`, or an array of those). `None` when empty.
pub fn parse_hover(result: &Value) -> Option<String> {
    let text = WireHoverContents::deserialize(&result["contents"])
        .map_or_else(|_| String::new(), WireHoverContents::render);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A document/workspace symbol, flattened from the (possibly nested) response.
#[derive(Clone, Debug, PartialEq)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: u64,
    pub uri: String,
    pub start: Position,
    pub container: String,
    /// Nesting depth (0 = top level) — for indentation in the list.
    pub depth: usize,
}

/// A `DocumentSymbol | SymbolInformation` on the wire: a flat
/// `SymbolInformation` carries its own `location`, a hierarchical
/// `DocumentSymbol` only ranges + `children`.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireSymbol {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: u64,
    location: Option<WireLocation>,
    container_name: Option<String>,
    #[serde(default)]
    range: WireRange,
    selection_range: Option<WireRange>,
    #[serde(default)]
    children: Vec<WireSymbol>,
}

#[derive(serde::Deserialize, Default)]
struct WireLocation {
    uri: Option<String>,
    #[serde(default)]
    range: WireRange,
}

/// Parse a `documentSymbol` (nested `DocumentSymbol[]` or flat
/// `SymbolInformation[]`) or `workspace/symbol` result. `fallback_uri` is used
/// for `DocumentSymbol`s, which carry only ranges, not URIs.
pub fn parse_symbols(result: &Value, fallback_uri: &str) -> Vec<SymbolInfo> {
    fn push(
        item: WireSymbol,
        fallback_uri: &str,
        container: &str,
        depth: usize,
        out: &mut Vec<SymbolInfo>,
    ) {
        if let Some(loc) = item.location {
            // `SymbolInformation`: a flat item carrying its own location.
            out.push(SymbolInfo {
                name: item.name,
                kind: item.kind,
                uri: loc.uri.unwrap_or_else(|| fallback_uri.to_string()),
                start: loc.range.start,
                container: item.container_name.unwrap_or_else(|| container.to_string()),
                depth,
            });
        } else {
            // `DocumentSymbol`: hierarchical, ranges only. Prefer the narrower
            // `selectionRange` (the identifier) over the whole definition.
            let range = item.selection_range.unwrap_or(item.range);
            out.push(SymbolInfo {
                name: item.name.clone(),
                kind: item.kind,
                uri: fallback_uri.to_string(),
                start: range.start,
                container: container.to_string(),
                depth,
            });
            for child in item.children {
                push(child, fallback_uri, &item.name, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(arr) = result.as_array() {
        for item in arr {
            if let Ok(sym) = WireSymbol::deserialize(item) {
                push(sym, fallback_uri, "", 0, &mut out);
            }
        }
    }
    out
}

/// A short name for an LSP `SymbolKind`, for display in the symbol list.
pub fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "ctor",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        15 => "string",
        16 => "number",
        17 => "bool",
        18 => "array",
        23 => "struct",
        26 => "type",
        _ => "sym",
    }
}

/// A single text edit in LSP `(line, utf16-col)` coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct TextEdit {
    pub start: Position,
    pub end: Position,
    pub new_text: String,
}

/// A `TextEdit` on the wire. `range` is required (an edit without one is
/// meaningless and is skipped, as before); a missing `newText` deletes.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTextEdit {
    range: WireRange,
    #[serde(default)]
    new_text: String,
}

fn parse_one_edit(v: &Value) -> Option<TextEdit> {
    let edit: WireTextEdit = WireTextEdit::deserialize(v).ok()?;
    let (start, end) = edit.range.into();
    Some(TextEdit {
        start,
        end,
        new_text: edit.new_text,
    })
}

/// Parse a `TextEdit[]` (e.g. a `formatting` response).
pub fn parse_text_edits(result: &Value) -> Vec<TextEdit> {
    result
        .as_array()
        .map(|arr| arr.iter().filter_map(parse_one_edit).collect())
        .unwrap_or_default()
}

/// A workspace edit: per-document text edits. File create/rename/delete
/// operations in `documentChanges` are ignored (we only apply text edits).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspaceEdit {
    pub changes: Vec<(String, Vec<TextEdit>)>,
}

/// Parse a `WorkspaceEdit` from either the newer `documentChanges` array or the
/// older `changes` map.
pub fn parse_workspace_edit(result: &Value) -> WorkspaceEdit {
    let mut changes = Vec::new();
    if let Some(arr) = result["documentChanges"].as_array() {
        for dc in arr {
            // Only `TextDocumentEdit`s have an `edits` array; skip file ops.
            if let Some(uri) = dc["textDocument"]["uri"].as_str() {
                let edits = parse_text_edits(&dc["edits"]);
                if !edits.is_empty() {
                    changes.push((uri.to_string(), edits));
                }
            }
        }
    } else if let Some(map) = result["changes"].as_object() {
        for (uri, edits) in map {
            let edits = parse_text_edits(edits);
            if !edits.is_empty() {
                changes.push((uri.clone(), edits));
            }
        }
    }
    WorkspaceEdit { changes }
}

/// A code action: a title plus an optional inline `WorkspaceEdit` and/or a
/// server `command` to run via `workspace/executeCommand`.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeAction {
    pub title: String,
    pub edit: Option<WorkspaceEdit>,
    /// `(command, arguments)` for `workspace/executeCommand`.
    pub command: Option<(String, Value)>,
    /// The raw `CodeAction` literal when it arrived without an `edit`: the
    /// server may fill the edit in lazily, so applying it first round-trips
    /// this through `codeAction/resolve`. `None` for bare `Command`s and for
    /// actions that already carry their edit.
    pub resolve: Option<Value>,
}

/// A `Command | CodeAction` on the wire. `edit` (a `WorkspaceEdit`, itself two
/// shapes — see [`parse_workspace_edit`]) and `command` (a bare string on a
/// `Command`, a nested object on a `CodeAction`) are genuinely polymorphic, so
/// they stay raw `Value`s.
#[derive(serde::Deserialize, Default)]
struct WireCodeAction {
    title: Option<String>,
    #[serde(default)]
    edit: Value,
    #[serde(default)]
    command: Value,
    #[serde(default)]
    arguments: Value,
}

/// Parse a `(Command | CodeAction)[]` result.
pub fn parse_code_actions(result: &Value) -> Vec<CodeAction> {
    let Some(arr) = result.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let wire: WireCodeAction = WireCodeAction::deserialize(item).ok()?;
            let title = wire.title?;
            // A bare `Command` has a string `command`; a `CodeAction` may have an
            // `edit` and/or a nested `command` object.
            let (edit, command, resolve) = if let Value::String(cmd) = &wire.command {
                (None, Some((cmd.clone(), wire.arguments)), None)
            } else {
                let edit = (!wire.edit.is_null()).then(|| parse_workspace_edit(&wire.edit));
                let command = wire.command["command"]
                    .as_str()
                    .map(|c| (c.to_string(), wire.command["arguments"].clone()));
                let resolve = edit.is_none().then(|| item.clone());
                (edit, command, resolve)
            };
            Some(CodeAction {
                title,
                edit,
                command,
                resolve,
            })
        })
        .collect()
}

/// One decoded semantic token: `(line, start_char, length, type_name)` in LSP
/// coordinates (UTF-16 column).
pub type SemanticToken = (usize, usize, usize, String);

/// Decode a `semanticTokens/full` result. The `data` array is groups of five
/// integers — `[deltaLine, deltaStartChar, length, tokenType, tokenModifiers]`
/// — delta-encoded against the previous token (same line ⇒ delta column adds,
/// new line ⇒ column resets). `legend` maps `tokenType` ids to names.
/// Decode an already-extracted packed `data` array (used after a delta merge).
pub fn decode_semantic_data(data: &[i64], legend: &[String]) -> Vec<SemanticToken> {
    let mut tokens = Vec::new();
    let (mut line, mut col) = (0usize, 0usize);
    for group in data.as_chunks::<5>().0 {
        let delta_line = group[0].max(0) as usize;
        let delta_start = group[1].max(0) as usize;
        let length = group[2].max(0) as usize;
        let token_type = group[3].max(0) as usize;
        if delta_line > 0 {
            line += delta_line;
            col = delta_start;
        } else {
            col += delta_start;
        }
        let name = legend.get(token_type).cloned().unwrap_or_default();
        tokens.push((line, col, length, name));
    }
    tokens
}

/// The packed `data` integer array from a `semanticTokens` result.
pub fn semantic_data(result: &Value) -> Vec<i64> {
    result["data"]
        .as_array()
        .map(|a| a.iter().map(|v| v.as_i64().unwrap_or(0)).collect())
        .unwrap_or_default()
}

/// A `semanticTokens` result's `resultId` (for follow-up delta requests).
pub fn semantic_result_id(result: &Value) -> Option<String> {
    result["resultId"].as_str().map(str::to_string)
}

/// Apply a `semanticTokens/full/delta` result to the previous packed `data`.
/// A delta result has `edits: [{start, deleteCount, data}]` splicing the array;
/// a full result (no `edits`) just carries fresh `data`.
pub fn apply_semantic_delta(prev: &[i64], result: &Value) -> Vec<i64> {
    let Some(edits) = result["edits"].as_array() else {
        return semantic_data(result); // server sent a full set instead
    };
    let mut data = prev.to_vec();
    // Apply edits high-offset-first so earlier splices don't shift later ones.
    let mut edits: Vec<&Value> = edits.iter().collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e["start"].as_u64().unwrap_or(0)));
    for edit in edits {
        let start = edit["start"].as_u64().unwrap_or(0) as usize;
        let delete = edit["deleteCount"].as_u64().unwrap_or(0) as usize;
        let new: Vec<i64> = edit["data"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_i64().unwrap_or(0)).collect())
            .unwrap_or_default();
        // Both counts are the server's: saturate so a bogus `deleteCount`
        // (u64::MAX) clamps to the end instead of wrapping below `start`.
        let start = start.min(data.len());
        let end = start.saturating_add(delete).min(data.len());
        data.splice(start..end, new);
    }
    data
}

/// Which way a call hierarchy is walked: callers (`incomingCalls`) or callees
/// (`outgoingCalls`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

impl CallDirection {
    /// The field of each call holding the other end's `CallHierarchyItem`.
    pub fn key(self) -> &'static str {
        match self {
            CallDirection::Incoming => "from",
            CallDirection::Outgoing => "to",
        }
    }

    /// Title-case name, for the results buffer header.
    pub fn label(self) -> &'static str {
        match self {
            CallDirection::Incoming => "Incoming",
            CallDirection::Outgoing => "Outgoing",
        }
    }

    /// The second-leg request method.
    pub fn method(self) -> &'static str {
        match self {
            CallDirection::Incoming => "callHierarchy/incomingCalls",
            CallDirection::Outgoing => "callHierarchy/outgoingCalls",
        }
    }
}

/// Extract call locations from a `callHierarchy/incomingCalls` or
/// `outgoingCalls` result. Each call's item contributes a
/// `Location` (its `selectionRange`, falling back to `range`) plus name via the
/// returned `(name, Location)` pairs.
pub fn parse_call_hierarchy(result: &Value, direction: CallDirection) -> Vec<(String, Location)> {
    /// The `CallHierarchyItem` inside an incoming/outgoing call.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireHierarchyItem {
        name: Option<String>,
        #[serde(default)]
        uri: String,
        #[serde(default)]
        range: WireRange,
        selection_range: Option<WireRange>,
    }

    let Some(arr) = result.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|call| {
            let item: WireHierarchyItem =
                WireHierarchyItem::deserialize(call.get(direction.key())?).ok()?;
            // Prefer the narrower `selectionRange` (the identifier).
            let range = item.selection_range.unwrap_or(item.range);
            Some((
                item.name.unwrap_or_else(|| "?".to_string()),
                Location {
                    uri: item.uri,
                    start: range.start,
                    end: range.end,
                },
            ))
        })
        .collect()
}

/// The active signature from a `signatureHelp` result, as a single line. `None`
/// when there are no signatures.
pub fn parse_signature_help(result: &Value) -> Option<String> {
    let sigs = result["signatures"].as_array()?;
    if sigs.is_empty() {
        return None;
    }
    let active = result["activeSignature"].as_u64().unwrap_or(0) as usize;
    let sig = sigs.get(active).or_else(|| sigs.first())?;
    sig["label"].as_str().map(str::to_string)
}

/// Apply LSP `TextEdit`s to `text`, returning the new text. Edits are applied
/// last-position-first so earlier byte offsets stay valid as we splice.
pub fn apply_edits_to_text(text: &str, edits: &[TextEdit]) -> String {
    let buf = crate::buffer::Buffer::new(text);
    let mut spans: Vec<(usize, usize, usize, &str)> = edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let a = buf.position_to_byte(e.start.line, e.start.character);
            let b = buf.position_to_byte(e.end.line, e.end.character);
            (a.min(b), a.max(b), i, e.new_text.as_str())
        })
        .collect();
    // One forward pass over the original text, ascending by start. Ties (e.g.
    // several inserts at one position, or a replace and an insert sharing a
    // start) go in array order, so the result reads in array order, as the LSP
    // spec requires — in either order of the replace and the insert. A span
    // overlapping one already applied (a server bug) is clipped to the text
    // after it rather than splicing into replaced text (which could split a
    // UTF-8 sequence and panic).
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, _, new) in spans {
        let start = start.clamp(cursor, text.len());
        let end = end.clamp(start, text.len());
        out.push_str(&text[cursor..start]);
        out.push_str(new);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Convert a `file://` URI to a filesystem path string (inverse of
/// [`path_to_uri`] for local paths), percent-decoding the path — servers like
/// rust-analyzer encode spaces and non-ASCII (`%20`, `%C3%A9`, …).
pub fn uri_to_path(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    let path = percent_decode(rest);
    // Windows drive form: `file:///C:/…` → `C:/…`.
    #[cfg(windows)]
    if let Some(drive) = path.strip_prefix('/') {
        let mut chars = drive.chars();
        if let (Some(letter), Some(':')) = (chars.next(), chars.next())
            && letter.is_ascii_alphabetic()
        {
            return drive.to_string();
        }
    }
    path
}

/// Percent-decode `%XX` escapes in `s`; malformed escapes pass through
/// literally. Decoded bytes are reassembled as (lossy) UTF-8.
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Some(mut group) = self.child.take() {
            // Kill the whole group (see `start`), then reap it, or it lingers
            // as a zombie. SIGKILL makes the reap prompt, but do it off-thread
            // so a drop on the UI thread never blocks.
            group.kill_now();
            std::thread::spawn(move || drop(group));
        }
    }
}

/// A language-server backend: an external process ([`LspClient`]) or the
/// in-process [`SteelLsp`]. The editor drives both through this trait, so
/// nothing downstream of here cares which kind it's talking to. Requests
/// return the id their response will carry; a backend that has no use for a
/// notification keeps the default no-op.
pub trait LanguageServer {
    /// Acknowledge the server's `initialize` response.
    fn initialized(&mut self) {}

    /// Ask the server to exit (`shutdown` + `exit`) and hand back its process
    /// group for the caller to reap with a grace period. `None` for an
    /// in-process backend, or once already called.
    fn begin_shutdown(&mut self) -> Option<ProcessGroup> {
        None
    }

    fn did_open(&mut self, uri: &str, language_id: &str, version: i32, text: &str);

    /// Full-document sync.
    fn did_change(&mut self, uri: &str, version: i32, text: &str);

    /// Incremental sync: one ranged change. `start`/`end` are pre-edit LSP
    /// positions; `text` is the replacement. (The builtin
    /// advertises full sync, so it never gets these.)
    fn did_change_incremental(
        &mut self,
        _uri: &str,
        _version: i32,
        _start: Position,
        _end: Position,
        _text: &str,
    ) {
    }

    /// Request completions at a position.
    fn completion(&mut self, uri: &str, line: usize, character: usize) -> i64;

    /// Send a generic request. Returns its id, or `None` when unsupported. The
    /// builtin Steel server answers definition/references (plus diagnostics and
    /// completion, handled elsewhere) and declines everything else.
    fn request(&mut self, method: &str, params: Value) -> Option<i64>;

    /// Reply to a server→client request with `result`. (The builtin never
    /// sends server→client requests.)
    fn respond(&mut self, _id: Value, _result: Value) {}

    /// Reply to a server→client request with a JSON-RPC error.
    fn respond_error(&mut self, _id: Value, _code: i64, _message: &str) {}

    fn did_close(&mut self, uri: &str);

    /// `textDocument/didSave`; `text` is included only when the server asked.
    /// (The builtin re-checks on every change already.)
    fn did_save(&mut self, _uri: &str, _text: Option<&str>) {}

    /// `$/cancelRequest` for an in-flight request we no longer want. (The
    /// builtin answers synchronously; nothing is ever in flight.)
    fn cancel(&mut self, _id: i64) {}
}

/// Synthetic notification the reader thread sends once the server's stdout
/// is gone (process exit, crash, or a protocol error), so the editor can fail
/// in-flight requests and allow a restart. Independent of stderr, which may
/// stay open (or never have been piped) after stdout closes.
pub const EXITED_METHOD: &str = "$/macros/exited";

/// Read Content-Length-framed JSON-RPC messages until EOF, forwarding each,
/// then report the exit.
fn read_loop(stdout: std::process::ChildStdout, tx: UnboundedSender<Value>) {
    transport::read_frames(BufReader::new(stdout), &tx);
    let _ = tx.unbounded_send(json!({ "method": EXITED_METHOD }));
}

/// Convert a filesystem path to a `file://` URI, percent-encoding exactly what
/// the `url` crate's `Url::from_file_path` encodes (controls, non-ASCII, space,
/// `"`, `#`, `%`, `<`, `>`, `?`, `` ` ``, `{`, `}`) — most servers (rust-analyzer,
/// gopls, …) build their URIs that way, and diagnostics are keyed by URI, so a
/// stricter encoding (e.g. `@` → `%40`) would never match. On Windows, `\`
/// separators become `/` and the drive path gains the leading slash
/// (`file:///C:/…`).
pub fn path_to_uri(path: &Path) -> String {
    let path = path.to_string_lossy();
    let mut out = String::with_capacity(path.len() + 8);
    out.push_str("file://");
    if !path.starts_with('/') {
        out.push('/'); // Windows drive form, e.g. `C:\…`
    }
    for &b in path.as_bytes() {
        match b {
            b'\\' if cfg!(windows) => out.push('/'),
            0x00..=0x20
            | 0x7F..
            | b'"'
            | b'#'
            | b'%'
            | b'<'
            | b'>'
            | b'?'
            | b'`'
            | b'{'
            | b'}'
            | b'\\' => out.push_str(&format!("%{b:02X}")),
            _ => out.push(b as char),
        }
    }
    out
}

/// Canonicalise a server-sent URI so it compares equal to ours: a `file://`
/// URI is decoded and re-encoded with [`path_to_uri`]'s rules (servers differ
/// on what they escape — `%40` vs `@`, `%3A` vs `:`, hex case). Other schemes
/// pass through unchanged.
pub fn normalize_uri(uri: &str) -> String {
    if uri.starts_with("file://") {
        path_to_uri(Path::new(&uri_to_path(uri)))
    } else {
        uri.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `begin_shutdown` sends `shutdown` + `exit`, and a well-behaved server
    /// exits on its own (status 0) inside the grace period instead of being
    /// SIGKILLed. A tiny python server stands in for a real one.
    #[cfg(unix)]
    #[test]
    fn begin_shutdown_lets_the_server_exit_cleanly() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 not available");
            return;
        }
        let script = r#"
import sys, json
inp = sys.stdin.buffer
while True:
    n = 0
    while True:
        line = inp.readline()
        if not line:
            sys.exit(2)
        line = line.strip()
        if not line:
            break
        if line.lower().startswith(b"content-length:"):
            n = int(line.split(b":")[1])
    if json.loads(inp.read(n)).get("method") == "exit":
        sys.exit(0)
"#;
        let dir = std::env::temp_dir().join(format!("macros-lsp-shutdown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.py");
        std::fs::write(&path, script).unwrap();
        let command = format!("python3 {}", path.display());
        let (mut client, _rx) = LspClient::start(&command, &dir, &EnvOverrides::default()).unwrap();
        let mut group = client
            .begin_shutdown()
            .expect("external server has a group");
        let status = group.reap_with_grace(std::time::Duration::from_secs(5));
        assert_eq!(status.and_then(|s| s.code()), Some(0));
        assert!(client.begin_shutdown().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_capabilities_advertise_what_we_handle() {
        let caps = client_capabilities();
        assert_eq!(caps["workspace"]["applyEdit"], json!(true));
        assert_eq!(caps["workspace"]["configuration"], json!(true));
        assert_eq!(
            caps["textDocument"]["synchronization"]["didSave"],
            json!(true)
        );
        let code_action = &caps["textDocument"]["codeAction"];
        let kinds = code_action["codeActionLiteralSupport"]["codeActionKind"]["valueSet"]
            .as_array()
            .unwrap();
        for kind in ["quickfix", "refactor", "source", "source.organizeImports"] {
            assert!(kinds.contains(&json!(kind)), "missing kind {kind}");
        }
        assert_eq!(code_action["isPreferredSupport"], json!(true));
        assert_eq!(code_action["resolveSupport"]["properties"], json!(["edit"]));
    }

    #[test]
    fn default_server_replies() {
        let config = json!({ "items": [{ "section": "a" }, { "section": "b" }] });
        assert_eq!(
            default_server_reply("workspace/configuration", &config),
            Ok(json!([null, null]))
        );
        for method in [
            "window/workDoneProgress/create",
            "client/registerCapability",
            "workspace/semanticTokens/refresh",
        ] {
            assert_eq!(default_server_reply(method, &json!({})), Ok(Value::Null));
        }
        let err = default_server_reply("some/unknownThing", &json!({})).unwrap_err();
        assert_eq!(err.0, METHOD_NOT_FOUND);
    }

    #[test]
    fn sync_kind_shapes_and_lenient_capabilities() {
        let sync = |sync: Value| {
            ServerCapabilities::from_initialize(
                &json!({ "capabilities": { "textDocumentSync": sync } }),
            )
            .sync_kind
        };
        assert_eq!(sync(json!(2)), SyncKind::Incremental);
        assert_eq!(sync(json!({ "change": 1 })), SyncKind::Full);
        assert_eq!(sync(json!({ "change": 0 })), SyncKind::None);
        assert_eq!(sync(json!(7)), SyncKind::None);
        // Missing or malformed → full.
        assert_eq!(sync(json!({})), SyncKind::Full);
        assert_eq!(sync(json!({ "change": "x" })), SyncKind::Full);
        assert_eq!(sync(json!("x")), SyncKind::Full);
        assert_eq!(
            ServerCapabilities::from_initialize(&Value::Null),
            ServerCapabilities::default()
        );
        // One malformed capability doesn't sink the others.
        let caps = ServerCapabilities::from_initialize(&json!({ "capabilities": {
            "textDocumentSync": 2,
            "semanticTokensProvider": "bogus",
            "completionProvider": { "triggerCharacters": [".", 3] },
            "codeActionProvider": { "resolveProvider": true },
        } }));
        assert_eq!(caps.sync_kind, SyncKind::Incremental);
        assert!(caps.semantic_legend.is_empty());
        assert_eq!(caps.completion_triggers, vec!["."]);
        assert!(caps.code_action_resolve);
    }

    #[test]
    fn save_option_shapes() {
        let caps = |sync: Value| json!({ "capabilities": { "textDocumentSync": sync } });
        assert_eq!(
            ServerCapabilities::from_initialize(&caps(json!(2))).save,
            Some(false)
        );
        assert_eq!(
            ServerCapabilities::from_initialize(&caps(json!({ "change": 2 }))).save,
            None
        );
        assert_eq!(
            ServerCapabilities::from_initialize(&caps(json!({ "save": true }))).save,
            Some(false)
        );
        assert_eq!(
            ServerCapabilities::from_initialize(&caps(json!({ "save": false }))).save,
            None
        );
        assert_eq!(
            ServerCapabilities::from_initialize(&caps(json!({ "save": { "includeText": true } })))
                .save,
            Some(true)
        );
        assert_eq!(
            ServerCapabilities::from_initialize(&caps(json!({ "save": {} }))).save,
            Some(false)
        );
        assert_eq!(
            ServerCapabilities::from_initialize(&json!({ "capabilities": {} })).save,
            None
        );
    }

    #[test]
    fn diagnostics_keep_their_raw_json() {
        let params = json!({ "uri": "file:///a.rs", "diagnostics": [{
            "range": { "start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5} },
            "severity": 2, "message": "unused", "code": "E0001", "source": "rustc",
            "data": { "fix": 7 }
        }] });
        let (_, diags) = parse_diagnostics(&params);
        assert_eq!(diags[0].raw["code"], json!("E0001"));
        assert_eq!(diags[0].raw["source"], json!("rustc"));
        assert_eq!(diags[0].raw["data"], json!({ "fix": 7 }));
        assert_eq!(diags[0].raw, params["diagnostics"][0]);
    }

    #[test]
    fn edit_less_code_actions_are_marked_for_resolve() {
        let result = json!([
            { "title": "Lazy", "kind": "quickfix", "data": { "id": 3 } },
            { "title": "Eager", "edit": { "changes": {} } },
            { "title": "Bare", "command": "do.thing" }
        ]);
        let actions = parse_code_actions(&result);
        assert_eq!(actions[0].resolve.as_ref(), Some(&result[0]));
        assert!(actions[1].resolve.is_none());
        assert!(actions[2].resolve.is_none());
    }

    #[test]
    fn progress_text_formats_begin_and_report() {
        let begin = json!({ "kind": "begin", "title": "Indexing", "percentage": 0 });
        assert_eq!(
            progress_text("rust", "Indexing", &begin),
            "rust: Indexing 0%"
        );
        let report = json!({ "kind": "report", "message": "3/10", "percentage": 30 });
        assert_eq!(
            progress_text("rust", "Indexing", &report),
            "rust: Indexing 3/10 30%"
        );
    }

    #[test]
    fn text_edits_past_line_end_do_not_eat_the_newline() {
        // A server's "end of line" (a large character) must clamp before the
        // terminator, not swallow it and join the next line.
        let edits = [TextEdit {
            start: Position::new(0, 3),
            end: Position::new(0, 999),
            new_text: "!".into(),
        }];
        assert_eq!(apply_edits_to_text("abc\r\ndef\n", &edits), "abc!\r\ndef\n");
        let past_eof = [TextEdit {
            start: Position::new(9, 0),
            end: Position::new(9, 0),
            new_text: "tail".into(),
        }];
        assert_eq!(apply_edits_to_text("ab\ncd", &past_eof), "ab\ncdtail");
    }

    use futures::StreamExt as _;

    #[test]
    fn parse_completion_extracts_detail_and_kind() {
        let result = json!([
            { "label": "foo", "kind": 3,
              "labelDetails": { "detail": "(x: i32)", "description": "bool" } },
            { "label": "bar", "insertText": "bar!", "detail": "a constant" },
            { "label": "begin", "kind": 14 },
        ]);
        let items = parse_completion(&result);
        assert_eq!(items.len(), 3);
        // Function with labelDetails → "fn" + combined signature.
        assert_eq!(items[0].kind.as_deref(), Some("fn"));
        assert_eq!(items[0].detail.as_deref(), Some("(x: i32) bool"));
        // insertText honoured; plain detail fallback; no kind.
        assert_eq!(items[1].insert_text, "bar!");
        assert_eq!(items[1].detail.as_deref(), Some("a constant"));
        assert_eq!(items[1].kind, None);
        // Keyword kind.
        assert_eq!(items[2].kind.as_deref(), Some("kw"));
    }

    #[test]
    fn parse_locations_handles_all_shapes() {
        // Single Location.
        let single = json!({
            "uri": "file:///a.rs",
            "range": { "start": {"line": 3, "character": 5}, "end": {"line": 3, "character": 8} }
        });
        let locs = parse_locations(&single);
        assert_eq!(
            locs,
            vec![Location {
                uri: "file:///a.rs".into(),
                start: Position::new(3, 5),
                end: Position::new(3, 8)
            }]
        );

        // Array of LocationLink (prefers targetSelectionRange).
        let links = json!([{
            "targetUri": "file:///b.rs",
            "targetRange": { "start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0} },
            "targetSelectionRange": { "start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 7} }
        }]);
        let locs = parse_locations(&links);
        assert_eq!(
            locs,
            vec![Location {
                uri: "file:///b.rs".into(),
                start: Position::new(1, 4),
                end: Position::new(1, 7)
            }]
        );

        // A link without a selection range falls back to targetRange; a
        // malformed entry is skipped, not fatal to the rest.
        let mixed = json!([
            { "targetUri": "file:///c.rs",
              "targetRange": { "start": {"line": 2, "character": 0}, "end": {"line": 4, "character": 1} } },
            { "nope": true },
        ]);
        assert_eq!(
            parse_locations(&mixed),
            vec![Location {
                uri: "file:///c.rs".into(),
                start: Position::new(2, 0),
                end: Position::new(4, 1)
            }]
        );

        // Null → empty.
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn parse_hover_renders_markup_and_arrays() {
        assert_eq!(
            parse_hover(&json!({ "contents": "plain" })).as_deref(),
            Some("plain")
        );
        assert_eq!(
            parse_hover(&json!({ "contents": { "kind": "markdown", "value": "**x**" } }))
                .as_deref(),
            Some("**x**")
        );
        assert_eq!(
            parse_hover(&json!({ "contents": ["a", { "language": "rust", "value": "b" }] }))
                .as_deref(),
            Some("a\nb")
        );
        assert_eq!(parse_hover(&json!({ "contents": "" })), None);
        assert_eq!(parse_hover(&Value::Null), None);
        // Unrenderable pieces contribute nothing (but don't sink the rest).
        assert_eq!(parse_hover(&json!({ "contents": { "kind": "x" } })), None);
        assert_eq!(
            parse_hover(&json!({ "contents": [1, "a", { "value": 2 }] })).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn parse_symbols_flattens_hierarchy_and_flat() {
        // Hierarchical DocumentSymbol with children → fallback uri + depth.
        let hierarchical = json!([{
            "name": "Foo", "kind": 23,
            "range": { "start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 0} },
            "selectionRange": { "start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 10} },
            "children": [{
                "name": "bar", "kind": 6,
                "range": { "start": {"line": 1, "character": 4}, "end": {"line": 2, "character": 4} },
                "selectionRange": { "start": {"line": 1, "character": 8}, "end": {"line": 1, "character": 11} }
            }]
        }]);
        let syms = parse_symbols(&hierarchical, "file:///f.rs");
        assert_eq!(syms.len(), 2);
        assert_eq!(
            (syms[0].name.as_str(), syms[0].depth, syms[0].start),
            ("Foo", 0, Position::new(0, 7))
        );
        assert_eq!(
            (
                syms[1].name.as_str(),
                syms[1].depth,
                syms[1].container.as_str()
            ),
            ("bar", 1, "Foo")
        );
        assert_eq!(syms[1].uri, "file:///f.rs");

        // Flat SymbolInformation carries its own location.
        let flat = json!([{
            "name": "baz", "kind": 12,
            "location": { "uri": "file:///g.rs", "range": { "start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 3} } }
        }]);
        let syms = parse_symbols(&flat, "file:///f.rs");
        assert_eq!(syms[0].uri, "file:///g.rs");
        assert_eq!(syms[0].start, Position::new(9, 0));
    }

    #[test]
    fn parse_workspace_edit_both_shapes_and_apply() {
        // documentChanges shape.
        let dc = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///a.rs", "version": 1 },
                "edits": [
                    { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3} }, "newText": "bar" }
                ]
            }]
        });
        let we = parse_workspace_edit(&dc);
        assert_eq!(we.changes.len(), 1);
        assert_eq!(we.changes[0].0, "file:///a.rs");

        // changes-map shape.
        let map = json!({
            "changes": {
                "file:///a.rs": [
                    { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3} }, "newText": "bar" }
                ]
            }
        });
        let we = parse_workspace_edit(&map);
        assert_eq!(we.changes[0].0, "file:///a.rs");

        // Applying multiple edits to a line, last-first, stays correct.
        let edits = vec![
            TextEdit {
                start: Position::new(0, 0),
                end: Position::new(0, 3),
                new_text: "X".into(),
            },
            TextEdit {
                start: Position::new(0, 4),
                end: Position::new(0, 7),
                new_text: "Y".into(),
            },
        ];
        assert_eq!(apply_edits_to_text("foo bar baz", &edits), "X Y baz");
    }

    #[test]
    fn parse_code_actions_distinguishes_command_and_edit() {
        let result = json!([
            { "title": "Fix it", "edit": { "changes": { "file:///a.rs": [
                { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} }, "newText": "z" }
            ] } } },
            { "title": "Run", "command": "do.thing", "arguments": [1, 2] },
            { "title": "Nested", "command": { "command": "nested.cmd", "arguments": ["a"] } }
        ]);
        let actions = parse_code_actions(&result);
        assert_eq!(actions.len(), 3);
        assert!(actions[0].edit.is_some() && actions[0].command.is_none());
        assert_eq!(actions[1].command.as_ref().unwrap().0, "do.thing");
        assert_eq!(actions[2].command.as_ref().unwrap().0, "nested.cmd");

        let help = json!({ "signatures": [{ "label": "fn foo(x: i32)" }], "activeSignature": 0 });
        assert_eq!(
            parse_signature_help(&help).as_deref(),
            Some("fn foo(x: i32)")
        );
    }

    #[test]
    fn parse_call_hierarchy_extracts_items() {
        let incoming = json!([{
            "from": {
                "name": "caller", "uri": "file:///c.rs",
                "range": { "start": {"line": 10, "character": 0}, "end": {"line": 12, "character": 0} },
                "selectionRange": { "start": {"line": 10, "character": 3}, "end": {"line": 10, "character": 9} }
            },
            "fromRanges": []
        }]);
        let calls = parse_call_hierarchy(&incoming, CallDirection::Incoming);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "caller");
        assert_eq!(calls[0].1.start, Position::new(10, 3)); // prefers selectionRange
        assert_eq!(calls[0].1.uri, "file:///c.rs");
        // Wrong key / empty → nothing.
        assert!(parse_call_hierarchy(&incoming, CallDirection::Outgoing).is_empty());
        assert!(parse_call_hierarchy(&Value::Null, CallDirection::Incoming).is_empty());
    }

    #[test]
    fn decode_semantic_tokens_delta_decodes() {
        let legend = vec![
            "function".to_string(),
            "variable".to_string(),
            "keyword".to_string(),
        ];
        // Token A: line 0 col 0 len 3 type function
        // Token B: same line, +4 cols, len 2, type variable
        // Token C: +2 lines, col 1, len 5, type keyword
        let result = json!({ "data": [0,0,3,0,0,  0,4,2,1,0,  2,1,5,2,0] });
        let toks = decode_semantic_data(&semantic_data(&result), &legend);
        assert_eq!(
            toks,
            vec![
                (0, 0, 3, "function".to_string()),
                (0, 4, 2, "variable".to_string()),
                (2, 1, 5, "keyword".to_string()),
            ]
        );

        // Delta: splice [start=5, deleteCount=5] with a new variable token,
        // producing function + variable (the second token replaced).
        let delta = json!({ "resultId": "2", "edits": [{ "start": 5, "deleteCount": 5, "data": [0,4,2,1,0] }] });
        let merged = apply_semantic_delta(&semantic_data(&result), &delta);
        let toks = decode_semantic_data(&merged, &legend);
        assert_eq!(toks[1], (0, 4, 2, "variable".to_string()));
        assert_eq!(semantic_result_id(&delta).as_deref(), Some("2"));

        // A hostile/buggy deleteCount clamps to the end rather than panicking.
        let huge = json!({ "edits": [{ "start": 5, "deleteCount": u64::MAX, "data": [] }] });
        let clipped = apply_semantic_delta(&semantic_data(&result), &huge);
        assert_eq!(clipped, vec![0, 0, 3, 0, 0]);

        // Legend parse from an initialize result.
        let init = json!({ "capabilities": { "semanticTokensProvider": {
            "legend": { "tokenTypes": ["a", "b"], "tokenModifiers": [] }
        } } });
        assert_eq!(
            ServerCapabilities::from_initialize(&init).semantic_legend,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn prepare_rename_all_shapes() {
        // Plain Range.
        let r = json!({ "start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 9} });
        assert_eq!(
            parse_prepare_rename(&r),
            Some(PrepareRename {
                placeholder: None,
                range: Some((Position::new(2, 4), Position::new(2, 9)))
            })
        );
        // {range, placeholder}.
        let rp = json!({ "range": r, "placeholder": "foo" });
        assert_eq!(
            parse_prepare_rename(&rp).unwrap().placeholder.as_deref(),
            Some("foo")
        );
        // defaultBehavior.
        assert_eq!(
            parse_prepare_rename(&json!({ "defaultBehavior": true })),
            Some(PrepareRename {
                placeholder: None,
                range: None
            })
        );
        // null / defaultBehavior:false / anything else → not renameable.
        assert_eq!(parse_prepare_rename(&Value::Null), None);
        assert_eq!(
            parse_prepare_rename(&json!({ "defaultBehavior": false })),
            None
        );
        assert_eq!(parse_prepare_rename(&json!({ "x": 1 })), None);
        // capability flag.
        let init = json!({ "capabilities": { "renameProvider": { "prepareProvider": true } } });
        assert!(ServerCapabilities::from_initialize(&init).prepare_rename);
        assert!(
            !ServerCapabilities::from_initialize(
                &json!({ "capabilities": { "renameProvider": true } })
            )
            .prepare_rename
        );
    }

    #[test]
    fn completion_triggers_parse_from_capabilities() {
        let init = json!({
            "capabilities": { "completionProvider": { "triggerCharacters": [".", "#", "~"] } }
        });
        assert_eq!(
            ServerCapabilities::from_initialize(&init).completion_triggers,
            vec![".", "#", "~"]
        );
        // No completionProvider, or no triggerCharacters → empty.
        assert!(
            ServerCapabilities::from_initialize(&json!({ "capabilities": {} }))
                .completion_triggers
                .is_empty()
        );
        assert!(
            ServerCapabilities::from_initialize(&json!({
                "capabilities": { "completionProvider": {} }
            }))
            .completion_triggers
            .is_empty()
        );
    }

    #[test]
    fn completion_detail_and_label_collapse_newlines() {
        // ocamllsp puts multi-line type signatures in `detail`; the popup renders
        // label+detail on one line via `shape_line`, which panics on a newline.
        let item = &parse_completion(&json!([{
            "label": "map",
            "detail": "('a -> 'b) ->\n  'a list ->\n  'b list",
            "insertText": "map"
        }]))[0];
        assert!(!item.label.contains('\n'));
        let detail = item.detail.as_deref().unwrap();
        assert!(!detail.contains('\n') && !detail.contains('\t'));
        assert_eq!(detail, "('a -> 'b) -> 'a list -> 'b list");

        // A newline in the label is collapsed for display, but a missing
        // insertText still inserts the raw label text.
        let item = &parse_completion(&json!([{ "label": "a\nb" }]))[0];
        assert_eq!(item.label, "a b");
        assert_eq!(item.insert_text, "a\nb");
    }

    #[test]
    fn completion_resolve_and_code_lens() {
        // Resolve fills documentation + additionalTextEdits onto the item.
        let mut item = parse_completion(&json!([{ "label": "foo" }]))[0].clone();
        assert!(item.documentation.is_none() && !item.resolved);
        let resolved = json!({
            "documentation": { "kind": "markdown", "value": "Does the thing." },
            "additionalTextEdits": [
                { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0} }, "newText": "use x;\n" }
            ]
        });
        merge_resolved_completion(&mut item, &resolved);
        assert_eq!(item.documentation.as_deref(), Some("Does the thing."));
        assert_eq!(item.additional_edits.len(), 1);
        assert!(item.resolved);

        // Code lens: inlined command vs needs-resolve.
        let lenses = parse_code_lenses(&json!([
            { "range": { "start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 4} },
              "command": { "title": "3 refs", "command": "x" } },
            { "range": { "start": {"line": 8, "character": 0}, "end": {"line": 8, "character": 4} } }
        ]));
        assert_eq!(lenses.len(), 2);
        assert_eq!(lenses[0].title.as_deref(), Some("3 refs"));
        assert_eq!((lenses[0].line, lenses[1].title.clone()), (3, None));
        assert_eq!(
            parse_code_lens_title(&json!({ "command": { "title": "impl" } })).as_deref(),
            Some("impl")
        );
    }

    #[test]
    fn same_position_inserts_apply_in_array_order() {
        // LSP: inserts at one position appear in the result in array order.
        let ins = |t: &str| TextEdit {
            start: Position::new(0, 3),
            end: Position::new(0, 3),
            new_text: t.into(),
        };
        assert_eq!(
            apply_edits_to_text("abcdef", &[ins("1"), ins("2"), ins("3")]),
            "abc123def"
        );
        // An insert and a replace starting at the same position: the insert
        // (first in the array) precedes the replacement text.
        let edits = [
            ins("<"),
            TextEdit {
                start: Position::new(0, 3),
                end: Position::new(0, 4),
                new_text: "D".into(),
            },
        ];
        assert_eq!(apply_edits_to_text("abcdef", &edits), "abc<Def");
        // The other array order: the replacement first, then the insert at
        // its start — the insert must survive and follow it.
        let edits = [edits[1].clone(), edits[0].clone()];
        assert_eq!(apply_edits_to_text("abcdef", &edits), "abcD<ef");
    }

    #[test]
    fn overlapping_edits_do_not_split_a_codepoint() {
        // Overlapping ranges are a server bug, but must not panic: here the
        // second edit ends inside text the first one replaced with a
        // multi-byte character.
        let edit = |s: usize, e: usize, t: &str| TextEdit {
            start: Position::new(0, s),
            end: Position::new(0, e),
            new_text: t.into(),
        };
        let out = apply_edits_to_text("abcdefgh", &[edit(2, 3, "é"), edit(0, 3, "x")]);
        assert_eq!(out, "xédefgh");
    }

    #[test]
    fn completion_text_edit_wins_and_resolved_detail_is_single_line() {
        let item = &parse_completion(&json!([{
            "label": "foo",
            "insertText": "wrong",
            "textEdit": {
                "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} },
                "newText": "right"
            }
        }]))[0];
        assert_eq!(item.insert_text, "right");

        let mut item = parse_completion(&json!([{ "label": "map" }]))[0].clone();
        merge_resolved_completion(&mut item, &json!({ "detail": "'a ->\n  'b" }));
        assert_eq!(item.detail.as_deref(), Some("'a -> 'b"));
    }

    #[test]
    fn spawn_with_missing_cwd_reports_the_directory() {
        let missing = Path::new("/definitely/not/a/real/dir/macros-test");
        let err = LspClient::start("sh", missing, &Default::default())
            .err()
            .unwrap();
        assert!(err.contains("working directory"), "{err}");
        assert!(!err.contains("not found in PATH"), "{err}");
    }

    #[test]
    fn uri_encoding_matches_url_crate_and_normalizes() {
        // Characters the `url` crate leaves alone in file paths stay literal.
        assert_eq!(
            path_to_uri(Path::new("/tmp/a@b+c=d,e;f:g!h$i&j'k(l)m*n[o]p~q.rs")),
            "file:///tmp/a@b+c=d,e;f:g!h$i&j'k(l)m*n[o]p~q.rs"
        );
        assert_eq!(
            path_to_uri(Path::new("/tmp/a b#c?d%e{f}g`h\"i<j>.rs")),
            "file:///tmp/a%20b%23c%3Fd%25e%7Bf%7Dg%60h%22i%3Cj%3E.rs"
        );
        // A server URI escaping more (or less, or lower-case hex) than we do
        // normalises to ours, so diagnostics keyed by it match.
        let ours = path_to_uri(Path::new("/tmp/my dir/@scope/a.ts"));
        assert_eq!(normalize_uri("file:///tmp/my%20dir/%40scope/a.ts"), ours);
        let (uri, _) = parse_diagnostics(&json!({
            "uri": "file:///tmp/my%20dir/%40scope/a.ts", "diagnostics": []
        }));
        assert_eq!(uri, ours);
        assert_eq!(normalize_uri("untitled:Untitled-1"), "untitled:Untitled-1");
    }

    #[test]
    fn uri_path_roundtrip() {
        assert_eq!(uri_to_path("file:///Users/x/a.rs"), "/Users/x/a.rs");
        assert_eq!(uri_to_path("/already/a/path"), "/already/a/path");
    }

    #[test]
    fn uri_percent_encoding_roundtrips() {
        // A plain path is untouched apart from the scheme.
        assert_eq!(
            path_to_uri(Path::new("/Users/x/a.rs")),
            "file:///Users/x/a.rs"
        );
        // Spaces and non-ASCII must be percent-encoded on the way out (that's
        // what rust-analyzer et al. send back) and decoded on the way in.
        let space = Path::new("/Users/x/my project/a b.rs");
        assert_eq!(path_to_uri(space), "file:///Users/x/my%20project/a%20b.rs");
        assert_eq!(
            uri_to_path(&path_to_uri(space)),
            "/Users/x/my project/a b.rs"
        );

        let unicode = Path::new("/tmp/café/naïve.rs");
        assert_eq!(path_to_uri(unicode), "file:///tmp/caf%C3%A9/na%C3%AFve.rs");
        assert_eq!(uri_to_path(&path_to_uri(unicode)), "/tmp/café/naïve.rs");

        // Other reserved characters round-trip too.
        let odd = Path::new("/tmp/a#b?c&d.rs");
        assert_eq!(uri_to_path(&path_to_uri(odd)), "/tmp/a#b?c&d.rs");

        // Decoding tolerates malformed escapes (passed through literally).
        assert_eq!(uri_to_path("file:///x/100%zz"), "/x/100%zz");
        assert_eq!(uri_to_path("file:///x/50%"), "/x/50%");
    }

    #[test]
    #[ignore = "spawns the real rust-analyzer; run with --ignored"]
    fn rust_analyzer_responds_to_initialize() {
        let root = std::env::current_dir().unwrap();
        let (_client, mut rx) = LspClient::start("rust-analyzer", &root, &Default::default())
            .expect("rust-analyzer should spawn");
        // Blocks until the server sends its first framed message — the
        // initialize response. Exercises spawn + framing + handshake end to end.
        let message = futures::executor::block_on(rx.next()).expect("a message from the server");
        assert_eq!(message["jsonrpc"], json!("2.0"));
    }
}
