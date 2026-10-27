use std::collections::HashMap;

use gpui::Global;

/// Parse a string as a boolean. The single source of truth for which spellings
/// count as "true" (mirrors common Emacs truthiness). Used when a boolean option
/// is set from a *string* value, e.g. `(set-option "completion-auto" "true")`.
fn parse_bool(value: &str) -> bool {
    matches!(value, "true" | "t" | "yes" | "1" | "on")
}

/// A typed option value. Options are set from Scheme via `(set-option "name"
/// value)` where `value` may be a boolean, number, or string; the value is
/// coerced **once, at write time** to the option's declared type (see
/// [`option_kind`]) and stored as-is. Reads are then a direct typed lookup — no
/// per-read parsing.
#[derive(Debug, Clone)]
pub enum OptionValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A Scheme symbol (kept distinct from `Str` so it round-trips back to a
    /// symbol — hash keys written `'name` stay symbols on read).
    Symbol(String),
    /// A Scheme list — `(set-option "k" (list …))` stores it structurally and
    /// `(get-option "k")` returns the list. Elements recurse.
    List(Vec<OptionValue>),
    /// A Scheme hash — `(set-option "k" (hash 'a 1 …))`. Keys and values both
    /// recurse; iteration order is not preserved (hashes are unordered).
    Map(Vec<(OptionValue, OptionValue)>),
    /// A reference to an authinfo.gpg entry (`(secret-get host user)` with no
    /// callback). Only the reference is stored; `get-option` resolves it from
    /// the unlocked store on every read, so the secret itself never sits in
    /// the option table.
    Secret(crate::authinfo::SecretRef),
}

/// Compare an option value against a string literal — true only for the `Str`
/// variant. Keeps the command-queue tests able to write `v == "false"`.
impl PartialEq<&str> for OptionValue {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, OptionValue::Str(s) if s.as_str() == *other)
    }
}

impl OptionValue {
    /// Interpret the value as a boolean, loosely: native booleans pass through,
    /// numbers are truthy when non-zero, and strings use [`parse_bool`]. Used by
    /// other stringly-or-natively-typed setters (e.g. `set-search-option`).
    pub fn truthy(&self) -> bool {
        match self {
            OptionValue::Bool(b) => *b,
            OptionValue::Int(i) => *i != 0,
            OptionValue::Float(f) => *f != 0.0,
            OptionValue::Str(s) | OptionValue::Symbol(s) => parse_bool(s),
            OptionValue::List(v) => !v.is_empty(),
            OptionValue::Map(m) => !m.is_empty(),
            OptionValue::Secret(_) => true,
        }
    }

    /// Interpret the value as a non-negative integer, accepting a native number
    /// or a numeric string. `None` if it isn't one.
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            OptionValue::Int(i) if *i >= 0 => Some(*i as usize),
            OptionValue::Float(f) if *f >= 0.0 => Some(*f as usize),
            OptionValue::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
}

/// The `cursor-style` option: the cursor's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    /// A character-wide block (Zed-style).
    Block,
    /// A `cursor-width`-px bar.
    Bar,
    /// A block in evil Normal/Visual, otherwise a bar.
    Auto,
}

/// The `completion-style` option: where completion candidates are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStyle {
    /// An in-buffer popup (corfu-style overlay).
    Popup,
    /// The minibuffer.
    Minibuffer,
}

/// The declared type of an option (the family it belongs to). Per-mode options
/// like `line-numbers:dired-sidebar-mode` take their type from the base name.
#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Int,
    Float,
    Str,
}

/// An option's default — the value its getter returns while it is unset (or
/// was set to something that couldn't be coerced). Its variant is also the
/// option's declared type.
#[derive(Clone, Copy)]
enum DefaultValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'static str),
}

/// One known option: its user-facing name, default (and so declared type),
/// and (for numbers) the bounds enforced at write time. The single source of
/// truth — [`KNOWN_OPTIONS`], [`option_kind`] and the [`Settings`] getters'
/// defaults all derive from it.
struct OptionSpec {
    name: &'static str,
    default: DefaultValue,
    min: Option<f64>,
    max: Option<f64>,
}

impl OptionSpec {
    fn kind(&self) -> Kind {
        match self.default {
            DefaultValue::Bool(_) => Kind::Bool,
            DefaultValue::Int(_) => Kind::Int,
            DefaultValue::Float(_) => Kind::Float,
            DefaultValue::Str(_) => Kind::Str,
        }
    }
}

const fn flag(name: &'static str, default: bool) -> OptionSpec {
    OptionSpec {
        name,
        default: DefaultValue::Bool(default),
        min: None,
        max: None,
    }
}

const fn int(name: &'static str, default: i64, min: Option<f64>, max: Option<f64>) -> OptionSpec {
    OptionSpec {
        name,
        default: DefaultValue::Int(default),
        min,
        max,
    }
}

const fn float(name: &'static str, default: f64, min: Option<f64>, max: Option<f64>) -> OptionSpec {
    OptionSpec {
        name,
        default: DefaultValue::Float(default),
        min,
        max,
    }
}

const fn text(name: &'static str, default: &'static str) -> OptionSpec {
    OptionSpec {
        name,
        default: DefaultValue::Str(default),
        min: None,
        max: None,
    }
}

