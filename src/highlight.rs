use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use ropey::Rope;
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Language, Node, Parser, Point, Query, QueryCursor, Tree};
use tree_sitter_language::LanguageFn;

/// The vendored Mermaid grammar (`grammars/tree-sitter-mermaid`), compiled from
/// C by build.rs. No crate exposes Mermaid against tree-sitter 0.26+, so we link
/// its generated `tree_sitter_mermaid()` and wrap it as a `LanguageFn`, matching
/// the `LANGUAGE` const every other grammar crate provides.
mod mermaid {
    use tree_sitter_language::LanguageFn;

    unsafe extern "C" {
        fn tree_sitter_mermaid() -> *const ();
    }

    // SAFETY: `tree_sitter_mermaid` is the generated parser entry point that
    // build.rs compiles and links; it takes no arguments and returns a pointer
    // to a static `TSLanguage`, which is the contract `from_raw` requires.
    pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_mermaid) };
}

/// The vendored Racket grammar (`grammars/tree-sitter-racket`), compiled from C
/// by build.rs (parser + external scanner). No crate publishes Racket against
/// tree-sitter 0.26+, so we link its generated `tree_sitter_racket()` and wrap it
/// as a `LanguageFn`, matching the `LANGUAGE` const every grammar crate provides.
mod racket {
    use tree_sitter_language::LanguageFn;

    unsafe extern "C" {
        fn tree_sitter_racket() -> *const ();
    }

    // SAFETY: `tree_sitter_racket` is the generated parser entry point that
    // build.rs compiles and links; it takes no arguments and returns a pointer
    // to a static `TSLanguage`, which is the contract `from_raw` requires.
    pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_racket) };
}

/// The syntax faces captures resolve to. Each maps 1:1 to a theme face of the
/// same name ([`Face::name`]). Dotted captures in a grammar's query (e.g.
/// `constant.builtin`) resolve to their prefix (see `face_for_capture`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Face {
    Comment,
    String,
    Keyword,
    Function,
    Number,
    Constant,
    Type,
    Operator,
    Punctuation,
    Variable,
    /// Brackets/parens. Distinct from `Punctuation` so themes can render the
    /// structural delimiters a touch dimmer than ordinary punctuation. Fed by
    /// the `@punctuation.bracket` capture (see `face_for_capture`).
    Bracket,
}

impl Face {
    /// The theme face name.
    pub fn name(self) -> &'static str {
        match self {
            Face::Comment => "comment",
            Face::String => "string",
            Face::Keyword => "keyword",
            Face::Function => "function",
            Face::Number => "number",
            Face::Constant => "constant",
            Face::Type => "type",
            Face::Operator => "operator",
            Face::Punctuation => "punctuation",
            Face::Variable => "variable",
            Face::Bracket => "bracket",
        }
    }

    /// The face a capture's base name (`string` of `string.special`) names
    /// directly, if any.
    fn from_base(base: &str) -> Option<Face> {
        Some(match base {
            "comment" => Face::Comment,
            "string" => Face::String,
            "keyword" => Face::Keyword,
            "function" => Face::Function,
            "number" => Face::Number,
            "constant" => Face::Constant,
            "type" => Face::Type,
            "operator" => Face::Operator,
            "punctuation" => Face::Punctuation,
            "variable" => Face::Variable,
            "bracket" => Face::Bracket,
            _ => return None,
        })
    }
}

/// The theme face name for a highlight face.
pub fn face_name(face: Face) -> &'static str {
    face.name()
}

/// Everything bundled for one language, in one place: the grammar, its
/// highlight query, its optional indent query, the file extensions that map to
/// it, and the fenced-code-block tags that inject it. All the per-language
/// lookups ([`BUNDLED_LANGUAGES`], [`grammar`], [`indent_query`],
/// [`injection_lang_id`], [`language_for_extension`],
/// [`language_for_filename`]) derive from this table,
/// so adding a language is a single entry here (plus its grammar crate in
/// Cargo.toml, and its extensions in assets/file-types.txt so the OS offers
/// Macros under "Open With" — a test fails until they're listed).
struct LangSpec {
    id: &'static str,
    language: LanguageFn,
    highlights: &'static str,
    /// Structural-indent query (`indents.scm`), or `None` to fall back to
    /// copy-previous-line indentation (Lisps use a dedicated aligner instead).
    indent: Option<&'static str>,
    /// File extensions that open in this language (case-sensitive, no dot).
    extensions: &'static [&'static str],
    /// Exact basenames that open in this language — extensionless config
    /// files like Dune's `dune` / `dune-project`, which the extension column
    /// can never match. Checked before extensions in `language_for_path`.
    filenames: &'static [&'static str],
    /// Fenced-block info-string tags (lowercased) that inject this language
    /// into markdown. Empty for languages that are never injected (markdown
    /// itself is excluded so a nested ```markdown block can't recurse).
    injection_tags: &'static [&'static str],
}

const LANGUAGES: &[LangSpec] = &[
    LangSpec {
        id: "scheme",
        // A Steel-aware highlights query (richer than the crate's stock R7RS
        // `HIGHLIGHTS_QUERY`, which left most Steel code uncolored).
        language: tree_sitter_scheme::LANGUAGE,
        highlights: include_str!("../grammars/queries/scheme.scm"),
        indent: None,
        extensions: &["scm", "ss", "sld", "sls", "steel"],
        // Dune build files are plain s-expressions; the scheme grammar parses
        // them and the Lisp aligner indents them.
        filenames: &["dune", "dune-project", "dune-workspace"],
        injection_tags: &["scheme", "scm", "lisp"],
    },
    LangSpec {
        id: "rust",
        language: tree_sitter_rust::LANGUAGE,
        highlights: tree_sitter_rust::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/rust.scm")),
        extensions: &["rs"],
        filenames: &[],
        injection_tags: &["rust", "rs"],
    },
    LangSpec {
        id: "python",
        language: tree_sitter_python::LANGUAGE,
        highlights: tree_sitter_python::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/python.scm")),
        extensions: &["py", "pyi"],
        filenames: &[],
        injection_tags: &["python", "py"],
    },
    LangSpec {
        id: "javascript",
        // The crate's `HIGHLIGHT_QUERY` opens with blanket identifier/property
        // rules that out-rank every context rule in this engine, leaving
        // function names and calls uncolored. Ours is adapted to the engine
        // (the sibling of typescript.scm, minus the type layer, plus JSX).
        language: tree_sitter_javascript::LANGUAGE,
        highlights: include_str!("../grammars/queries/javascript.scm"),
        indent: Some(include_str!("../grammars/queries/indents/javascript.scm")),
        extensions: &["js", "jsx", "mjs", "cjs"],
        filenames: &[],
        injection_tags: &["javascript", "js", "jsx", "mjs"],
    },
    LangSpec {
        id: "json",
        language: tree_sitter_json::LANGUAGE,
        highlights: tree_sitter_json::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/json.scm")),
        extensions: &["json"],
        filenames: &[],
        injection_tags: &["json"],
    },
    LangSpec {
        id: "c",
        language: tree_sitter_c::LANGUAGE,
        highlights: tree_sitter_c::HIGHLIGHT_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/c.scm")),
        extensions: &["c", "h"],
        filenames: &[],
        injection_tags: &["c", "h"],
    },
    LangSpec {
        id: "cpp",
        language: tree_sitter_cpp::LANGUAGE,
        highlights: tree_sitter_cpp::HIGHLIGHT_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/cpp.scm")),
        extensions: &["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        filenames: &[],
        injection_tags: &["cpp", "c++", "cc", "cxx", "hpp"],
    },
    LangSpec {
        id: "go",
        language: tree_sitter_go::LANGUAGE,
        highlights: tree_sitter_go::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/go.scm")),
        extensions: &["go"],
        filenames: &[],
        injection_tags: &["go"],
    },
    LangSpec {
        id: "html",
        language: tree_sitter_html::LANGUAGE,
        highlights: tree_sitter_html::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/html.scm")),
        extensions: &["html", "htm"],
        filenames: &[],
        injection_tags: &["html"],
    },
    LangSpec {
        id: "css",
        language: tree_sitter_css::LANGUAGE,
        highlights: tree_sitter_css::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/css.scm")),
        extensions: &["css"],
        filenames: &[],
        injection_tags: &["css"],
    },
    LangSpec {
        id: "bash",
        language: tree_sitter_bash::LANGUAGE,
        highlights: tree_sitter_bash::HIGHLIGHT_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/bash.scm")),
        extensions: &["sh", "bash", "zsh"],
        filenames: &[],
        injection_tags: &["bash", "sh", "shell", "zsh", "console"],
    },
    LangSpec {
        id: "typescript",
        // The crate's `HIGHLIGHTS_QUERY` is only the TypeScript *addition* —
        // upstream layers it on the JavaScript query, so alone it left most
        // TS code uncolored. Ours is the full combined query.
        language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        highlights: include_str!("../grammars/queries/typescript.scm"),
        indent: Some(include_str!("../grammars/queries/indents/typescript.scm")),
        extensions: &["ts"],
        filenames: &[],
        injection_tags: &["typescript", "ts"],
    },
    LangSpec {
        id: "tsx",
        // The TypeScript base plus the JSX-only rules (those node kinds don't
        // exist in the plain TypeScript grammar, so they live in their own file).
        language: tree_sitter_typescript::LANGUAGE_TSX,
        highlights: concat!(
            include_str!("../grammars/queries/typescript.scm"),
            include_str!("../grammars/queries/tsx.scm"),
        ),
        indent: Some(include_str!("../grammars/queries/indents/tsx.scm")),
        extensions: &["tsx"],
        filenames: &[],
        injection_tags: &["tsx"],
    },
    LangSpec {
        id: "java",
        language: tree_sitter_java::LANGUAGE,
        highlights: tree_sitter_java::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/java.scm")),
        extensions: &["java"],
        filenames: &[],
        injection_tags: &["java"],
    },
    LangSpec {
        id: "ruby",
        language: tree_sitter_ruby::LANGUAGE,
        highlights: tree_sitter_ruby::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/ruby.scm")),
        extensions: &["rb"],
        filenames: &[],
        injection_tags: &["ruby", "rb"],
    },
    LangSpec {
        id: "lua",
        language: tree_sitter_lua::LANGUAGE,
        highlights: tree_sitter_lua::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/lua.scm")),
        extensions: &["lua"],
        filenames: &[],
        injection_tags: &["lua"],
    },
    LangSpec {
        id: "yaml",
        language: tree_sitter_yaml::LANGUAGE,
        highlights: tree_sitter_yaml::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/yaml.scm")),
        extensions: &["yaml", "yml"],
        filenames: &[],
        injection_tags: &["yaml", "yml"],
    },
    LangSpec {
        id: "toml",
        language: tree_sitter_toml_ng::LANGUAGE,
        highlights: tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        indent: None,
        extensions: &["toml"],
        filenames: &[],
        injection_tags: &["toml"],
    },
    LangSpec {
        id: "markdown",
        // Markdown is a split block/inline grammar; we highlight block structure
        // (headings, code fences, lists) with the block parser.
        language: tree_sitter_md::LANGUAGE,
        highlights: tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
        indent: None,
        extensions: &["md", "markdown"],
        filenames: &[],
        injection_tags: &[],
    },
    LangSpec {
        id: "zig",
        language: tree_sitter_zig::LANGUAGE,
        highlights: tree_sitter_zig::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/zig.scm")),
        extensions: &["zig"],
        filenames: &[],
        injection_tags: &["zig"],
    },
    LangSpec {
        id: "r",
        language: tree_sitter_r::LANGUAGE,
        highlights: tree_sitter_r::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/r.scm")),
        extensions: &["r", "R"],
        filenames: &[],
        injection_tags: &["r"],
    },
    LangSpec {
        id: "ocaml",
        language: tree_sitter_ocaml::LANGUAGE_OCAML,
        highlights: tree_sitter_ocaml::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/ocaml.scm")),
        extensions: &["ml", "mli"],
        filenames: &[],
        injection_tags: &["ocaml", "ml"],
    },
    LangSpec {
        id: "haskell",
        language: tree_sitter_haskell::LANGUAGE,
        highlights: tree_sitter_haskell::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/haskell.scm")),
        extensions: &["hs"],
        filenames: &[],
        injection_tags: &["haskell", "hs"],
    },
    LangSpec {
        id: "scala",
        language: tree_sitter_scala::LANGUAGE,
        highlights: tree_sitter_scala::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/scala.scm")),
        extensions: &["scala", "sc", "sbt"],
        filenames: &[],
        injection_tags: &["scala", "sc"],
    },
    LangSpec {
        id: "clojure",
        language: tree_sitter_clojure_orchard::LANGUAGE,
        highlights: tree_sitter_clojure_orchard::HIGHLIGHTS_QUERY,
        indent: None,
        extensions: &["clj", "cljs", "cljc", "edn"],
        filenames: &[],
        injection_tags: &["clojure", "clj", "cljs", "cljc", "edn"],
    },
    LangSpec {
        id: "sql",
        language: tree_sitter_sequel::LANGUAGE,
        highlights: tree_sitter_sequel::HIGHLIGHTS_QUERY,
        indent: None,
        extensions: &["sql"],
        filenames: &[],
        injection_tags: &["sql"],
    },
    LangSpec {
        id: "elixir",
        language: tree_sitter_elixir::LANGUAGE,
        highlights: tree_sitter_elixir::HIGHLIGHTS_QUERY,
        indent: Some(include_str!("../grammars/queries/indents/elixir.scm")),
        extensions: &["ex", "exs"],
        filenames: &[],
        injection_tags: &["elixir", "ex", "exs"],
    },
    LangSpec {
        id: "powershell",
        language: tree_sitter_powershell::LANGUAGE,
        highlights: tree_sitter_powershell::HIGHLIGHTS_QUERY,
        indent: None,
        extensions: &["ps1", "psm1", "psd1"],
        filenames: &[],
        injection_tags: &["powershell", "pwsh", "ps1"],
    },
    // LaTeX, Typst, and Mermaid bundle no highlights query with the parser,
    // so we vendor nvim-treesitter's under `grammars/queries/`. (LaTeX/Typst
    // parsers come from the `codebook-*` crates; Mermaid from build.rs.)
    LangSpec {
        id: "latex",
        language: codebook_tree_sitter_latex::LANGUAGE,
        highlights: include_str!("../grammars/queries/latex.scm"),
        indent: None,
        extensions: &["tex", "sty", "cls"],
        filenames: &[],
        injection_tags: &[],
    },
    LangSpec {
        id: "typst",
        language: codebook_tree_sitter_typst::LANGUAGE,
        highlights: include_str!("../grammars/queries/typst.scm"),
        indent: None,
        extensions: &["typ"],
        filenames: &[],
        injection_tags: &[],
    },
    LangSpec {
        id: "mermaid",
        language: mermaid::LANGUAGE,
        highlights: include_str!("../grammars/queries/mermaid.scm"),
        indent: None,
        extensions: &["mmd", "mermaid"],
        filenames: &[],
        injection_tags: &[],
    },
    // Racket has its own grammar (`.rkt`); Steel/Scheme files still use the
    // `scheme` grammar above. Query mirrors scheme.scm's classic-Lisp style.
    LangSpec {
        id: "racket",
        language: racket::LANGUAGE,
        highlights: include_str!("../grammars/queries/racket.scm"),
        indent: None,
        extensions: &["rkt"],
        filenames: &[],
        injection_tags: &["racket", "rkt"],
    },
];

/// The ids in [`LANGUAGES`], as a const array so [`BUNDLED_LANGUAGES`] keeps
/// its `&[&str]` shape without a second hand-maintained list.
const fn bundled_ids() -> [&'static str; LANGUAGES.len()] {
    let mut out = [""; LANGUAGES.len()];
    let mut i = 0;
    while i < LANGUAGES.len() {
        out[i] = LANGUAGES[i].id;
        i += 1;
    }
    out
}

/// Every language id with a bundled grammar — derived from [`LANGUAGES`]. The
/// grammar-load tests iterate it, and Scheme reads it via `(bundled-languages)`
/// to generate an `<id>-mode` M-x command per language (see language-modes.scm).
pub const BUNDLED_LANGUAGES: &[&str] = &bundled_ids();

/// The bundled language claiming file extension `ext` (case-sensitive, no
/// dot), if any. The extension side of `language_for_path`, which also
/// consults the runtime-installed grammar registry on a miss.
pub fn language_for_extension(ext: &str) -> Option<&'static str> {
    LANGUAGES
        .iter()
        .find(|spec| spec.extensions.contains(&ext))
        .map(|spec| spec.id)
}

