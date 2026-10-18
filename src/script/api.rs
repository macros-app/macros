//! The package-API reference: every Rust primitive with its category, argument
//! and return types, and doc, plus the bundled Scheme library — rendered as
//! plain text for `(api-reference category search)` and the MCP `api` tool.
//!
//! Nothing here is hand-maintained. Categories come from `reg.section(…)` calls
//! at registration ([`super::Registrar`]), types from the registered closure's
//! own signature, and docs from the `//` comment block right above each
//! registration in the Rust source (the same "preceding comment is the doc"
//! rule [`crate::runtime::scrape_docs`] applies to `;;` in Scheme). Adding a
//! primitive therefore documents it, and the reference can't drift from the
//! code.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One registered Rust primitive.
#[derive(Debug, Clone)]
pub(crate) struct ApiEntry {
    pub(crate) name: String,
    pub(crate) section: Section,
    pub(crate) args: Vec<String>,
    pub(crate) ret: String,
}

impl ApiEntry {
    /// Build an entry from the `std::any::type_name`s of steel's `RegisterFn`
    /// marker (`Wrapper<(A, B)>`) and the closure's return type.
    pub(crate) fn new(name: &str, section: Section, args_ty: &str, ret_ty: &str) -> Self {
        ApiEntry {
            name: name.to_string(),
            section,
            args: wrapper_args(args_ty)
                .iter()
                .map(|t| friendly_type(t))
                .collect(),
            ret: friendly_type(ret_ty),
        }
    }

    /// An entry from explicit argument type names (a primitive whose closure
    /// takes raw `SteelVal`s and converts them itself).
    pub(crate) fn with_types(name: &str, section: Section, args: &[&str], ret_ty: &str) -> Self {
        ApiEntry {
            name: name.to_string(),
            section,
            args: args.iter().map(|t| friendly_type(t)).collect(),
            ret: friendly_type(ret_ty),
        }
    }
}

/// The API reference's categories. `reg.section(…)` files every following
/// registration under one ([`super::Registrar::section`]).
macro_rules! sections {
    ($($variant:ident = $name:literal, $blurb:literal;)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(crate) enum Section {
            $($variant,)*
        }

        impl Section {
            /// Every category, in display order.
            pub(crate) const ALL: &'static [Section] = &[$(Section::$variant,)*];

            /// The category's name, as `(api-reference category …)` takes it.
            pub(crate) fn name(self) -> &'static str {
                match self {
                    $(Section::$variant => $name,)*
                }
            }

            /// A one-line description for the category index.
            pub(crate) fn blurb(self) -> &'static str {
                match self {
                    $(Section::$variant => $blurb,)*
                }
            }
        }
    };
}