/// Every option key Rust knows about, with its default, type and bounds.
/// Scheme-only options (stored verbatim) aren't listed — Rust doesn't know
/// their names ahead of time.
const OPTIONS: &[OptionSpec] = &[
    // Zero/negative sizes would divide by zero in the renderer's row math.
    float("font-size", 15.0, Some(4.0), None),
    float("line-height", 22.0, Some(4.0), None),
    float("helm-height", 0.5, Some(0.1), Some(0.95)),
    float("cursor-width", 2.0, Some(1.0), None),
    float("divider-width", 1.0, Some(1.0), None),
    float("cursor-blink-interval", 530.0, Some(60.0), None),
    float("helm-bevel-top", 1.0, Some(0.0), None),
    float("helm-bevel-right", 1.0, Some(0.0), None),
    float("helm-bevel-bottom", 1.0, Some(0.0), None),
    float("helm-bevel-left", 1.0, Some(0.0), None),
    int("tab-width", 4, Some(0.0), None),
    int("completion-auto-prefix", 1, Some(0.0), None),
    int("completion-max-rows", 10, Some(1.0), None),
    text("completion-style", "popup"),
    text("completion-trigger-chars", "."),
    text("indent-after", ""),
    text("align-after", ""),
    text("cursor-style", "block"),
    text("font-family", "Lilex"),
    text("terminal-passthrough-keys", ""),
    flag("completion-auto", true),
    flag("completion-show-detail", true),
    flag("completion-show-kind", true),
    flag("completion-capf", true),
    flag("cursor-blink", true),
    flag("scroll-bar", true),
    flag("diagnostics-gutter", true),
    flag("magit-gutter", true),
    float("magit-gutter-width", 2.0, Some(1.0), Some(8.0)),
    flag("magit-gutter-deleted", true),
    flag("lsp-log-on-start", false),
    flag("winner-mode", true),
    flag("auto-update", true),
    flag("helm-fuzzy", true),
    flag("line-numbers", true),
    flag("hl-line", false),
    flag("cursor-visible", true),
    flag("word-wrap", true),
    flag("auto-revert", false),
    flag("electric-indent", true),
    flag("show-paren", true),
    flag("mcp-server", false),
    int("mcp-server-port", 7811, Some(1.0), Some(65535.0)),
    text("mcp-server-token", ""),
    flag("server", true),
];

/// `a == b`, usable in a `const` context.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The [`OPTIONS`] entry for `name`. The getters resolve theirs in a `const`
/// block, so a getter naming an unknown option fails to compile.
const fn spec(name: &str) -> &'static OptionSpec {
    let mut i = 0;
    while i < OPTIONS.len() {
        if str_eq(OPTIONS[i].name, name) {
            return &OPTIONS[i];
        }
        i += 1;
    }
    panic!("not in OPTIONS")
}

/// The names in [`OPTIONS`], as a const array so [`KNOWN_OPTIONS`] keeps its
/// `&[&str]` shape without a second hand-maintained list.
const fn option_names() -> [&'static str; OPTIONS.len()] {
    let mut out = [""; OPTIONS.len()];
    let mut i = 0;
    while i < OPTIONS.len() {
        out[i] = OPTIONS[i].name;
        i += 1;
    }
    out
}

/// Every option key Rust knows about — exposed to Scheme as `(option-list)` so
/// `helm-apropos` can surface options (e.g. searching "font" finds `font-size`)
/// alongside functions and variables. Derived from [`OPTIONS`].
pub const KNOWN_OPTIONS: &[&str] = &option_names();

/// Type + numeric bounds for a known option, looked up by its base name (the
/// part before any `:mode` suffix). `None` means the option is unknown to Rust —
/// it is stored verbatim so Scheme-only options still round-trip. A linear scan
/// of [`OPTIONS`] (cold path: options are only set from Scheme).
fn option_kind(key: &str) -> Option<(Kind, Option<f64>, Option<f64>)> {
    // Per-mode options (`base:mode`) carry their type in the base name.
    let base = key.split(':').next().unwrap_or(key);
    OPTIONS
        .iter()
        .find(|spec| spec.name == base)
        .map(|spec| (spec.kind(), spec.min, spec.max))
}

/// `spec`'s default as a `usize` (an integer option).
fn spec_usize(spec: &OptionSpec) -> usize {
    match spec.default {
        DefaultValue::Int(i) => i as usize,
        _ => unreachable!("{} is not an integer option", spec.name),
    }
}

/// `spec`'s default as a `bool` (a boolean option).
fn spec_bool(spec: &OptionSpec) -> bool {
    match spec.default {
        DefaultValue::Bool(b) => b,
        _ => unreachable!("{} is not a boolean option", spec.name),
    }
}

