//! The in-process Steel/Scheme language server: the backend behind
//! `(set-lsp "scheme" "builtin")`, driven through the same [`LanguageServer`]
//! trait as an external server.
//!
//! [`SteelLsp`] is a thin handle the editor talks to on the UI thread; the
//! server proper ([`SteelServer`], which owns a compile-only Steel `Engine`)
//! runs on its own thread, so compiling a large file on every edit never
//! stalls typing. Responses flow back over the same message channel an
//! external server's reader thread uses.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;

use super::{LanguageServer, path_to_uri, uri_to_path};
use crate::jsonrpc;
use crate::sexp::{self, Kind, Node};

/// Requests the builtin server answers (besides completion). Anything else is
/// declined synchronously, so the editor can fall back at once.
const REQUEST_METHODS: &[&str] = &[
    "textDocument/definition",
    "textDocument/declaration",
    "textDocument/typeDefinition",
    "textDocument/implementation",
    "textDocument/references",
    "textDocument/hover",
    "textDocument/prepareRename",
    "textDocument/rename",
    "textDocument/documentSymbol",
    "textDocument/signatureHelp",
];

/// Work for the server thread. Ids are allocated by [`SteelLsp`] on the UI
/// thread (the trait returns them synchronously) and carried here.
enum Job {
    Open(String, String),
    Change(String, String),
    Close(String),
    Completion {
        id: i64,
        uri: String,
        line: usize,
        character: usize,
    },
    Request {
        id: i64,
        method: String,
        params: Value,
    },
}

/// The editor's handle on the builtin Steel server. Every call just queues a
/// [`Job`] for the server thread.
pub struct SteelLsp {
    jobs: Sender<Job>,
    next_id: i64,
}

impl SteelLsp {
    /// Start the server thread and return the handle plus the message receiver.
    /// The thread builds its engine and then answers the (implicit)
    /// `initialize`, so the editor's handshake completes once it is ready.
    /// `known_globals` are editor-registered Steel commands (e.g. `set-lsp`);
    /// `seed_editor_api` picks the `macros-scheme` server (see
    /// [`SteelServer::new`]).
    pub fn start(
        seed_editor_api: bool,
        known_globals: &[String],
    ) -> (Self, UnboundedReceiver<Value>) {
        let (tx, rx) = unbounded();
        let (jobs, jobs_rx) = channel();
        let globals = known_globals.to_vec();
        let spawned = std::thread::Builder::new()
            .name("steel-lsp".into())
            // The Steel compiler recurses on the source's nesting depth.
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let server = SteelServer::new(seed_editor_api, &globals, tx);
                server_thread(server, jobs_rx);
            });
        if let Err(e) = spawned {
            eprintln!("steel-lsp: could not start the server thread: {e}");
        }
        (Self { jobs, next_id: 1 }, rx)
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl LanguageServer for SteelLsp {
    fn did_open(&mut self, uri: &str, _language_id: &str, _version: i32, text: &str) {
        let _ = self.jobs.send(Job::Open(uri.into(), text.into()));
    }

    fn did_change(&mut self, uri: &str, _version: i32, text: &str) {
        let _ = self.jobs.send(Job::Change(uri.into(), text.into()));
    }

    fn completion(&mut self, uri: &str, line: usize, character: usize) -> i64 {
        let id = self.alloc_id();
        let _ = self.jobs.send(Job::Completion {
            id,
            uri: uri.into(),
            line,
            character,
        });
        id
    }

    fn request(&mut self, method: &str, params: Value) -> Option<i64> {
        if !REQUEST_METHODS.contains(&method) {
            return None;
        }
        let id = self.alloc_id();
        let _ = self.jobs.send(Job::Request {
            id,
            method: method.into(),
            params,
        });
        Some(id)
    }

    fn did_close(&mut self, uri: &str) {
        let _ = self.jobs.send(Job::Close(uri.into()));
    }
}

/// The server thread: process jobs in order. Edits that pile up while a
/// compile runs are applied as they come (later requests see the latest text),
/// but each document's diagnostics are recomputed once per batch, for its
/// latest text only.
fn server_thread(mut server: SteelServer, jobs: Receiver<Job>) {
    while let Ok(first) = jobs.recv() {
        let mut dirty: Vec<String> = Vec::new();
        let mut job = Some(first);
        while let Some(j) = job {
            match j {
                Job::Open(uri, text) | Job::Change(uri, text) => {
                    server.docs.insert(uri.clone(), text);
                    if !dirty.contains(&uri) {
                        dirty.push(uri);
                    }
                }
                Job::Close(uri) => {
                    server.docs.remove(&uri);
                    dirty.retain(|u| *u != uri);
                }
                Job::Completion {
                    id,
                    uri,
                    line,
                    character,
                } => server.completion_request(id, &uri, line, character),
                Job::Request { id, method, params } => server.handle_request(id, &method, &params),
            }
            job = jobs.try_recv().ok();
        }
        for uri in dirty {
            server.publish_diagnostics(&uri);
        }
    }
}

/// The builtin Steel/Scheme language server proper. It keeps a copy of each
/// open document (full sync — free in-process, and it avoids the drift an
/// incremental copy could accumulate) and offers:
///   * **diagnostics** — a fast delimiter/string/comment balance check, then,
///     once parens balance, the real Steel compiler (with the file's path, so
///     relative `require`s resolve) for semantic errors with spans, then a
///     conservative unknown-function scan,
///   * **completion** — the file's own bindings, every binding and macro the
///     Steel environment knows, special forms, and names defined in files the
///     document `include`s / `require`s / `load`s,
///   * **navigation** — definition, references, rename (reader-aware, scope-
///     checked), hover, signature help and document symbols.
struct SteelServer {
    tx: UnboundedSender<Value>,
    /// Ids for requests made directly on the server (tests); the editor's
    /// handle allocates its own.
    #[cfg(test)]
    next_id: i64,
    docs: HashMap<String, String>,
    /// A compile-only engine: used to type-check source, never to run it.
    engine: Engine,
    /// name → (doc, source file, 1-based line) for every bundled define, so
    /// go-to-definition and hover can resolve editor-API symbols. Empty for the
    /// plain `scheme` server (which seeds no editor API).
    defs: HashMap<String, (String, String, usize)>,
}

/// Outcome of a sandboxed diagnostic compile.
enum CompileResult {
    Ok,
    Error(steel::rerrs::SteelErr),
    Panicked,
}

/// Forms that define the name in their second position: `(define name …)` or
/// `(define (name args…) …)`.
const DEFINERS: &[&str] = &[
    "define",
    "define-syntax",
    "define-command",
    "define/contract",
    "define-values",
    "struct",
    "define-struct",
    "define-record-type",
];

/// One definition found by the reader.
struct Definition {
    name: String,
    /// Byte range of the defined name.
    start: usize,
    end: usize,
    /// Whether it is at top level (vs an internal define).
    top_level: bool,
    /// Byte range of the form *containing* the definition (for scoping an
    /// internal define's rename), or the whole text at top level.
    scope: (usize, usize),
    /// `(define (f …) …)` / lambda-valued: a function rather than a value.
    function: bool,
    /// The defining form's head (`define`, `struct`, …).
    head: String,
}

/// An atom's text without its quote prefix (`'name` → `name`), and where that
/// text starts.
fn atom_body<'t>(text: &'t str, node: &Node) -> (usize, &'t str) {
    let raw = &text[node.start..node.end];
    let body = raw.trim_start_matches(['\'', '`', ',', '@']);
    let body = body.strip_prefix("#'").unwrap_or(body);
    (node.end - body.len(), body)
}