// Display order, with each category's one-line description.
sections! {
    Commands = "commands", "Nullary editor commands (motions, edits, UI toggles); call for effect";
    Editing = "editing", "Mutate the current buffer: insert, goto, regions, selections";
    BufferReads = "buffer-reads", "Read the current buffer: point, text, lines, regions";
    Motions = "motions", "Pure position math: compute a new byte offset without moving";
    Markers = "markers", "Positions that move with edits";
    Regexp = "regexp", "Regex over strings and over the buffer";
    Text = "text", "Pure string utilities: diffs, fuzzy match, quoting";
    Buffers = "buffers", "Buffers and windows: names, files, buffer-locals, window layout";
    SpecialBuffers = "special-buffers",
        "Read-only rendered buffers (magit-style) and their highlights";
    Overlays = "overlays", "Overlays, text properties, folds, line suffixes";
    Decorations = "decorations", "Per-line icons/images/indent/scale, ghost text, staleness guards";
    Faces = "faces", "Faces (colors, fonts, underline) and themes";
    Prompts = "prompts", "Messages, minibuffer reads, pickers (helm), y-or-n-p, transients";
    Grid = "grid", "Grid (CSV) buffers: read cells and state, edit, view, and style";
    Ui = "ui", "Mode line and titlebar";
    Keymaps = "keymaps", "Key bindings and the command list";
    Modal = "modal", "Evil/Helix state: marks, ex, counts, search query";
    HooksTimers = "hooks-timers", "Hooks and timers";
    Options = "options", "Editor options (get/set/list)";
    Help = "help", "Introspection: docs, versions, this API reference";
    Languages = "languages", "Bundled/installed tree-sitter grammars";
    TreeSitter = "tree-sitter", "Syntax-tree queries over the current buffer";
    Lsp = "lsp", "Language-server requests and diagnostics";
    Spell = "spell", "Native spell checking";
    Plotting = "plotting", "Text plotting: charts rendered to text + faces";
    Files = "files", "File system: read/write/list/watch, directories";
    Search = "search", "ripgrep-backed file walking and grep";
    Git = "git", "gix-backed git reads (log, diff, status, blame, refs)";
    Processes = "processes", "Subprocesses (sync and streaming) and the environment";
    Http = "http", "HTTP client (streaming), loopback HTTP server, open-url";
    Sockets = "sockets", "TCP/TLS/UDP sockets, async sockets, WebSockets, DNS";
    Sqlite = "sqlite", "Embedded SQLite (with FTS5)";
    Crypto = "crypto", "Random bytes, sha256, base64url/hex, charset decoding";
    TimeIds = "time-ids", "Timestamps and unique IDs";
    Dap = "dap", "Debug Adapter Protocol transport";
    Anvil = "anvil", "ANVIL: a second, independent Steel interpreter";
    Terminal = "terminal", "Embedded terminals";
    Session = "session", "Session persistence (savehist / desktop)";
    Secrets = "secrets", "The unlocked authinfo.gpg secrets store";
}

const TYPES_LEGEND: &str = "Types: string, int, number, bool, any (any Steel value), \
callback (a 'symbol or \"string\" naming a global function — not a lambda), \
list<T>, T|#f (may return #f), void (called for effect). Byte offsets are \
buffer bytes; positions are 0-based.";

/// The arguments inside steel's marker `…::Wrapper<(A, B)>` (or `Wrapper<()>`).
fn wrapper_args(ty: &str) -> Vec<String> {
    let Some(open) = ty.find('<') else {
        return Vec::new();
    };
    let inner = ty[open + 1..].strip_suffix('>').unwrap_or(&ty[open + 1..]);
    let inner = inner.trim();
    let inner = inner
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(inner);
    split_top_level(inner)
}

/// Split on commas not nested inside `<…>` / `(…)`, dropping empties (the
/// trailing comma of a 1-tuple).
fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '<' | '(' => depth += 1,
            '>' | ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A Rust type name (`alloc::vec::Vec<alloc::string::String>`) as a Scheme-ish
/// one (`list<string>`).
fn friendly_type(ty: &str) -> String {
    let ty = ty.trim();
    if ty == "()" {
        return "void".into();
    }
    if let Some(inner) = ty.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<String> = split_top_level(inner)
            .iter()
            .map(|t| friendly_type(t))
            .collect();
        return format!("({})", parts.join(" "));
    }
    let ty = ty.trim_start_matches('&').trim_start_matches("'static ");
    let (head, generics) = match ty.find('<') {
        Some(i) => (
            &ty[..i],
            split_top_level(ty[i + 1..].strip_suffix('>').unwrap_or("")),
        ),
        None => (ty, Vec::new()),
    };
    let base = head.rsplit("::").next().unwrap_or(head);
    let arg = |i: usize| {
        generics
            .get(i)
            .map(|t| friendly_type(t))
            .unwrap_or_default()
    };
    match base {
        "String" | "str" | "SteelString" | "Cow" | "Name" => "string".into(),
        "bool" => "bool".into(),
        "isize" | "usize" | "i64" | "u64" | "i32" | "u32" | "u16" | "u8" | "NonNeg"
        | "NonNeg64" => "int".into(),
        "Port" => "port".into(),
        "f64" | "f32" => "number".into(),
        "SteelVal" => "any".into(),
        "Callback" => "callback".into(),
        "StringList" => "list<string>".into(),
        "Vec" | "VecDeque" => format!("list<{}>", arg(0)),
        "Option" => format!("{}|#f", arg(0)),
        "Result" => arg(0),
        "HashMap" | "BTreeMap" => format!("hash<{} {}>", arg(0), arg(1)),
        other => other.to_string(),
    }
}