/// The bundled language claiming exact file basename `name` (case-sensitive),
/// if any. The filename side of `language_for_path` — how extensionless config
/// files like `dune-project` get a language.
pub fn language_for_filename(name: &str) -> Option<&'static str> {
    LANGUAGES
        .iter()
        .find(|spec| spec.filenames.contains(&name))
        .map(|spec| spec.id)
}

/// A contiguous run of source bytes and the face (if any) to draw it with.
pub struct HlSpan {
    pub range: Range<usize>,
    pub face: Option<Face>,
}

/// The byte and point deltas of a single edit, for incremental reparsing.
/// Points are `(row, byte-column)` pairs, as tree-sitter expects.
pub struct Edit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: (usize, usize),
    pub old_end_point: (usize, usize),
    pub new_end_point: (usize, usize),
}

/// A tree-sitter highlighter that keeps a persistent parse tree.
///
/// Parsing reads directly from the rope in chunks (no whole-buffer string copy)
/// and reuses the previous tree, so an edit reparses incrementally. Highlight
/// spans aren't cached: [`spans_in`](Self::spans_in) runs the query over just a
/// byte range on demand, so the painter asks only for the visible region.
pub struct Highlighter {
    parser: Parser,
    query: Query,
    tree: Option<Tree>,
    /// The grammar, kept so scripts can compile ad-hoc queries against it
    /// (`query_captures`) without us re-resolving the language.
    language: Language,
    /// Maps a query capture index to a face index (`None` if unrecognized).
    capture_faces: Vec<Option<Face>>,
    /// Language injection (e.g. fenced code blocks in markdown): a query that
    /// finds `(language, content)` regions, plus a lazily-built, per-language
    /// cache of sub-highlighters that color those regions and a per-block span
    /// cache (keyed by block content) so paints don't reparse. `None` for grammars
    /// that don't inject. Interior-mutable so it can warm during `spans_in`.
    injection: Option<Injection>,
    /// Structural auto-indent query (the `@indent`/`@outdent`/`@extend` captures
    /// of a tree-sitter `indents.scm`). `None` for grammars with no indent query;
    /// the editor then falls back to copying the previous line's indentation.
    indent: Option<IndentQuery>,
}

/// What an indent-query capture says about a node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IndentKind {
    /// Indent the lines inside this node (a brace block, a bracketed list). Only
    /// counts while the node is genuinely open past the reference point.
    Indent,
    /// Like [`Indent`] but exempt from the same-row dedup — two `@indent.always`
    /// nodes opening on one line each add a level.
    IndentAlways,
    /// A closing token (`}`, `)`, `end`) whose own line dedents one level.
    Outdent,
    /// Indent lines that follow this node even when its body is still empty —
    /// for layout languages where the block (`if x:` … ) has no delimiter token,
    /// so [`Indent`]'s "open past the point" test can't fire on the first line.
    Extend,
}

/// A compiled indent query plus the indent kind of each capture index.
struct IndentQuery {
    query: Query,
    kinds: Vec<Option<IndentKind>>,
    /// Capture indices for `@align` (a node whose continuation lines align to a
    /// column) and its paired `@anchor` (the node whose column they align to).
    /// Helix-style column alignment — `None` when the query uses neither.
    align_idx: Option<u32>,
    anchor_idx: Option<u32>,
}

/// Detection + sub-highlighter cache for language injection.
struct Injection {
    query: Query,
    lang_capture: u32,
    content_capture: u32,
    cache: RefCell<HashMap<String, Option<Box<Highlighter>>>>,
    /// Highlighted spans of each injected block, keyed by its content, so a
    /// paint doesn't reparse every visible block from scratch.
    spans: RefCell<HashMap<InjectionKey, InjectedSpans>>,
}

/// `(language id, content hash, content length)` of one injected block.
type InjectionKey = (&'static str, u64, usize);

/// A block's spans relative to its content start, plus whether a paint used
/// it since the last buffer change (unused entries are pruned on the next).
struct InjectedSpans {
    spans: Vec<(Range<usize>, Face)>,
    used: bool,
}

impl Injection {
    /// The buffer changed: drop cached blocks no paint has asked for since the
    /// previous change (edited blocks hash to a new key, so their old entry is
    /// garbage), keeping on-screen untouched blocks warm.
    fn prune_spans(&self) {
        self.spans
            .borrow_mut()
            .retain(|_, e| std::mem::take(&mut e.used));
    }
}

/// Hash `range` of `rope` without copying it out.
fn hash_rope_range(rope: &Rope, range: Range<usize>) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for chunk in rope.byte_slice(range).chunks() {
        h.write(chunk.as_bytes());
    }
    h.finish()
}

impl Highlighter {
    /// Build a highlighter from a grammar's `LanguageFn` and highlight query, or
    /// `None` if the grammar/query won't load. This is grammar-agnostic: capture
    /// names map to faces by [`face_for_capture`], so any standard tree-sitter
    /// highlights query works.
    pub fn new(language_fn: LanguageFn, query: &str) -> Option<Self> {
        let language = Language::new(language_fn);
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        let query = Query::new(&language, query).ok()?;
        let capture_faces = query
            .capture_names()
            .iter()
            .map(|name| face_for_capture(name))
            .collect();
        Some(Self {
            parser,
            query,
            tree: None,
            language,
            capture_faces,
            injection: None,
            indent: None,
        })
    }

    /// Build a Scheme highlighter, or `None` if the grammar/query won't load.
    pub fn scheme() -> Option<Self> {
        Self::new(
            tree_sitter_scheme::LANGUAGE,
            include_str!("../grammars/queries/scheme.scm"),
        )
    }