/// Every definition in `text`, in document order (top-level and internal).
fn definitions(text: &str) -> Vec<Definition> {
    let parsed = sexp::parse(text);
    let mut out = Vec::new();
    // (siblings, depth, enclosing form's range)
    let mut stack: Vec<(&[Node], usize, (usize, usize))> =
        vec![(&parsed.nodes, 0, (0, text.len()))];
    while let Some((nodes, depth, scope)) = stack.pop() {
        for node in nodes.iter().rev() {
            if !node.children.is_empty() {
                stack.push((&node.children, depth + 1, (node.start, node.end)));
            }
        }
        for node in nodes {
            // An unquoted `( … )` form whose head is a symbol.
            let Kind::List { open, .. } = node.kind else {
                continue;
            };
            if node.start != open || text.as_bytes()[open] != b'(' {
                continue;
            }
            let [head, target, rest @ ..] = node.children.as_slice() else {
                continue;
            };
            if !matches!(head.kind, Kind::Atom) {
                continue;
            }
            let head_name = &text[head.start..head.end];
            if !DEFINERS.contains(&head_name) {
                continue;
            }
            let (name_node, function) = match target.kind {
                Kind::Atom => (
                    target,
                    rest.first().is_some_and(|v| {
                        v.children
                            .first()
                            .is_some_and(|h| matches!(&text[h.start..h.end], "lambda" | "λ"))
                    }),
                ),
                Kind::List { .. } => match target.children.first() {
                    Some(n) if matches!(n.kind, Kind::Atom) => (n, true),
                    _ => continue,
                },
                Kind::Str { .. } => continue,
            };
            let (start, name) = atom_body(text, name_node);
            out.push(Definition {
                name: name.to_string(),
                start,
                end: name_node.end,
                top_level: depth == 0,
                scope: if depth == 0 { (0, text.len()) } else { scope },
                function,
                head: head_name.to_string(),
            });
        }
    }
    out.sort_by_key(|d| d.start);
    out
}

/// Byte ranges of every reference to `sym` in `text`: atoms whose name (after
/// any quote prefix) is exactly `sym`. Strings, comments and `#:keyword`s never
/// match.
fn symbol_occurrences(text: &str, sym: &str) -> Vec<(usize, usize)> {
    let parsed = sexp::parse(text);
    let mut out = Vec::new();
    let mut stack: Vec<&Node> = parsed.nodes.iter().collect();
    while let Some(node) = stack.pop() {
        match node.kind {
            Kind::Atom => {
                let (start, body) = atom_body(text, node);
                if body == sym {
                    out.push((start, node.end));
                }
            }
            Kind::List { .. } => stack.extend(node.children.iter()),
            Kind::Str { .. } => {}
        }
    }
    out.sort_unstable();
    out
}

/// The references a rename / find-references of `sym` should touch: inside an
/// internal define's enclosing form only; for a top-level one, everywhere
/// except where a local binding of the same name shadows it.
fn scoped_occurrences(text: &str, sym: &str) -> Vec<(usize, usize)> {
    let defs = definitions(text);
    let def = defs
        .iter()
        .find(|d| d.name == sym && d.top_level)
        .or_else(|| defs.iter().find(|d| d.name == sym));
    let occurrences = symbol_occurrences(text, sym);
    match def {
        Some(d) if !d.top_level => occurrences
            .into_iter()
            .filter(|(s, _)| *s >= d.scope.0 && *s < d.scope.1)
            .collect(),
        Some(d) => {
            let binders = local_binder_sites(text, sym);
            let parsed = crate::analyze::Parsed::new(text);
            occurrences
                .into_iter()
                .filter(|(s, _)| {
                    *s == d.start
                        || (!binders.contains(s)
                            && !parsed
                                .as_ref()
                                .is_some_and(|p| p.local_bindings_at(*s).contains(sym)))
                })
                .collect()
        }
        None => occurrences,
    }
}