/// Coerce a raw value (as supplied to `set-option`) to `kind`, applying the
/// `min`/`max` bounds to numbers. Returns `None` if the raw value can't sensibly
/// become that type (e.g. `"abc"` for an integer) — the caller then leaves the
/// option unset so its default applies.
fn coerce(
    kind: Kind,
    min: Option<f64>,
    max: Option<f64>,
    raw: &OptionValue,
) -> Option<OptionValue> {
    // NaN / ±inf never make a usable number: reject them (the option keeps
    // its default) rather than letting them reach the renderer.
    let clamp = |mut x: f64| -> Option<f64> {
        if !x.is_finite() {
            return None;
        }
        if let Some(lo) = min {
            x = x.max(lo);
        }
        if let Some(hi) = max {
            x = x.min(hi);
        }
        Some(x)
    };
    Some(match kind {
        Kind::Bool => OptionValue::Bool(match raw {
            OptionValue::Bool(b) => *b,
            OptionValue::Int(i) => *i != 0,
            OptionValue::Str(s) => parse_bool(s),
            _ => return None,
        }),
        Kind::Int => OptionValue::Int(clamp(match raw {
            OptionValue::Int(i) => *i as f64,
            OptionValue::Float(f) => *f,
            OptionValue::Str(s) => s.trim().parse::<f64>().ok()?,
            _ => return None,
        })? as i64),
        Kind::Float => OptionValue::Float(clamp(match raw {
            OptionValue::Float(f) => *f,
            OptionValue::Int(i) => *i as f64,
            OptionValue::Str(s) => s.trim().parse::<f64>().ok()?,
            _ => return None,
        })?),
        Kind::Str => OptionValue::Str(match raw {
            OptionValue::Str(s) => s.clone(),
            OptionValue::Bool(b) => b.to_string(),
            OptionValue::Int(i) => i.to_string(),
            OptionValue::Float(f) => f.to_string(),
            // A symbol coerces to its text; a list/hash can't sensibly become a
            // scalar string option — drop it (keep the default).
            OptionValue::Symbol(s) => s.clone(),
            _ => return None,
        }),
    })
}

/// User-tunable options, set from Scheme via `(set-option "name" value)` and read
/// by the renderer / commands. Each value is coerced to the option's declared
/// type when it is set ([`Settings::set`]) and stored typed, so the read-side
/// getters are plain lookups. Unknown option names are stored verbatim.
///
/// Held as a GPUI global (like [`crate::theme::Theme`]); the typed getters fall
/// back to each option's [`OPTIONS`] default while it is unset.
#[derive(Default)]
pub struct Settings {
    values: HashMap<String, OptionValue>,
}

impl Global for Settings {}

impl Settings {
    /// A clone of the currently-set option values, for publishing to the script
    /// engine so Scheme can read them via `(get-option …)`.
    pub fn snapshot(&self) -> HashMap<String, OptionValue> {
        self.values.clone()
    }

    /// Set an option, coercing the raw value to the option's declared type. A
    /// value that can't be coerced is dropped (the option keeps its default); an
    /// unknown option name is stored as-is.
    pub fn set(&mut self, key: String, value: OptionValue) {
        let stored = match option_kind(&key) {
            Some((kind, min, max)) => match coerce(kind, min, max, &value) {
                Some(v) => v,
                None => return,
            },
            None => value,
        };
        self.values.insert(key, stored);
    }

    fn f32_of(&self, spec: &OptionSpec) -> f32 {
        match self.values.get(spec.name) {
            Some(OptionValue::Float(f)) => *f as f32,
            Some(OptionValue::Int(i)) => *i as f32,
            _ => match spec.default {
                DefaultValue::Float(f) => f as f32,
                _ => unreachable!("{} is not a float option", spec.name),
            },
        }
    }

    fn usize_of(&self, spec: &OptionSpec) -> usize {
        self.values
            .get(spec.name)
            .and_then(Self::stored_usize)
            .unwrap_or_else(|| spec_usize(spec))
    }

    /// A stored number as a `usize` (coercion stores numbers natively).
    fn stored_usize(v: &OptionValue) -> Option<usize> {
        match v {
            OptionValue::Int(i) if *i >= 0 => Some(*i as usize),
            OptionValue::Float(f) if *f >= 0.0 => Some(*f as usize),
            _ => None,
        }
    }

    fn bool_of(&self, spec: &OptionSpec) -> bool {
        match self.values.get(spec.name) {
            Some(OptionValue::Bool(b)) => *b,
            _ => spec_bool(spec),
        }
    }

    fn str_of(&self, spec: &OptionSpec) -> &str {
        match self.values.get(spec.name) {
            Some(OptionValue::Str(s)) => s.as_str(),
            _ => match spec.default {
                DefaultValue::Str(s) => s,
                _ => unreachable!("{} is not a string option", spec.name),
            },
        }
    }

    /// Resolve a *per-major-mode* boolean option: a `base:<mode>` value, if set,
    /// overrides the global `base`, whose own fallback is `default`. Used for
    /// options like line numbers and hl-line that a specific mode can flip
    /// without changing the default.
    fn bool_per_mode(&self, base: &OptionSpec, mode: Option<&str>, default: bool) -> bool {
        mode.and_then(|m| match self.values.get(&format!("{}:{m}", base.name)) {
            Some(OptionValue::Bool(b)) => Some(*b),
            _ => None,
        })
        .unwrap_or_else(|| match self.values.get(base.name) {
            Some(OptionValue::Bool(b)) => *b,
            _ => default,
        })
    }