    /// A highlighter for a language id, or `None` when nothing provides it.
    /// Bundled grammars ([`grammar`]) win; otherwise we fall back to a
    /// runtime-installed grammar from the [`crate::grammar`] registry (a user
    /// language macros downloaded and compiled). Wired from file extensions by
    /// `language_for_path` and the active mode's language.
    pub fn for_language(language: &str) -> Option<Self> {
        if let Some((lang, query)) = grammar(language) {
            let mut hl = Self::new(lang, query)?;
            if let Some(src) = injection_query(language) {
                hl.injection = build_injection(&hl.language, src);
            }
            if let Some(src) = indent_query(language) {
                hl.indent = build_indent(&hl.language, src);
            }
            return Some(hl);
        }
        // Runtime-installed grammar: owns its highlights query as a `String`.
        // No injection wiring — only the bundled markdown grammar injects. An
        // `indents.scm` in the grammar repo enables structural auto-indent.
        let (lang, query) = crate::grammar::installed(language)?;
        let mut hl = Self::new(lang, &query)?;
        if let Some(src) = crate::grammar::installed_indent(language) {
            hl.indent = build_indent(&hl.language, &src);
        }
        Some(hl)
    }
}

/// The injection-detection query for a grammar that embeds other languages, or
/// `None`. Markdown fenced code blocks carry a language tag and content we color
/// with that language's own grammar.
fn injection_query(language: &str) -> Option<&'static str> {
    match language {
        "markdown" => Some(
            "(fenced_code_block \
               (info_string (language) @injection.language) \
               (code_fence_content) @injection.content)",
        ),
        _ => None,
    }
}

/// Compile an injection query and locate its `@injection.language` /
/// `@injection.content` captures. `None` if the query won't compile or is
/// missing either capture.
fn build_injection(language: &Language, src: &str) -> Option<Injection> {
    let query = Query::new(language, src).ok()?;
    let names = query.capture_names();
    let idx = |want: &str| names.iter().position(|n| *n == want).map(|i| i as u32);
    Some(Injection {
        lang_capture: idx("injection.language")?,
        content_capture: idx("injection.content")?,
        query,
        cache: RefCell::new(HashMap::new()),
        spans: RefCell::new(HashMap::new()),
    })
}

/// The bundled structural-indent query (`indents.scm`) for a language id, or
/// `None`. Languages without one fall back to copy-previous-line indentation;
/// Lisps are handled by a dedicated aligner in the editor, not here. Derived
/// from the `indent` column of [`LANGUAGES`].
fn indent_query(language: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|spec| spec.id == language)?.indent
}

/// Compile an indent query and record the indent kind of each capture, or `None`
/// if it won't compile against this grammar (the language then falls back to
/// copy-previous-line indentation rather than breaking).
fn build_indent(language: &Language, src: &str) -> Option<IndentQuery> {
    let query = Query::new(language, src).ok()?;
    let kinds = query
        .capture_names()
        .iter()
        .map(|name| match *name {
            "indent" => Some(IndentKind::Indent),
            "indent.always" => Some(IndentKind::IndentAlways),
            "outdent" | "outdent.always" => Some(IndentKind::Outdent),
            "extend" => Some(IndentKind::Extend),
            _ => None,
        })
        .collect();
    let align_idx = query.capture_index_for_name("align");
    let anchor_idx = query.capture_index_for_name("anchor");
    Some(IndentQuery {
        query,
        kinds,
        align_idx,
        anchor_idx,
    })
}

/// Map a fenced-block info-string tag (`rust`, `rs`, `py`, `c++`, …) to a
/// bundled grammar id, or `None` if we don't bundle it. Derived from the
/// `injection_tags` column of [`LANGUAGES`]; markdown lists no tags so a
/// nested ```markdown block can't recurse.
fn injection_lang_id(tag: &str) -> Option<&'static str> {
    let tag = tag.trim().to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|spec| spec.injection_tags.contains(&tag.as_str()))
        .map(|spec| spec.id)
}

/// The bundled `(LanguageFn, highlights-query)` for a language id, if any.
/// Derived from [`LANGUAGES`] — adding a language is a single entry there plus
/// its grammar crate in Cargo.toml.
fn grammar(language: &str) -> Option<(LanguageFn, &'static str)> {
    LANGUAGES
        .iter()
        .find(|spec| spec.id == language)
        .map(|spec| (spec.language, spec.highlights))
}

impl Highlighter {
    /// Full parse from scratch — used on initial load and after undo, where the
    /// edit deltas aren't tracked.
    pub fn set_text(&mut self, rope: &Rope) {
        self.tree = parse(&mut self.parser, rope, None);
        if let Some(inj) = &self.injection {
            inj.prune_spans();
        }
    }

    /// Apply an edit to the current tree. Call this (with the pre-edit byte/point
    /// positions) before `reparse`.
    pub fn apply_edit(&mut self, edit: &Edit) {
        if let Some(tree) = self.tree.as_mut() {
            tree.edit(&InputEdit {
                start_byte: edit.start_byte,
                old_end_byte: edit.old_end_byte,
                new_end_byte: edit.new_end_byte,
                start_position: point(edit.start_point),
                old_end_position: point(edit.old_end_point),
                new_end_position: point(edit.new_end_point),
            });
        }
    }

    /// Incrementally reparse, reusing the edited tree.
    pub fn reparse(&mut self, rope: &Rope) {
        self.tree = parse(&mut self.parser, rope, self.tree.as_ref());
        if let Some(inj) = &self.injection {
            inj.prune_spans();
        }
    }

    /// Highlight spans tiling `range`, computed on demand by querying just that
    /// byte range of the current tree. Reading node text for query predicates
    /// goes straight to the rope.
    pub fn spans_in(&self, rope: &Rope, range: Range<usize>) -> Vec<HlSpan> {
        let Some(tree) = self.tree.as_ref() else {
            return Vec::new();
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());

        let provider = rope_text(rope);
        let mut captures: Vec<(usize, usize, Face)> = Vec::new();
        let mut matches = cursor.matches(&self.query, tree.root_node(), provider);
        while let Some(m) = matches.next() {
            for capture in m.captures() {
                if let Some(face) = self
                    .capture_faces
                    .get(capture.index as usize)
                    .copied()
                    .flatten()
                {
                    let node_range = capture.node.byte_range();
                    let start = node_range.start.max(range.start);
                    let end = node_range.end.min(range.end);
                    if end > start {
                        captures.push((start, end, face));
                    }
                }
            }
        }
        // Injected sub-language spans (e.g. a ```rust block). They nest inside
        // the markdown `text.literal` span covering the block, so `flatten`'s
        // innermost-wins sweep makes them take precedence over the base face.
        self.append_injected_spans(rope, &range, &mut captures);
        flatten(captures, range.start, range.end)
    }