/// Byte starts of every spot that *binds* `sym` locally: a lambda or define
/// parameter, a let-family binding (or a named let's name), or an internal
/// define of that name. A reference to a top-level `sym` is never one of these.
fn local_binder_sites(text: &str, sym: &str) -> HashSet<usize> {
    let parsed = sexp::parse(text);
    let mut out = HashSet::new();
    let atom_named = |n: &Node| matches!(n.kind, Kind::Atom) && atom_body(text, n).1 == sym;
    let add_atoms = |nodes: &[Node], out: &mut HashSet<usize>| {
        for n in nodes.iter().filter(|n| atom_named(n)) {
            out.insert(atom_body(text, n).0);
        }
    };
    // (nodes, depth)
    let mut stack: Vec<(&[Node], usize)> = vec![(&parsed.nodes, 0)];
    while let Some((nodes, depth)) = stack.pop() {
        for node in nodes {
            if !node.children.is_empty() {
                stack.push((&node.children, depth + 1));
            }
            let [head, second, rest @ ..] = node.children.as_slice() else {
                continue;
            };
            if !matches!(head.kind, Kind::Atom) {
                continue;
            }
            match &text[head.start..head.end] {
                "define" | "define/contract" | "define-command" => match second.kind {
                    // Params; an internal define's own name binds locally too.
                    Kind::List { .. } => {
                        let names = &second.children;
                        if depth > 0 {
                            add_atoms(&names[..names.len().min(1)], &mut out);
                        }
                        add_atoms(names.get(1..).unwrap_or_default(), &mut out);
                    }
                    Kind::Atom if depth > 0 => add_atoms(std::slice::from_ref(second), &mut out),
                    _ => {}
                },
                "lambda" | "λ" => match second.kind {
                    Kind::List { .. } => add_atoms(&second.children, &mut out),
                    _ => add_atoms(std::slice::from_ref(second), &mut out),
                },
                "let" | "let*" | "letrec" | "letrec*" | "let-values" => {
                    let bindings = match second.kind {
                        // Named let: the name, then the bindings.
                        Kind::Atom => {
                            add_atoms(std::slice::from_ref(second), &mut out);
                            rest.first()
                        }
                        _ => Some(second),
                    };
                    for binding in bindings.map(|b| b.children.as_slice()).unwrap_or_default() {
                        add_atoms(&binding.children[..binding.children.len().min(1)], &mut out);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Whether `name` is a valid identifier to rename to.
fn valid_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(is_ident_char)
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// String arguments of `(include "…")`, `(require "…")` and `(load "…")` forms,
/// resolved against `dir`.
fn referenced_files(text: &str, dir: &Path) -> Vec<PathBuf> {
    let parsed = sexp::parse(text);
    let mut out = Vec::new();
    for node in &parsed.nodes {
        let [head, rest @ ..] = node.children.as_slice() else {
            continue;
        };
        if !matches!(&text[head.start..head.end], "include" | "require" | "load") {
            continue;
        }
        for arg in rest {
            if let Kind::Str { closed: true } = arg.kind {
                let raw = &text[arg.start + 1..arg.end - 1];
                out.push(dir.join(raw));
            }
        }
    }
    out
}

impl SteelServer {
    /// Build the server (on its own thread — `Engine` is `!Send`).
    ///
    /// `seed_editor_api` distinguishes the two Scheme servers: the `macros-scheme`
    /// server (config / extension code) seeds the whole editor API — the
    /// registered commands plus every name the bundled Scheme library defines —
    /// so editor functions complete and aren't flagged "unknown". The plain
    /// `scheme` server seeds neither: it's a vanilla Steel environment, "just the
    /// interpreter", and references to editor functions are correctly unknown.
    fn new(seed_editor_api: bool, known_globals: &[String], tx: UnboundedSender<Value>) -> Self {
        let mut engine = Engine::new();
        let mut defs = HashMap::new();
        if seed_editor_api {
            for name in known_globals {
                engine.register_value(name, SteelVal::Void);
            }
            // Also seed the names the bundled Scheme library defines (lsp-language,
            // evil-define-key, register-command, …) so user configs that call them
            // aren't flagged "unknown function".
            for name in crate::runtime::defined_names() {
                engine.register_value(&name, SteelVal::Void);
            }
            // …and their docs + source locations, so go-to-definition can jump to
            // the bundled implementation of an editor command referenced by name,
            // and hover can show its doc-comment.
            for d in crate::runtime::builtin_docs() {
                defs.insert(d.name, (d.doc, d.file, d.line));
            }
        }
        let server = Self {
            tx,
            #[cfg(test)]
            next_id: 1,
            docs: HashMap::new(),
            engine,
            defs,
        };
        // textDocumentSync = 1 (full): the editor resends the whole document on
        // every change, which is free in-process and keeps this server simple.
        // `prepareProvider` makes the editor validate a rename target first, so we
        // can decline (in `prepare_rename`) anything not defined in this document.
        let _ = server.tx.unbounded_send(jsonrpc::response(
            0,
            json!({ "capabilities": {
                "textDocumentSync": 1,
                "completionProvider": {},
                "hoverProvider": true,
                "definitionProvider": true,
                "referencesProvider": true,
                "documentSymbolProvider": true,
                "signatureHelpProvider": { "triggerCharacters": [" "] },
                "renameProvider": { "prepareProvider": true }
            } }),
        ));
        server
    }

    /// A server plus its message receiver, run on the caller's thread (tests).
    #[cfg(test)]
    fn start(seed_editor_api: bool, known_globals: &[String]) -> (Self, UnboundedReceiver<Value>) {
        let (tx, rx) = unbounded();
        (Self::new(seed_editor_api, known_globals, tx), rx)
    }

    /// An open document's text (`""` for one we don't have).
    fn doc(&self, uri: &str) -> &str {
        self.docs.get(uri).map_or("", String::as_str)
    }

    /// Store a document's new full text and publish its diagnostics (tests;
    /// the server thread batches these).
    #[cfg(test)]
    fn set_document(&mut self, uri: &str, text: &str) {
        self.docs.insert(uri.to_string(), text.to_string());
        self.publish_diagnostics(uri);
    }

    /// Compute and publish diagnostics for an open document.
    fn publish_diagnostics(&mut self, uri: &str) {
        let Some(text) = self.docs.get(uri).cloned() else {
            return;
        };
        // Steel resolves the program's path on disk, so a file that doesn't
        // exist yet (a new, unsaved buffer) compiles path-less.
        let path = uri
            .starts_with("file://")
            .then(|| PathBuf::from(uri_to_path(uri)))
            .filter(|p| p.is_file());
        let diagnostics = self.compute_diagnostics(&text, path.as_deref());
        let _ = self.tx.unbounded_send(jsonrpc::notification(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": diagnostics }),
        ));
    }

    /// Queue a response carrying `result` under request `id`.
    fn reply(&self, id: i64, result: Value) {
        let _ = self.tx.unbounded_send(jsonrpc::response(id, result));
    }

    /// A fresh id, for requests made directly on the server (tests).
    #[cfg(test)]
    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Diagnostics in layers: (1) delimiter/string balance (precise spots);
    /// (2) the Steel compiler for structural/semantic errors — trusted files
    /// only (see [`crate::runtime::is_trusted_scheme_path`]), since compiling runs
    /// `defmacro` bodies and so would execute code from any file just opened;
    /// (3) when the file otherwise compiles clean, a conservative
    /// unbound-operator scan over the AST (likely typos — reported as warnings).
    fn compute_diagnostics(&mut self, text: &str, path: Option<&Path>) -> Vec<Value> {
        let balance = steel_diagnostics(text);
        if !balance.is_empty() {
            return balance;
        }
        let trusted = path.is_some_and(crate::runtime::is_trusted_scheme_path);
        let compiled = if trusted {
            self.try_compile(text, path)
        } else {
            CompileResult::Ok
        };
        let mut diagnostics = match compiled {
            CompileResult::Ok => Vec::new(),
            CompileResult::Error(err) => vec![self.steel_error_diagnostic(text, path, &err)],
            // Steel's compiler has panicked on malformed-but-balanced input
            // (`(define)` did before steel-core 0.8.3), which is common while
            // typing. We catch it (see `try_compile`) and report nothing for
            // that transient state rather than crash the editor.
            CompileResult::Panicked => Vec::new(),
        };
        if diagnostics.is_empty() {
            let mut known = self.known_names();
            if let Some(dir) = path.and_then(Path::parent) {
                known.extend(names_in_referenced_files(text, dir));
            }
            let buf = crate::buffer::Buffer::new(text);
            for (name, start, end) in crate::analyze::unbound_operators(text, &known) {
                diagnostics.push(warning(
                    buf.byte_to_position(start),
                    buf.byte_to_position(end),
                    &format!("unknown function: {name}"),
                ));
            }
        }
        diagnostics
    }

    /// Every name the environment knows: special forms, the engine's globals,
    /// and its macros (`while`, `case-lambda`, `define/contract`, `->`, … are
    /// prelude macros, not globals).
    fn known_names(&self) -> HashSet<String> {
        let mut set: HashSet<String> = SPECIAL_FORMS.iter().map(|s| s.to_string()).collect();
        for global in self.engine.readable_globals(0) {
            set.insert(global.resolve().to_string());
        }
        for name in self.engine.in_scope_macros().keys() {
            set.insert(name.resolve().to_string());
        }
        set
    }

    /// Compile `text` for diagnostics only, sandboxed: a compiler panic is caught
    /// (it would otherwise take down the editor, since this runs in-process). The
    /// engine stays usable afterwards. The panic hook is silenced for the call so
    /// half-typed forms don't spam stderr. With a `path`, relative `require`s
    /// resolve against the file's directory.
    ///
    /// The compiler's macro table is restored afterwards: a `define-syntax` in
    /// one document (or a since-deleted one) must not keep expanding in later
    /// compiles of this or any other document.
    fn try_compile(&mut self, text: &str, path: Option<&Path>) -> CompileResult {
        let macros = self.engine.in_scope_macros().clone();
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match path {
            Some(p) => self
                .engine
                .emit_raw_program(text.to_string(), p.to_path_buf()),
            None => self.engine.emit_raw_program_no_path(text.to_string()),
        }));
        std::panic::set_hook(previous_hook);
        *self.engine.in_scope_macros_mut() = macros;
        match outcome {
            Ok(Ok(_)) => CompileResult::Ok,
            Ok(Err(err)) => CompileResult::Error(err),
            Err(_) => CompileResult::Panicked,
        }
    }

    /// Turn a compiler error into an LSP diagnostic, using its span when it
    /// points into this document. An error raised inside a required module
    /// carries that module's span, which would be meaningless offsets here: it
    /// is reported at the top of the file with the module named instead.
    fn steel_error_diagnostic(
        &self,
        text: &str,
        path: Option<&Path>,
        err: &steel::rerrs::SteelErr,
    ) -> Value {
        // SteelErr's Display is "Error: Kind: message"; drop the "Error: " prefix.
        let message = err.to_string();
        let message = message.strip_prefix("Error: ").unwrap_or(&message);
        let buf = crate::buffer::Buffer::new(text);
        let elsewhere = err.span().and_then(|span| {
            let other = self.engine.get_path_for_source_id(&span.source_id?)?;
            (Some(other.as_path()) != path).then_some(other)
        });
        match (err.span(), elsewhere) {
            (_, Some(other)) => diagnostic(
                (0, 0),
                (0, 1),
                &format!("in {}: {message}", other.display()),
            ),
            (Some(span), None) if (span.end as usize) <= text.len() => diagnostic(
                buf.byte_to_position(span.start as usize),
                buf.byte_to_position(span.end as usize),
                message,
            ),
            _ => diagnostic((0, 0), (0, 1), message),
        }
    }

    /// Completion candidates: the file's own identifiers, every binding and
    /// macro the engine knows, special forms, and names defined by referenced
    /// files — filtered by the prefix at point, with bindings in scope at the
    /// cursor ranked first.
    #[cfg(test)]
    fn complete(&self, text: &str, line: usize, character: usize) -> Vec<Value> {
        self.complete_in(text, None, line, character)
    }

    fn complete_in(
        &self,
        text: &str,
        dir: Option<&Path>,
        line: usize,
        character: usize,
    ) -> Vec<Value> {
        let prefix = prefix_at(text, line, character);
        let in_scope =
            crate::analyze::bindings_in_scope_at(text, lsp_to_byte(text, line, character));

        let mut candidates: HashSet<String> = collect_identifiers(text);
        candidates.extend(self.known_names());
        candidates.extend(in_scope.iter().cloned());
        if let Some(dir) = dir {
            candidates.extend(names_in_referenced_files(text, dir));
        }
        let functions: HashSet<String> = definitions(text)
            .into_iter()
            .filter(|d| d.function)
            .map(|d| d.name)
            .collect();

        let matching = |want_local: bool| {
            let mut v: Vec<String> = candidates
                .iter()
                .filter(|c| {
                    in_scope.contains(*c) == want_local && *c != &prefix && c.starts_with(&prefix)
                })
                .cloned()
                .collect();
            v.sort();
            v
        };
        // In-scope locals first, then everything else.
        matching(true)
            .into_iter()
            .chain(matching(false))
            // Generous cap: a large single namespace (e.g. the ~180 `magit-*`
            // defines) must not crowd stdlib bindings like `map` out of the
            // results. The completion popup limits its own visible rows.
            .take(500)
            .map(|c| {
                // LSP kinds: 14 keyword (special forms), 3 function, 6 variable.
                let kind = if SPECIAL_FORMS.contains(&c.as_str()) {
                    json!(14)
                } else if functions.contains(&c) {
                    json!(3)
                } else if in_scope.contains(&c) {
                    json!(6)
                } else {
                    Value::Null
                };
                json!({ "label": c, "insertText": c, "kind": kind })
            })
            .collect()
    }

    /// Answer a completion request.
    fn completion_request(&mut self, id: i64, uri: &str, line: usize, character: usize) {
        let dir = uri
            .starts_with("file://")
            .then(|| PathBuf::from(uri_to_path(uri)))
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let items = self.complete_in(self.doc(uri), dir.as_deref(), line, character);
        self.reply(id, json!(items));
    }

    /// Answer one of [`REQUEST_METHODS`] under `id` (`null` when it doesn't
    /// apply).
    fn handle_request(&mut self, id: i64, method: &str, params: &Value) {
        let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
        if method == "textDocument/documentSymbol" {
            let result = self.document_symbols(uri);
            self.reply(id, result);
            return;
        }
        let (Some(line), Some(character)) = (
            params["position"]["line"].as_u64().map(|v| v as usize),
            params["position"]["character"].as_u64().map(|v| v as usize),
        ) else {
            self.reply(id, Value::Null);
            return;
        };
        let result = match method {
            // declaration/type/impl all collapse to "the definition" here.
            "textDocument/definition"
            | "textDocument/declaration"
            | "textDocument/typeDefinition"
            | "textDocument/implementation" => self.definition(uri, line, character),
            "textDocument/references" => self.references(uri, line, character),
            "textDocument/hover" => self.hover(uri, line, character),
            "textDocument/signatureHelp" => self.signature_help(uri, line, character),
            "textDocument/prepareRename" => self.prepare_rename(uri, line, character),
            "textDocument/rename" => {
                let new_name = params["newName"].as_str().unwrap_or_default();
                self.rename(uri, line, character, new_name)
            }
            _ => Value::Null,
        };
        self.reply(id, result);
    }

    /// Go-to-definition: prefer a definition in the open document (so a config's
    /// own `define`/`define-command` and any overrides win), then the bundled doc
    /// index. A `'name` reference resolves like a bare one — the quote is a word
    /// boundary. A single `Location`, or `null` when unresolved.
    fn definition(&self, uri: &str, line: usize, character: usize) -> Value {
        let text = self.doc(uri);
        symbol_at(text, line, character)
            .and_then(|sym| self.locate_definition(text, uri, &sym))
            .unwrap_or(Value::Null)
    }

    fn locate_definition(&self, text: &str, uri: &str, sym: &str) -> Option<Value> {
        // 1) A definition in the open document (the user's own, or an override),
        // at any depth; a top-level one wins over an internal one.
        let defs = definitions(text);
        if let Some(d) = defs
            .iter()
            .find(|d| d.name == sym && d.top_level)
            .or_else(|| defs.iter().find(|d| d.name == sym))
        {
            return Some(range_location(text, uri, d.start, d.end));
        }
        // 2) The bundled library — only when its source is present on disk (a
        // checkout); an installed binary has no on-disk sources, so we decline.
        if let Some((_, file, ln)) = self.defs.get(sym)
            && Path::new(file).exists()
        {
            let uri = path_to_uri(Path::new(file));
            return Some(location_json(&uri, ln.saturating_sub(1), 0, sym));
        }
        None
    }

    /// Hover: the doc-comment for the symbol at point — the open document's own
    /// `;;` comment block if it defines the symbol, else the bundled editor-API
    /// doc, else the Steel environment's own docstring. `null` when nothing is
    /// documented.
    fn hover(&self, uri: &str, line: usize, character: usize) -> Value {
        let text = self.doc(uri);
        symbol_at(text, line, character)
            .and_then(|sym| self.doc_for(text, &sym))
            .map(|doc| json!({ "contents": { "kind": "plaintext", "value": doc } }))
            .unwrap_or(Value::Null)
    }

    fn doc_for(&self, text: &str, sym: &str) -> Option<String> {
        // The open document's own definition wins (so a config documents itself),
        // scraped by the same rules as the bundled library.
        let in_file = crate::runtime::scrape_docs(text, "")
            .into_iter()
            .find(|e| e.name == sym && !e.doc.is_empty())
            .map(|e| e.doc);
        in_file
            .or_else(|| {
                self.defs
                    .get(sym)
                    .map(|(doc, _, _)| doc.clone())
                    .filter(|d| !d.is_empty())
            })
            .or_else(|| {
                self.engine
                    .get_doc_for_identifier(sym)
                    .filter(|d| !d.trim().is_empty())
            })
    }

    /// Signature help: the first line of the documentation of the function
    /// whose arguments the cursor is in (by Steel's `@doc` convention that line
    /// is the call signature), with the argument index as `activeParameter`.
    fn signature_help(&self, uri: &str, line: usize, character: usize) -> Value {
        let text = self.doc(uri);
        let before = &text[..lsp_to_byte(text, line, character).min(text.len())];
        let Some(op) = sexp::enclosing_operator(before) else {
            return Value::Null;
        };
        let Some(doc) = self.doc_for(text, &op) else {
            return Value::Null;
        };
        let label = doc
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(&op)
            .to_string();
        json!({
            "signatures": [{ "label": label, "documentation": doc }],
            "activeSignature": 0,
            "activeParameter": active_argument(before),
        })
    }

    /// Document symbols: the top-level definitions, as `SymbolInformation`.
    fn document_symbols(&self, uri: &str) -> Value {
        let text = self.doc(uri);
        let symbols: Vec<Value> = definitions(text)
            .into_iter()
            .filter(|d| d.top_level)
            .map(|d| {
                // LSP SymbolKind: 12 function, 13 variable, 23 struct, 25 macro
                // (operator — the closest fit).
                let kind = match d.head.as_str() {
                    "struct" | "define-struct" | "define-record-type" => 23,
                    "define-syntax" => 25,
                    _ if d.function => 12,
                    _ => 13,
                };
                json!({
                    "name": d.name,
                    "kind": kind,
                    "location": range_location(text, uri, d.start, d.end),
                })
            })
            .collect();
        json!(symbols)
    }

    /// References to the symbol at point within the open document: reader-aware
    /// (never inside strings or comments; quoted `'name` references count) and
    /// scope-checked (a local binding of the same name shadows a top-level one).
    /// Cross-file references aren't tracked.
    fn references(&self, uri: &str, line: usize, character: usize) -> Value {
        let text = self.doc(uri);
        let locs: Vec<Value> = symbol_at(text, line, character)
            .map(|sym| {
                scoped_occurrences(text, &sym)
                    .into_iter()
                    .map(|(s, e)| range_location(text, uri, s, e))
                    .collect()
            })
            .unwrap_or_default();
        json!(locs)
    }

    /// prepareRename: allow renaming only a symbol **defined in this document**
    /// (so a config renames its own commands, never a bundled or external one we
    /// can't fully update). The token's range, or `null` to decline.
    fn prepare_rename(&self, uri: &str, line: usize, character: usize) -> Value {
        let text = self.doc(uri);
        symbol_at_span(text, line, character)
            .filter(|(sym, _, _)| defined_in_doc(text, sym))
            .map(|(_, s, e)| {
                json!({
                    "start": { "line": line, "character": s },
                    "end": { "line": line, "character": e }
                })
            })
            .unwrap_or(Value::Null)
    }

    /// rename: a `WorkspaceEdit` replacing the symbol's scoped references (see
    /// [`Self::references`]) within this document. Only for symbols it defines —
    /// `prepare_rename` already gated that, and this re-checks so a direct rename
    /// can't escape the guard — and only to a valid identifier.
    fn rename(&self, uri: &str, line: usize, character: usize, new_name: &str) -> Value {
        let text = self.doc(uri);
        let mut edits: Vec<Value> = Vec::new();
        if let Some(sym) = symbol_at(text, line, character)
            && defined_in_doc(text, &sym)
            && valid_identifier(new_name)
        {
            let buf = crate::buffer::Buffer::new(text);
            for (s, e) in scoped_occurrences(text, &sym) {
                let (sl, sc) = buf.byte_to_position(s);
                let (el, ec) = buf.byte_to_position(e);
                edits.push(json!({
                    "range": {
                        "start": { "line": sl, "character": sc },
                        "end": { "line": el, "character": ec }
                    },
                    "newText": new_name
                }));
            }
        }
        let mut changes = serde_json::Map::new();
        changes.insert(uri.to_string(), Value::Array(edits));
        json!({ "changes": changes })
    }
}

/// Driving the server directly, on the caller's thread (tests).
#[cfg(test)]
impl LanguageServer for SteelServer {
    fn did_open(&mut self, uri: &str, _language_id: &str, _version: i32, text: &str) {
        self.set_document(uri, text);
    }

    fn did_change(&mut self, uri: &str, _version: i32, text: &str) {
        self.set_document(uri, text);
    }

    fn completion(&mut self, uri: &str, line: usize, character: usize) -> i64 {
        let id = self.alloc_id();
        self.completion_request(id, uri, line, character);
        id
    }

    fn request(&mut self, method: &str, params: Value) -> Option<i64> {
        if !REQUEST_METHODS.contains(&method) {
            return None;
        }
        let id = self.alloc_id();
        self.handle_request(id, method, &params);
        Some(id)
    }

    fn did_close(&mut self, uri: &str) {
        self.docs.remove(uri);
    }
}

/// Names defined by the files `text` includes / requires / loads (relative to
/// `dir`), so calls into them aren't "unknown" and they complete. One level
/// deep; unreadable or huge files are skipped.
fn names_in_referenced_files(text: &str, dir: &Path) -> HashSet<String> {
    const MAX_BYTES: u64 = 1024 * 1024;
    referenced_files(text, dir)
        .into_iter()
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= MAX_BYTES))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|src| definitions(&src).into_iter().map(|d| d.name))
        .collect()
}

/// Which argument of the innermost open call the end of `before` is in (0 =
/// the first argument), for `activeParameter`.
fn active_argument(before: &str) -> usize {
    let parsed = sexp::parse(before);
    let mut nodes = &parsed.nodes;
    let mut innermost: Option<&Node> = None;
    while let Some(last) = nodes.last().filter(|n| !n.is_complete()) {
        innermost = Some(last);
        nodes = &last.children;
    }
    let Some(list) = innermost else {
        return 0;
    };
    // Children after the head; a datum still being typed is the current one.
    let args = list.children.len().saturating_sub(1);
    let typing = list
        .children
        .last()
        .is_some_and(|c| c.end == before.len() && list.children.len() > 1);
    if typing { args - 1 } else { args }
}

/// An LSP `Location` for the byte range `start..end` of `text`.
fn range_location(text: &str, uri: &str, start: usize, end: usize) -> Value {
    let buf = crate::buffer::Buffer::new(text);
    let (sl, sc) = buf.byte_to_position(start);
    let (el, ec) = buf.byte_to_position(end);
    json!({
        "uri": uri,
        "range": {
            "start": { "line": sl, "character": sc },
            "end": { "line": el, "character": ec }
        }
    })
}

/// Whether `text` defines `sym` (at any depth). Gates rename to a document's
/// own symbols.
fn defined_in_doc(text: &str, sym: &str) -> bool {
    definitions(text).iter().any(|d| d.name == sym)
}

/// The identifier covering `(line, character)` (UTF-16 column), or `None`.
/// Extends over identifier chars in both directions; a leading `'` is a boundary,
/// so a `'name` reference yields `name`.
fn symbol_at(text: &str, line: usize, character: usize) -> Option<String> {
    symbol_at_span(text, line, character).map(|(name, _, _)| name)
}

/// Like [`symbol_at`] but also returns the token's `(start, end)` UTF-16 columns
/// on its line — used to report a rename range. Position/column conversion goes
/// through the canonical rope implementation in [`crate::buffer::Buffer`]; only
/// the identifier-boundary walk is local.
fn symbol_at_span(text: &str, line: usize, character: usize) -> Option<(String, usize, usize)> {
    let line_str = text.lines().nth(line)?;
    let buf = crate::buffer::Buffer::new(line_str);
    let idx = buf.position_to_byte(0, character);
    let mut start = idx.min(line_str.len());
    while let Some(c) = line_str[..start].chars().next_back() {
        if !is_ident_char(c) {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = idx.min(line_str.len());
    while let Some(c) = line_str[end..].chars().next() {
        if !is_ident_char(c) {
            break;
        }
        end += c.len_utf8();
    }
    if start >= end {
        return None;
    }
    Some((
        line_str[start..end].to_string(),
        buf.byte_to_position(start).1,
        buf.byte_to_position(end).1,
    ))
}

/// A one-line LSP `Location` JSON at `(line, character)` spanning `sym`.
fn location_json(uri: &str, line: usize, character: usize, sym: &str) -> Value {
    let len: usize = sym.chars().map(char::len_utf16).sum();
    json!({
        "uri": uri,
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character + len }
        }
    })
}

/// Map an LSP position `(line, utf16-col)` to a byte offset in `text`, via the
/// canonical rope conversion in [`crate::buffer::Buffer`]. For repeated
/// conversions over one document build the `Buffer` once instead.
fn lsp_to_byte(text: &str, line: usize, character: usize) -> usize {
    crate::buffer::Buffer::new(text).position_to_byte(line, character)
}

/// A warning-severity (2) LSP diagnostic — used for heuristic findings.
fn warning(start: (usize, usize), end: (usize, usize), message: &str) -> Value {
    diagnostic_with_severity(start, end, 2, message)
}

/// Scheme symbol-constituent characters (a superset of identifiers; good enough
/// for completion-prefix and symbol scraping).
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || "-_?!*+/<>=.:$%&~^".contains(c)
}