    /// Resolve a *per-major-mode* unsigned-int option: a `base:<mode>` value, if
    /// set, overrides the global `base`. `Settings::set` coerces `base:<mode>` to
    /// the base's declared type (via [`option_kind`], which strips the `:mode`
    /// suffix), so for a registered base like `tab-width` the value is already an
    /// `Int`; [`OptionValue::as_usize`] still accepts the string form
    /// defensively.
    fn usize_per_mode(&self, base: &OptionSpec, mode: Option<&str>) -> usize {
        mode.and_then(|m| self.values.get(&format!("{}:{m}", base.name)))
            .and_then(OptionValue::as_usize)
            .unwrap_or_else(|| self.usize_of(base))
    }

    /// Resolve a *per-major-mode* string option: a `base:<mode>` value, if set,
    /// overrides the global `base`. Returns an owned `String` since it may come
    /// from either the mode key or the base.
    fn str_per_mode(&self, base: &OptionSpec, mode: Option<&str>) -> String {
        mode.and_then(|m| match self.values.get(&format!("{}:{m}", base.name)) {
            Some(OptionValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| self.str_of(base).to_string())
    }

    /// Editor font family. A single family name (not a fallback list); the
    /// named font must be installed or GPUI falls back to its own default.
    pub fn font_family(&self) -> &str {
        self.str_of(const { spec("font-family") })
    }

    /// Editor font size in pixels.
    pub fn font_size(&self) -> f32 {
        self.f32_of(const { spec("font-size") })
    }

    /// Line height in pixels.
    pub fn line_height(&self) -> f32 {
        self.f32_of(const { spec("line-height") })
    }

    /// Spaces per indentation step, used by auto-indent across all languages:
    /// each tree-sitter structural level is `tab-width` columns, and a Lisp
    /// special form indents its body `tab-width` past the opener. Set from Scheme
    /// with `(set-option "tab-width" N)`; default 4.
    pub fn tab_width(&self) -> usize {
        self.usize_of(const { spec("tab-width") })
    }

    /// The indent step for a specific language: a `tab-width:<mode>` override, if
    /// set, otherwise the global `tab-width`. Bundled `scheme/indent.scm` seeds
    /// per-language conventions (OCaml/Ruby → 2, …); `(set-option "tab-width:ocaml"
    /// N)` in `init.scm` (loaded last) overrides them.
    pub fn tab_width_for(&self, mode: Option<&str>) -> usize {
        self.usize_per_mode(const { spec("tab-width") }, mode)
    }

    /// Extra completion trigger characters for a language, *added to* whatever the
    /// language server advertises. Defaults to `.` everywhere so member access
    /// auto-opens completion even under a server that under-advertises (e.g.
    /// ocamllsp, which doesn't list `.`). Override globally or per language with
    /// `(set-option "completion-trigger-chars[:<lang>]" "…")` ("" disables).
    pub fn completion_trigger_chars(&self, mode: Option<&str>) -> String {
        self.str_per_mode(const { spec("completion-trigger-chars") }, mode)
    }

    /// Trailing-token indent rules for a language (a token-level layer on top of
    /// the structural tree-sitter indent). `indent-after` lists tokens whose
    /// presence at the END of a line opens the next line one step deeper (OCaml
    /// `->`); `align-after` lists tokens that keep the next line at the current
    /// line's column (OCaml `in` / `with`, which align rather than indent). Each is
    /// a whitespace-separated token list, set per language, e.g.
    /// `(set-option "indent-after:ocaml" "->")`.
    pub fn indent_after(&self, mode: Option<&str>) -> String {
        self.str_per_mode(const { spec("indent-after") }, mode)
    }

    pub fn align_after(&self, mode: Option<&str>) -> String {
        self.str_per_mode(const { spec("align-after") }, mode)
    }

    /// Whether RET keeps the current indentation (and, in Lisp buffers, indents
    /// to the enclosing form) and TAB reindents the current Lisp line. Emacs
    /// `electric-indent-mode`. Set `electric-indent` to `#f` to disable.
    pub fn electric_indent(&self) -> bool {
        self.bool_of(const { spec("electric-indent") })
    }

    /// Whether the bracket at (or just before) point and its match are
    /// highlighted (Emacs `show-paren-mode`). A *per-major-mode* option: on
    /// everywhere by default except prose/data buffers (`markdown`, `toml`, and
    /// plain text in `fundamental`), where bracket matching is just noise. A
    /// `show-paren` / `show-paren:<mode>` option overrides — e.g.
    /// `(set-option "show-paren:markdown" #t)` to re-enable it there.
    pub fn show_paren_for(&self, mode: Option<&str>) -> bool {
        const SHOW_PAREN: &OptionSpec = spec("show-paren");
        let default = spec_bool(SHOW_PAREN)
            && !matches!(
                mode,
                None | Some("markdown" | "toml" | "fundamental" | "text")
            );
        self.bool_per_mode(SHOW_PAREN, mode, default)
    }

    /// Chords (whitespace-separated, e.g. `"ctrl-] alt-o"`) that a focused
    /// terminal hands to the editor instead of the shell — an escape hatch for
    /// window/buffer keys beyond the built-in `C-x` prefix and the alt leader.
    pub fn terminal_passthrough_keys(&self) -> &str {
        self.str_of(const { spec("terminal-passthrough-keys") })
    }

    /// How completion candidates are presented: `"popup"` (corfu-style overlay,
    /// the default) or `"minibuffer"`. Any other value is a popup.
    pub fn completion_style(&self) -> CompletionStyle {
        match self.str_of(const { spec("completion-style") }) {
            "minibuffer" => CompletionStyle::Minibuffer,
            _ => CompletionStyle::Popup,
        }
    }

    /// Whether the popup appears automatically as you type (corfu `corfu-auto`).
    pub fn completion_auto(&self) -> bool {
        self.bool_of(const { spec("completion-auto") })
    }

    /// Minimum prefix length before auto-completion triggers
    /// (corfu `corfu-auto-prefix`): the first word character typed.
    pub fn completion_auto_prefix(&self) -> usize {
        self.usize_of(const { spec("completion-auto-prefix") })
    }

    /// Maximum candidate rows shown in the popup (corfu `corfu-count`). The
    /// minimum of 1 is enforced when the option is set.
    pub fn completion_max_rows(&self) -> usize {
        self.usize_of(const { spec("completion-max-rows") })
    }

    /// Show the signature / type detail next to each candidate.
    pub fn completion_show_detail(&self) -> bool {
        self.bool_of(const { spec("completion-show-detail") })
    }

    /// Show the kind tag (`fn`, `var`, …) for each candidate.
    pub fn completion_show_kind(&self) -> bool {
        self.bool_of(const { spec("completion-show-kind") })
    }

    /// Whether `completion-at-point` consults the registered Scheme
    /// completion-at-point-functions before falling back to the language server.
    /// On by default; set `(set-option "completion-capf" #f)` for strictly
    /// LSP-driven completion (skipping CAPFs and their engine round-trip).
    pub fn completion_capf(&self) -> bool {
        self.bool_of(const { spec("completion-capf") })
    }

    /// Height of the Helm panel as a fraction of the window (default 0.5). The
    /// 0.1–0.95 range is enforced when the option is set.
    pub fn helm_height(&self) -> f32 {
        self.f32_of(const { spec("helm-height") })
    }

    /// Cursor shape: `"block"` (default, Zed-style), `"bar"`, or `"auto"`
    /// (block, but a bar in evil insert mode). Any other value means auto.
    pub fn cursor_style(&self) -> CursorStyle {
        match self.str_of(const { spec("cursor-style") }) {
            "block" => CursorStyle::Block,
            "bar" => CursorStyle::Bar,
            _ => CursorStyle::Auto,
        }
    }

    /// Bar-cursor width in pixels (default 2; minimum 1 enforced on set).
    pub fn cursor_width(&self) -> f32 {
        self.f32_of(const { spec("cursor-width") })
    }

    /// Whether the cursor blinks. On by default (Zed-style blinking block); set
    /// `(set-option "cursor-blink" #f)` for a steady cursor (no idle redraw).
    pub fn cursor_blink(&self) -> bool {
        self.bool_of(const { spec("cursor-blink") })
    }

    /// Whether winner-mode records window layouts for `C-c left` / `C-c right`
    /// (on by default; `(set-option "winner-mode" #f)` turns it off).
    pub fn winner_mode(&self) -> bool {
        self.bool_of(const { spec("winner-mode") })
    }

    /// Cursor blink half-period in milliseconds (time the cursor stays shown,
    /// then hidden). Default 530, matching Emacs `blink-cursor-interval` (0.53s).
    /// The 60ms minimum is enforced when the option is set.
    pub fn cursor_blink_interval(&self) -> u64 {
        self.f32_of(const { spec("cursor-blink-interval") }) as u64
    }

    /// Whether to show the line-number gutter for a buffer whose major mode is
    /// `mode` (Emacs `display-line-numbers-mode`). It's a *per-major-mode*
    /// option: a `line-numbers:<mode>` value overrides the global `line-numbers`
    /// default, which is on for every mode unless a mode (e.g. dired-sidebar) turns
    /// it off.
    pub fn line_numbers_for(&self, mode: Option<&str>) -> bool {
        const LINE_NUMBERS: &OptionSpec = spec("line-numbers");
        self.bool_per_mode(LINE_NUMBERS, mode, spec_bool(LINE_NUMBERS))
    }

    /// Highlight the line the cursor is on (Emacs `hl-line-mode`). A *per-major-
    /// mode* option: a `hl-line:<mode>` value overrides the global `hl-line`
    /// default, which is off everywhere unless a mode (e.g. magit-status) turns
    /// it on. Painted as a subtle full-width background band behind the text.
    pub fn hl_line_for(&self, mode: Option<&str>) -> bool {
        const HL_LINE: &OptionSpec = spec("hl-line");
        self.bool_per_mode(HL_LINE, mode, spec_bool(HL_LINE))
    }

    /// Paint the cursor in a buffer whose major mode is `mode`. A *per-major-
    /// mode* option: on everywhere unless a mode turns it off — the
    /// dired-sidebar tree hides it and lets the `hl-line` band mark the row.
    pub fn cursor_visible_for(&self, mode: Option<&str>) -> bool {
        const CURSOR_VISIBLE: &OptionSpec = spec("cursor-visible");
        self.bool_per_mode(CURSOR_VISIBLE, mode, spec_bool(CURSOR_VISIBLE))
    }

    /// Show a scroll bar on the right edge of each pane (Emacs `scroll-bar-mode`).
    pub fn scroll_bar(&self) -> bool {
        self.bool_of(const { spec("scroll-bar") })
    }

    /// Soft-wrap long lines at the window edge (Emacs `visual-line-mode` /
    /// `word-wrap`). On by default. When off, long lines are not wrapped and the
    /// buffer scrolls horizontally to reveal off-screen text. A *per-major-mode*
    /// option: a `word-wrap:<mode>` value overrides the global `word-wrap`
    /// default, so a mode (e.g. the dired-sidebar tree) can truncate long lines
    /// while ordinary buffers keep wrapping.
    pub fn word_wrap_for(&self, mode: Option<&str>) -> bool {
        const WORD_WRAP: &OptionSpec = spec("word-wrap");
        self.bool_per_mode(WORD_WRAP, mode, spec_bool(WORD_WRAP))
    }

    /// Show a colored dot in the line-number gutter on lines that carry an LSP
    /// diagnostic (red for an error, orange for a warning, blue for info/hint) —
    /// the worst severity on the line wins. On by default; set
    /// `(set-option "diagnostics-gutter" #f)` to hide the dots. Only drawn when
    /// the line-number gutter itself is shown.
    pub fn diagnostics_gutter(&self) -> bool {
        self.bool_of(const { spec("diagnostics-gutter") })
    }

    /// Show working-tree-vs-HEAD change bars in the line-number gutter (the
    /// git-gutter / diff-hl feature): a bar for added/modified lines, a wedge
    /// where lines were deleted. On by default; `(set-option "magit-gutter" #f)`
    /// hides it (or toggle with `M-x magit-gutter-mode`). Like the diagnostic
    /// dots, only drawn when the line-number gutter itself is shown.
    pub fn magit_gutter(&self) -> bool {
        self.bool_of(const { spec("magit-gutter") })
    }

    /// Width (px) of a git-gutter change bar. Default 2; `(set-option
    /// "magit-gutter-width" N)`, clamped to 1–8.
    pub fn magit_gutter_width(&self) -> f32 {
        self.f32_of(const { spec("magit-gutter-width") })
    }

    /// Whether the git-gutter draws a wedge for deleted lines. On by default;
    /// `(set-option "magit-gutter-deleted" #f)` shows only added/modified bars.
    pub fn magit_gutter_deleted(&self) -> bool {
        self.bool_of(const { spec("magit-gutter-deleted") })
    }

    /// Pop open `*lsp-log*` in a split when an external language server starts,
    /// streaming its stderr (handy for diagnosing startup). Off by default — the
    /// log is always captured; open it with `C-h l` (`M-x list-lsp-log`). Only
    /// fires on an actual server spawn (once per language/session).
    pub fn lsp_log_on_start(&self) -> bool {
        self.bool_of(const { spec("lsp-log-on-start") })
    }

    /// Whether Macros checks macros.dev for a newer release at launch (once a
    /// day) and, on macOS, stages it in the background so a "restart to update"
    /// notice can offer a one-click relaunch. On by default; the Scheme
    /// `(set-auto-update #f)` flips this off (no network check, no notice).
    pub fn auto_update(&self) -> bool {
        self.bool_of(const { spec("auto-update") })
    }

    /// Width (px) of the line dividing split windows. Also its drag-to-resize
    /// hit area, so bump it if the divider is hard to grab with the mouse. The
    /// 1px minimum is enforced when the option is set.
    pub fn divider_width(&self) -> f32 {
        self.f32_of(const { spec("divider-width") })
    }

    /// Helm matching: fuzzy (nucleo, fzf-style ranking) vs plain substring.
    pub fn helm_fuzzy(&self) -> bool {
        self.bool_of(const { spec("helm-fuzzy") })
    }

    /// Auto-revert (Emacs `global-auto-revert-mode`): when a file-visiting buffer
    /// with no unsaved edits changes on disk, re-read it automatically. Off by
    /// default — enabled with `(set-option "auto-revert" #t)`. Buffers with unsaved
    /// changes are never clobbered (a warning is echoed instead). Independent of the
    /// always-on dired-sidebar/dired directory refresh, which watches dirs, not buffers.
    pub fn auto_revert(&self) -> bool {
        self.bool_of(const { spec("auto-revert") })
    }

    /// The MCP server (src/mcp.rs) for debugging and package development: lets
    /// an AI assistant eval Steel in the live editor. OFF by default — enable
    /// with `(set-option "mcp-server" #t)` or `M-x mcp-server-start`.
    pub fn mcp_server(&self) -> bool {
        self.bool_of(const { spec("mcp-server") })
    }

    /// Loopback port for the MCP server (default 7811).
    pub fn mcp_server_port(&self) -> u16 {
        const PORT: &OptionSpec = spec("mcp-server-port");
        u16::try_from(self.usize_of(PORT)).unwrap_or(spec_usize(PORT) as u16)
    }

    /// A fixed bearer token for the MCP server; `None` (the default) means a
    /// random one generated once and kept in the config dir's `mcp-token`.
    pub fn mcp_server_token(&self) -> Option<&str> {
        Some(self.str_of(const { spec("mcp-server-token") }).trim()).filter(|t| !t.is_empty())
    }

    /// The edit server (src/server.rs) that `macros --client` hands files to.
    /// ON by default: it only listens on a socket private to the user, and
    /// without it every `macros --client` would start a second editor.
    pub fn server(&self) -> bool {
        self.bool_of(const { spec("server") })
    }

    /// Bevel edge widths (px) per side, default 1 (minimum 0 enforced on set).
    pub fn helm_bevel_top(&self) -> f32 {
        self.f32_of(const { spec("helm-bevel-top") })
    }
    pub fn helm_bevel_right(&self) -> f32 {
        self.f32_of(const { spec("helm-bevel-right") })
    }
    pub fn helm_bevel_bottom(&self) -> f32 {
        self.f32_of(const { spec("helm-bevel-bottom") })
    }
    pub fn helm_bevel_left(&self) -> f32 {
        self.f32_of(const { spec("helm-bevel-left") })
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletionStyle, CursorStyle, KNOWN_OPTIONS, OptionValue, Settings, option_kind};

    /// Every getter's default comes from `OPTIONS` with the type the getter
    /// expects (a mismatch would panic here), and the defaults are unchanged.
    #[test]
    fn getter_defaults() {
        let s = Settings::default();
        assert_eq!(s.font_family(), "Lilex");
        assert_eq!(s.font_size(), 15.0);
        assert_eq!(s.line_height(), 22.0);
        assert_eq!(s.tab_width(), 4);
        assert_eq!(s.tab_width_for(Some("rust")), 4);
        assert_eq!(s.completion_trigger_chars(None), ".");
        assert_eq!(s.indent_after(None), "");
        assert_eq!(s.align_after(None), "");
        assert!(s.electric_indent());
        assert!(s.show_paren_for(Some("rust")));
        assert_eq!(s.terminal_passthrough_keys(), "");
        assert_eq!(s.completion_style(), CompletionStyle::Popup);
        assert!(s.completion_auto());
        assert_eq!(s.completion_auto_prefix(), 1);
        assert_eq!(s.completion_max_rows(), 10);
        assert!(s.completion_show_detail());
        assert!(s.completion_show_kind());
        assert!(s.completion_capf());
        assert_eq!(s.helm_height(), 0.5);
        assert_eq!(s.cursor_style(), CursorStyle::Block);
        assert_eq!(s.cursor_width(), 2.0);
        assert!(s.cursor_blink());
        assert_eq!(s.cursor_blink_interval(), 530);
        assert!(s.line_numbers_for(None));
        assert!(!s.hl_line_for(None));
        assert!(s.scroll_bar());
        assert!(s.word_wrap_for(None));
        assert!(s.diagnostics_gutter());
        assert!(s.magit_gutter());
        assert_eq!(s.magit_gutter_width(), 2.0);
        assert!(s.magit_gutter_deleted());
        assert!(!s.lsp_log_on_start());
        assert!(s.auto_update());
        assert_eq!(s.divider_width(), 1.0);
        assert!(s.helm_fuzzy());
        assert!(!s.auto_revert());
        assert!(!s.mcp_server());
        assert_eq!(s.mcp_server_port(), 7811);
        assert_eq!(s.mcp_server_token(), None);
        assert!(s.server());
        assert_eq!(s.helm_bevel_top(), 1.0);
        assert_eq!(s.helm_bevel_right(), 1.0);
        assert_eq!(s.helm_bevel_bottom(), 1.0);
        assert_eq!(s.helm_bevel_left(), 1.0);
    }

    #[test]
    fn style_options_parse_to_enums() {
        let mut s = Settings::default();
        s.set("cursor-style".into(), OptionValue::Str("auto".into()));
        assert_eq!(s.cursor_style(), CursorStyle::Auto);
        s.set("cursor-style".into(), OptionValue::Str("bogus".into()));
        assert_eq!(s.cursor_style(), CursorStyle::Auto);
        s.set(
            "completion-style".into(),
            OptionValue::Str("minibuffer".into()),
        );
        assert_eq!(s.completion_style(), CompletionStyle::Minibuffer);
        s.set("completion-style".into(), OptionValue::Str("bogus".into()));
        assert_eq!(s.completion_style(), CompletionStyle::Popup);
    }

    /// Every advertised option must actually be a known Rust option — guards the
    /// `KNOWN_OPTIONS` list from drifting out of sync with `option_kind`.
    #[test]
    fn known_options_have_a_kind() {
        for key in KNOWN_OPTIONS {
            assert!(
                option_kind(key).is_some(),
                "{key} is in KNOWN_OPTIONS but has no option_kind arm"
            );
        }
        // No accidental duplicates.
        let mut seen = std::collections::HashSet::new();
        for key in KNOWN_OPTIONS {
            assert!(seen.insert(*key), "{key} listed twice in KNOWN_OPTIONS");
        }
    }

    #[test]
    fn show_paren_defaults_per_language_and_overrides() {
        let mut s = Settings::default();
        // On for code, off for prose/data buffers and plain text.
        assert!(s.show_paren_for(Some("rust")));
        assert!(s.show_paren_for(Some("python")));
        assert!(!s.show_paren_for(Some("markdown")));
        assert!(!s.show_paren_for(Some("toml")));
        assert!(!s.show_paren_for(Some("fundamental")));
        assert!(!s.show_paren_for(None));
        // A per-mode override wins over the per-language default…
        s.set("show-paren:markdown".into(), OptionValue::Bool(true));
        assert!(s.show_paren_for(Some("markdown")));
        // …and a global override applies where no per-mode value is set.
        s.set("show-paren".into(), OptionValue::Bool(false));
        assert!(!s.show_paren_for(Some("rust")));
        assert!(s.show_paren_for(Some("markdown"))); // per-mode still wins
    }

    #[test]
    fn coerces_native_and_string_values() {
        let mut s = Settings::default();
        // String form (back-compat) and native numbers both land as the float.
        s.set("font-size".into(), OptionValue::Str("13".into()));
        assert_eq!(s.font_size(), 13.0);
        s.set("font-size".into(), OptionValue::Int(16));
        assert_eq!(s.font_size(), 16.0);
        // Booleans from a native #t/#f and from a "true"/"false" string.
        s.set("completion-auto".into(), OptionValue::Bool(false));
        assert!(!s.completion_auto());
        s.set("completion-auto".into(), OptionValue::Str("true".into()));
        assert!(s.completion_auto());
        // String-typed option.
        s.set("cursor-style".into(), OptionValue::Str("bar".into()));
        assert_eq!(s.cursor_style(), CursorStyle::Bar);
    }

    #[test]
    fn rejects_degenerate_font_metrics() {
        let mut s = Settings::default();
        // Zero / negative sizes clamp to the floor instead of reaching the
        // renderer (a zero line height divides by zero there).
        s.set("line-height".into(), OptionValue::Float(0.0));
        assert_eq!(s.line_height(), 4.0);
        s.set("font-size".into(), OptionValue::Int(-3));
        assert_eq!(s.font_size(), 4.0);
        // Non-finite values are rejected; the previous value stays in force.
        s.set("line-height".into(), OptionValue::Float(20.0));
        s.set("line-height".into(), OptionValue::Float(f64::NAN));
        assert_eq!(s.line_height(), 20.0);
        s.set("font-size".into(), OptionValue::Str("inf".into()));
        assert_eq!(s.font_size(), 4.0);
    }

    #[test]
    fn clamps_at_write_time() {
        let mut s = Settings::default();
        s.set("helm-height".into(), OptionValue::Float(2.0)); // above the 0.95 max
        assert_eq!(s.helm_height(), 0.95);
        s.set("completion-max-rows".into(), OptionValue::Int(0)); // below the 1 min
        assert_eq!(s.completion_max_rows(), 1);
    }

    #[test]
    fn per_mode_override_and_uncoercible_is_ignored() {
        let mut s = Settings::default();
        // A `base:mode` value overrides only that mode; others keep the default.
        s.set(
            "line-numbers:dired-sidebar-mode".into(),
            OptionValue::Bool(false),
        );
        assert!(!s.line_numbers_for(Some("dired-sidebar-mode")));
        assert!(s.line_numbers_for(Some("rust")));
        // An un-coercible value leaves the previous value intact.
        s.set("font-size".into(), OptionValue::Int(16));
        s.set("font-size".into(), OptionValue::Str("abc".into()));
        assert_eq!(s.font_size(), 16.0);
    }

    #[test]
    fn completion_trigger_chars_per_language() {
        let mut s = Settings::default();
        // No config: `.` everywhere (added to server-advertised triggers).
        assert_eq!(s.completion_trigger_chars(Some("ocaml")), ".");
        // A per-language value applies only to that language.
        s.set(
            "completion-trigger-chars:ocaml".into(),
            OptionValue::Str(".".into()),
        );
        assert_eq!(s.completion_trigger_chars(Some("ocaml")), ".");
        assert_eq!(s.completion_trigger_chars(Some("rust")), ".");
        // A global value applies to languages without an override.
        s.set(
            "completion-trigger-chars".into(),
            OptionValue::Str(".:".into()),
        );
        assert_eq!(s.completion_trigger_chars(Some("rust")), ".:");
        assert_eq!(s.completion_trigger_chars(Some("ocaml")), "."); // override wins
    }

    #[test]
    fn tab_width_per_language_override() {
        let mut s = Settings::default();
        // No override: every language uses the global tab-width.
        assert_eq!(s.tab_width_for(Some("ocaml")), 4);
        assert_eq!(s.tab_width_for(None), 4);
        // A `tab-width:<lang>` override applies only to that language. `set`
        // coerces it to the base's Int type…
        s.set("tab-width:ocaml".into(), OptionValue::Int(2));
        assert_eq!(s.tab_width_for(Some("ocaml")), 2);
        assert_eq!(s.tab_width_for(Some("rust")), 4); // untouched
        // …including the string form `(set-option "tab-width:ruby" "2")`.
        s.set("tab-width:ruby".into(), OptionValue::Str("2".into()));
        assert_eq!(s.tab_width_for(Some("ruby")), 2);
        // A changed global tab-width flows to languages without an override.
        s.set("tab-width".into(), OptionValue::Int(8));
        assert_eq!(s.tab_width_for(Some("go")), 8);
        assert_eq!(s.tab_width_for(Some("ocaml")), 2); // override still wins
    }
}