    /// For each injected region (fenced code block) intersecting `range`, run the
    /// embedded language's highlighter over its content and push the resulting
    /// spans (offset into the buffer, clipped to `range`) onto `captures`. No-op
    /// unless this grammar injects. Only visible regions are parsed, so the cost
    /// scales with the viewport, not the whole file.
    fn append_injected_spans(
        &self,
        rope: &Rope,
        range: &Range<usize>,
        captures: &mut Vec<(usize, usize, Face)>,
    ) {
        let (Some(inj), Some(tree)) = (self.injection.as_ref(), self.tree.as_ref()) else {
            return;
        };
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());
        let provider = rope_text(rope);
        let mut cache = inj.cache.borrow_mut();
        let mut spans_cache = inj.spans.borrow_mut();
        let mut matches = cursor.matches(&inj.query, tree.root_node(), provider);
        while let Some(m) = matches.next() {
            let mut tag: Option<String> = None;
            let mut content: Option<Node> = None;
            for cap in m.captures() {
                if cap.index == inj.lang_capture {
                    tag = Some(rope.byte_slice(cap.node.byte_range()).to_string());
                } else if cap.index == inj.content_capture {
                    content = Some(cap.node);
                }
            }
            let (Some(tag), Some(content)) = (tag, content) else {
                continue;
            };
            let Some(lang_id) = injection_lang_id(&tag) else {
                continue;
            };
            let cr = content.byte_range();
            let cs = cr.start;
            let key = (lang_id, hash_rope_range(rope, cr.clone()), cr.len());
            let entry = match spans_cache.entry(key) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    // Lazily build (and cache) the sub-highlighter for this language.
                    let sub = cache
                        .entry(lang_id.to_string())
                        .or_insert_with(|| Highlighter::for_language(lang_id).map(Box::new));
                    let Some(sub) = sub.as_mut() else {
                        continue;
                    };
                    // Parse the whole block as a standalone document (so the
                    // sub-grammar never sees a truncated fragment); spans are
                    // stored relative to the block and reused until it changes.
                    let text: String = rope.byte_slice(cr.clone()).to_string();
                    let sub_rope = Rope::from_str(&text);
                    sub.set_text(&sub_rope);
                    let spans = sub
                        .spans_in(&sub_rope, 0..text.len())
                        .into_iter()
                        .filter_map(|s| s.face.map(|f| (s.range, f)))
                        .collect();
                    v.insert(InjectedSpans { spans, used: false })
                }
            };
            entry.used = true;
            // Shift to absolute offsets and clip to the requested range.
            // `set_byte_range` above already limited us to blocks intersecting
            // the viewport.
            for (r, face) in &entry.spans {
                let start = (r.start + cs).max(range.start);
                let end = (r.end + cs).min(range.end);
                if end > start {
                    captures.push((start, end, *face));
                }
            }
        }
    }

    /// The chain of nodes covering `byte`, innermost first, each as
    /// `(kind, start_byte, end_byte)`. Empty when there's no tree yet. Lets a
    /// script ask "what am I inside?" — e.g. skip a bracket that sits in a string
    /// or comment node.
    /// The current syntax tree (a cheap refcounted copy is published to
    /// Scheme for the synchronous `ts-*` reads).
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Whether this grammar carries a structural indent query.
    pub fn has_indent_query(&self) -> bool {
        self.indent.is_some()
    }

    pub fn node_at(&self, byte: usize) -> Vec<(String, usize, usize)> {
        self.tree
            .as_ref()
            .map_or_else(Vec::new, |tree| tree_ancestors(tree, byte))
    }

    /// The byte offset of the bracket matching the single-char delimiter token
    /// that begins at `byte` (`(`/`)`, `[`/`]`, `{`/`}`), found structurally via
    /// the parse tree — so a bracket inside a string or comment isn't a token
    /// and yields `None`. `None` when `byte` isn't on a bracket token or its
    /// partner can't be located. The editor falls back to a text scan.
    pub fn bracket_match(&self, byte: usize) -> Option<usize> {
        let tree = self.tree.as_ref()?;
        let node = tree.root_node().descendant_for_byte_range(byte, byte + 1)?;
        if node.start_byte() != byte {
            return None;
        }
        let partner = match node.kind() {
            "(" => ")",
            ")" => "(",
            "[" => "]",
            "]" => "[",
            "{" => "}",
            "}" => "{",
            _ => return None,
        };
        let opening = matches!(node.kind(), "(" | "[" | "{");
        let parent = node.parent()?;
        // The pair's other delimiter is a sibling token of the partner kind,
        // walking away from `byte` and counting nesting: a well-formed node has
        // just the one pair, but an ERROR node flattens every bracket it
        // swallowed into one sibling list (`{ … { }` mid-edit), where the
        // nearest partner token is not the match.
        let mut siblings = Vec::new();
        let mut i = 0;
        while let Some(child) = parent.child(i) {
            i += 1;
            siblings.push(child);
        }
        let at = siblings.iter().position(|c| c.start_byte() == byte)?;
        let walk: Box<dyn Iterator<Item = &Node>> = if opening {
            Box::new(siblings[at + 1..].iter())
        } else {
            Box::new(siblings[..at].iter().rev())
        };
        let mut depth = 0usize;
        for child in walk {
            if child.kind() == node.kind() {
                depth += 1;
            } else if child.kind() == partner {
                if depth == 0 {
                    return Some(child.start_byte());
                }
                depth -= 1;
            }
        }
        None
    }

    /// The structural indentation depth, in steps (each step = one `tab_width`),
    /// for a line. `anchor` is a byte sitting on the syntax the line should
    /// follow — for a fresh newline, the previous non-blank byte; for reindent,
    /// the line's own first non-blank byte — and the ancestor chain is walked
    /// from there. `ref_byte` is the line's logical start, against which an
    /// `@indent` scope is tested for being genuinely open. `target_row` is the
    /// line's display row. `outdent_byte`, when set, dedents one step if an
    /// `@outdent` node begins exactly there. Returns `None` when this grammar
    /// carries no indent query, so the caller can fall back.
    ///
    /// Splitting `anchor` from `ref_byte` is what lets a layout language indent
    /// the first line of an as-yet-empty body: anchoring on the header's colon
    /// finds the (still-open) compound statement that a raw position past the
    /// newline would have fallen outside of. `@indent` scopes still open past
    /// `ref_byte` add a step (deduped per start row, so `if x {` doesn't
    /// double-count the `if` and its block); `@extend` scopes add a step purely
    /// by being an open ancestor (no delimiter token needed); an `@outdent`
    /// token on the line removes one.
    pub fn indent_level(
        &self,
        rope: &Rope,
        anchor: usize,
        ref_byte: usize,
        target_row: usize,
        outdent_byte: Option<usize>,
    ) -> Option<usize> {
        let iq = self.indent.as_ref()?;
        let tree = self.tree.as_ref()?;
        let root = tree.root_node();

        // Bound the query to the enclosing top-level node so cost is one form's
        // worth, not the whole file, on every keystroke.
        let mut scope = root
            .descendant_for_byte_range(anchor, anchor)
            .unwrap_or(root);
        while let Some(parent) = scope.parent() {
            if parent == root {
                break;
            }
            scope = parent;
        }

        let provider = rope_text(rope);
        let mut cursor = QueryCursor::new();
        let mut kinds: HashMap<usize, IndentKind> = HashMap::new();
        let mut outdent_starts: Vec<usize> = Vec::new();
        let mut matches = cursor.matches(&iq.query, scope, provider);
        while let Some(m) = matches.next() {
            for cap in m.captures() {
                let Some(kind) = iq.kinds.get(cap.index as usize).copied().flatten() else {
                    continue;
                };
                if kind == IndentKind::Outdent {
                    outdent_starts.push(cap.node.start_byte());
                } else {
                    // Prefer Extend if a node carries both (empty-body case).
                    let slot = kinds.entry(cap.node.id()).or_insert(kind);
                    if kind == IndentKind::Extend {
                        *slot = kind;
                    }
                }
            }
        }

        let mut levels = 0usize;
        let mut used_rows: Vec<usize> = Vec::new();
        let mut node = root.descendant_for_byte_range(anchor, anchor);
        while let Some(n) = node {
            if let Some(kind) = kinds.get(&n.id()).copied() {
                let srow = n.start_position().row;
                let open_past = n.start_byte() < ref_byte && n.end_byte() > ref_byte;
                let qualifies = match kind {
                    IndentKind::Indent | IndentKind::IndentAlways => srow < target_row && open_past,
                    IndentKind::Extend => srow < target_row,
                    IndentKind::Outdent => false,
                };
                if qualifies && (kind == IndentKind::IndentAlways || !used_rows.contains(&srow)) {
                    levels += 1;
                    used_rows.push(srow);
                }
            }
            node = n.parent();
        }
        if let Some(ob) = outdent_byte
            && levels > 0
            && outdent_starts.contains(&ob)
        {
            levels -= 1;
        }
        Some(levels)
    }

    /// Column-alignment indent (Helix `@align`/`@anchor`): if the line at
    /// `target_row` sits inside an open `@align` node whose paired `@anchor` began
    /// on an earlier row, return the `@anchor`'s start column — so continuation
    /// lines align under a delimiter (e.g. call arguments under the first one)
    /// rather than stepping in by a fixed amount. `None` when no `@align` scope
    /// applies (the caller then uses the step-based [`indent_level`]). The column
    /// is a byte offset within the row — exact for the ASCII/space code this
    /// targets. `anchor`/`ref_byte`/`target_row` mirror [`indent_level`].
    pub fn align_column(
        &self,
        rope: &Rope,
        anchor: usize,
        ref_byte: usize,
        target_row: usize,
    ) -> Option<usize> {
        let iq = self.indent.as_ref()?;
        let (align_idx, anchor_idx) = (iq.align_idx?, iq.anchor_idx?);
        let tree = self.tree.as_ref()?;
        let root = tree.root_node();

        let mut scope = root
            .descendant_for_byte_range(anchor, anchor)
            .unwrap_or(root);
        while let Some(parent) = scope.parent() {
            if parent == root {
                break;
            }
            scope = parent;
        }

        let provider = rope_text(rope);
        // Pair each `@align` node with its match's `@anchor` node.
        let mut anchors: HashMap<usize, Node> = HashMap::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&iq.query, scope, provider);
        while let Some(m) = matches.next() {
            let mut align_node = None;
            let mut anchor_node = None;
            for cap in m.captures() {
                if cap.index == align_idx {
                    align_node = Some(cap.node);
                } else if cap.index == anchor_idx {
                    anchor_node = Some(cap.node);
                }
            }
            if let (Some(a), Some(anc)) = (align_node, anchor_node) {
                anchors.entry(a.id()).or_insert(anc);
            }
        }
        if anchors.is_empty() {
            return None;
        }

        // Innermost enclosing `@align` node still open past `ref_byte`, whose
        // anchor began before the target row.
        let mut node = root.descendant_for_byte_range(anchor, anchor);
        while let Some(n) = node {
            if let Some(anc) = anchors.get(&n.id()) {
                let open_past = n.start_byte() < ref_byte && n.end_byte() > ref_byte;
                if n.start_position().row < target_row
                    && open_past
                    && anc.start_position().row < target_row
                {
                    return Some(anc.start_position().column);
                }
            }
            node = n.parent();
        }
        None
    }

    /// Run an arbitrary tree-sitter query over the current tree, returning each
    /// capture as `(capture_name, start_byte, end_byte, node_kind)`. Returns
    /// `None` if the query source won't compile against this grammar, and an
    /// empty vec if it compiles but matches nothing. Node text for query
    /// predicates is read straight from the rope.
    pub fn query_captures(
        &self,
        rope: &Rope,
        query_src: &str,
    ) -> Option<Vec<(String, usize, usize, String)>> {
        let tree = self.tree.as_ref()?;
        let query = Query::new(&self.language, query_src).ok()?;
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let provider = rope_text(rope);
        let mut out = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), provider);
        while let Some(m) = matches.next() {
            for capture in m.captures() {
                let name = names.get(capture.index as usize).copied().unwrap_or("");
                let node = capture.node;
                out.push((
                    name.to_string(),
                    node.start_byte(),
                    node.end_byte(),
                    node.kind().to_string(),
                ));
            }
        }
        Some(out)
    }
}

/// Parse `rope` by feeding tree-sitter chunks straight from the rope (no
/// whole-buffer string allocation), reusing `old_tree` for incremental reparse.
/// Parse `text` as `language` from scratch (tests drive the `ts-*` reads with
/// a real tree).
#[cfg(test)]
pub fn parse_for_test(language: &str, text: &str) -> Option<Tree> {
    let (lang, _) = grammar(language)?;
    let mut parser = Parser::new();
    parser.set_language(&lang.into()).ok()?;
    parser.parse(text, None)
}