/// An LSP diagnostic object at the given LSP positions and severity.
fn diagnostic_with_severity(
    start: (usize, usize),
    end: (usize, usize),
    severity: u8,
    message: &str,
) -> Value {
    json!({
        "range": {
            "start": { "line": start.0, "character": start.1 },
            "end":   { "line": end.0,   "character": end.1 },
        },
        "severity": severity,
        "message": message,
    })
}

/// An error-severity (1) LSP diagnostic object at the given LSP positions.
fn diagnostic(start: (usize, usize), end: (usize, usize), message: &str) -> Value {
    diagnostic_with_severity(start, end, 1, message)
}

/// Balance-check Steel source: unmatched/mismatched `()[]{}`, unterminated
/// strings, and unterminated block comments. Aware of `;` line comments,
/// `"…"` strings (with `\` escapes), `#| … |#` (nestable) block comments,
/// `#;` datum comments, and `#\x` character literals.
fn steel_diagnostics(text: &str) -> Vec<Value> {
    let chars: Vec<char> = text.chars().collect();
    // LSP position (line, utf16-col) before each char, plus an end sentinel.
    let mut pos = Vec::with_capacity(chars.len() + 1);
    let (mut line, mut col) = (0usize, 0usize);
    for &c in &chars {
        pos.push((line, col));
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16();
        }
    }
    pos.push((line, col));

    let mut diagnostics = Vec::new();
    let mut stack: Vec<usize> = Vec::new(); // indices of open delimiters
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ';' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '"' => {
                let start = i;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    match chars[i] {
                        '\\' => i += 2,
                        '"' => {
                            i += 1;
                            closed = true;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                if !closed {
                    diagnostics.push(diagnostic(
                        pos[start],
                        pos[chars.len()],
                        "unterminated string",
                    ));
                }
            }
            '#' if i + 1 < chars.len() && chars[i + 1] == '\\' => {
                // Character literal `#\x` — skip `#`, `\`, and the char.
                i = (i + 3).min(chars.len());
            }
            '#' if i + 1 < chars.len() && chars[i + 1] == '|' => {
                let start = i;
                i += 2;
                let mut depth = 1;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '#' && i + 1 < chars.len() && chars[i + 1] == '|' {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '|' && i + 1 < chars.len() && chars[i + 1] == '#' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth > 0 {
                    diagnostics.push(diagnostic(
                        pos[start],
                        pos[chars.len()],
                        "unterminated block comment",
                    ));
                }
            }
            '#' if i + 1 < chars.len() && chars[i + 1] == ';' => i += 2, // datum comment marker
            '(' | '[' | '{' => {
                stack.push(i);
                i += 1;
            }
            ')' | ']' | '}' => {
                match stack.pop() {
                    Some(open) => {
                        let expected = match chars[open] {
                            '(' => ')',
                            '[' => ']',
                            _ => '}',
                        };
                        if c != expected {
                            diagnostics.push(diagnostic(
                                pos[i],
                                pos[i + 1],
                                &format!("mismatched '{c}', expected '{expected}'"),
                            ));
                        }
                    }
                    None => diagnostics.push(diagnostic(
                        pos[i],
                        pos[i + 1],
                        &format!("unmatched '{c}'"),
                    )),
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    for open in stack {
        let c = chars[open];
        diagnostics.push(diagnostic(
            pos[open],
            pos[open + 1],
            &format!("unmatched '{c}'"),
        ));
    }
    diagnostics
}

/// Scheme special forms — syntax, not values, so they aren't in the engine's
/// global table and must be offered for completion separately.
const SPECIAL_FORMS: &[&str] = &[
    "define",
    "lambda",
    "let",
    "let*",
    "letrec",
    "if",
    "cond",
    "case",
    "when",
    "unless",
    "begin",
    "set!",
    "quote",
    "quasiquote",
    "unquote",
    "and",
    "or",
    "define-syntax",
    "syntax-rules",
    "require",
    "provide",
    // Steel error handling: a form, not a readable global, so the analyzer's
    // vanilla engine doesn't surface it — list it so `(with-handler …)` (used in
    // the bundled library and configs) isn't flagged "unknown".
    "with-handler",
];

/// The identifier prefix immediately before the LSP position `(line, char)`.
/// The UTF-16 column maps to bytes via the canonical rope conversion.
fn prefix_at(text: &str, line: usize, character: usize) -> String {
    let line_str = text.lines().nth(line).unwrap_or("");
    let end = crate::buffer::Buffer::new(line_str).position_to_byte(0, character);
    let mut start = end;
    while let Some(c) = line_str[..start].chars().next_back() {
        if !is_ident_char(c) {
            break;
        }
        start -= c.len_utf8();
    }
    line_str[start..end].to_string()
}

/// Distinct symbols appearing in `text` as code — never words from strings or
/// comments — excluding numbers and other non-identifier atoms.
fn collect_identifiers(text: &str) -> HashSet<String> {
    let parsed = sexp::parse(text);
    let mut set = HashSet::new();
    let mut stack: Vec<&Node> = parsed.nodes.iter().collect();
    while let Some(node) = stack.pop() {
        match node.kind {
            Kind::Atom => {
                let name = atom_body(text, node).1;
                if valid_identifier(name) {
                    set.insert(name.to_string());
                }
            }
            Kind::List { .. } => stack.extend(node.children.iter()),
            Kind::Str { .. } => {}
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn steel_diagnostics_balance_checks() {
        // Well-formed source is clean.
        assert!(steel_diagnostics("(define (f x) (+ x 1))").is_empty());
        // Parens inside strings and comments don't count.
        assert!(steel_diagnostics(r#"(display "((")  ; ((("#).is_empty());

        // An unclosed form reports the unmatched opener at its position.
        let diags = steel_diagnostics("(define (f x)");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0]["range"]["start"],
            json!({ "line": 0, "character": 0 })
        );
        assert!(diags[0]["message"].as_str().unwrap().contains("unmatched"));

        // Mismatched delimiter kinds are flagged.
        let diags = steel_diagnostics("(]");
        assert_eq!(diags.len(), 1);
        assert!(diags[0]["message"].as_str().unwrap().contains("mismatched"));
    }

    #[test]
    fn steel_completion_is_environment_aware() {
        let (server, _rx) = SteelServer::start(true, &["set-lsp".to_string()]);

        // Buffer symbols complete, the prefix itself is excluded.
        let items = server.complete("(define foobar 1)\n(foo", 1, 4);
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(labels.contains(&"foobar"), "got {labels:?}");
        assert!(!labels.contains(&"foo"));

        // Special forms complete.
        let items = server.complete("(defi", 0, 5);
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(labels.contains(&"define"), "got {labels:?}");

        // Stdlib bindings the engine knows about complete (e.g. `map`).
        let items = server.complete("(ma", 0, 3);
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(
            labels.contains(&"map"),
            "expected stdlib `map`, got {labels:?}"
        );

        // A 'symbol command reference completes: the leading quote is a boundary,
        // so the prefix is the name and the candidate inserts after the quote.
        // This is the canonical form for keybindings/commands across the API.
        let src = "(define-key \"m\" \"c\" 'set-l";
        let items = server.complete(src, 0, src.chars().count());
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(
            labels.contains(&"set-lsp"),
            "quoted 'set-l should complete set-lsp, got {labels:?}"
        );
    }

    #[test]
    fn builtin_definition_and_references_resolve_symbols() {
        let (mut server, mut rx) = SteelServer::start(true, &[]);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // initialize

        let uri = "file:///cfg.scm";
        // A command, a quoted reference (keybinding), and a call.
        let text = "(define (my-cmd) 1)\n(bind-key \"m\" \"c\" 'my-cmd)\n(my-cmd)";
        server.set_document(uri, text);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // diagnostics

        // Go-to-definition from the *quoted* reference on line 1 jumps to the
        // define on line 0 — the quote is a word boundary, so symbols resolve.
        let col = text.lines().nth(1).unwrap().find("my-cmd").unwrap();
        let params = json!({"textDocument":{"uri":uri},"position":{"line":1,"character":col}});
        let id = server
            .request("textDocument/definition", params.clone())
            .unwrap();
        let resp = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(resp["id"], json!(id));
        assert_eq!(resp["result"]["uri"], json!(uri));
        assert_eq!(resp["result"]["range"]["start"]["line"], json!(0));

        // References include the define, the quoted use, and the call.
        let id = server
            .request("textDocument/references", params.clone())
            .unwrap();
        let resp = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(resp["id"], json!(id));
        let lines: Vec<i64> = resp["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["range"]["start"]["line"].as_i64().unwrap())
            .collect();
        assert!(lines.contains(&0), "define site missing: {lines:?}");
        assert!(
            lines.contains(&1),
            "quoted ref counts as a reference: {lines:?}"
        );
        assert!(lines.contains(&2), "call site missing: {lines:?}");
    }

    #[test]
    fn builtin_hover_shows_doc_comment() {
        let (mut server, mut rx) = SteelServer::start(true, &[]);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // initialize

        let uri = "file:///cfg.scm";
        // A documented command, referenced (quoted) from a keybinding.
        let text = ";; Greet the user.\n(define (greet) 1)\n(bind-key \"m\" \"c\" 'greet)";
        server.set_document(uri, text);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // diagnostics

        // Hover on the quoted reference shows the symbol's doc-comment.
        let col = text.lines().nth(2).unwrap().find("greet").unwrap();
        let params = json!({"textDocument":{"uri":uri},"position":{"line":2,"character":col}});
        let id = server
            .request("textDocument/hover", params.clone())
            .unwrap();
        let resp = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(resp["id"], json!(id));
        assert_eq!(
            resp["result"]["contents"]["value"],
            json!("Greet the user.")
        );
    }

    #[test]
    fn builtin_rename_scopes_to_document_defines() {
        let (mut server, mut rx) = SteelServer::start(true, &[]);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // initialize

        let uri = "file:///cfg.scm";
        let text = "(define (greet) 1)\n(bind-key \"m\" \"c\" 'greet)\n(greet)";
        server.set_document(uri, text);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // diagnostics

        let col = text.lines().nth(1).unwrap().find("greet").unwrap();
        let mut pos = json!({"textDocument":{"uri":uri},"position":{"line":1,"character":col}});

        // prepareRename accepts a symbol defined in this document.
        let id = server
            .request("textDocument/prepareRename", pos.clone())
            .unwrap();
        let resp = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(resp["id"], json!(id));
        assert!(
            !resp["result"].is_null(),
            "own symbol is renameable: {resp}"
        );

        // rename rewrites every occurrence — define, quoted reference, and call.
        pos["newName"] = json!("welcome");
        let id = server.request("textDocument/rename", pos.clone()).unwrap();
        let resp = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(resp["id"], json!(id));
        let edits = resp["result"]["changes"][uri].as_array().unwrap();
        assert_eq!(edits.len(), 3, "define + quoted + call: {edits:?}");
        assert!(edits.iter().all(|e| e["newText"] == json!("welcome")));

        // A symbol NOT defined here (a bundled command) is declined — we'd only
        // half-rename it.
        let text2 = "(bind-key \"m\" \"c\" 'magit-status)";
        server.set_document(uri, text2);
        let _ = futures::executor::block_on(rx.next()).unwrap(); // diagnostics
        let col = text2.find("magit-status").unwrap();
        let pos2 = json!({"textDocument":{"uri":uri},"position":{"line":0,"character":col}});
        let id = server
            .request("textDocument/prepareRename", pos2.clone())
            .unwrap();
        let resp = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(resp["id"], json!(id));
        assert!(
            resp["result"].is_null(),
            "a symbol defined elsewhere isn't renameable here: {resp}"
        );
    }

    #[test]
    fn steel_lsp_reports_semantic_errors() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        let _init = futures::executor::block_on(rx.next()).unwrap();

        // Balanced but malformed special form → a compiler diagnostic with a span
        // (trusted files only: the compiler runs macros).
        let uri = trusted_uri("semantic", "s.scm", "");
        server.set_document(&uri, "(if 1 2 3 4)");
        let msg = futures::executor::block_on(rx.next()).unwrap();
        let diags = msg["params"]["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1, "expected a semantic diagnostic: {msg}");
        assert!(
            diags[0]["message"].as_str().unwrap().contains("if"),
            "{msg}"
        );

        // An untrusted file never reaches the compiler, so no semantic error.
        server.set_document("file:///s.scm", "(if 1 2 3 4)");
        let msg = futures::executor::block_on(rx.next()).unwrap();
        assert!(
            msg["params"]["diagnostics"].as_array().unwrap().is_empty(),
            "untrusted files get syntax checks only: {msg}"
        );

        // Valid code using a stdlib function compiles clean.
        server.set_document(&uri, "(define (f x) (map (lambda (y) y) x))");
        let msg = futures::executor::block_on(rx.next()).unwrap();
        assert!(
            msg["params"]["diagnostics"].as_array().unwrap().is_empty(),
            "valid code should have no diagnostics: {msg}"
        );
    }

    #[test]
    fn steel_lsp_warns_on_unknown_function() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        let _init = futures::executor::block_on(rx.next()).unwrap();

        // Compiles clean, but calls an undefined function → a warning (severity 2).
        server.set_document("file:///u.scm", "(define (f x) (frobnicate x))");
        let msg = futures::executor::block_on(rx.next()).unwrap();
        let diags = msg["params"]["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1, "{msg}");
        assert_eq!(diags[0]["severity"], json!(2));
        assert!(diags[0]["message"].as_str().unwrap().contains("frobnicate"));

        // Defining it clears the warning (forward reference within the file).
        server.set_document(
            "file:///u.scm",
            "(define (f x) (frobnicate x))\n(define (frobnicate y) y)",
        );
        let msg = futures::executor::block_on(rx.next()).unwrap();
        assert!(
            msg["params"]["diagnostics"].as_array().unwrap().is_empty(),
            "{msg}"
        );
    }

    #[test]
    fn editor_api_is_scoped_to_the_macros_scheme_server() {
        // The split that gives `macros-steel` mode its reason to exist: a call to
        // an editor function is "unknown" on the plain `scheme` server (just the
        // interpreter) but in scope on the editor-aware `macros-scheme` server.
        let editor_call = "(define (f) (buffer-line))";

        // Plain server: editor API not seeded → unknown-function warning.
        let (mut plain, mut rx) = SteelServer::start(false, &[]);
        let _ = futures::executor::block_on(rx.next()).unwrap();
        plain.set_document("file:///plain.scm", editor_call);
        let msg = futures::executor::block_on(rx.next()).unwrap();
        let diags = msg["params"]["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1, "plain server should flag editor API: {msg}");
        assert!(
            diags[0]["message"]
                .as_str()
                .unwrap()
                .contains("buffer-line")
        );

        // Editor-aware server: `buffer-line` is a seeded global → clean.
        let (engine, _outcomes) = crate::script::ScriptEngine::new();
        let (mut aware, mut rx) = SteelServer::start(true, engine.global_names());
        let _ = futures::executor::block_on(rx.next()).unwrap();
        aware.set_document("file:///cfg.macros.scm", editor_call);
        let msg = futures::executor::block_on(rx.next()).unwrap();
        assert!(
            msg["params"]["diagnostics"].as_array().unwrap().is_empty(),
            "macros-scheme server should know the editor API: {msg}"
        );
    }

    #[test]
    fn steel_lsp_survives_compiler_panics() {
        // `(define)` is balanced but malformed. It panicked Steel's compiler
        // before steel-core 0.8.3 (caught by `try_compile`'s sandbox); it is
        // now a plain compile error. Either way: no crash, and the server must
        // keep working.
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        let _init = futures::executor::block_on(rx.next()).unwrap();

        let uri = trusted_uri("panics", "p.scm", "");
        server.set_document(&uri, "(define)");
        let msg = futures::executor::block_on(rx.next()).unwrap();
        assert!(msg["params"]["diagnostics"].is_array(), "{msg}");

        // Still alive: a subsequent real error is still reported.
        server.set_document(&uri, "(if 1 2 3 4)");
        let msg = futures::executor::block_on(rx.next()).unwrap();
        assert!(!msg["params"]["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn steel_lsp_drives_the_full_protocol() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);

        // 1. Handshake: an initialize response advertising full sync.
        let init = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(init["result"]["capabilities"]["textDocumentSync"], json!(1));

        // 2. didOpen on broken source publishes diagnostics for that URI.
        server.set_document("file:///x.scm", "(define (f");
        let published = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(
            published["method"],
            json!("textDocument/publishDiagnostics")
        );
        assert_eq!(published["params"]["uri"], json!("file:///x.scm"));
        assert!(
            !published["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        // 3. Fixing the source republishes an empty (cleared) diagnostic list.
        server.set_document("file:///x.scm", "(define (f x) x)");
        let cleared = futures::executor::block_on(rx.next()).unwrap();
        assert!(
            cleared["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        // 4. A completion request gets a matching-id response.
        let id = server.completion("file:///x.scm", 0, 9);
        let response = futures::executor::block_on(rx.next()).unwrap();
        assert_eq!(response["id"], json!(id));
        assert!(response["result"].is_array());
    }

    #[test]
    fn editor_scheme_files_have_no_false_positives() {
        // The editor's own config uses `set-lsp`, `define-key`, etc. With the
        // real registered globals, the analyzer must not flag them.
        let (engine, _outcomes) = crate::script::ScriptEngine::new();
        let (mut server, mut rx) = SteelServer::start(true, engine.global_names());
        let _init = futures::executor::block_on(rx.next()).unwrap();
        for src in [
            include_str!("../../scheme/init.scm"),
            include_str!("../../scheme/fundamental.scm"),
            include_str!("../../scheme/steel-mode.scm"),
        ] {
            server.set_document("file:///cfg.scm", src);
            let msg = futures::executor::block_on(rx.next()).unwrap();
            assert!(
                msg["params"]["diagnostics"].as_array().unwrap().is_empty(),
                "config should be clean: {msg}"
            );
        }
    }

    #[test]
    fn user_config_calling_library_functions_is_clean() {
        // A realistic personal config: calls Scheme-library functions defined in
        // other bundled files (lsp-language, evil-mode, evil-define-keys-for-modes)
        // — the analyzer must know them, not flag "unknown function".
        let (engine, _outcomes) = crate::script::ScriptEngine::new();
        let (mut server, mut rx) = SteelServer::start(true, engine.global_names());
        let _init = futures::executor::block_on(rx.next()).unwrap();
        let config = r#"
            (evil-mode)
            (lsp-language "rust"
              'extensions     '("rs")
              'command        "rust-analyzer"
              'format-on-save 'lsp
              'inlay-hints    #t)
            (evil-define-keys-for-modes "normal" '("rust")
              (list (cons "g g" lsp-find-definition)
                    (cons "g r" lsp-rename)))
            (define (leader keys cmd)
              (evil-bind-key "evil-normal" (string-append "space " keys) cmd))
            (leader "f f" find-file)
        "#;
        server.set_document("file:///init.scm", config);
        let msg = futures::executor::block_on(rx.next()).unwrap();
        let diags = msg["params"]["diagnostics"].as_array().unwrap();
        assert!(diags.is_empty(), "user config should be clean: {diags:?}");
    }

    /// Next message that answers request `id` (skipping notifications).
    fn response_for(rx: &mut UnboundedReceiver<Value>, id: i64) -> Value {
        loop {
            let msg = futures::executor::block_on(rx.next()).expect("server message");
            if msg["id"] == json!(id) {
                return msg["result"].clone();
            }
        }
    }

    /// Diagnostics from the next publish.
    fn next_diagnostics(rx: &mut UnboundedReceiver<Value>) -> Vec<Value> {
        loop {
            let msg = futures::executor::block_on(rx.next()).expect("server message");
            if msg["method"] == "textDocument/publishDiagnostics" {
                return msg["params"]["diagnostics"].as_array().unwrap().clone();
            }
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("macros-lsp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The URI of `file` written into a fresh scratch dir registered as a local
    /// package dir, so the server treats it as trusted and runs the compiler.
    fn trusted_uri(dir_name: &str, file: &str, text: &str) -> String {
        let dir = scratch_dir(dir_name);
        crate::runtime::register_local_scheme_dir(dir.canonicalize().unwrap());
        let path = dir.join(file);
        std::fs::write(&path, text).unwrap();
        path_to_uri(&path)
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_files_are_never_macro_expanded() {
        // Compiling expands `defmacro`, which runs its body — and Steel's
        // expansion kernel can spawn processes. A file the user merely opened
        // (a cloned repo, a download) must not get that far.
        let dir = scratch_dir("untrusted");
        let marker = dir.join("PWNED");
        let src = format!(
            "(defmacro (m stx) (begin (spawn-process (command \"touch\" (list {:?}))) (quote (define z 1))))\n(m)\n",
            marker.to_string_lossy()
        );
        let file = dir.join("evil.scm");
        std::fs::write(&file, &src).unwrap();
        for seed in [false, true] {
            let (mut server, mut rx) = SteelServer::start(seed, &[]);
            server.set_document(&path_to_uri(&file), &src);
            let _ = next_diagnostics(&mut rx);
            server.set_document("file:///unsaved.macros.scm", &src);
            let _ = next_diagnostics(&mut rx);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!marker.exists(), "opening an untrusted file ran its macro");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_require_resolves_against_the_file() {
        let dir = scratch_dir("require");
        std::fs::write(
            dir.join("util.scm"),
            "(provide helper)\n(define (helper x) x)\n",
        )
        .unwrap();
        let main = dir.join("main.scm");
        let src = "(require \"util.scm\")\n(helper 1)\n";
        std::fs::write(&main, src).unwrap();
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        server.set_document(&path_to_uri(&main), src);
        let diags = next_diagnostics(&mut rx);
        assert!(diags.is_empty(), "require should resolve: {diags:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prelude_macros_are_not_unknown_functions() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        for src in [
            "(define (f) (while #f 1))",
            "(define g (case-lambda ((a) a) ((a b) b)))",
        ] {
            server.set_document("file:///m.scm", src);
            let diags = next_diagnostics(&mut rx);
            assert!(diags.is_empty(), "{src}: {diags:?}");
        }
    }

    #[test]
    fn macros_do_not_leak_between_compiles() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        server.set_document(
            "file:///a.scm",
            "(define-syntax my-m (syntax-rules () ((_ x) x)))\n(my-m 1)",
        );
        let _ = next_diagnostics(&mut rx);
        // Another document using a macro it never defines: still unknown.
        server.set_document("file:///b.scm", "(define (f) (my-m 1))");
        let diags = next_diagnostics(&mut rx);
        assert!(
            diags
                .iter()
                .any(|d| d["message"].as_str().unwrap().contains("my-m")),
            "my-m must not survive from a.scm: {diags:?}"
        );
    }

    #[test]
    fn included_files_names_are_known() {
        let dir = scratch_dir("include");
        std::fs::write(dir.join("keys.scm"), "(define (my-key-helper) 1)\n").unwrap();
        let init = dir.join("init.scm");
        let src = "(include \"keys.scm\")\n(define (go) (my-key-helper))\n";
        std::fs::write(&init, src).unwrap();
        let (mut server, mut rx) = SteelServer::start(true, &[]);
        server.set_document(&path_to_uri(&init), src);
        let diags = next_diagnostics(&mut rx);
        assert!(
            !diags
                .iter()
                .any(|d| d["message"].as_str().unwrap().contains("my-key-helper")),
            "{diags:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_skips_strings_comments_and_shadowed_locals() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        let uri = "file:///r.scm";
        let text = "(define x 1)\n(display \"x marks\") ; x here\n(define (f x) x)\n(+ x 1)";
        server.set_document(uri, text);
        let params = |name: &str| json!({"textDocument":{"uri":uri},"position":{"line":0,"character":8},"newName":name});
        let id = server.request("textDocument/rename", params("y")).unwrap();
        let result = response_for(&mut rx, id);
        let edits = result["changes"][uri].as_array().unwrap();
        let lines: Vec<u64> = edits
            .iter()
            .map(|e| e["range"]["start"]["line"].as_u64().unwrap())
            .collect();
        // The define and the use on the last line; not the string, the comment,
        // or `f`'s parameter (which shadows it).
        assert_eq!(lines, vec![0, 3], "{edits:?}");
        // An invalid new name is refused.
        let id = server
            .request("textDocument/rename", params("bad name"))
            .unwrap();
        let result = response_for(&mut rx, id);
        assert!(result["changes"][uri].as_array().unwrap().is_empty());
    }

    #[test]
    fn definition_finds_internal_defines_and_symbols_list_top_level() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        let uri = "file:///d.scm";
        let text = "(define (outer)\n  (define (inner y) y)\n  (inner 1))\n(define v 2)";
        server.set_document(uri, text);
        let params = json!({"textDocument":{"uri":uri},"position":{"line":2,"character":4}});
        let id = server.request("textDocument/definition", params).unwrap();
        let loc = response_for(&mut rx, id);
        assert_eq!(loc["range"]["start"], json!({"line": 1, "character": 11}));
        let params = json!({"textDocument":{"uri":uri}});
        let id = server
            .request("textDocument/documentSymbol", params)
            .unwrap();
        let symbols = response_for(&mut rx, id);
        let names: Vec<&str> = symbols
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["outer", "v"]);
        assert_eq!(symbols[0]["kind"], json!(12));
        assert_eq!(symbols[1]["kind"], json!(13));
    }

    #[test]
    fn signature_help_names_the_enclosing_call() {
        let (mut server, mut rx) = SteelServer::start(false, &[]);
        let uri = "file:///s.scm";
        // A documented local function; cursor on its second argument.
        let text = ";; (add a b) — sum two numbers.\n(define (add a b) (+ a b))\n(add 1 ";
        server.set_document(uri, text);
        let params = json!({"textDocument":{"uri":uri},"position":{"line":2,"character":7}});
        let id = server
            .request("textDocument/signatureHelp", params)
            .unwrap();
        let help = response_for(&mut rx, id);
        assert!(
            help["signatures"][0]["label"]
                .as_str()
                .unwrap()
                .contains("add"),
            "{help}"
        );
        assert_eq!(help["activeParameter"], json!(1));
    }

    #[test]
    fn threaded_handle_answers_with_the_ids_it_returns() {
        let (mut lsp, mut rx) = SteelLsp::start(false, &[]);
        lsp.did_open("file:///t.scm", "scheme", 1, "(define foobar 1)\n(foo");
        let id = lsp.completion("file:///t.scm", 1, 4);
        let items = response_for(&mut rx, id);
        assert!(
            items
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["label"] == "foobar"),
            "{items}"
        );
        assert!(lsp.request("textDocument/formatting", json!({})).is_none());
    }
}