/// Rust sources whose registrations carry `//` doc comments, by path
/// (relative to `src/`). Every `script/prims/*.rs` must be listed — the
/// `doc_sources_cover_every_prims_file` test enforces it.
const DOC_SOURCES: &[(&str, &str)] = &[
    ("script/registry.rs", include_str!("registry.rs")),
    ("script/session.rs", include_str!("session.rs")),
    ("authinfo.rs", include_str!("../authinfo.rs")),
    ("script/prims/anvil.rs", include_str!("prims/anvil.rs")),
    (
        "script/prims/buffer_reads.rs",
        include_str!("prims/buffer_reads.rs"),
    ),
    ("script/prims/buffers.rs", include_str!("prims/buffers.rs")),
    ("script/prims/crypto.rs", include_str!("prims/crypto.rs")),
    ("script/prims/dap.rs", include_str!("prims/dap.rs")),
    (
        "script/prims/decorations.rs",
        include_str!("prims/decorations.rs"),
    ),
    ("script/prims/editing.rs", include_str!("prims/editing.rs")),
    ("script/prims/faces.rs", include_str!("prims/faces.rs")),
    ("script/prims/files.rs", include_str!("prims/files.rs")),
    ("script/prims/git.rs", include_str!("prims/git.rs")),
    ("script/prims/grid.rs", include_str!("prims/grid.rs")),
    ("script/prims/help.rs", include_str!("prims/help.rs")),
    ("script/prims/hooks.rs", include_str!("prims/hooks.rs")),
    ("script/prims/http.rs", include_str!("prims/http.rs")),
    ("script/prims/keymaps.rs", include_str!("prims/keymaps.rs")),
    (
        "script/prims/languages.rs",
        include_str!("prims/languages.rs"),
    ),
    ("script/prims/lsp.rs", include_str!("prims/lsp.rs")),
    ("script/prims/markers.rs", include_str!("prims/markers.rs")),
    ("script/prims/modal.rs", include_str!("prims/modal.rs")),
    ("script/prims/motions.rs", include_str!("prims/motions.rs")),
    ("script/prims/options.rs", include_str!("prims/options.rs")),
    (
        "script/prims/overlays.rs",
        include_str!("prims/overlays.rs"),
    ),
    (
        "script/prims/plotting.rs",
        include_str!("prims/plotting.rs"),
    ),
    ("script/prims/process.rs", include_str!("prims/process.rs")),
    ("script/prims/prompts.rs", include_str!("prims/prompts.rs")),
    ("script/prims/regexp.rs", include_str!("prims/regexp.rs")),
    ("script/prims/search.rs", include_str!("prims/search.rs")),
    ("script/prims/sockets.rs", include_str!("prims/sockets.rs")),
    (
        "script/prims/special_buffers.rs",
        include_str!("prims/special_buffers.rs"),
    ),
    ("script/prims/spell.rs", include_str!("prims/spell.rs")),
    ("script/prims/sqlite.rs", include_str!("prims/sqlite.rs")),
    ("script/prims/text.rs", include_str!("prims/text.rs")),
    ("script/prims/time.rs", include_str!("prims/time.rs")),
    (
        "script/prims/tree_sitter.rs",
        include_str!("prims/tree_sitter.rs"),
    ),
    ("script/prims/ui.rs", include_str!("prims/ui.rs")),
];

/// name → the `//` comment block immediately above its registration.
fn rust_docs() -> &'static HashMap<String, String> {
    static DOCS: OnceLock<HashMap<String, String>> = OnceLock::new();
    DOCS.get_or_init(|| {
        let mut docs = HashMap::new();
        for (_, src) in DOC_SOURCES {
            scrape_rust_docs(src, &mut docs);
        }
        docs
    })
}