/// `(kind, start, end)` of every node containing `byte`, innermost first —
/// the chain behind `(ts-ancestors pos)`.
/// A node's text, chunk by chunk, for tree-sitter queries over a rope
/// (`QueryCursor::matches`'s text provider).
type RopeChunks<'a> = std::iter::Map<ropey::iter::Chunks<'a>, fn(&'a str) -> &'a [u8]>;

fn rope_text<'a>(rope: &'a Rope) -> impl Fn(Node) -> RopeChunks<'a> + 'a {
    move |node: Node| {
        rope.byte_slice(node.byte_range())
            .chunks()
            .map(str::as_bytes as fn(&'a str) -> &'a [u8])
    }
}

pub fn tree_ancestors(tree: &Tree, byte: usize) -> Vec<(String, usize, usize)> {
    let mut node = tree.root_node().descendant_for_byte_range(byte, byte);
    let mut chain = Vec::new();
    while let Some(n) = node {
        chain.push((n.kind().to_string(), n.start_byte(), n.end_byte()));
        node = n.parent();
    }
    chain
}

/// The children `(kind, start, end, named)` of the innermost node spanning
/// exactly `[start, end)` — `(ts-children s e)`.
pub fn tree_children(tree: &Tree, start: usize, end: usize) -> Vec<(String, usize, usize, bool)> {
    let mut node = tree.root_node().descendant_for_byte_range(start, end);
    while let Some(n) = node {
        if n.start_byte() == start && n.end_byte() == end {
            let mut cursor = n.walk();
            return n
                .children(&mut cursor)
                .map(|c| {
                    (
                        c.kind().to_string(),
                        c.start_byte(),
                        c.end_byte(),
                        c.is_named(),
                    )
                })
                .collect();
        }
        if n.start_byte() < start || n.end_byte() > end {
            break;
        }
        node = n.parent();
    }
    Vec::new()
}

/// `(start, end)` of every node whose kind is in `kinds`, in document order —
/// `(ts-ranges-of-kinds kinds)`, behind `]m` / `[[` style motions.
pub fn tree_ranges_of_kinds(tree: &Tree, kinds: &[String]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        let n = cursor.node();
        if kinds.iter().any(|k| k == n.kind()) {
            out.push((n.start_byte(), n.end_byte()));
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    out
}

/// Every string literal and comment in the tree, outermost only and in
/// document order — the spans the generic sexp reader treats as one unit, so
/// a bracket inside `"(…"` or `// )` never unbalances `forward-sexp`.
pub fn tree_literal_spans(tree: &Tree) -> Vec<crate::sexp::Literal> {
    fn literal(kind: &str) -> Option<bool> {
        if kind.contains("comment") {
            Some(true)
        } else if kind.contains("string")
            || kind.ends_with("char_literal")
            || kind.ends_with("character_literal")
            || kind == "rune_literal"
            || kind == "regex"
            || kind == "regex_literal"
        {
            Some(false)
        } else {
            None
        }
    }
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        let n = cursor.node();
        let lit = literal(n.kind()).filter(|_| n.start_byte() < n.end_byte());
        if let Some(comment) = lit {
            out.push(crate::sexp::Literal {
                start: n.start_byte(),
                end: n.end_byte(),
                comment,
            });
        } else if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    out
}

fn parse(parser: &mut Parser, rope: &Rope, old_tree: Option<&Tree>) -> Option<Tree> {
    let mut callback = |byte: usize, _: Point| -> &[u8] {
        if byte >= rope.len_bytes() {
            return &[];
        }
        let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - chunk_start..]
    };
    parser.parse_with_options(&mut callback, old_tree, None)
}

fn point((row, column): (usize, usize)) -> Point {
    Point::new(row, column)
}

/// Resolve a capture name to a face, using the dotted prefix
/// (`string.special` -> `string`).
fn face_for_capture(name: &str) -> Option<Face> {
    // Brackets get their own face so themes can dim structural delimiters
    // without touching other punctuation. Checked before the base-name fallback
    // below, which would otherwise resolve `punctuation.bracket` to `punctuation`.
    if name == "punctuation.bracket" {
        return Some(Face::Bracket);
    }
    let base = name.split('.').next().unwrap_or(name);
    if let Some(face) = Face::from_base(base) {
        return Some(face);
    }
    // Many grammars use capture names that don't share a base with our 10
    // faces — markdown's `text.*`, HTML/CSS `tag`/`attribute`, CSS/YAML/TOML
    // `property`, Lua/R `conditional`/`repeat`, `boolean`, etc. Without aliasing
    // these onto the nearest existing face they render uncolored. Themes already
    // color the target faces, so no new face entries are needed. Names that
    // aren't visible faces (`none`, `spell`, `error`, `embedded`, `_*`) stay
    // unmapped on purpose.
    let alias = match base {
        // markdown block/inline structure
        "text" => match name {
            "text.title" | "text.strong" => Face::Keyword, // headings, bold
            "text.emphasis" => Face::Type,                 // italic
            "text.literal" => Face::String,                // code fences / spans
            "text.uri" => Face::Constant,                  // link / image targets
            "text.reference" => Face::Variable,            // link labels
            _ => return None,
        },
        // The `@markup.*` family (nvim-treesitter's modern markup naming, used by
        // the LaTeX / Typst / Mermaid queries) mirrors the `text.*` mapping above.
        "markup" => {
            if name.starts_with("markup.heading") || name == "markup.strong" {
                Face::Keyword // headings, bold
            } else if name == "markup.italic" {
                Face::Type // italic
            } else if name.starts_with("markup.raw") {
                Face::String // inline code / code blocks
            } else if name == "markup.link.url" || name == "markup.math" {
                Face::Constant // link targets, math content
            } else if name.starts_with("markup.link") {
                Face::Variable // link labels
            } else {
                return None;
            }
        }
        "boolean" => Face::Constant,            // true / false / nil
        "float" => Face::Number,                // float literals (Scala et al.)
        "character" | "escape" => Face::String, // char literals, \n
        "property" | "field" | "parameter" | "attribute" => Face::Variable, // keys, obj.prop, attrs
        // <tag>, if/for, #define, import, throw/catch, `lazy`/`implicit`
        "tag" | "conditional" | "repeat" | "label" | "preproc" | "include" | "exception"
        | "storageclass" => Face::Keyword,
        "constructor" | "namespace" | "module" => Face::Type, // Foo(), modules
        "method" => Face::Function,                           // method calls
        "delimiter" => Face::Punctuation,                     // bare `@delimiter`
        _ => return None,
    };
    Some(alias)
}

/// Flatten possibly-nested capture ranges into contiguous spans tiling
/// `[start, end)`. Query captures are tree nodes, so any two are either disjoint
/// or nested — never partially overlapping — which lets a simple stack sweep
/// pick the innermost (most specific) face at each byte.
fn flatten(mut captures: Vec<(usize, usize, Face)>, start: usize, end: usize) -> Vec<HlSpan> {
    // Sort by start ascending, then by end descending so that at a shared start
    // the outer (larger) capture is processed before the inner one.
    captures.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

    let mut spans = Vec::new();
    let mut stack: Vec<(usize, Face)> = Vec::new(); // (end, face), innermost on top
    let mut pos = start;
    let mut i = 0;
    while pos < end {
        while let Some(&(span_end, _)) = stack.last() {
            if span_end <= pos {
                stack.pop();
            } else {
                break;
            }
        }
        while i < captures.len() && captures[i].0 <= pos {
            let (_, span_end, face) = captures[i];
            if span_end > pos {
                stack.push((span_end, face));
            }
            i += 1;
        }

        let next_start = captures.get(i).map_or(end, |c| c.0);
        let top_end = stack.last().map_or(end, |&(span_end, _)| span_end);
        let seg_end = next_start.min(top_end).min(end);
        let face = stack.last().map(|&(_, face)| face);
        if seg_end > pos {
            spans.push(HlSpan {
                range: pos..seg_end,
                face,
            });
            pos = seg_end;
        } else {
            break; // safety: never seen, but guarantees termination
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faces(spans: &[HlSpan]) -> Vec<&str> {
        spans.iter().filter_map(|s| s.face.map(face_name)).collect()
    }

    /// Every bundled grammar's parser AND highlight query must load — a query
    /// using syntax the tree-sitter version rejects would otherwise silently
    /// disable highlighting for that language.
    #[test]
    fn all_bundled_grammars_load() {
        for lang in BUNDLED_LANGUAGES {
            assert!(
                Highlighter::for_language(lang).is_some(),
                "grammar or highlights query failed to load for {lang}"
            );
        }
        assert!(Highlighter::for_language("cobol").is_none());
    }

    /// The face at the start of `needle` in `src`, per `spans_in` over the
    /// whole buffer. `None` = uncaptured (default foreground).
    fn face_of(h: &Highlighter, rope: &Rope, src: &str, needle: &str) -> Option<&'static str> {
        let at = src
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in src"));
        h.spans_in(rope, 0..src.len())
            .iter()
            .find(|s| s.range.contains(&at))
            .and_then(|s| s.face.map(face_name))
    }

    /// TypeScript uses a vendored combined query: the crate's stock
    /// `HIGHLIGHTS_QUERY` is only the TS *addition* to the JavaScript query it
    /// upstream expects underneath, which left keywords, strings, calls, and
    /// punctuation uncolored. Both layers must color: the JavaScript base and
    /// the TypeScript type layer.
    #[test]
    fn typescript_colors_base_and_type_layer() {
        let mut h = Highlighter::for_language("typescript").unwrap();
        let src = "interface Shape { area(): number }\n\
                   const MAX_N = 3;\n\
                   enum Color { Red }\n\
                   async function render(s: Shape): Promise<string> {\n  \
                     return `size ${s.area().toFixed(MAX_N)} px`;\n\
                   }\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        for (needle, want) in [
            // JavaScript base layer.
            ("const", "keyword"),
            ("async", "keyword"),
            ("return", "keyword"),
            ("render", "function"),  // function declaration name
            ("toFixed", "function"), // method call
            ("3;", "number"),
            ("size ", "string"),   // template string body
            ("${", "punctuation"), // substitution delimiter
            // TypeScript layer.
            ("interface", "keyword"),
            ("Shape", "type"),  // type_identifier
            ("number", "type"), // predefined_type
            ("Promise", "type"),
            ("<string>", "bracket"), // generics delimit, they don't compare
            ("area()", "function"),  // method_signature name
            ("Red", "constant"),     // enum member
            ("MAX_N", "constant"),   // SCREAMING_CASE binding
        ] {
            assert_eq!(
                face_of(&h, &rope, src, needle),
                Some(want),
                "face of {needle:?}"
            );
        }
        // Substitution contents are plain code, not string-colored: `s` is an
        // ordinary variable inside `${...}`.
        assert_eq!(face_of(&h, &rope, src, "s.area"), Some("variable"));
        // A comparison `<` is still an operator (generics excluded it from the
        // blanket token list, so it's only captured in comparison position).
        let cmp = "if (a < b) { }\n";
        let crope = Rope::from_str(cmp);
        h.set_text(&crope);
        assert_eq!(face_of(&h, &crope, cmp, "<"), Some("operator"));
    }

    /// JavaScript uses a vendored query too: the crate's stock one opens with
    /// blanket `(identifier) @variable` / `(property_identifier) @property`
    /// rules that out-rank every context rule in this engine, so function
    /// names, calls, and method calls all painted as plain variables.
    #[test]
    fn javascript_colors_functions_and_jsx() {
        let mut h = Highlighter::for_language("javascript").unwrap();
        let src = "async function go(n) {\n  \
                     return n ? fetch(URL_BASE) : cache.get(`v ${n} z`);\n\
                   }\n\
                   const App = () => <div id=\"x\">{items.map(go)}<Child /></div>;\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        for (needle, want) in [
            ("async", "keyword"),
            ("function", "keyword"),
            ("return", "keyword"),
            ("go(n)", "function"), // declaration name
            ("fetch", "function"), // call
            ("get(", "function"),  // method call
            ("URL_BASE", "constant"),
            ("v ", "string"),      // template string body
            ("${", "punctuation"), // substitution delimiter
            ("div", "keyword"),    // JSX tag
            ("Child", "type"),     // capitalized component
            ("\"x\"", "string"),   // JSX attribute value
        ] {
            assert_eq!(
                face_of(&h, &rope, src, needle),
                Some(want),
                "face of {needle:?}"
            );
        }
        // Substitution contents are plain code, not string-colored.
        assert_eq!(face_of(&h, &rope, src, "n} z"), Some("variable"));
        // A function-valued binding out-ranks the Capitalized rule (its match
        // completes later, at the arrow body), so `App` reads as a function.
        assert_eq!(face_of(&h, &rope, src, "App"), Some("function"));
    }

    /// TSX = the TypeScript base query plus JSX rules: lowercase elements are
    /// tags, capitalized components resolve as types (the identifier-rooted
    /// Capitalized rule out-ranks the enclosing @tag rule), and the base layer
    /// still colors expressions inside JSX.
    #[test]
    fn tsx_colors_jsx_elements() {
        let mut h = Highlighter::for_language("tsx").unwrap();
        let src = "const App = () => <div id=\"x\">{items.map(render)}<Child /></div>;\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        assert_eq!(face_of(&h, &rope, src, "div"), Some("keyword")); // @tag
        assert_eq!(face_of(&h, &rope, src, "Child"), Some("type"));
        assert_eq!(face_of(&h, &rope, src, "map"), Some("function"));
        assert_eq!(face_of(&h, &rope, src, "\"x\""), Some("string"));
        // A function-valued binding out-ranks the Capitalized rule (its match
        // completes later, at the arrow body), so `App` reads as a function.
        assert_eq!(face_of(&h, &rope, src, "App"), Some("function"));
    }

    /// Every bundled indent query must compile against its grammar — a bad node
    /// name would otherwise silently drop that language back to copy-previous
    /// indentation.
    #[test]
    fn bundled_indent_queries_compile() {
        for lang in BUNDLED_LANGUAGES {
            let Some(src) = indent_query(lang) else {
                continue;
            };
            let (langfn, _) = grammar(lang).unwrap();
            let language: Language = langfn.into();
            assert!(
                Query::new(&language, src).is_ok(),
                "indent query for {lang} failed to compile"
            );
        }
    }

    /// Extension → language mappings derived from the unified table match the
    /// old hand-maintained list in `language_for_path` (spot checks across the
    /// tricky cases: multi-extension languages, case sensitivity, ts vs tsx).
    #[test]
    fn extensions_map_to_languages() {
        for (ext, lang) in [
            ("scm", "scheme"),
            ("steel", "scheme"),
            ("rkt", "racket"),
            ("rs", "rust"),
            ("pyi", "python"),
            ("cjs", "javascript"),
            ("ts", "typescript"),
            ("tsx", "tsx"),
            ("h", "c"),
            ("hxx", "cpp"),
            ("yml", "yaml"),
            ("htm", "html"),
            ("zsh", "bash"),
            ("markdown", "markdown"),
            ("R", "r"),
            ("mli", "ocaml"),
            ("sbt", "scala"),
            ("edn", "clojure"),
            ("exs", "elixir"),
            ("psm1", "powershell"),
            ("cls", "latex"),
            ("typ", "typst"),
            ("mmd", "mermaid"),
        ] {
            assert_eq!(language_for_extension(ext), Some(lang), "extension {ext}");
        }
        assert_eq!(language_for_extension("nope"), None);
        // Exact basenames resolve through the filenames column of the same
        // table — extensionless files like Dune's can't match by extension.
        for name in ["dune", "dune-project", "dune-workspace"] {
            assert_eq!(language_for_filename(name), Some("scheme"), "file {name}");
        }
        assert_eq!(language_for_filename("main.rs"), None, "exact match only");
        // Injection tags resolve through the same table.
        assert_eq!(injection_lang_id("C++"), Some("cpp"));
        assert_eq!(injection_lang_id(" shell "), Some("bash"));
        assert_eq!(injection_lang_id("markdown"), None, "no recursion");
    }

    /// Rust: a brace block indents its body one step and dedents on the closer.
    #[test]
    fn rust_indent_levels() {
        let src = "fn main() {\n    foo();\n}\n";
        let rope = Rope::from_str(src);
        let mut h = Highlighter::for_language("rust").unwrap();
        h.set_text(&rope);
        // The body line sits one step in (reindent: anchor == ref).
        let body = src.find("foo").unwrap();
        assert_eq!(h.indent_level(&rope, body, body, 1, Some(body)), Some(1));
        // The closing brace line dedents back to zero.
        let close = src.rfind('}').unwrap();
        assert_eq!(h.indent_level(&rope, close, close, 2, Some(close)), Some(0));
        // Pressing RET right after the opener indents the new line one step
        // (newline: anchor is the `{`, ref is the position past it).
        let brace = src.find('{').unwrap();
        assert_eq!(
            h.indent_level(&rope, brace, brace + 1, 1, Some(brace + 1)),
            Some(1)
        );
        // Nested: a body inside two blocks sits two steps in.
        let nested = "fn f() {\n    if x {\n        y();\n    }\n}\n";
        let nrope = Rope::from_str(nested);
        h.set_text(&nrope);
        let y = nested.find("y()").unwrap();
        assert_eq!(h.indent_level(&nrope, y, y, 2, Some(y)), Some(2));
    }

    /// OCaml's indent query covers the keyword-delimited forms (`let =`,
    /// `match with`, `struct`/`sig`/`do`, records), not just brackets — so a body
    /// line indents one step and the closing token dedents. Each program lists the
    /// expected per-line level (the `reindent` path: anchor == first non-blank).
    #[test]
    fn ocaml_indent_covers_keyword_forms() {
        let cases: &[(&str, &[usize])] = &[
            ("let f x =\n  x + 1\n", &[0, 1]),
            ("let a = 1\nlet b = 2\n", &[0, 0]), // sibling bindings don't indent
            // `match`/`try` arms align with the keyword (ocamlformat style).
            ("match x with\n| 1 -> a\n| _ -> b\n", &[0, 0, 0]),
            ("module M = struct\n  let x = 1\nend\n", &[0, 1, 0]),
            ("try f () with\n| Not_found -> 0\n", &[0, 0]),
            ("let r = { base with\n  a = 1;\n}\n", &[0, 1, 0]),
            // Nested match: arms sit at the match's column (1), not one deeper.
            (
                "let f x =\n  match x with\n  | 0 -> a\n  | _ -> b\n",
                &[0, 1, 1, 1],
            ),
        ];
        let mut h = Highlighter::for_language("ocaml").unwrap();
        for (prog, expected) in cases {
            let rope = Rope::from_str(prog);
            h.set_text(&rope);
            let mut off = 0usize;
            for (row, (line, want)) in prog.lines().zip(expected.iter()).enumerate() {
                let first = line.len() - line.trim_start().len();
                let b = off + first;
                assert_eq!(
                    h.indent_level(&rope, b, b, row, Some(b)),
                    Some(*want),
                    "line {row} of {prog:?}"
                );
                off += line.len() + 1;
            }
        }

        // Pressing RET into a still-empty body (closer not yet typed) indents via
        // the opener-keyword captures, since the enclosing node hasn't formed.
        for (opener, want) in [
            ("module M = struct", 1),
            ("type t = {", 1),
            ("let f x =", 1),
        ] {
            let src = format!("{opener}\n\n");
            let rope = Rope::from_str(&src);
            h.set_text(&rope);
            let anchor = src.trim_end().len() - 1;
            assert_eq!(
                h.indent_level(&rope, anchor, opener.len(), 1, Some(opener.len())),
                Some(want),
                "RET after {opener:?}"
            );
        }
    }

    /// Pressing RET at the end of a line: reproduce the newline handler exactly —
    /// the buffer holds the text BEFORE the newline is inserted (no trailing
    /// newline), anchor = last non-whitespace byte, target_row = current row + 1.
    /// This is the path the user drives when writing OCaml top-to-bottom.
    #[test]
    fn ocaml_newline_indents_after_openers() {
        let cases: &[(&str, usize)] = &[
            ("let f x =", 1),
            ("module M = struct", 1),
            ("type t = {", 1),
            ("let x = 1\nlet f y =", 1), // a prior binding above doesn't confuse it
            ("let () =\n  foo ();", 1),  // continuing a let body keeps its indent
            // `match`/`try` arms align with the keyword, not one step in.
            ("match x with", 0),
            ("match x with\n| 0 -> a", 0), // RET after a complete arm → next arm, col 0
            ("let f x =\n  match x with\n  | 0 -> a", 1), // nested → next arm aligns at col 2
            ("let f = function\n  | 0 -> a", 1), // function arms DO indent one step
        ];
        let mut h = Highlighter::for_language("ocaml").unwrap();
        for (src, want) in cases {
            let rope = Rope::from_str(src);
            h.set_text(&rope);
            let p = src.len();
            let anchor = src
                .char_indices()
                .rev()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(0);
            let target_row = src.matches('\n').count() + 1;
            assert_eq!(
                h.indent_level(&rope, anchor, p, target_row, Some(p)),
                Some(*want),
                "RET at end of {src:?}"
            );
        }
    }

    /// `let … in` bodies align with the binding (no extra step), both top-level
    /// and nested — the RET path with `tail` already present on the next line.
    #[test]
    fn ocaml_letin_body_aligns_with_binding() {
        let cases: &[(&str, &str, usize)] = &[
            // (head-before-RET, tail-already-below, expected level)
            ("let x = 1 in", "\nbody", 0),
            ("let f x =\n  let y = 2 in", "\n  body", 1),
            ("let () =\n  let x = 1 in", "\n  body", 1),
            ("let x = 1 in\nlet y = 2 in", "\nbody", 0),
        ];
        let mut h = Highlighter::for_language("ocaml").unwrap();
        for (head, tail, want) in cases {
            let src = format!("{head}{tail}");
            let rope = Rope::from_str(&src);
            h.set_text(&rope);
            let p = head.len();
            let anchor = head
                .char_indices()
                .rev()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(0);
            let row = head.matches('\n').count() + 1;
            assert_eq!(
                h.indent_level(&rope, anchor, p, row, Some(p)),
                Some(*want),
                "RET after {head:?}"
            );
        }
    }

    /// `let open … in` (and `let … in`) bodies sit at ONE structural level under
    /// the enclosing binding — the reported `let main_loop … =` case. The
    /// per-language 2-space step (not this level) is what makes it render as 2
    /// columns; see `language_indent_width`.
    #[test]
    fn ocaml_let_open_in_is_one_level() {
        let mut h = Highlighter::for_language("ocaml").unwrap();
        let src = "let main_loop state : unit Fiber.t =\n  let open Fiber.O in\n  body";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        // RET at the end of line 2 (`let open Fiber.O in`).
        let p = src.find(" in").unwrap() + 3;
        let anchor = p - 1; // the `n` of `in`
        assert_eq!(h.indent_level(&rope, anchor, p, 2, Some(p)), Some(1));
    }

    /// `@align`/`@anchor`: continuation lines align under the anchored node's
    /// column instead of stepping in. Uses a synthetic query (no bundled grammar
    /// ships one yet) — align args under the first argument.
    #[test]
    fn align_column_aligns_under_anchor() {
        let mut h = Highlighter::for_language("rust").unwrap();
        h.indent = build_indent(&h.language, "(arguments . (_) @anchor) @align");
        assert!(h.indent.is_some(), "align query should compile");
        // `bar` starts at column 4; `baz` on the next line aligns to it.
        let src = "foo(bar,\n    baz)\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        let baz = src.find("baz").unwrap();
        assert_eq!(h.align_column(&rope, baz, baz, 1), Some(4));
        // Outside any `@align` scope → None, so the caller uses the step model.
        let plain = Rope::from_str("let x = 1;\n");
        h.set_text(&plain);
        assert_eq!(h.align_column(&plain, 0, 0, 0), None);
    }

    /// A query with no `@align`/`@anchor` captures never reports alignment.
    #[test]
    fn align_column_absent_without_captures() {
        let mut h = Highlighter::for_language("rust").unwrap();
        let src = "fn f() {\n    g();\n}\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        let g = src.find("g()").unwrap();
        assert_eq!(h.align_column(&rope, g, g, 1), None);
    }

    /// Python: `@extend` indents the body of a compound statement even though
    /// there is no brace token to open a block — including an empty body.
    #[test]
    fn python_indent_uses_extend() {
        let mut h = Highlighter::for_language("python").unwrap();

        // Non-empty body indents one step under the def.
        let src = "def f():\n    pass\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        let body = src.find("pass").unwrap();
        assert_eq!(h.indent_level(&rope, body, body, 1, Some(body)), Some(1));

        // Empty body: RET right after the header colon still indents — anchoring
        // on the colon finds the compound statement the bare position would miss.
        let empty = "def f():\n";
        let erope = Rope::from_str(empty);
        h.set_text(&erope);
        let colon = empty.find(':').unwrap();
        assert_eq!(
            h.indent_level(&erope, colon, colon + 1, 1, Some(colon + 1)),
            Some(1)
        );
    }

    /// Spot-check the other indent-query styles actually produce levels (not
    /// just compile): a JS statement block, a JSON object, and Ruby's
    /// keyword-delimited `def … end` (an `@extend` body with an `end` outdent).
    #[test]
    fn other_languages_indent() {
        // JavaScript: function body indents, `}` dedents.
        let mut h = Highlighter::for_language("javascript").unwrap();
        let js = "function f() {\n  go();\n}\n";
        let jr = Rope::from_str(js);
        h.set_text(&jr);
        let go = js.find("go(").unwrap();
        assert_eq!(h.indent_level(&jr, go, go, 1, Some(go)), Some(1));
        let jclose = js.rfind('}').unwrap();
        assert_eq!(
            h.indent_level(&jr, jclose, jclose, 2, Some(jclose)),
            Some(0)
        );

        // JSON: a value inside an object sits one step in.
        let mut h = Highlighter::for_language("json").unwrap();
        let json = "{\n  \"a\": 1\n}\n";
        let jsr = Rope::from_str(json);
        h.set_text(&jsr);
        let key = json.find("\"a\"").unwrap();
        assert_eq!(h.indent_level(&jsr, key, key, 1, Some(key)), Some(1));

        // Ruby: method body indents via @extend; `end` dedents to zero.
        let mut h = Highlighter::for_language("ruby").unwrap();
        let rb = "def f\n  x\nend\n";
        let rr = Rope::from_str(rb);
        h.set_text(&rr);
        let x = rb.find("x").unwrap();
        assert_eq!(h.indent_level(&rr, x, x, 1, Some(x)), Some(1));
        let end = rb.find("end").unwrap();
        assert_eq!(h.indent_level(&rr, end, end, 2, Some(end)), Some(0));
    }

    /// Functional check for the newly-added indent queries: each indents a body
    /// one step and dedents the closing line back to zero.
    #[test]
    fn added_languages_indent() {
        // (language, source, body-needle, closer-needle)
        let cases = [
            ("r", "f <- function(x) {\n  g(x)\n}\n", "g(x)", "}"),
            ("elixir", "def f do\n  x\nend\n", "x", "end"),
            ("html", "<div>\n  <p>hi</p>\n</div>\n", "<p>", "</div>"),
            ("bash", "if true; then\n  echo hi\nfi\n", "echo", "fi"),
            ("ocaml", "let r = {\n  a = 1;\n}\n", "a = 1", "}"),
            (
                "haskell",
                "main = do\n  putStrLn \"hi\"\n  pure ()\n",
                "putStrLn",
                "",
            ),
        ];
        for (lang, src, body, closer) in cases {
            let mut h = Highlighter::for_language(lang).unwrap();
            let rope = Rope::from_str(src);
            h.set_text(&rope);
            let b = src.find(body).unwrap();
            let brow = src[..b].matches('\n').count();
            assert_eq!(
                h.indent_level(&rope, b, b, brow, Some(b)),
                Some(1),
                "{lang}: body line should indent one step"
            );
            if !closer.is_empty() {
                let c = src.rfind(closer).unwrap();
                let crow = src[..c].matches('\n').count();
                assert_eq!(
                    h.indent_level(&rope, c, c, crow, Some(c)),
                    Some(0),
                    "{lang}: closing line should dedent to zero"
                );
            }
        }
    }

    /// `bracket_match` pairs delimiters via the tree, from either side, and
    /// ignores a bracket that lives inside a string literal (it isn't a token).
    #[test]
    fn bracket_match_pairs_and_skips_strings() {
        let src = "fn f() { g([1, 2]); }\n";
        let rope = Rope::from_str(src);
        let mut h = Highlighter::for_language("rust").unwrap();
        h.set_text(&rope);
        // Outer braces pair, from both directions.
        let open = src.find('{').unwrap();
        let close = src.rfind('}').unwrap();
        assert_eq!(h.bracket_match(open), Some(close));
        assert_eq!(h.bracket_match(close), Some(open));
        // The square brackets pair.
        let lb = src.find('[').unwrap();
        let rb = src.find(']').unwrap();
        assert_eq!(h.bracket_match(lb), Some(rb));
        assert_eq!(h.bracket_match(rb), Some(lb));
        // A non-bracket byte has no match.
        assert_eq!(h.bracket_match(src.find('g').unwrap()), None);

        // A `)` inside a string is not a token: the call's `(` matches the real
        // closing `)`, skipping the one in the string.
        let s = "fn f() { g(\")\"); }\n";
        let srope = Rope::from_str(s);
        h.set_text(&srope);
        let call_open = s.find("g(").unwrap() + 1; // the call paren, not `f()`
        let real_close = s.rfind(')').unwrap(); // the call's closer, after the string
        assert_eq!(h.bracket_match(call_open), Some(real_close));
    }

    /// A non-Scheme grammar actually colors tokens (Rust here).
    #[test]
    fn rust_grammar_colors() {
        let mut h = Highlighter::for_language("rust").unwrap();
        let rope = Rope::from_str("fn main() { let x = 1; }");
        h.set_text(&rope);
        let spans = h.spans_in(&rope, 0..rope.len_bytes());
        let names = faces(&spans);
        assert!(
            names.contains(&"keyword"),
            "expected a keyword face, got {names:?}"
        );
    }

    /// The three newest grammars (LaTeX/Typst via the `codebook-*` crates,
    /// Mermaid vendored + compiled by build.rs) parse and emit colored spans.
    /// Catches a grammar that loads but whose vendored query matches nothing.
    #[test]
    fn latex_typst_mermaid_grammars_color() {
        let cases = [
            ("latex", "\\section{Hi}\n% a comment\n", "comment"),
            ("typst", "= Heading\n#let x = 1\n", "keyword"),
            (
                "mermaid",
                "sequenceDiagram\n  Alice->>John: hi\n",
                "keyword",
            ),
        ];
        for (lang, src, want) in cases {
            let mut h = Highlighter::for_language(lang).unwrap();
            let rope = Rope::from_str(src);
            h.set_text(&rope);
            let spans = h.spans_in(&rope, 0..rope.len_bytes());
            let names = faces(&spans);
            assert!(
                names.contains(&want),
                "{lang}: expected a {want} face, got {names:?}"
            );
        }
    }

    /// The `markup.*` aliases (used by the LaTeX/Typst/Mermaid queries) resolve.
    #[test]
    fn markup_captures_alias_to_faces() {
        assert_eq!(
            face_for_capture("markup.heading.1").map(face_name),
            Some("keyword")
        );
        assert_eq!(
            face_for_capture("markup.strong").map(face_name),
            Some("keyword")
        );
        assert_eq!(
            face_for_capture("markup.italic").map(face_name),
            Some("type")
        );
        assert_eq!(
            face_for_capture("markup.raw.block").map(face_name),
            Some("string")
        );
        assert_eq!(
            face_for_capture("markup.link.url").map(face_name),
            Some("constant")
        );
        assert_eq!(
            face_for_capture("markup.link.label").map(face_name),
            Some("variable")
        );
    }

    /// The extensions in assets/file-types.txt — what Macros registers with the
    /// OS as an "Open With" handler.
    fn os_file_types() -> Vec<&'static str> {
        include_str!("../assets/file-types.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    }

    /// wix/file-types.wxi for `exts`: an OpenWithProgids entry (the per-type
    /// "Open with" list) and a SupportedTypes entry (Macros' own list) each.
    /// Windows reads only the value names; the data is ignored.
    fn render_wix_file_types(exts: &[&str]) -> String {
        let mut out = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <!-- GENERATED from assets/file-types.txt — do not edit. Regenerate with\n     \
             UPDATE_FILE_TYPES=1 cargo test os_file_associations -->\n\
             <Include xmlns=\"http://wixtoolset.org/schemas/v4/wxs\">\n",
        );
        for ext in exts {
            out.push_str(&format!(
                "  <RegistryValue Root=\"HKLM\" Key=\"Software\\Classes\\.{ext}\\OpenWithProgids\" \
                 Name=\"Macros.File\" Type=\"string\" Value=\"Macros.File\" />\n\
                 \x20 <RegistryValue Root=\"HKLM\" \
                 Key=\"Software\\Classes\\Applications\\macros.exe\\SupportedTypes\" \
                 Name=\".{ext}\" Type=\"string\" Value=\".{ext}\" />\n"
            ));
        }
        out.push_str("</Include>\n");
        out
    }

    /// Guard: the OS "Open With" registrations (assets/file-types.txt) cover
    /// every bundled grammar's extensions and the grid's file types, and the
    /// generated Windows include is current. Set UPDATE_FILE_TYPES=1 to rewrite
    /// wix/file-types.wxi after editing the list.
    #[test]
    fn os_file_associations_cover_bundled_languages() {
        let listed = os_file_types();
        let mut seen = std::collections::HashSet::new();
        for ext in &listed {
            assert_eq!(
                *ext,
                ext.to_ascii_lowercase(),
                "file-types.txt: lower case only"
            );
            assert!(seen.insert(*ext), "file-types.txt: {ext} listed twice");
        }
        // The grid's default `grid-extensions` (src/editor/grid.rs).
        let grid = ["csv", "tsv", "tab", "psv", "xlsx"];
        for spec in LANGUAGES {
            for ext in spec.extensions {
                let ext = ext.to_ascii_lowercase();
                assert!(
                    listed.contains(&ext.as_str()),
                    "{}: .{ext} missing from assets/file-types.txt",
                    spec.id
                );
            }
        }
        for ext in grid {
            assert!(
                listed.contains(&ext),
                "grid: .{ext} missing from assets/file-types.txt"
            );
        }

        let wxi = render_wix_file_types(&listed);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/wix/file-types.wxi");
        if std::env::var_os("UPDATE_FILE_TYPES").is_some() {
            std::fs::write(path, &wxi).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(path).unwrap_or_default(),
            wxi,
            "wix/file-types.wxi is stale; run UPDATE_FILE_TYPES=1 cargo test os_file_associations"
        );
    }

    /// Guard: every bundled grammar's highlight captures must resolve to a face,
    /// except a small set of capture names that are deliberately not visible
    /// faces (spell/region markers). Catches a future grammar (or one whose
    /// query changes) using capture names we don't map — the bug that left
    /// markdown, HTML, CSS, YAML, Lua, etc. effectively uncolored.
    #[test]
    fn every_grammar_capture_resolves_to_a_face() {
        // Not visible faces: injected-region and spellcheck markers, error nodes,
        // and PowerShell's whole-span `@array` / `@assignvalue` markers (coloring
        // those would paint an entire `@(...)` or assignment right-hand side).
        const ALLOWED_UNMAPPED: &[&str] = &[
            "embedded",
            "spell",
            "nospell",
            "conceal",
            "error",
            "array",
            "assignvalue",
        ];
        for lang in BUNDLED_LANGUAGES {
            let lang = *lang;
            let (langfn, query_src) = grammar(lang).unwrap();
            let language: Language = langfn.into();
            let query = Query::new(&language, query_src).unwrap();
            let bad: Vec<&str> = query
                .capture_names()
                .iter()
                .copied()
                .filter(|n| {
                    *n != "none"
                        && !n.starts_with('_')
                        && !ALLOWED_UNMAPPED.contains(&n.split('.').next().unwrap_or(n))
                        && face_for_capture(n).is_none()
                })
                .collect();
            assert!(
                bad.is_empty(),
                "{lang}: capture names resolve to no face (would render uncolored): {bad:?}"
            );
        }
    }

    /// Markdown headings and fenced code must color: the grammar's `text.*`
    /// captures are aliased onto existing faces (regression for no-highlight).
    #[test]
    fn markdown_grammar_colors() {
        let mut h = Highlighter::for_language("markdown").unwrap();
        let rope = Rope::from_str("# Title\n\n```\ncode\n```\n");
        h.set_text(&rope);
        let spans = h.spans_in(&rope, 0..rope.len_bytes());
        let names = faces(&spans);
        assert!(
            names.contains(&"keyword"),
            "expected a heading (keyword) face, got {names:?}"
        );
        assert!(
            names.contains(&"string"),
            "expected a code (string) face, got {names:?}"
        );
    }

    /// A fenced ```rust block is highlighted with Rust's own grammar
    /// (injection): the `fn`/`let` keywords inside the block produce a `keyword`
    /// face, which the markdown grammar alone never emits.
    #[test]
    fn markdown_injects_fenced_code_language() {
        let mut h = Highlighter::for_language("markdown").unwrap();
        let src = "# Title\n\n```rust\nfn main() { let x = 1; }\n```\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        let spans = h.spans_in(&rope, 0..rope.len_bytes());

        // The `fn`/`let` keywords sit inside the code block; find a keyword span
        // whose range is within the fenced content (after the ```rust line).
        let code_start = src.find("fn main").unwrap();
        let keyword_in_code = spans
            .iter()
            .any(|s| s.face.map(face_name) == Some("keyword") && s.range.start >= code_start);
        assert!(
            keyword_in_code,
            "expected Rust keyword highlighting inside the fenced block, got {:?}",
            shape(&spans)
        );
    }

    /// Injected blocks are parsed once and reused across paints; editing the
    /// buffer prunes entries for blocks whose content changed.
    #[test]
    fn markdown_injection_spans_are_cached_per_block_content() {
        let mut h = Highlighter::for_language("markdown").unwrap();
        let src = "```rust\nfn a() {}\n```\n\n```python\ndef b(): pass\n```\n";
        let rope = Rope::from_str(src);
        h.set_text(&rope);
        let cached = |h: &Highlighter| h.injection.as_ref().unwrap().spans.borrow().len();
        let first = shape(&h.spans_in(&rope, 0..rope.len_bytes()));
        assert_eq!(cached(&h), 2);
        // A repaint hits the cache and yields identical spans.
        assert_eq!(shape(&h.spans_in(&rope, 0..rope.len_bytes())), first);
        assert_eq!(cached(&h), 2);

        // Edit the rust block: its new content is a new entry; the stale one is
        // pruned on the next change, while the untouched python block survives.
        let edited = src.replace("fn a() {}", "fn a() { let x = 1; }");
        let rope2 = Rope::from_str(&edited);
        h.set_text(&rope2);
        let spans = h.spans_in(&rope2, 0..rope2.len_bytes());
        let in_block = edited.find("let").unwrap();
        assert!(
            spans
                .iter()
                .any(|s| s.face.map(face_name) == Some("keyword") && s.range.start == in_block),
            "edited block must be re-highlighted, got {:?}",
            shape(&spans)
        );
        h.set_text(&rope2);
        assert_eq!(cached(&h), 2, "stale rust entry pruned, live ones kept");
    }

    /// An unknown / untagged fence is left to the markdown grammar (no panic, no
    /// injection) — only the base `text.literal`→string face applies.
    #[test]
    fn markdown_unknown_fence_has_no_injection() {
        let mut h = Highlighter::for_language("markdown").unwrap();
        let rope = Rope::from_str("```nonsense-lang\nx y z\n```\n");
        h.set_text(&rope);
        let spans = h.spans_in(&rope, 0..rope.len_bytes());
        let names = faces(&spans);
        assert!(
            !names.contains(&"keyword"),
            "unknown fence should not inject any grammar, got {names:?}"
        );
    }

    /// The `text.*` capture aliases resolve; `none` stays unmapped.
    #[test]
    fn markdown_text_captures_alias_to_faces() {
        assert_eq!(
            face_for_capture("text.title").map(face_name),
            Some("keyword")
        );
        assert_eq!(
            face_for_capture("text.literal").map(face_name),
            Some("string")
        );
        assert_eq!(
            face_for_capture("text.uri").map(face_name),
            Some("constant")
        );
        assert_eq!(
            face_for_capture("text.reference").map(face_name),
            Some("variable")
        );
        assert_eq!(face_for_capture("none"), None);
        // Existing dotted captures still resolve by prefix.
        assert_eq!(
            face_for_capture("punctuation.special").map(face_name),
            Some("punctuation")
        );
    }

    fn shape(spans: &[HlSpan]) -> Vec<(usize, usize, Option<Face>)> {
        spans
            .iter()
            .map(|s| (s.range.start, s.range.end, s.face))
            .collect()
    }

    #[test]
    fn scheme_grammar_loads_and_colors() {
        let rope = Rope::from_str("; a comment\n(define x \"hi\")\n");
        let mut highlighter = Highlighter::scheme().expect("Scheme grammar/query should load");
        highlighter.set_text(&rope);
        let spans = highlighter.spans_in(&rope, 0..rope.len_bytes());

        let faces = faces(&spans);
        assert!(!spans.is_empty(), "expected contiguous spans");
        assert!(
            faces.contains(&"comment"),
            "expected a comment face, got {faces:?}"
        );
        assert!(
            faces.contains(&"string"),
            "expected a string face, got {faces:?}"
        );
    }

    /// The Steel-aware query colors *roles*, not call position (the classic
    /// Emacs/DrRacket convention): a special form (`require`) as a keyword, the
    /// name a `define` introduces as a function, a Steel builtin (`displayln`)
    /// as a function, a `#:kw` argument as a constant, and parens as the dim
    /// `bracket` face. An ordinary, unknown call head (`bind-key`) is left
    /// uncaptured so it paints with the plain foreground, same as its arguments.
    #[test]
    fn scheme_highlights_steel_forms() {
        let src = "(require \"x\")\n(define (greet n) (displayln n #:end \"!\"))\n(bind-key \"m\" 'greet)\n";
        let rope = Rope::from_str(src);
        let mut h = Highlighter::scheme().unwrap();
        h.set_text(&rope);
        let spans = h.spans_in(&rope, 0..rope.len_bytes());

        // Map: for a substring, the face covering its first byte.
        let face_at = |needle: &str| {
            let at = src.find(needle).unwrap();
            spans
                .iter()
                .find(|s| s.range.contains(&at))
                .and_then(|s| s.face.map(face_name))
        };

        assert_eq!(face_at("require"), Some("keyword"), "Steel special form");
        assert_eq!(face_at("greet"), Some("function"), "defined name");
        assert_eq!(face_at("displayln"), Some("function"), "Steel builtin");
        assert_eq!(face_at("#:end"), Some("constant"), "keyword argument");
        assert_eq!(face_at("("), Some("bracket"), "dim bracket face");
        // An unknown user-defined call head is NOT painted as a function.
        assert_eq!(face_at("bind-key"), None, "ordinary call head stays plain");
    }

    /// The vendored Racket grammar loads and its query colors the same roles as
    /// the Scheme one: `#lang`, special forms, defined names, builtins, keyword
    /// args, and dim brackets — while ordinary call heads stay plain.
    #[test]
    fn racket_highlights_roles() {
        let src = "#lang racket\n(define (greet n) (printf \"~a\" n))\n(my-call 1 #:x 2)\n";
        let rope = Rope::from_str(src);
        let mut h = Highlighter::for_language("racket").expect("racket grammar/query loads");
        h.set_text(&rope);
        let spans = h.spans_in(&rope, 0..rope.len_bytes());

        let face_at = |needle: &str| {
            let at = src.find(needle).unwrap();
            spans
                .iter()
                .find(|s| s.range.contains(&at))
                .and_then(|s| s.face.map(face_name))
        };

        assert_eq!(face_at("racket"), Some("keyword"), "#lang name");
        assert_eq!(face_at("define"), Some("keyword"), "special form");
        assert_eq!(face_at("greet"), Some("function"), "defined name");
        assert_eq!(face_at("printf"), Some("function"), "builtin");
        assert_eq!(face_at("#:x"), Some("constant"), "keyword argument");
        assert_eq!(face_at("("), Some("bracket"), "dim bracket face");
        assert_eq!(face_at("my-call"), None, "ordinary call head stays plain");
    }

    #[test]
    fn query_captures_finds_brackets_skipping_strings() {
        // A "(" inside a string must NOT be reported as a bracket node — the
        // whole reason a script can trust the tree over raw character scanning.
        let rope = Rope::from_str("(define x \"a ( b\")\n");
        let mut h = Highlighter::scheme().unwrap();
        h.set_text(&rope);

        let caps = h
            .query_captures(&rope, "[\"(\" \")\"] @b")
            .expect("query should compile");
        // Two real parens: the opening at 0 and the closing at 17. The "(" at
        // byte 13 lives inside the string and is not a node.
        let starts: Vec<usize> = caps.iter().map(|(_, s, _, _)| *s).collect();
        assert_eq!(starts, vec![0, 17], "got {caps:?}");
        assert!(
            caps.iter()
                .all(|(name, _, _, kind)| name == "b" && (kind == "(" || kind == ")"))
        );

        // A malformed query reports failure rather than panicking.
        assert!(h.query_captures(&rope, "(((").is_none());
    }

    #[test]
    fn node_at_reports_enclosing_chain() {
        let rope = Rope::from_str("(define x 1)\n");
        let mut h = Highlighter::scheme().unwrap();
        h.set_text(&rope);
        // Byte 8 is the `x` symbol; its chain should climb to the list and root.
        let chain = h.node_at(8);
        let kinds: Vec<&str> = chain.iter().map(|(k, _, _)| k.as_str()).collect();
        assert!(
            kinds.contains(&"list"),
            "expected a list ancestor, got {kinds:?}"
        );
        // Innermost is first, the document root last.
        assert_eq!(chain.last().map(|(k, _, _)| k.as_str()), Some("program"));
    }

    #[test]
    fn incremental_reparse_matches_full_parse() {
        let before = Rope::from_str("(define x 1)\n");
        let after = Rope::from_str("(define x 1)\n(define y 2)\n");

        let mut h = Highlighter::scheme().unwrap();
        h.set_text(&before);
        h.apply_edit(&Edit {
            start_byte: before.len_bytes(),
            old_end_byte: before.len_bytes(),
            new_end_byte: after.len_bytes(),
            start_point: (1, 0),
            old_end_point: (1, 0),
            new_end_point: (2, 0),
        });
        h.reparse(&after);
        let incremental = h.spans_in(&after, 0..after.len_bytes());

        let mut fresh = Highlighter::scheme().unwrap();
        fresh.set_text(&after);
        let full = fresh.spans_in(&after, 0..after.len_bytes());

        assert_eq!(
            shape(&incremental),
            shape(&full),
            "incremental must match full parse"
        );
    }

    #[test]
    fn range_restricted_query_matches_the_full_slice() {
        // Querying just the second line should give the same spans (offset-wise)
        // as slicing the full-buffer spans to that range.
        let rope = Rope::from_str("(define x 1)\n; comment here\n(define y 2)\n");
        let mut h = Highlighter::scheme().unwrap();
        h.set_text(&rope);

        let line2 = 13..28; // the comment line's byte range
        let restricted = h.spans_in(&rope, line2.clone());
        assert!(
            faces(&restricted).contains(&"comment"),
            "range query should still see the comment"
        );
        // Spans stay within the requested range.
        assert!(
            restricted
                .iter()
                .all(|s| s.range.start >= line2.start && s.range.end <= line2.end)
        );
    }
}