/// Pair each `register…("name"` with the `//` block above it. The block
/// survives setup lines (`{`, `let q = queue.clone();`) between it and the
/// call; a blank line, a closing `}`, a `// ---` section rule, or a consumed
/// registration ends it. The first doc for a name wins.
pub(crate) fn scrape_rust_docs(src: &str, docs: &mut HashMap<String, String>) {
    let mut comment: Vec<&str> = Vec::new();
    let mut pending = false; // a `register…(` whose name is on the next line
    // Inside a multi-line `for … in [` table: its doc.
    let mut table: Option<String> = None;
    for line in src.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("//") {
            if rest.trim_start().starts_with("---") {
                comment.clear();
            } else {
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                comment.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
            continue;
        }
        if t.is_empty() || t.starts_with('}') {
            comment.clear();
            pending = false;
            continue;
        }
        // `for (name, …) in [("a", …), ("b", …)] {` registering in a loop:
        // the block documents every name in the table, which may run over
        // several lines (until the line that closes it with `]`).
        if t.starts_with("for ") && t.contains("in [") || table.is_some() {
            let doc = table
                .take()
                .unwrap_or_else(|| comment.join("\n").trim().to_string());
            for name in t.split("(\"").skip(1).filter_map(|r| r.split('"').next()) {
                if !doc.is_empty() {
                    docs.entry(name.to_string()).or_insert_with(|| doc.clone());
                }
            }
            if !t.contains(']') {
                table = Some(doc);
            }
            comment.clear();
            continue;
        }
        let is_call = t.contains("register") && t.contains('(') && !t.starts_with("fn ");
        if is_call || pending {
            match first_literal(t) {
                Some(name) => {
                    let doc = comment.join("\n").trim().to_string();
                    if !doc.is_empty() {
                        docs.entry(name.to_string()).or_insert(doc);
                    }
                    comment.clear();
                    pending = false;
                }
                None => pending = pending || t.ends_with('('),
            }
        }
    }
}

fn first_literal(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let len = line[start..].find('"')?;
    Some(&line[start..start + len])
}

/// The Scheme-library half: (module, signature, doc) for every bundled define.
fn scheme_entries() -> &'static Vec<(String, String, String)> {
    static ENTRIES: OnceLock<Vec<(String, String, String)>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        crate::runtime::builtin_docs()
            .into_iter()
            .map(|e| {
                let module = std::path::Path::new(&e.file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?");
                (format!("lib/{module}"), e.sig, e.doc)
            })
            .collect()
    })
}

fn rust_signature(e: &ApiEntry) -> String {
    let mut sig = format!("({}", e.name);
    for a in &e.args {
        sig.push(' ');
        sig.push_str(a);
    }
    sig.push(')');
    if e.ret != "void" {
        sig.push_str(" → ");
        sig.push_str(&e.ret);
    }
    sig
}

fn push_entry(out: &mut String, sig: &str, doc: &str) {
    out.push_str(sig);
    out.push('\n');
    for l in doc.lines() {
        out.push_str("    ");
        out.push_str(l);
        out.push('\n');
    }
}

/// Words all present (case-insensitively) in `hay` — `apropos` semantics.
fn matches(words: &[String], hay: &str) -> bool {
    let hay = hay.to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// The reference text. Empty `category` and `search` → the category index;
/// `category` → that category in full (a Rust section, or `lib/<module>`);
/// `search` → matching entries across everything (within `category`, if set).
pub(crate) fn render(entries: &[ApiEntry], category: &str, search: &str) -> String {
    let (category, search) = (category.trim(), search.trim());
    if category.is_empty() && search.is_empty() {
        return render_index(entries);
    }
    let words: Vec<String> = search.split_whitespace().map(str::to_lowercase).collect();
    let docs = rust_docs();
    let mut out = String::new();
    let mut hits = 0;
    let in_cat = |c: &str| category.is_empty() || c == category;

    let mut last = None;
    for e in entries.iter().filter(|e| in_cat(e.section.name())) {
        let doc = docs.get(&e.name).map(String::as_str).unwrap_or("");
        let sig = rust_signature(e);
        if !matches(&words, &format!("{sig} {doc}")) {
            continue;
        }
        if last != Some(e.section) {
            out.push_str(&format!("\n== {} (Rust primitives) ==\n", e.section.name()));
            last = Some(e.section);
        }
        push_entry(&mut out, &sig, doc);
        hits += 1;
    }
    let mut last = String::new();
    for (module, sig, doc) in scheme_entries().iter().filter(|(m, _, _)| in_cat(m)) {
        if !matches(&words, &format!("{sig} {doc}")) {
            continue;
        }
        if *module != last {
            out.push_str(&format!("\n== {module} (Scheme library) ==\n"));
            last = module.clone();
        }
        push_entry(&mut out, sig, doc);
        hits += 1;
    }
    if hits == 0 {
        let what = if category.is_empty() {
            format!("nothing matches {search:?}")
        } else if search.is_empty() {
            format!("no category {category:?}")
        } else {
            format!("nothing in {category:?} matches {search:?}")
        };
        return format!("{what}. Call with no arguments for the category index.");
    }
    format!("{}\n\n{TYPES_LEGEND}\n", out.trim())
}

fn render_index(entries: &[ApiEntry]) -> String {
    let mut counts: HashMap<Section, usize> = HashMap::new();
    for e in entries {
        *counts.entry(e.section).or_default() += 1;
    }
    let mut out = format!(
        "Macros package API: {} Rust primitives in {} categories, plus the bundled Scheme library.\n\
         Pass `category` for full signatures and docs, `search` (words) to search names and docs.\n\n\
         Rust primitives (the stable core, implemented in the editor):\n",
        entries.len(),
        counts.len()
    );
    let mut row = |name: &str, n: usize, blurb: &str| {
        out.push_str(&format!("  {name:<16} {n:>4}  {blurb}\n"));
    };
    for &section in Section::ALL {
        if let Some(&n) = counts.get(&section) {
            row(section.name(), n, section.blurb());
        }
    }

    out.push_str(
        "\nScheme library (bundled scheme/*.scm: higher-level helpers built on the \
         primitives; less stable, and internal helpers are included):\n",
    );
    let mut modules: Vec<(&str, usize)> = Vec::new();
    for (m, _, _) in scheme_entries() {
        match modules.last_mut() {
            Some((last, n)) if *last == m => *n += 1,
            _ => modules.push((m, 1)),
        }
    }
    let line: Vec<String> = modules.iter().map(|(m, n)| format!("{m} ({n})")).collect();
    out.push_str(&wrap_words(&line.join(", "), 88, "  "));
    out.push_str(&format!("\n\n{TYPES_LEGEND}\n"));
    out
}

/// Word-wrap `text` at `width`, prefixing every line with `indent`.
pub(crate) fn wrap_words(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    let mut line = String::from(indent);
    for w in text.split(' ') {
        if line.len() > indent.len() && line.len() + 1 + w.len() > width {
            out.push_str(line.trim_end());
            out.push('\n');
            line = String::from(indent);
        }
        if line.len() > indent.len() {
            line.push(' ');
        }
        line.push_str(w);
    }
    out.push_str(line.trim_end());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<A: 'static, R: 'static>(name: &str) -> ApiEntry {
        ApiEntry::new(
            name,
            Section::Faces,
            std::any::type_name::<steel::steel_vm::register_fn::Wrapper<A>>(),
            std::any::type_name::<R>(),
        )
    }

    #[test]
    fn types_render_scheme_side() {
        let e = entry::<(String, isize, crate::script::Callback), Vec<String>>("f");
        assert_eq!(e.args, ["string", "int", "callback"]);
        assert_eq!(e.ret, "list<string>");
        assert_eq!(rust_signature(&e), "(f string int callback) → list<string>");
        let e = entry::<(), ()>("g");
        assert!(e.args.is_empty());
        assert_eq!(rust_signature(&e), "(g)");
        let e = entry::<(steel::rvals::SteelVal,), Option<(usize, usize)>>("h");
        assert_eq!(e.args, ["any"]);
        assert_eq!(e.ret, "(int int)|#f");
        let e = entry::<(f64, bool), Result<HashMap<String, i64>, String>>("k");
        assert_eq!(e.args, ["number", "bool"]);
        assert_eq!(e.ret, "hash<string int>");
    }

    #[test]
    fn rust_docs_pair_comments_with_registrations() {
        let src = r#"
    // --- Section rule -----------
    // `(alpha x)` — the alpha doc.
    reg.register_fn("alpha", |x: String| x);
    {
        // Beta spans
        // two lines.
        let q = queue.clone();
        register_unary(
            &mut reg,
            queue,
            "beta",
            Cmd::Beta,
        );
    }
    reg.register_fn("gamma", || {
        // an inner comment, not gamma's doc
        do_it();
    });
    reg.register_fn("delta", || 1);

    // detached comment

    reg.register_fn("epsilon", || 2);
    // Both of zeta's
    for (name, full) in [("zeta", false), ("zeta-full", true)] {
        reg.register_fn(name, || 3);
    }
    // Eta's table spans lines
    for (name, n) in [
        ("eta", 1),
        ("eta-2", 2),
    ] {
        reg.register_fn(name, || 4);
    }
    reg.register_fn("theta", || 5);
"#;
        let mut docs = HashMap::new();
        scrape_rust_docs(src, &mut docs);
        assert_eq!(docs["alpha"], "`(alpha x)` — the alpha doc.");
        assert_eq!(docs["beta"], "Beta spans\ntwo lines.");
        assert!(!docs.contains_key("gamma"));
        assert!(!docs.contains_key("delta"));
        assert!(!docs.contains_key("epsilon"));
        assert_eq!(docs["zeta"], "Both of zeta's");
        assert_eq!(docs["zeta-full"], "Both of zeta's");
        assert_eq!(docs["eta"], "Eta's table spans lines");
        assert_eq!(docs["eta-2"], "Eta's table spans lines");
        assert!(!docs.contains_key("theta"), "the table ended");
    }

    #[test]
    fn real_registry_docs_are_found() {
        let docs = rust_docs();
        assert!(
            docs["set-face-font"].contains("font family"),
            "{:?}",
            docs.get("set-face-font")
        );
        assert!(docs["buffer-regex-matches"].contains("byte ranges"));
        for name in [
            "grid-sheets",
            "grid-current-sheet",
            "grid-open-sheet",
            "grid-set-cell-bold",
            "grid-set-cell-wrap",
            "grid-set-cell-text-color",
            "grid-set-cell-fill-color",
            "grid-clear-cell-formats",
            "grid-set-cell-number-format",
            "grid-set-number-format",
        ] {
            assert!(docs.contains_key(name), "no doc for {name}");
        }
        assert!(docs["grid-set-cell-fill-color"].contains("purple"));
    }

    /// A primitives file missing from `DOC_SOURCES` would silently lose every
    /// doc it carries from the API reference.
    #[test]
    fn doc_sources_cover_every_prims_file() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/script/prims");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name == "mod.rs" {
                continue;
            }
            let path = format!("script/prims/{name}");
            assert!(
                DOC_SOURCES.iter().any(|(p, _)| *p == path),
                "{path} is not in DOC_SOURCES"
            );
        }
    }

    #[test]
    fn render_index_category_and_search() {
        let entries = vec![
            entry::<(String,), ()>("set-face-font"),
            ApiEntry::new(
                "tcp-connect",
                Section::Sockets,
                "Wrapper<(String, u16)>",
                "usize",
            ),
        ];
        let idx = render(&entries, "", "");
        assert!(idx.contains("2 Rust primitives"));
        assert!(idx.contains("sockets"));
        assert!(idx.contains("lib/core"));

        let cat = render(&entries, "sockets", "");
        assert!(cat.contains("(tcp-connect string int) → int"), "{cat}");
        assert!(!cat.contains("set-face-font"));

        let found = render(&entries, "", "font family");
        assert!(found.contains("set-face-font"), "{found}");
        assert!(!found.contains("tcp-connect"));

        let lib = render(&entries, "lib/help", "apropos");
        assert!(lib.contains("(apropos-matches pat)"), "{lib}");

        assert!(render(&entries, "nope", "").starts_with("no category"));
    }
}
