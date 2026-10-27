use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, Bounds, Context, CursorStyle, EntityInputHandler, ExternalPaths, FocusHandle, Focusable,
    Keystroke, MouseButton, Pixels, Point, RenderImage, SharedString, Task, UTF16Selection, Window,
    actions, div, point, prelude::*, px,
};
use notify_debouncer_mini::notify::RecommendedWatcher;
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use unicode_segmentation::UnicodeSegmentation;

use crate::anvil::{AnvilEngine, AnvilOutcome};
use crate::buffer::Buffer;
use crate::element::{EditorElement, LineLayout};
use crate::highlight::{Edit, Highlighter, HlSpan};
use crate::lsp::{
    self, CompletionItem, Diagnostic, LanguageServer, LspClient, RawDiagnostic, SteelLsp,
};
use crate::mode::{Keymap, Mode, ModeStack};
use crate::script::{EditorCommand, ScriptEngine, ScriptOutcome};
use crate::sexp::{last_sexp, top_level_form_at};
use crate::terminal::{TermEvent, Terminal};
use crate::theme::Theme;

mod anvil_repl;
mod buffers;
mod commands;
mod completion;
mod dap;
#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod e2e_tests;
mod editing_extras;
#[cfg(test)]
mod editing_tests;
mod evil;
mod ex;
mod grammars;
mod grid;
mod grid_cursors;
#[cfg(test)]
mod grid_dedup_tests;
#[cfg(test)]
mod grid_e2e_tests;
#[cfg(test)]
mod grid_editor_tests;
mod grid_element;
mod grid_formula;
mod grid_ops;
#[cfg(test)]
mod grid_regression_tests;
mod grid_sheets;
mod grid_wrap;
mod grid_xlsx;
mod helm;
mod input;
mod isearch;
mod json_print;
#[cfg(test)]
mod key_tests;
mod kill_ring;
mod lisp_indent;
mod lsp_methods;
mod mcp;
mod minibuffer;
mod modeline;
mod net;
mod overlays;
mod panes;
mod process;
mod registers;
#[cfg(test)]
mod save_tests;
mod session;
mod special_select;
#[cfg(test)]
mod special_select_tests;
mod structural;
mod terminal;
mod undo;
mod vim;

pub use helm::HelmView;
use helm::{HelmAction, HelmSession, HelmSource, helm_sources_from_specs, wrap_index};
use input::UargState;
use isearch::IsearchState;
use lisp_indent::{LispIndent, LispIndentRules, is_lisp_language, lisp_indent_for};
use minibuffer::MinibufferHistory;
use modeline::{MlSeg, default_mode_line_format, parse_mode_line_format};
use overlays::{Overlays, shift_line_map};
use panes::{Child, SplitAxis, WindowLayout, union_bounds};
use process::ProcessHandle;
use session::BoundedRing;
pub use structural::StructuralOp;
use terminal::keystroke_to_bytes;
use undo::{UndoGrouping, UndoRecord, UndoTree};

// Keys that the OS treats specially are captured as GPUI actions (bound by
// [`key_bindings`]) rather than through the Scheme keymap, then delegated to the same commands.
actions!(
    editor,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        Newline,
        Tab,
        SelectLeft,
        SelectRight,
        SelectAll,
    ]
);

/// The OS-special keys bound to the actions above, in the editor's key context.
pub fn key_bindings() -> Vec<gpui::KeyBinding> {
    use gpui::KeyBinding;
    vec![
        KeyBinding::new("backspace", Backspace, Some("Editor")),
        KeyBinding::new("delete", Delete, Some("Editor")),
        KeyBinding::new("left", Left, Some("Editor")),
        KeyBinding::new("right", Right, Some("Editor")),
        KeyBinding::new("up", Up, Some("Editor")),
        KeyBinding::new("down", Down, Some("Editor")),
        KeyBinding::new("home", Home, Some("Editor")),
        KeyBinding::new("end", End, Some("Editor")),
        KeyBinding::new("enter", Newline, Some("Editor")),
        KeyBinding::new("tab", Tab, Some("Editor")),
        KeyBinding::new("shift-left", SelectLeft, Some("Editor")),
        KeyBinding::new("shift-right", SelectRight, Some("Editor")),
        KeyBinding::new("cmd-a", SelectAll, Some("Editor")),
    ]
}

/// An action deferred until the user confirms it in a yes/no minibuffer.
/// The kill variants carry the buffer's name: the prompt is asynchronous, so
/// the active buffer may change (e.g. process output activating another
/// buffer) before the user answers.
#[derive(Clone)]
enum PendingAction {
    KillBuffer(String),
    KillBufferAndWindow(String),
    /// Open this (very large) file as a grid anyway.
    OpenLargeGrid(String),
    /// Save this buffer although its file changed on disk since it was read.
    SaveOverChanged(String),
}

/// Which way a search or a selection cycle moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Forward,
    Backward,
}

impl Direction {
    /// `Forward` when `forward`, else `Backward` — for callers that still hold
    /// a `bool` direction.
    pub(super) fn from_forward(forward: bool) -> Self {
        if forward {
            Self::Forward
        } else {
            Self::Backward
        }
    }

    pub(super) fn is_forward(self) -> bool {
        self == Self::Forward
    }
}

/// What a minibuffer read is for, deciding completion candidates and what
/// happens on submit.
#[derive(Clone)]
enum MinibufferPurpose {
    /// `M-x`: run the entered command name.
    ExecuteCommand,
    /// `C-x C-f`: visit the entered file path.
    FindFile,
    /// `C-x b`: switch to the entered buffer name.
    SwitchBuffer,
    /// A yes/no confirmation gating `action`.
    Confirm(PendingAction),
    /// Quit-time save offer: `yes` saves all modified buffers then quits, `no`
    /// quits discarding changes, `C-g` cancels the quit. `relaunch`: the quit
    /// was requested by the update pill, so relaunch into the staged build
    /// once (and only once) the quit actually goes ahead.
    QuitSave { relaunch: bool },
    /// LSP completion: replace the word at `completion_range` with the choice.
    CompleteAtPoint,
    /// evil `/` (forward) / `?` (backward) — regex search.
    Search(Direction),
    /// evil `d/pat` / `d?pat`: the search motion of a pending operator.
    OperatorSearch(Direction),
    /// Emacs `C-s` / `C-r` — incremental search. The query is read in the
    /// minibuffer and matched live on every keystroke (see `isearch_*`).
    Isearch,
    /// evil `:` — an ex command line (e.g. `%s/a/b/g`, `w`, `q`).
    ExCommand,
    /// Grid `gn` / `C-x g`: jump to a cell address (`C7`, `B2:D9`).
    GridGoto,
    /// Grid `/` (forward) / `C-r` / `?` (backward): search cell contents
    /// (regex).
    GridSearch(Direction),
    /// Grid helix `s`: put a cursor on each selected cell matching a regex.
    GridSelectMatching,
    /// Saving a CSV grid that holds formulas: the `.xlsx` path to save as.
    GridSaveAsXlsx,
    /// Grid `grid-filter`: a filter for column `.0`.
    GridFilter(usize),
    /// Grid `grid-complete-cell`: the new value of cell `(row, col)`,
    /// completing against `completing_candidates`.
    GridComplete(usize, usize),
    /// Grid `grid-jump-to-column`: a column, completing on header names.
    GridColumn,
    /// `M-:` — evaluate the entered Steel expression, echoing the result.
    EvalExpression,
    /// `read-string`: call the named Steel fn with the entered text.
    SchemeCallback(String),
    /// `completing-read`: complete against `completing_candidates`, then call fn.
    SchemeCompleting(String),
    /// Dynamic `completing-read`: candidates come from a Scheme fn re-queried per
    /// keystroke (see `mb_dynamic`); shown as-is (the fn does the filtering).
    SchemeCompletingDynamic(String),
    /// `read-file-name`: file-path completion, then call fn with the path.
    SchemeReadFile(String),
    /// `y-or-n-p`: call fn with "yes" or "no".
    SchemeYesNo(String),
    /// First-trust confirmation for the grammar install at the front of
    /// `pending_grammar_installs`. `yes` records trust + installs; either way the
    /// next queued install (if any) is then prompted.
    ConfirmGrammarInstall,
}

/// One buffer's full state. The active buffer lives in [`Editor::active`];
/// the others are snapshots in [`Editor::inactive`]. Switching buffers swaps
/// whole [`Document`]s (see `snapshot_active` / `install_active`), so a
/// buffer-local field can never leak across a switch.
struct Document {
    /// Buffer name shown in the mode line (e.g. `*scratch*`, `main.scm`).
    name: String,
    /// File this buffer is visiting, if any.
    path: Option<PathBuf>,
    buffer: Buffer,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    mark_active: bool,
    undo: UndoTree,
    scroll_offset: Pixels,
    modes: ModeStack,
    highlighter: Option<Highlighter>,
    /// Unsaved changes since the last load/save.
    modified: bool,
    /// Whether this buffer's mode hooks have run (Emacs: once per buffer).
    hooks_ran: bool,
    /// When `Some(mode)`, this is a read-only program-generated buffer (the
    /// magit pattern): plain keys are commands resolved against `mode`'s keymap
    /// rather than self-inserting.
    special: Option<String>,
    /// Per-line whole-line face overrides, by line index (special buffers).
    line_faces: HashMap<usize, SharedString>,
    /// Per-line icons (char column to paint at, asset name, optional tint face —
    /// `None` takes the line's face), for special buffers like the file tree.
    /// Mirrors `line_faces`.
    line_icons: HashMap<usize, Vec<(usize, SharedString, Option<SharedString>)>>,
    /// Gutter dot faces by line index — breakpoint signs (`set-gutter-dot`,
    /// dap.scm). Mirrors `line_faces`.
    gutter_dots: HashMap<usize, SharedString>,
    /// Per-line inline image: `line -> image file path`. The decoded bitmap lives
    /// in the editor-wide [`Editor::image_cache`]; this just records which lines
    /// display which image (Org inline images / LaTeX previews).
    line_images: HashMap<usize, (PathBuf, usize)>,
    /// Overlays / text properties / folds for this buffer.
    overlays: Overlays,
    /// Buffer-local variables (Scheme `buffer-local-set`/`-get`).
    locals: HashMap<String, String>,
    /// Working-tree-vs-HEAD gutter marks by line index (git-gutter / diff-hl),
    /// recomputed by `magit-gutter-refresh`. Mirrors `line_faces`.
    diff_marks: HashMap<usize, DiffMark>,
    /// Copilot-style inline suggestion (`ghost-text-show`), if one is showing.
    ghost: Option<GhostText>,
    /// For a grid buffer (a CSV/TSV shown as a spreadsheet): its data and
    /// view. The rope above stays empty; see [`Document::save_bytes`].
    grid: Option<Box<grid::GridDoc>>,
    /// A grid shown as raw text (`grid-toggle-view`): what only the grid
    /// holds, put back when it turns into a grid again.
    grid_stash: Option<Box<grid::GridStash>>,
    /// For the ANVIL REPL buffer: where its editable input region starts (see
    /// [`anvil_repl`]). Moves with edits like a marker.
    repl_input: Option<usize>,
}

impl Document {
    /// The bytes this buffer saves as: the serialized workbook for a grid,
    /// the rope's text otherwise. Every write path goes through here, so an
    /// inactive grid can never be saved as its (empty) rope.
    ///
    /// A CSV grid holding formulas or formatting can't be written — CSV
    /// stores neither — so it's an error naming the fix (save it as `.xlsx`).
    fn save_bytes(&self) -> std::io::Result<Vec<u8>> {
        match &self.grid {
            // Only part of a big file is parsed yet: saving now would cut it.
            Some(g) if g.loading.is_some() => Err(std::io::Error::other(format!(
                "{} is still loading — save it once the load finishes",
                self.name
            ))),
            Some(g) if g.csv_blocked() => Err(std::io::Error::other(format!(
                "{} has {}, which CSV can't store — save it as .xlsx",
                self.name,
                g.csv_would_lose().unwrap_or("formulas")
            ))),
            Some(g) if !g.xlsx.lossy.is_empty() => Err(std::io::Error::other(format!(
                "{} has {}, which saving over it would lose — save a copy with :w",
                self.name,
                g.xlsx.lossy.join(", ")
            ))),
            Some(g) => match g.encoding_error() {
                Some(e) => Err(std::io::Error::other(e)),
                None => Ok(g.bytes()),
            },
            None => Ok(self.buffer.full_text().into_bytes()),
        }
    }
}

/// The URI identifying `doc` for LSP. File buffers use a `file://` URI;
/// unsaved buffers (e.g. `*scratch*`) get a stable `untitled:` URI so they can
/// still be served (the built-in Steel LSP just uses it as a key).
fn document_uri(doc: &Document) -> String {
    match doc.path.as_deref() {
        Some(path) => lsp::path_to_uri(path),
        None => format!("untitled:{}", doc.name),
    }
}

/// A copilot-style inline suggestion: display-only text rendered dim at point.
/// It occupies no buffer bytes (offsets, search, undo, and save are untouched)
/// and is deliberately ephemeral — any edit clears it (see [`Editor::edit`]),
/// and it only renders while point still sits at `at` (see
/// [`Editor::ghost_at`]), so cursor motion hides it without hooking every
/// movement path.
struct GhostText {
    /// Byte offset of point when the ghost was shown.
    at: usize,
    text: String,
}

/// A working-tree-vs-HEAD change marker for one buffer line — the git-gutter /
/// diff-hl feature. Computed by [`crate::script::git_diff_marks`] and painted in
/// the line-number gutter's far-left edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffMark {
    /// New since HEAD — a full-height bar (`magit-gutter-added`).
    Added,
    /// Content changed since HEAD — a full-height bar (`magit-gutter-modified`).
    Modified,
    /// One or more lines were deleted just above this one — a small wedge at the
    /// line's top edge (`magit-gutter-removed`). Deletions own no line of their
    /// own, so the mark rides the line that now follows the removed text.
    Removed,
}

/// A decoded raster image kept in the editor-wide cache, with its intrinsic
/// pixel size so the layout can compute how many rows it needs.
#[derive(Clone)]
struct CachedImage {
    data: Arc<RenderImage>,
    width: u32,
    height: u32,
}

/// Inline images wider than this (logical px) are scaled down to fit; narrower
/// ones display at their natural size. A fixed cap (rather than the pane width)
/// keeps row-height math independent of window size, so folds/scroll/cursor all
/// stay consistent. Content past the pane edge is clipped by the viewport mask.
const INLINE_IMAGE_MAX_WIDTH: f32 = 600.0;

/// Cap on the `*Messages*` scrollback ring (every `echo` appends here).
const MESSAGES_CAP: usize = 1000;

/// Cap (in lines) on an inactive log buffer (`*Messages*` / `*lsp-log*`) —
/// trimmed from the front so background streaming can't grow it unboundedly.
const LOG_BUFFER_LINES_CAP: usize = 5000;

/// The shared "this buffer has no ready language server" echo, used by every
/// LSP entry point that can't dispatch.
pub(super) const NO_LSP_FOR_BUFFER: &str = "No language server for this buffer";

/// How long a save waits on the server's format-on-save response before
/// writing the buffer unformatted.
const FORMAT_ON_SAVE_TIMEOUT: Duration = Duration::from_secs(3);

/// An active minibuffer prompt: a single-line input read in the echo area.
struct Minibuffer {
    prompt: String,
    input: String,
    /// Byte offset of point within `input`.
    cursor: usize,
    /// Transient hint shown after the input (e.g. completion feedback).
    message: String,
    purpose: MinibufferPurpose,
    /// When set, C-g/Escape cannot abort this read — it stays open until the
    /// user submits an answer. Used for permission prompts that must resolve a
    /// pending request (a dropped read would strand the requester forever).
    sticky: bool,
    /// Set for a secret read (`read-passwd`): the input renders as `•`, never
    /// enters or browses history, is passed untrimmed, and is dispatched as a
    /// secret call (errors can't echo it). Holding the guard keeps macOS
    /// Secure Event Input on for exactly as long as this read is open.
    secret: Option<crate::secure_input::SecureInputGuard>,
    /// Steel fn called with "" when this read ends WITHOUT a submit — C-g, or
    /// displaced by another read — so a caller waiting on it can unwind.
    on_cancel: Option<String>,
}

/// A snapshot of the active minibuffer for rendering, split at the cursor.
pub struct MinibufferView {
    pub prompt: String,
    pub before: String,
    pub after: String,
    pub message: String,
}

/// Filesystem watching (auto-revert + dired-sidebar/dired live refresh),
/// reconciled as a unit by [`Editor::reconcile_watches`].
struct FsWatchState {
    /// The debounced OS watcher. Its background thread forwards each coalesced
    /// batch onto `task`; dropping it stops all watches. `None` if the platform
    /// watcher failed to initialise (the feature then silently no-ops).
    debouncer: Option<Debouncer<RecommendedWatcher>>,
    /// Keeps the FS-event drain task alive.
    task: Option<Task<()>>,
    /// Paths currently registered with the watcher (visiting files + watched
    /// dirs), so `reconcile_watches` adds/removes only the delta.
    watched: HashSet<PathBuf>,
    /// Directories to watch on behalf of each Scheme owner (`dired-sidebar`, `dired`).
    /// Replace-per-owner via `(watch-directories owner dirs)`.
    owners: HashMap<String, Vec<PathBuf>>,
    /// Flattened union of `owners`' values, for O(1) "is this changed path
    /// inside a watched dir?" membership tests in `on_fs_events`.
    dirs: HashSet<PathBuf>,
    /// Recursive watches (whole subtree), keyed by Scheme owner — for magit
    /// watching a repo root. Reconciled with `RecursiveMode::Recursive`.
    tree_owners: HashMap<String, Vec<PathBuf>>,
    /// Flattened recursive roots, for descendant matching in `on_fs_events`.
    trees: HashSet<PathBuf>,
    /// Paths currently watched recursively (delta tracking, mirrors `watched`).
    watched_recursive: HashSet<PathBuf>,
}

/// Drag-select autoscroll. While the pointer is held near/past the top or
/// bottom edge during a drag-selection, a self-rescheduling timer scrolls the
/// view and extends the selection. `dy` is the signed per-tick velocity in
/// pixels (0 = idle), `pos` the last pointer position to re-hit-test from, and
/// `active` guards against spawning more than one timer chain at a time.
struct AutoscrollState {
    active: bool,
    dy: f32,
    pos: Point<Pixels>,
}

/// A press in a special buffer that has a `mouse-1` binding. As in Emacs
/// (`mouse-1-click-follows-link`), the binding runs on the release only when
/// the pointer stayed within [`CLICK_SLOP`] of `origin`; a press that moves
/// further is a drag and selects text instead.
struct SpecialClick {
    origin: Point<Pixels>,
    expr: String,
}

/// How far (in pixels, either axis) the pointer may move between press and
/// release and still count as a click rather than a drag.
const CLICK_SLOP: f32 = 4.0;

/// Whether the pointer moved far enough from `origin` to make a press a drag.
fn past_click_slop(origin: Point<Pixels>, now: Point<Pixels>) -> bool {
    (f32::from(now.x) - f32::from(origin.x)).abs() > CLICK_SLOP
        || (f32::from(now.y) - f32::from(origin.y)).abs() > CLICK_SLOP
}

/// Cursor blink state, mutated as a unit by the blink timer chain.
struct BlinkState {
    /// Whether the cursor is currently shown. Always true when `cursor-blink`
    /// is off. Toggled by a self-rescheduling timer and reset to true on every
    /// keystroke (so the cursor stays solid while typing).
    visible: bool,
    /// Bumped on each keystroke to invalidate in-flight blink timers, so the
    /// blink chain restarts from "shown" after activity (and old chains die).
    token: u64,
    /// Whether the OS window is active (focused). The cursor only blinks while
    /// active; when the window loses focus the blink stops and the cursor holds
    /// steady (shown).
    window_active: bool,
    /// Subscription to window activate/deactivate, set up lazily on first
    /// render (kept alive by storing it). Drives `window_active`.
    observer: Option<gpui::Subscription>,
}

/// The marker pair backing one secondary (multiple-cursors / Helix) selection:
/// `head` is the moving cursor end, `anchor` the fixed end. For a bare cursor
/// the anchor marker sits on the head (empty selection, no highlight); Helix's
/// selection-first model sets them apart so each secondary selection paints its
/// own span. Both are ids in the script engine's marker store, so they track
/// their positions across edits.
#[derive(Clone, Copy)]
struct SelectionMarkers {
    head: u64,
    anchor: u64,
}

/// A text editor over a single [`Buffer`], with a stack of active [`Mode`]s and
/// an embedded [`ScriptEngine`]. This is the GPUI entity that owns all buffer
/// state; the [`EditorElement`] only draws it.
pub struct Editor {
    /// The active buffer's full state ([`Document`]): text, selection, undo,
    /// modes, highlighter, overlays, and per-line decorations. Switching
    /// buffers swaps this whole value with an entry of `inactive` (see
    /// `snapshot_active` / `install_active`), so per-buffer state can never
    /// leak across a switch.
    active: Document,
    pub focus_handle: FocusHandle,
    pub script: ScriptEngine,
    /// Drains async script outcomes from the engine thread onto the foreground.
    script_task: Option<Task<()>>,
    /// The ANVIL REPL: a second, independent Steel interpreter (see
    /// [`crate::anvil`]). Used by `anvil-eval` to run forms in an isolated,
    /// interruptible image without touching the config engine.
    pub anvil: AnvilEngine,
    /// Drains async ANVIL REPL results onto the foreground.
    anvil_task: Option<Task<()>>,
    /// User Lisp indent rules by head symbol (`lisp-indent-rule`).
    lisp_indent_rules: LispIndentRules,
    /// Forms submitted in the ANVIL REPL, oldest first (M-p / M-n).
    anvil_history: Vec<String>,
    /// The history entry M-p / M-n last recalled, if any.
    anvil_history_pos: Option<usize>,
    /// The operator the REPL's eldoc last looked up (re-query on change only).
    anvil_eldoc_last: Option<String>,
    /// Global key bindings, shared across all buffers.
    keymap: Keymap,
    /// Echo-area message (the minibuffer line).
    pub status: SharedString,

    is_selecting: bool,
    /// A press in a special buffer whose `mouse-1` binding waits for the
    /// release — see [`SpecialClick`].
    special_click: Option<SpecialClick>,
    /// The special buffer holding a keyboard selection (evil/helix `v`), if
    /// any — see `special_select.rs`.
    special_select: Option<String>,
    /// Drag-select autoscroll state — see [`AutoscrollState`].
    autoscroll: AutoscrollState,
    /// Scrollback of echo-area messages (Emacs `*Messages*`), capped. Every
    /// `echo` appends here so transient status can be reviewed later.
    messages: BoundedRing,
    /// Messages echoed since startup (the ring above drops old ones), so the
    /// MCP `press_keys` tool can tell which messages its keys produced.
    echo_count: usize,
    /// The MCP server (src/mcp.rs), when the `mcp-server` option is on.
    mcp: Option<mcp::McpState>,
    /// The window this editor renders in, recorded on first render: the MCP
    /// `press_keys` tool dispatches keystrokes through it.
    window_handle: Option<gpui::AnyWindowHandle>,
    /// Its request log, shown by `M-x mcp-server-info` (capped).
    mcp_log: BoundedRing,
    /// Captured stderr from external language servers (Emacs-style `*lsp-log*`),
    /// capped — the only window into why a server failed/crashed.
    lsp_log: BoundedRing,
    /// The diagnostic message currently echoed because the pointer is hovering
    /// it — so a later move off the diagnostic clears only our own echo.
    hover_message: Option<SharedString>,
    /// electric-pair-mode: auto-insert a closing delimiter after a typed opener.
    electric_pair: bool,
    /// Active yasnippet session: (field marker id, placeholder byte length) in
    /// tab order, plus the current field index. Empty when no snippet is active.
    snippet_fields: Vec<(u64, usize)>,
    snippet_index: usize,
    /// avy jump hints: (byte offset, label) painted over the buffer until a jump
    /// is made (or cleared). Empty when avy isn't active.
    avy_hints: Vec<(usize, SharedString)>,
    /// Secondary selections (multiple-cursors / Helix), each a head+anchor
    /// marker pair so they ride along on edits via the marker store. Edits
    /// (typed text, newline, delete) are replicated to each head. Empty when
    /// only the primary cursor is active.
    extra_cursors: Vec<SelectionMarkers>,
    /// When true the modal layer resolves plain keys against the `helix-*`
    /// keymaps instead of the `evil-*` ones (Helix's selection-first bindings,
    /// installed by scheme/helix.scm). The Normal/Insert state machine itself is
    /// shared with evil; only the keymap names change.
    modal_helix: bool,
    /// Mode name -> Steel functions to run when that mode is enabled.
    hooks: HashMap<String, Vec<String>>,
    /// Point's buffer line at the last `post-command-hook` fire, so the hook runs
    /// once per cursor-line change rather than on every intra-command move.
    last_hook_line: Option<usize>,
    /// Scheme completion-at-point functions, tried in order before LSP.
    capf_functions: Vec<String>,

    kill_ring: Vec<String>,
    kill_index: usize,
    last_yank: Option<Range<usize>>,
    last_was_yank: bool,
    /// Open undo groups (see [`UndoGrouping`]): while one is open, edits
    /// append to one undo node.
    undo_grouping: UndoGrouping,

    /// Keys accumulated in an in-progress prefix sequence (e.g. after `C-x`).
    pending_keys: Vec<String>,
    /// Emacs universal-argument (`C-u` / `M-<digit>`) state — see [`UargState`].
    uarg: UargState,
    /// Vim/Helix count prefix being typed in Normal/Visual state (`3` in `3dd`).
    /// Accumulates digits until a binding resolves, which then runs that many
    /// times. Distinct from [`UargState`]: the evil layer swallows plain keys
    /// before the universal-argument layer ever sees them, and `C-u` is bound to
    /// half-page scroll under evil, so Emacs's prefix arg can't serve here.
    modal_count: Option<usize>,
    /// The active minibuffer prompt, if a read is in progress.
    minibuffer: Option<Minibuffer>,
    /// Candidates for an active completing minibuffer (`completing-read`, the
    /// grid's cell/column reads, LSP `CompleteAtPoint`).
    completing_candidates: Vec<String>,
    /// Shared minibuffer input history — see [`MinibufferHistory`].
    minibuffer_history: MinibufferHistory,
    /// For a dynamic `completing-read` (candidates from a Scheme fn): the fn name
    /// and the latest query id, re-dispatched per keystroke. `None` otherwise.
    mb_dynamic: Option<(String, u64)>,

    /// Set whenever point moves and the view should follow it (`ensure_cursor_visible`),
    /// consumed by the post-paint `reconcile_cursor_scroll`. Gating the reconcile on
    /// this flag means it only corrects the scroll right after a cursor move — it does
    /// NOT fight a passive repaint, so a mouse-wheel scroll away from the cursor sticks
    /// instead of being yanked back to the (unmoved) cursor.
    cursor_follow_pending: bool,
    /// Horizontal scroll of the selected pane, in pixels. Always 0 when word-wrap
    /// is on; otherwise it shifts long lines left so off-screen text is reachable.
    h_scroll_offset: Pixels,
    lines: Vec<LineLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    viewport_height: Pixels,
    /// Text-area width of the selected pane (past the gutter), for h-scroll clamp.
    viewport_width: Pixels,
    /// One monospace character's advance, reported by the renderer — used to
    /// follow the cursor horizontally when word-wrap is off.
    char_width: Pixels,
    /// Mirror of the `word-wrap` setting, cached from the renderer so the
    /// (cx-less) scroll helpers can branch on it.
    word_wrap: bool,

    /// Grammar installs from untrusted repos awaiting first-trust confirmation
    /// (see `install_grammar`). A queue, not a single slot, so several
    /// `install-language` calls in one `init.scm` each get prompted in turn
    /// rather than clobbering one another. The front is the one being prompted.
    pending_grammar_installs: VecDeque<crate::grammar::InstallSpec>,

    /// Process cwd, resolved once at startup (the editor never `chdir`s). Used as
    /// the fallback `default-directory` for fileless buffers so `publish_point`
    /// doesn't make a `current_dir` syscall on every snapshot publish.
    process_cwd: String,
    /// Files whose async `find_file` read is in flight, by canonical path — a
    /// second visit of the same file meanwhile is dropped instead of racing
    /// to create a duplicate buffer.
    pending_opens: HashSet<PathBuf>,
    /// Other open buffers (snapshots), most-recently-used last.
    inactive: Vec<Document>,

    // --- LSP (one server per language, many at once) ---
    /// Configured servers: LSP language id -> launch command (from `set-lsp`).
    lsp_commands: HashMap<String, lsp_methods::LspCommand>,
    /// File extension -> server key / config language (from `set-extension`).
    extensions: HashMap<String, String>,
    /// File extension -> wire `languageId` sent in `didOpen` (from
    /// `set-language-id`). Distinct from `extensions`: several extensions can
    /// share one server (e.g. `ts`/`tsx` → the `typescript` server) yet must
    /// announce different ids (`typescript` vs `typescriptreact`) so the server
    /// parses JSX. Falls back to the server key when unset.
    language_ids: HashMap<String, String>,
    /// Running servers, keyed by language id.
    servers: HashMap<String, LspServer>,
    /// Latest diagnostics per URI, in LSP coordinates (any server).
    diagnostics: HashMap<String, Vec<RawDiagnostic>>,
    /// Jump targets `(uri, line, character)` for the rows of the current
    /// `*lsp-xref*` / `*lsp-symbols*` list buffer (RET visits row N's target).
    xref_targets: Vec<(String, usize, usize)>,
    /// Location ring for jump-back (`xref-pop-marker`, Emacs `pop-tag-mark`):
    /// `(path, byte offset)` pushed before each definition/reference jump.
    xref_marker_stack: Vec<(Option<PathBuf>, usize)>,
    /// Code actions backing the rows of the current `*lsp-actions*` buffer.
    code_actions: Vec<lsp::CodeAction>,
    /// The buffer `code_actions` were requested in — where applying one must
    /// return to before editing.
    code_actions_source: Option<String>,
    /// Languages whose buffers are LSP-formatted on save (set via the Scheme
    /// `lsp-language` config / `(set-lsp-format-on-save …)`).
    lsp_format_on_save: HashSet<String>,
    /// Saves waiting on an LSP format response, keyed by the formatted
    /// document's URI → the token its `Format` request carries. The response
    /// (or the timeout, or the server dying) writes THAT document — not
    /// whichever buffer happens to be active by then.
    formatting_for_save: HashMap<String, u64>,
    /// Source of `formatting_for_save` tokens, so a stale timeout or a late
    /// response can't complete a newer save of the same file.
    format_save_seq: u64,
    /// In-flight `$/progress` reports, `(language:token, title, text)` in
    /// begin order; the newest is shown in the `lsp-progress` mode-line slot.
    lsp_progress: Vec<(String, String, String)>,
    /// Replacement range for the active completion minibuffer (its candidates
    /// are in `completing_candidates`).
    completion_range: Option<Range<usize>>,
    /// The in-buffer completion popup (corfu-style), when active.
    completion_popup: Option<CompletionPopup>,
    /// An auto-triggered completion request is in flight (suppresses duplicate
    /// requests and the "no completions" echo while typing).
    completion_auto_pending: bool,
    /// Buffer state the in-flight async completion request was made against
    /// (see [`completion::CompletionStamp`]); a stale reply is dropped.
    completion_stamp: Option<completion::CompletionStamp>,
    /// The active Helm selection session, if any.
    helm: Option<HelmSession>,
    /// Monotonic id for dynamic-candidate queries (stale results are dropped).
    /// Shared by Helm sessions and dynamic `completing-read`s, so it outlives
    /// any one session.
    helm_query_counter: u64,
    /// Modal-editing (evil) state.
    evil: EvilState,
    /// Vim `r` replace-pending: armed by `evil-replace-char`, it makes the very
    /// next printable keystroke overwrite the character under the cursor instead
    /// of being dispatched as a command. Cleared by that keystroke or `Escape`.
    evil_replace_pending: bool,
    /// How many chars the pending `r` overwrites (`3rx`).
    evil_replace_count: usize,
    /// Vim grammar state: pending operator, `%c` capture, … (see vim.rs).
    vim: vim::VimState,
    /// The count published to Scheme as `(prefix-count)` while a count-aware
    /// command is dispatched (0 otherwise).
    prefix_count: usize,
    /// Command names that take their count via `(prefix-count)` instead of
    /// being run N times (`set-count-aware!`).
    count_aware: HashSet<String>,
    /// A counted Scheme command still repeating (see `dispatch_counted`).
    count_repeat: Option<CountRepeat>,
    /// While applying repetition 2..N of a counted command: kills append to the
    /// newest kill-ring entry so `3dd` then `p` puts back all three lines.
    kill_append: bool,
    /// Set when a kill was appended, so the combined text reaches the clipboard.
    kill_appended: bool,
    /// A `read-char` capture in flight: the Scheme callback the very next
    /// printable keystroke is passed to (vim `f`/`t`/`F`/`T` char-find and
    /// friends). Cleared by that keystroke; any non-text chord cancels.
    read_char_pending: Option<String>,
    /// A sticky `read-key-start` capture: the Scheme callback EVERY keystroke
    /// is passed to (as a key name) until `read-key-stop` / Escape / C-g.
    /// query-replace's y/n/!/q dialog runs on it.
    read_key_pending: Option<String>,
    /// Undo state recorded by `(undo-amalgamate-begin)`; the matching end
    /// squashes every undo node added since into one.
    undo_amalgamate: Option<UndoBase>,
    /// True until startup config (bundled scripts + `init.scm`) has been applied.
    /// While set, plain keystrokes are swallowed so they can't self-insert before
    /// `(evil-mode)` runs — the window can be revealed by the load watchdog before
    /// config finishes, and a stray `j`/`k` in that gap must not land as text.
    config_pending: bool,
    /// The last `/`/`?` or `C-s` regex, for `n`/`N` repeat and match highlight.
    search_query: Option<String>,
    /// The compiled `search_query`, reused across frames (the match highlight
    /// asks every paint; recompiling the regex each time was pure waste).
    search_regex: std::cell::RefCell<Option<(String, regex::Regex)>>,
    /// Direction of the last evil `/` (true) or `?` (false) search, so `n`
    /// repeats it in the same direction and `N` in the opposite (vim semantics).
    evil_search_forward: bool,
    /// Whether the last evil yank/delete was line-wise (`yy`/`dd`) rather than
    /// character-wise (`yw`/visual). Decides how `p`/`P` paste.
    evil_yank_linewise: bool,
    /// Incremental-search (`C-s`) transient state — `Some` exactly while an
    /// `Isearch` minibuffer is open (see [`IsearchState`]).
    isearch: Option<IsearchState>,
    /// The last completed `C-s` string, so an empty `C-s` recalls it (Emacs).
    /// Lives outside [`Self::isearch`]: it must survive the search closing.
    last_isearch: Option<String>,
    /// Search ring + command history kept for session persistence (savehist).
    session: session::SessionRings,
    /// `C-l` (recenter-top-bottom) cycle position: 0 = middle, 1 = top, 2 = bottom.
    /// Advances on each consecutive press; resets when point has moved since.
    recenter_cycle: usize,
    /// Point at the last `C-l`, to detect a fresh (non-consecutive) invocation.
    recenter_point: usize,
    /// Decoded inline images, shared across buffers and keyed by file path so a
    /// re-scan (toggle) reuses the bitmap instead of re-decoding. Not swapped on
    /// buffer switch — it's a process-wide bitmap cache.
    image_cache: HashMap<PathBuf, CachedImage>,
    /// The open transient popup, if any (the magit / `transient.el` pattern).
    transient: Option<TransientState>,
    /// The title-bar menus (`set-titlebar-menu`, scheme/titlebar-menu.scm),
    /// left to right in first-publish order — the perspectives switcher
    /// registers first. Empty ones are kept (hidden) so they hold their place.
    titlebar_dropdowns: Vec<TitlebarDropdownState>,
    /// Named mode-line registers written from Scheme (`set-mode-line-slot`),
    /// e.g. "project" → "macros", "git" → "main". A `Slot` segment in
    /// `mode_line_format` looks up its value here; absent/empty = renders nothing.
    mode_line_slots: HashMap<String, SharedString>,
    /// The ordered mode-line segments, the Emacs `mode-line-format` analogue.
    /// Defined in Scheme (`scheme/modeline.scm`, via `set-mode-line-format`);
    /// seeded with [`default_mode_line_format`] so the bar works pre-Scheme.
    mode_line_format: Vec<MlSeg>,

    // --- Windows (split panes) ---
    /// All window panes (the selected one is the live view onto the buffer).
    panes: Vec<Pane>,
    /// The split layout tree over `panes`.
    layout: WindowLayout,
    /// The selected pane's id — where editing and the cursor live.
    selected_pane: usize,
    /// The previously selected pane, for `C-w p`.
    last_pane: Option<usize>,
    /// Monotonic id source for new panes.
    next_pane_id: usize,
    /// Each pane's last painted screen bounds, for click-to-focus routing.
    pane_bounds: Vec<(usize, Bounds<Pixels>)>,
    /// The completion popup's last painted geometry (selected pane only), for
    /// mapping a click to a candidate row. `None` when no popup is showing.
    completion_popup_hit: Option<PopupHit>,
    /// Fold chevron click regions from the last paint (selected pane only), for
    /// mapping a gutter click to the line whose fold should toggle.
    fold_hitboxes: Vec<FoldHitbox>,
    /// While dragging a split divider: the split node's path + axis.
    resizing: Option<ResizeDrag>,
    /// Live async child processes, by id (for stdin/kill).
    processes: HashMap<u64, ProcessHandle>,
    /// Live DAP sessions, by id (for dap-send / dap-kill). The transport
    /// (framing, adapter process/socket) lives in [`crate::dap::DapClient`];
    /// the protocol state machine lives in Scheme (dap.scm).
    dap_sessions: HashMap<u64, crate::dap::DapClient>,
    /// Cancel flags for in-flight HTTP streams, by id. `http-stream-cancel` flips
    /// the flag; the stream's reader thread checks it between lines and stops.
    http_streams: HashMap<u64, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Stop flags for running local HTTP servers, by bound port. `http-server-stop`
    /// flips the flag; the server's accept loop checks it each poll and exits.
    http_servers: HashMap<u16, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Command senders for live async sockets (`socket-connect-async`) and
    /// WebSockets (`ws-connect`), by id. Each worker thread owns its stream and
    /// drains this channel for outbound sends and a close request.
    async_sockets: HashMap<u64, std::sync::mpsc::Sender<crate::net::SockCmd>>,
    /// Live embedded terminals, by id. A `term-mode` buffer's name maps to one
    /// of these via [`Terminal::buffer_name`].
    terminals: HashMap<u64, Terminal>,
    /// Sender cloned into each terminal so its background event loop can wake the
    /// editor (redraw / title / exit). Drained by `terminal_task`.
    term_events_tx: futures::channel::mpsc::UnboundedSender<(u64, TermEvent)>,
    /// Keeps the terminal-event drain task alive.
    terminal_task: Option<Task<()>>,
    /// Full-sync LSP coalescing: an edit to a full-document-sync server records
    /// the active `(language, uri)` here instead of serializing and shipping the
    /// whole document on every keystroke. Flushed on a short debounce (so live
    /// diagnostics still update), before any outbound request, and before the
    /// active buffer is swapped out.
    lsp_fullsync_dirty: Option<(String, String)>,
    /// Wakes the debounce task when a full-sync edit marks the document dirty.
    lsp_flush_tx: futures::channel::mpsc::UnboundedSender<()>,
    /// Last painted geometry of each terminal pane, for mapping mouse positions
    /// to grid cells (set by `TerminalElement` each frame).
    terminal_geom: HashMap<u64, TermGeom>,
    /// The terminal id whose selection a drag is currently extending, if any.
    terminal_drag: Option<u64>,
    /// Editor-wide grid state: settings, mouse and echo bookkeeping, the
    /// `ge` popup (see [`grid::GridUi`]).
    grid_ui: grid::GridUi,
    /// The interpreter for Scheme functions called from formulas, and its
    /// calls in flight (see [`grid_formula::GridFnState`]).
    grid_fns: grid_formula::GridFnState,
    /// The keyboard macro being recorded (`C-x (`), if any.
    macro_recording: Option<Vec<MacroEvent>>,
    /// The last completed keyboard macro, for replay (`C-x e`).
    last_macro: Vec<MacroEvent>,
    /// Cursor blink state (phase, timer token, window focus) — see [`BlinkState`].
    blink: BlinkState,

    /// Filesystem watching (auto-revert + dired-sidebar/dired live refresh) —
    /// see [`FsWatchState`].
    fs: FsWatchState,
    /// While `render-buffer` refreshes a special buffer that's shown in a
    /// *non-selected* pane (the dired-sidebar/dired view during an auto-refresh), the
    /// name of that inactive [`Document`] — so the icon/overlay commands in the
    /// same batch target it instead of the active buffer, and focus never moves.
    /// Resolved by name ([`Editor::redirect_index`]) so reshuffling `inactive`
    /// can't retarget it. Reset to `None` after each applied command batch.
    render_redirect: Option<String>,
    /// Set by a failed `GuardBufferUnchanged` (the buffer was edited after the
    /// script snapshotted it); `apply_script_outcome` drops the rest of that
    /// batch — its byte-offset decorations are stale. Reset around every batch.
    script_batch_stale: bool,
    /// `edit-file-text` reads awaiting their `set-file-text`, by file identity.
    file_edits: HashMap<PathBuf, buffers::FileEdit>,
}

/// An open transient popup: a title and its `(key, label, command)` rows.
struct TransientState {
    title: String,
    columns: Vec<crate::script::TransientColumn>,
    /// When set, selecting a row does NOT auto-close the popup — the row's
    /// command decides (re-open to refresh, or `transient-quit` to dismiss). For
    /// menus where you set several options in a row (e.g. agent model + mode).
    sticky: bool,
}

/// One rendered transient row: the key to press and what it does.
pub struct TransientRow {
    pub key: String,
    pub label: String,
    /// The bare command name this row runs, for click-to-invoke.
    pub command: String,
}

/// A rendered transient column: an optional header and its rows.
pub struct TransientColumnView {
    pub title: Option<String>,
    pub rows: Vec<TransientRow>,
}

/// A render snapshot of the transient popup (one or more side-by-side columns).
pub struct TransientView {
    pub title: String,
    pub columns: Vec<TransientColumnView>,
}

/// One title-bar menu, as published from Scheme via `set-titlebar-menu` (the
/// perspectives switcher is one). The editor owns the open/closed state, the
/// live filter, and the keyboard selection; the items and commands are
/// Scheme's.
pub(crate) struct TitlebarDropdownState {
    /// The menu's title: its identity across re-publishes.
    pub(crate) key: String,
    pub(crate) items: Vec<SharedString>,
    /// Per-item Scheme command, parallel to `items`; a missing or empty entry
    /// falls back to `callback`.
    pub(crate) commands: Vec<String>,
    /// The checkmarked item, shown on the trigger in place of `trigger`.
    pub(crate) active: SharedString,
    /// The Scheme function a click hands the picked label to.
    pub(crate) callback: String,
    /// The trigger's label while nothing is active (the title).
    pub(crate) trigger: SharedString,
    /// The dim heading over the rows.
    pub(crate) section: SharedString,
    /// The empty search field's placeholder.
    pub(crate) placeholder: SharedString,
    /// Whether the menu is unfolded beneath the title bar.
    pub(crate) open: bool,
    /// The search-field text; filters the visible rows (case-insensitive
    /// substring). Cleared whenever the menu opens or closes.
    pub(crate) filter: String,
    /// Caret position in `filter`, as a CHAR index (the filter is a short
    /// single line, so char-indexed ops are cheap and avoid byte/char mixups).
    pub(crate) cursor: usize,
    /// Keyboard selection, as an index into the FILTERED row list. `None`
    /// until an arrow key summons it — a freshly opened menu shows no
    /// highlight (the checkmark marks the active item; hover marks the mouse).
    pub(crate) selected: Option<usize>,
}

/// A render snapshot of one title-bar dropdown for `Workspace::render`. The
/// rows are pre-filtered; each carries its index into the full item list so a
/// click can name the item stably while the filter narrows.
pub struct TitlebarDropdownView {
    pub key: String,
    /// The trigger's label: the active item, else the dropdown's name.
    pub trigger: SharedString,
    pub section: SharedString,
    pub placeholder: SharedString,
    /// `(original_index, label)` for each row surviving the filter.
    pub items: Vec<(usize, SharedString)>,
    pub active: SharedString,
    pub open: bool,
    pub filter: String,
    /// Caret position in `filter` as a char index, for painting the caret.
    pub cursor: usize,
    /// Index into `items` of the keyboard-selected row, if arrows summoned one.
    pub selected: Option<usize>,
}

/// A message from an async worker (HTTP stream, socket, …): one output line,
/// or the stream closing.
enum ProcMsg {
    Line(String),
    Exit,
}

/// A terminal pane's last painted geometry, for mapping a mouse position to a
/// grid cell. `origin` is the pane's top-left; `cell_w`/`cell_h` are one cell.
#[derive(Clone, Copy)]
struct TermGeom {
    origin: Point<Pixels>,
    cell_w: Pixels,
    cell_h: Pixels,
    cols: usize,
    rows: usize,
}

/// One step of a keyboard macro: a resolved command, or self-inserted text.
#[derive(Clone)]
enum MacroEvent {
    Command(String),
    /// A modal command run under a count (`3dd`), replayed with that count.
    Counted(String, usize),
    Text(String),
    /// A key token typed by `:normal` (`x`, `<Esc>`, `<CR>`, …).
    Key(String),
}

/// Merge runs of consecutive [`EditorCommand::Insert`] into a single insert.
/// Each applied insert triggers a tree-sitter reparse and an LSP incremental
/// change, so a script inserting one character at a time otherwise pays that
/// cost per character. Only strictly-adjacent inserts merge — any other command
/// is a barrier — and the merge is byte-for-byte equivalent to applying them in
/// sequence (each insert lands at point, which the previous one advanced).
fn coalesce_inserts(commands: Vec<EditorCommand>) -> Vec<EditorCommand> {
    let mut out: Vec<EditorCommand> = Vec::with_capacity(commands.len());
    for command in commands {
        match (out.last_mut(), command) {
            (Some(EditorCommand::Insert(prev)), EditorCommand::Insert(next)) => {
                prev.push_str(&next);
            }
            (_, command) => out.push(command),
        }
    }
    out
}

/// An in-progress divider drag: which split (by path from the layout root) and
/// its axis ([`SplitAxis::Vertical`] = the divider is horizontal / stacked
/// split).
#[derive(Clone)]
struct ResizeDrag {
    path: Vec<Child>,
    axis: SplitAxis,
}

/// One window pane: a view onto a buffer with its own point and scroll. The
/// selected pane's buffer is the live active buffer; other panes render from
/// their buffer's inactive [`Document`]. This is the Emacs model — buffers are a
/// shared pool, windows are views — adapted to Macros' single-active-buffer
/// editor: only the selected pane is "live" (editable); switching panes
/// (`other-window`) makes that pane's buffer active.
struct Pane {
    id: usize,
    buffer: String,
    /// Saved point/scroll, used while this pane isn't the selected one.
    point: usize,
    scroll: Pixels,
}

/// Parse a yasnippet template into its plain text plus its tab-stop fields as
/// `(field_number, byte_offset_in_text, placeholder_len)`. Supports `$1`/`$2`/…,
/// `$0` (final stop), and `${1:default}` (placeholder text). Fields are returned
/// in tab order: ascending number, with `$0` last. An unrecognized `$` is kept
/// literally.
fn parse_snippet(template: &str) -> (String, Vec<(u32, usize, usize)>) {
    let mut text = String::new();
    let mut fields: Vec<(u32, usize, usize)> = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            text.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next();
                let mut num = String::new();
                while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                    num.push(*d);
                    chars.next();
                }
                let mut default = String::new();
                if chars.peek() == Some(&':') {
                    chars.next();
                    while let Some(&d) = chars.peek() {
                        chars.next();
                        if d == '}' {
                            break;
                        }
                        default.push(d);
                    }
                } else if chars.peek() == Some(&'}') {
                    chars.next();
                }
                if let Ok(n) = num.parse::<u32>() {
                    fields.push((n, text.len(), default.len()));
                    text.push_str(&default);
                }
            }
            Some(d) if d.is_ascii_digit() => {
                let mut num = String::new();
                while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                    num.push(*d);
                    chars.next();
                }
                if let Ok(n) = num.parse::<u32>() {
                    fields.push((n, text.len(), 0));
                }
            }
            _ => text.push('$'),
        }
    }
    fields.sort_by_key(|&(n, _, _)| if n == 0 { u32::MAX } else { n });
    (text, fields)
}

/// The single string a Scheme ANVIL callback receives. An error leads with
/// `error: ` (so a callback can test the prefix) followed by any captured
/// output; a value follows its output, matching a transcript.
fn anvil_callback_payload(
    result: Result<Option<String>, String>,
    output: Option<String>,
) -> String {
    let output = output
        .as_deref()
        .map(|o| o.trim_end_matches('\n'))
        .filter(|o| !o.is_empty())
        .map(str::to_string);
    match (result, output) {
        (Err(error), Some(out)) => format!("error: {error}\n{out}"),
        (Err(error), None) => format!("error: {error}"),
        (Ok(Some(value)), Some(out)) => format!("{out}\n{value}"),
        (Ok(None), Some(out)) => out,
        (Ok(value), None) => value.unwrap_or_default(),
    }
}

/// Largest buffer (bytes) the reader-aware Lisp bracket match parses whole.
const LISP_MATCH_LIMIT: usize = 1024 * 1024;

/// The closing partner for an opening delimiter, if `c` opens a pair.
fn open_delim_partner(c: char) -> Option<&'static str> {
    match c {
        '(' => Some(")"),
        '[' => Some("]"),
        '{' => Some("}"),
        '"' => Some("\""),
        '\'' => Some("'"),
        '`' => Some("`"),
        _ => None,
    }
}

/// Whether `c` is a closing delimiter electric-pair may "type over".
fn is_close_delim(c: char) -> bool {
    matches!(c, ')' | ']' | '}' | '"' | '\'' | '`')
}

/// Byte range [start,end) spanning lines [s,e) of `buf` (end-exclusive line
/// indices; `e` past the last line clamps to end-of-buffer). The byte start of
/// line `e` is just past line `e-1`'s break, so the range includes that break.
fn line_span(buf: &Buffer, s: usize, e: usize) -> (usize, usize) {
    let lc = buf.line_count();
    let sb = if s >= lc {
        buf.len()
    } else {
        buf.line(s).0.start
    };
    let eb = if e >= lc {
        buf.len()
    } else {
        buf.line(e).0.start
    };
    (sb, eb)
}

/// State for the floating completion popup: the full candidate set, where the
/// completed word starts, and which (filtered) row is selected. The visible set
/// is `items` filtered by the live prefix `buffer[anchor..point]`.
struct CompletionPopup {
    items: Vec<CompletionItem>,
    anchor: usize,
    selected: usize,
}

/// One rendered completion row: the label plus the optional signature/kind the
/// element shows according to the Scheme `completion-show-*` options.
pub struct CompletionRow {
    pub label: String,
    pub detail: Option<String>,
    pub kind: Option<String>,
}

/// A render snapshot of the completion popup: the visible rows, which one is
/// selected, and the selected item's documentation (resolved lazily).
pub struct CompletionView {
    pub items: Vec<CompletionRow>,
    pub selected: usize,
    pub doc: Option<String>,
}

/// The painted completion popup's clickable geometry, reported back from the
/// element each frame so `on_mouse_down` can hit-test a click to a row.
#[derive(Clone, Copy)]
pub struct PopupHit {
    /// The rows region in window coordinates (excludes the doc side-panel).
    pub bounds: Bounds<Pixels>,
    /// Height of one row.
    pub row_height: Pixels,
    /// Filtered-item index of the first visible row.
    pub start: usize,
    /// Number of visible rows.
    pub rows: usize,
}

/// A painted fold chevron's clickable region, reported by the element each frame
/// so `on_mouse_down` can route a gutter click to `(fold-toggle-at-line line)`.
#[derive(Clone, Copy)]
pub struct FoldHitbox {
    /// The chevron's bounds in window coordinates.
    pub bounds: Bounds<Pixels>,
    /// The buffer line the chevron sits on.
    pub line: usize,
}

/// A counted Scheme command mid-repetition (`3w` under Helix, `3dw`, …). Each
/// run is dispatched only after the previous one's commands have applied, so it
/// reads a fresh snapshot — dispatching all N up front would hand every run the
/// same stale pre-command snapshot and the command would act once.
struct CountRepeat {
    /// The expr wrapped with a trailing `(count-repeat-step)`.
    source: String,
    total: usize,
    done: usize,
    /// Undo-tree state when the chain began, so the runs squash into one undo.
    undo_base: UndoBase,
}

/// Where the undo tree stood before a multi-run command, for
/// [`Editor::squash_undo_since`].
#[derive(Clone)]
struct UndoBase {
    buffer: String,
    len: usize,
    current: usize,
    /// A grid buffer's undo depth then (its history is separate from the
    /// text undo tree); `(undo-amalgamate-end)` folds the grid steps since.
    grid: Option<usize>,
}

/// Evil (modal editing) state. `Normal`/`Visual` intercept plain keys as
/// commands; `Insert` is ordinary self-inserting editing; `Disabled` is plain
/// Emacs behaviour.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EvilState {
    Disabled,
    Normal,
    Insert,
    Visual,
    /// Vim line-wise Visual (`V`): the selection is always rounded out to whole
    /// lines (see [`Editor::visual_region`]).
    VisualLine,
    /// Vim block-wise Visual (`C-v`): a rectangular selection between the two
    /// corners `selected_range.start`/`.end` (see [`Editor::visual_block_rect`]).
    VisualBlock,
}

impl EvilState {
    /// The state's name as Scheme's `(evil-state)` reports it (`""` when
    /// modal editing is off).
    fn as_str(self) -> &'static str {
        match self {
            EvilState::Disabled => "",
            EvilState::Normal => "normal",
            EvilState::Insert => "insert",
            EvilState::Visual => "visual",
            EvilState::VisualLine => "visual-line",
            EvilState::VisualBlock => "visual-block",
        }
    }
}

/// A single running language server and the per-server state we track for it.
struct LspServer {
    /// Both backends are boxed: an `LspClient` holds a subprocess handle +
    /// channels, and a `SteelLsp` embeds a whole Steel engine.
    client: Box<dyn LanguageServer>,
    /// True once the `initialize` response has arrived.
    ready: bool,
    /// What the server advertised in its `initialize` result (defaults until
    /// then).
    caps: lsp::ServerCapabilities,
    /// The process is gone (stdout closed). Kept in the map — not ready, no
    /// open documents — until the next `maybe_start_lsp` / `lsp-restart`
    /// replaces it with a fresh spawn.
    exited: bool,
    /// URIs we've sent `didOpen` for to this server.
    open: HashSet<String>,
    versions: HashMap<String, i32>,
    /// The in-flight completion request, if any.
    pending_completion: Option<PendingCompletion>,
    /// In-flight non-completion requests (definition, references, rename, …),
    /// keyed by request id so the response can be routed to the right handler,
    /// with the document each was issued against.
    pending: HashMap<i64, (PendingRequest, RequestTarget)>,
    /// Cached semantic tokens per URI, for deltas.
    semantic_state: HashMap<String, SemanticCache>,
    /// The reader task forwarding this server's messages; dropped on removal.
    _task: Task<()>,
}

/// An in-flight completion request: its id, and the word range being
/// completed (the popup anchor and live filter prefix).
struct PendingCompletion {
    id: i64,
    range: Range<usize>,
}

/// A document's last semantic-tokens result, kept so a
/// `semanticTokens/full/delta` reply can be spliced onto it.
struct SemanticCache {
    /// The `resultId` to send as `previousResultId`, if the server gave one.
    result_id: Option<String>,
    /// The packed token data (groups of five integers).
    data: Vec<i64>,
}

/// The document an LSP request was issued against: its URI plus the buffer's
/// edit tick at send time ([`Buffer::tick`] — a document version that, unlike
/// the LSP one, also covers edits still waiting in the full-sync debounce).
/// Responses that edit or decorate a buffer resolve their target through this
/// rather than "whatever is active now", and drop themselves when the text has
/// changed since (their positions would be stale).
#[derive(Clone, Debug)]
struct RequestTarget {
    uri: String,
    tick: u64,
}

/// What a pending LSP request was for — picked up in `on_lsp_message` to route
/// the response to the right handler.
#[derive(Clone, Debug)]
enum PendingRequest {
    /// A Scheme-initiated request: deliver the raw result JSON to the named
    /// Steel callback. This is the generic bridge powering `(lsp-request …)`.
    Script(String),
    /// Go to a location: definition / declaration / type-definition /
    /// implementation. The label is shown if the result is a list.
    Goto(&'static str),
    /// A list of references / document highlights → the `*lsp-xref*` buffer.
    References,
    /// Document symbols → the `*lsp-symbols*` buffer.
    DocumentSymbols,
    /// Workspace symbols → the `*lsp-symbols*` buffer.
    WorkspaceSymbols,
    /// Hover → echo area (minibuffer).
    Hover,
    /// Signature help → echo.
    SignatureHelp,
    /// Rename → apply the resulting workspace edit.
    Rename,
    /// Formatting → apply the resulting text edits to the requesting buffer.
    /// `save` is the `formatting_for_save` token when this is format-on-save.
    Format { save: Option<u64> },
    /// Code actions → the `*lsp-actions*` buffer.
    CodeAction,
    /// `codeAction/resolve` result → apply the now-complete action (or, if the
    /// server returns nothing usable, the original one as-is).
    ResolveCodeAction(Box<lsp::CodeAction>),
    /// `prepareCallHierarchy` done → send the incoming/outgoing-calls request
    /// in that direction. The prepared item is echoed back verbatim.
    CallHierarchyPrepare(lsp::CallDirection),
    /// `incomingCalls`/`outgoingCalls` result → the `*lsp-xref*` buffer.
    CallHierarchyCalls(lsp::CallDirection),
    /// A semantic-tokens result (full / range / delta) → decode + deliver
    /// records to the named Steel callback.
    SemanticTokens {
        callback: String,
        kind: SemanticKind,
    },
    /// `prepareRename` result → open the rename prompt (or reject the position).
    PrepareRename,
    /// A `completionItem/resolve` result for the item with this label → fill in
    /// its documentation / auto-import edits.
    ResolveCompletion(String),
    /// `textDocument/codeLens` result → resolve + render the lenses.
    CodeLens,
    /// A `codeLens/resolve` result for the lens at this (line, character).
    ResolveCodeLens(usize, usize),
}

/// Which semantic-tokens request a [`PendingRequest::SemanticTokens`] is for.
#[derive(Clone, Copy, Debug)]
enum SemanticKind {
    /// `semanticTokens/full` — caches the resultId + data for later deltas.
    Full,
    /// `semanticTokens/range` — the viewport window; one-shot, no caching.
    Range,
    /// `semanticTokens/full/delta` — merge edits into the cached data.
    Delta,
}

impl Editor {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Keymaps are populated from Scheme at startup; the modes just need to
        // exist. `steel-mode` (the major mode) declares the highlighting
        // language and stacks on the always-present `fundamental` base.
        let modes = scheme_mode_stack(false);

        // The active highlighter follows the active mode's language.
        let highlighter = modes.active_language().and_then(Highlighter::for_language);

        // *scratch* defaults to Org mode (see org.scm), so the greeting uses Org
        // comment syntax (`#`) rather than Steel's `;;`.
        let welcome = "# Macros — the Emacs you know, with a modern engine\n\
                       # This buffer is for notes you don't want to save, in Org mode.\n\
                       # Movement: C-f/C-b/C-n/C-p, C-a/C-e, M-f/M-b\n\
                       # Kill/yank: C-k, C-w, M-w, C-y; mark: C-Space; undo: C-/\n\n";

        let (script, mut script_outcomes) = ScriptEngine::new();
        let (anvil, mut anvil_outcomes) = AnvilEngine::new();
        let (term_events_tx, mut term_events_rx) =
            futures::channel::mpsc::unbounded::<(u64, TermEvent)>();
        let (lsp_flush_tx, mut lsp_flush_rx) = futures::channel::mpsc::unbounded::<()>();

        // Filesystem watcher: the debouncer runs its own thread and hands us a
        // coalesced batch of changed paths, which we forward (deduped) onto the
        // foreground drain task below. Watches are added lazily by
        // `reconcile_watches`; if the platform watcher won't start we keep `None`
        // and the auto-revert / live-refresh features simply no-op.
        let (fs_tx, mut fs_rx) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
        let fs_debouncer = new_fs_debouncer(fs_tx);
        let fs_watcher_started = fs_debouncer.is_some();

        let mut editor = Self {
            active: Document {
                highlighter,
                ..blank_document("*scratch*".to_string(), None, welcome, modes)
            },
            focus_handle: cx.focus_handle(),
            script,
            script_task: None,
            anvil,
            anvil_task: None,
            lisp_indent_rules: LispIndentRules::new(),
            anvil_history: Vec::new(),
            anvil_history_pos: None,
            anvil_eldoc_last: None,
            keymap: Keymap::default(),
            status: "Macros ready".into(),
            is_selecting: false,
            special_click: None,
            special_select: None,
            messages: BoundedRing::new(MESSAGES_CAP),
            echo_count: 0,
            mcp: None,
            window_handle: None,
            mcp_log: BoundedRing::new(mcp::MCP_LOG_CAP),
            lsp_log: BoundedRing::new(lsp_methods::LSP_LOG_CAP),
            hover_message: None,
            electric_pair: false,
            snippet_fields: Vec::new(),
            snippet_index: 0,
            avy_hints: Vec::new(),
            extra_cursors: Vec::new(),
            modal_helix: false,
            hooks: HashMap::new(),
            last_hook_line: None,
            capf_functions: Vec::new(),
            kill_ring: Vec::new(),
            kill_index: 0,
            last_yank: None,
            last_was_yank: false,
            undo_grouping: UndoGrouping::default(),
            pending_keys: Vec::new(),
            minibuffer: None,
            completing_candidates: Vec::new(),
            minibuffer_history: MinibufferHistory::default(),
            mb_dynamic: None,
            uarg: UargState::default(),
            modal_count: None,
            autoscroll: AutoscrollState {
                active: false,
                dy: 0.0,
                pos: point(px(0.), px(0.)),
            },
            cursor_follow_pending: false,
            h_scroll_offset: px(0.),
            viewport_width: px(0.),
            char_width: px(8.),
            word_wrap: true,
            lines: Vec::new(),
            last_bounds: None,
            line_height: px(0.),
            viewport_height: px(0.),
            pending_grammar_installs: VecDeque::new(),
            process_cwd: startup_directory(
                std::env::current_dir().ok(),
                crate::runtime::home_dir(),
            ),
            pending_opens: HashSet::new(),
            inactive: Vec::new(),
            lsp_commands: HashMap::new(),
            extensions: HashMap::new(),
            language_ids: HashMap::new(),
            servers: HashMap::new(),
            diagnostics: HashMap::new(),
            xref_targets: Vec::new(),
            xref_marker_stack: Vec::new(),
            code_actions: Vec::new(),
            code_actions_source: None,
            lsp_format_on_save: HashSet::new(),
            formatting_for_save: HashMap::new(),
            format_save_seq: 0,
            lsp_progress: Vec::new(),
            completion_range: None,
            completion_popup: None,
            completion_auto_pending: false,
            completion_stamp: None,
            helm: None,
            helm_query_counter: 0,
            evil: EvilState::Disabled,
            evil_replace_pending: false,
            evil_replace_count: 1,
            vim: vim::VimState::default(),
            prefix_count: 0,
            count_aware: HashSet::new(),
            count_repeat: None,
            kill_append: false,
            kill_appended: false,
            read_char_pending: None,
            read_key_pending: None,
            undo_amalgamate: None,
            config_pending: true,
            search_query: None,
            search_regex: std::cell::RefCell::new(None),
            evil_search_forward: true,
            evil_yank_linewise: false,
            isearch: None,
            last_isearch: None,
            session: session::SessionRings::default(),
            recenter_cycle: 0,
            recenter_point: usize::MAX,
            image_cache: HashMap::new(),
            transient: None,
            titlebar_dropdowns: Vec::new(),
            mode_line_slots: HashMap::new(),
            mode_line_format: default_mode_line_format(),
            panes: vec![Pane {
                id: 0,
                buffer: "*scratch*".to_string(),
                point: 0,
                scroll: px(0.),
            }],
            layout: WindowLayout::Leaf(0),
            selected_pane: 0,
            last_pane: None,
            next_pane_id: 1,
            pane_bounds: Vec::new(),
            completion_popup_hit: None,
            fold_hitboxes: Vec::new(),
            resizing: None,
            processes: HashMap::new(),
            async_sockets: HashMap::new(),
            dap_sessions: HashMap::new(),
            http_streams: HashMap::new(),
            http_servers: HashMap::new(),
            terminals: HashMap::new(),
            term_events_tx,
            lsp_fullsync_dirty: None,
            lsp_flush_tx,
            terminal_task: None,
            terminal_geom: HashMap::new(),
            terminal_drag: None,
            grid_ui: Default::default(),
            grid_fns: Default::default(),
            macro_recording: None,
            last_macro: Vec::new(),
            blink: BlinkState {
                visible: true,
                token: 0,
                window_active: true,
                observer: None,
            },
            fs: FsWatchState {
                debouncer: fs_debouncer,
                task: None,
                watched: HashSet::new(),
                owners: HashMap::new(),
                dirs: HashSet::new(),
                tree_owners: HashMap::new(),
                trees: HashSet::new(),
                watched_recursive: HashSet::new(),
            },
            render_redirect: None,
            script_batch_stale: false,
            file_edits: HashMap::new(),
        };
        // Drain async script outcomes from the engine thread onto the foreground.
        editor.script_task = Some(cx.spawn(async move |this, cx| {
            while let Some(outcome) = script_outcomes.next().await {
                if this
                    .update(cx, |editor, cx| editor.apply_script_outcome(outcome, cx))
                    .is_err()
                {
                    break; // editor dropped
                }
            }
        }));
        // Drain debounced filesystem-change batches onto the foreground: revert
        // unmodified buffers whose file changed, and fire `directory-changed-hook`
        // for watched dirs (dired-sidebar / dired refresh). Only spawned when the
        // watcher actually started — with no watcher the channel is already
        // closed, and a drain task would be dead weight.
        if fs_watcher_started {
            editor.fs.task = Some(cx.spawn(async move |this, cx| {
                while let Some(paths) = fs_rx.next().await {
                    if this
                        .update(cx, |editor, cx| editor.on_fs_events(paths, cx))
                        .is_err()
                    {
                        break; // editor dropped
                    }
                }
            }));
        }
        // Drain ANVIL REPL results from its engine thread onto the foreground.
        editor.anvil_task = Some(cx.spawn(async move |this, cx| {
            while let Some(outcome) = anvil_outcomes.next().await {
                if this
                    .update(cx, |editor, cx| editor.apply_anvil_outcome(outcome, cx))
                    .is_err()
                {
                    break; // editor dropped
                }
            }
        }));
        // Coalesce full-sync LSP did_change: when a burst of edits wakes us, wait
        // for typing to settle, then send the whole document once.
        cx.spawn(async move |this, cx| {
            while lsp_flush_rx.next().await.is_some() {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                // Swallow signals that arrived during the debounce window — the
                // single flush below already covers them.
                while lsp_flush_rx.try_recv().is_ok() {}
                if this
                    .update(cx, |editor, _| editor.flush_lsp_fullsync())
                    .is_err()
                {
                    break; // editor dropped
                }
            }
        })
        .detach();
        // Drain terminal events (redraw / title / exit) from the alacritty event
        // loops onto the foreground.
        editor.terminal_task = Some(cx.spawn(async move |this, cx| {
            while let Some((id, event)) = term_events_rx.next().await {
                if this
                    .update(cx, |editor, cx| editor.on_terminal_event(id, event, cx))
                    .is_err()
                {
                    break; // editor dropped
                }
            }
        }));
        // Quitting takes make-process children (own process groups, so no
        // terminal SIGHUP reaches them) and embedded terminals down with us.
        cx.on_app_quit(|editor, _| {
            editor.shutdown_processes();
            // Dropping a DAP client kills its adapter's process group / socket,
            // so no adapter or debuggee outlives the editor.
            editor.dap_sessions.clear();
            async {}
        })
        .detach();
        editor.parse_full();
        editor.sync_buffers();
        // Start the cursor-blink loop now, so the cursor blinks from the first
        // frame (not only after the first keystroke). No-op when blinking is off.
        editor.schedule_blink(cx);
        editor
    }

    /// Apply what a dispatched script sent back: commands + echo, or refreshed
    /// dynamic Helm candidates.
    fn apply_script_outcome(&mut self, outcome: ScriptOutcome, cx: &mut Context<Self>) {
        match outcome {
            ScriptOutcome::Commands { commands, message } => {
                // All edits from one Scheme command batch undo as one unit.
                self.begin_undo_group();
                // Merge runs of adjacent `Insert`s so a script that inserts
                // character-by-character pays one tree-sitter reparse + LSP
                // did_change instead of one per character. Safe only when no
                // `after-change-functions` hook would observe the per-edit
                // granularity; undo is unaffected (the batch is one group).
                let commands = if self.has_active_hook("after-change-functions") {
                    commands
                } else {
                    coalesce_inserts(commands)
                };
                // A failed `GuardBufferUnchanged` in the batch aborts everything
                // after it: the script computed those commands against text the
                // user has since edited, so their byte offsets are stale.
                self.script_batch_stale = false;
                // Repetition 2..N of a counted command: its kills extend the
                // first run's kill-ring entry (see `dispatch_counted`).
                let has_step = commands
                    .iter()
                    .any(|c| matches!(c, EditorCommand::CountRepeatStep));
                self.kill_append =
                    self.count_repeat.as_ref().is_some_and(|r| r.done > 0) && has_step;
                for command in commands {
                    if self.script_batch_stale {
                        break;
                    }
                    self.apply_command(command, cx);
                }
                self.kill_append = false;
                self.sync_appended_kill(cx);
                self.evil_clamp_cursor();
                self.script_batch_stale = false;
                // `render-buffer` may have redirected icon/overlay writes to a
                // sidebar buffer for this batch; that scope ends here.
                self.render_redirect = None;
                self.end_undo_group();
                // A chained counted run (`3x` on a Scheme command) that errored
                // never reached its `(count-repeat-step)`: end the chain here,
                // squashing the runs that did apply, or `count_repeat` stays set
                // (corrupting later kills) and the undo steps stay split.
                if !has_step
                    && message.as_deref().is_some_and(|m| m.starts_with("error: "))
                    && let Some(r) = self.count_repeat.take()
                {
                    self.squash_undo_since(&r.undo_base);
                }
                if let Some(message) = message {
                    self.echo(message, cx);
                }
            }
            ScriptOutcome::Candidates {
                source,
                query_id,
                candidates,
            } => {
                // `usize::MAX` is the minibuffer's dynamic `completing-read`;
                // real source indices belong to Helm.
                if source == usize::MAX {
                    // The minibuffer has no annotation column — keep just the
                    // candidate text.
                    let names = candidates.into_iter().map(|(c, _)| c).collect();
                    self.mb_set_dynamic_candidates(query_id, names, cx);
                } else if source == usize::MAX - 1 {
                    // Helm preview lines (candidate text only).
                    let lines = candidates.into_iter().map(|(c, _)| c).collect();
                    self.helm_set_preview(query_id, lines, cx);
                } else {
                    self.helm_set_candidates(source, query_id, candidates, cx);
                }
            }
            ScriptOutcome::Capf { start, end, items } => {
                self.apply_capf_result(start, end, items, cx);
            }
        }
    }

    /// Deliver a finished ANVIL REPL evaluation to its Scheme callback. The
    /// result is rendered as a single string (errors keep the `error: …` prefix
    /// the config engine uses), and handed to the named callback in the *main*
    /// engine, whose `(append-to-buffer …)` / `(message …)` then flows back
    /// through the normal outcome drain — exactly like a subprocess callback.
    fn apply_anvil_outcome(&mut self, outcome: AnvilOutcome, cx: &mut Context<Self>) {
        match outcome {
            AnvilOutcome::Result {
                callback,
                result,
                output,
            } => {
                // The REPL transcript renders natively, in place before the
                // input region (see `anvil_repl`).
                if callback == anvil_repl::ANVIL_REPL_CALLBACK {
                    let entry = anvil_repl::format_repl_result(&result, output.as_deref());
                    self.anvil_repl_render(&entry, cx);
                    return;
                }
                self.script
                    .dispatch_action(&callback, Some(&anvil_callback_payload(result, output)));
            }
            // Image-backed completion candidates → the normal completion popup.
            // Empty means no live binding matches; show "No completions" rather
            // than falling through to LSP (the REPL has no language server).
            AnvilOutcome::Completions { start, end, items } => {
                let Some(range) = self.completion_reply_range(start, end) else {
                    return; // the buffer changed under the request
                };
                let items = items
                    .into_iter()
                    .map(|label| CompletionItem {
                        insert_text: label.clone(),
                        label,
                        detail: None,
                        kind: None,
                        documentation: None,
                        additional_edits: Vec::new(),
                        data: serde_json::Value::Null,
                        resolved: true,
                    })
                    .collect();
                self.show_completion(items, range, cx);
            }
        }
    }

    // --- Accessors used by the element ------------------------------------

    pub fn cursor_offset(&self) -> usize {
        if self.active.selection_reversed {
            self.active.selected_range.start
        } else {
            self.active.selected_range.end
        }
    }

    pub fn set_layout(
        &mut self,
        lines: Vec<LineLayout>,
        bounds: Bounds<Pixels>,
        line_height: Pixels,
        char_width: Pixels,
        word_wrap: bool,
    ) {
        self.lines = lines;
        self.last_bounds = Some(bounds);
        self.line_height = line_height;
        self.viewport_height = bounds.size.height;
        self.viewport_width = bounds.size.width;
        self.char_width = char_width;
        self.word_wrap = word_wrap;
    }

    /// Record (or clear) the completion popup's painted geometry, so a click can
    /// be hit-tested to a candidate row in `on_mouse_down`.
    pub fn report_completion_popup(&mut self, hit: Option<PopupHit>) {
        self.completion_popup_hit = hit;
    }

    /// Record the fold chevrons' painted regions (selected pane), so a click in
    /// the gutter can be hit-tested to a fold line in `on_mouse_down`.
    pub fn report_fold_hitboxes(&mut self, hits: Vec<FoldHitbox>) {
        self.fold_hitboxes = hits;
    }

    /// Horizontal scroll of the selected pane (0 when word-wrap is on).
    pub fn h_scroll(&self) -> Pixels {
        self.h_scroll_offset
    }

    fn point(&self) -> usize {
        self.cursor_offset()
    }

    // --- Cursor / selection movement --------------------------------------

    /// Move point to `offset`, extending the region if the mark is active.
    fn goto(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.active.mark_active {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.active.selected_range = offset..offset;
        self.active.selection_reversed = false;
        self.reveal_point();
        self.ensure_cursor_visible();
        if let Some(message) = self.diagnostic_at_point() {
            self.status = message.into();
        }
        self.run_post_command_hook();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.active.selection_reversed {
            self.active.selected_range.start = offset;
        } else {
            self.active.selected_range.end = offset;
        }
        if self.active.selected_range.end < self.active.selected_range.start {
            self.active.selection_reversed = !self.active.selection_reversed;
            self.active.selected_range =
                self.active.selected_range.end..self.active.selected_range.start;
        }
        self.ensure_cursor_visible();
        self.run_post_command_hook();
        cx.notify();
    }

    /// Fire `post-command-hook` when point's line changed since the last fire —
    /// the signal current-line decorations (e.g. inline blame) re-place on. The
    /// hook dispatches asynchronously (no reentrancy into cursor movement), and
    /// `run_hook` short-circuits when nothing is registered, so this is free
    /// unless a Scheme feature opts in.
    fn run_post_command_hook(&mut self) {
        if !self.has_active_hook("post-command-hook") {
            return;
        }
        let line = self.active.buffer.line_index_at_byte(self.point());
        if self.last_hook_line == Some(line) {
            return;
        }
        self.last_hook_line = Some(line);
        self.run_hook("post-command-hook");
    }

    /// Target offset `delta` lines away, preserving the character column.
    /// Computed from the buffer so it works off-screen. Folded (invisible) lines
    /// are skipped, so moving over a collapsed section lands on the next visible
    /// line rather than inside the fold (where the cursor wouldn't be painted).
    fn line_offset(&self, delta: isize) -> usize {
        let cursor = self.point();
        let line = self.active.buffer.line_index_at_byte(cursor);
        let target = if self.active.overlays.folds.is_empty() {
            (line as isize + delta).clamp(0, self.active.buffer.line_count() as isize - 1) as usize
        } else {
            self.visible_line_offset(line, delta)
        };
        if target == line {
            return cursor;
        }
        let goal = self.active.buffer.char_column(cursor);
        self.active.buffer.byte_at_char_column(target, goal)
    }

    /// The line `delta` *visible* lines from `line`, skipping folded lines and
    /// clamping to the buffer. If there's no visible line that far in the given
    /// direction, returns the last reachable visible line (so the cursor stops
    /// at the section edge instead of vanishing into a fold).
    fn visible_line_offset(&self, line: usize, delta: isize) -> usize {
        let last = self.active.buffer.line_count().saturating_sub(1) as isize;
        let step: isize = if delta >= 0 { 1 } else { -1 };
        let mut remaining = delta.unsigned_abs();
        let mut cur = line as isize;
        while remaining > 0 {
            let mut next = cur + step;
            while next >= 0 && next <= last && self.is_line_invisible(next as usize) {
                next += step;
            }
            if next < 0 || next > last {
                break; // no further visible line this way
            }
            cur = next;
            remaining -= 1;
        }
        cur as usize
    }

    /// Reveal (unfold) any fold hiding the line that now holds point, so a jump
    /// that lands inside a collapsed region — search, goto-line, xref, an LSP
    /// jump — makes the destination and the cursor visible. Vertical line motion
    /// skips folds instead of revealing them (see `line_offset`), so ordinary
    /// up/down navigation never triggers this. No-op when nothing is folded.
    fn reveal_point(&mut self) {
        if self.active.overlays.folds.is_empty() {
            return;
        }
        let line = self.active.buffer.line_index_at_byte(self.point());
        if self.is_line_invisible(line) {
            self.active.overlays.folds.retain(|r| !r.contains(&line));
        }
    }

    /// The largest vertical scroll offset that still shows a full viewport of
    /// content: display rows (folds collapsed, inline-image/scale rows included)
    /// times the *painted* line height (`self.line_height`, not the unstyled
    /// `window.line_height()`), minus `viewport_h`, floored at 0. Every scroll
    /// clamp must go through this — clamping against the raw
    /// `buffer.line_count()` ignores folds and over-scrolls into blank space.
    fn max_scroll_offset(&self, viewport_h: f32) -> f32 {
        (self.display_line_count() as f32 * f32::from(self.line_height) - viewport_h).max(0.0)
    }

    fn ensure_cursor_visible(&mut self) {
        // Point moved (or a command asked to reveal it): let the post-paint
        // `reconcile_cursor_scroll` correct the wrapped-geometry estimate this frame.
        self.cursor_follow_pending = true;
        let lh = f32::from(self.line_height);
        let vh = f32::from(self.viewport_height);
        if lh <= 0.0 || vh <= 0.0 {
            return;
        }
        // Position in display rows (folds collapsed) so scrolling stays correct
        // when lines are hidden.
        let buffer_line = self.active.buffer.line_index_at_byte(self.point());
        let line = self.display_row_of_line(buffer_line) as f32;
        let top = line * lh;
        // A multi-line ghost suggestion reserves rows below point's line;
        // include them so showing one near the viewport bottom scrolls the
        // whole suggestion into view, not just its first line.
        let ghost_rows = match self.ghost_rows() {
            Some((g, rows)) if g == buffer_line => rows as f32,
            _ => 0.0,
        };
        let bottom = top + lh * (1.0 + ghost_rows);
        let mut scroll = f32::from(self.active.scroll_offset);
        if top < scroll {
            scroll = top;
        } else if bottom > scroll + vh {
            scroll = bottom - vh;
        }
        // The cursor's row in display-row units (1 line == 1 line-height). Under
        // word-wrap a line can occupy several visual rows, so this *underestimates*
        // the cursor's true pixel position — `reconcile_cursor_scroll` corrects the
        // remainder after paint, using the real wrapped geometry. The clamp upper
        // bound therefore must not pull `scroll` back below a value that a prior
        // post-paint correction set (the buffer-line content height is itself an
        // underestimate when lines wrap), or the cursor would jump off-screen for a
        // frame on every edit.
        let max = self
            .max_scroll_offset(vh)
            .max(f32::from(self.active.scroll_offset));
        self.active.scroll_offset = px(scroll.clamp(0.0, max));
        self.ensure_cursor_visible_horizontal();
    }

    /// Correct the vertical scroll against the cursor's *real* painted position.
    ///
    /// [`ensure_cursor_visible`] works in buffer-line units, which underestimates
    /// the cursor's pixel position whenever lines above it (or its own line)
    /// soft-wrap into multiple visual rows. The renderer knows the exact geometry,
    /// so after each paint it reports the cursor's top, relative to the viewport,
    /// and we nudge the scroll by just enough to bring it fully into view. This is
    /// the authoritative scroll-follow; converges in one extra frame.
    pub fn reconcile_cursor_scroll(&mut self, cursor_top: Option<Pixels>, cx: &mut Context<Self>) {
        // Only follow the cursor on the frame after it moved. On a passive repaint
        // (mouse-wheel scroll, fontify, resize) the flag is clear, so a scroll away
        // from the cursor sticks instead of being yanked back to it.
        if !std::mem::take(&mut self.cursor_follow_pending) {
            return;
        }
        let Some(cursor_top) = cursor_top else {
            return;
        };
        let lh = f32::from(self.line_height);
        let vh = f32::from(self.viewport_height);
        if lh <= 0.0 || vh <= 0.0 {
            return;
        }
        let top = f32::from(cursor_top);
        let bottom = top + lh;
        let scroll = f32::from(self.active.scroll_offset);
        // Only nudge when the cursor falls outside the viewport. Going past the
        // buffer-line content height is fine here (it's an underestimate under
        // wrapping) — the lower clamp of 0 is the only hard bound.
        let target = if bottom > vh {
            scroll + (bottom - vh)
        } else if top < 0.0 {
            scroll + top
        } else {
            scroll
        };
        let target = target.max(0.0);
        if (target - scroll).abs() > 0.5 {
            self.active.scroll_offset = px(target);
            cx.notify();
        }
    }

    /// Keep the cursor within the visible columns when word-wrap is off, by
    /// scrolling horizontally. Approximates the cursor's x as (byte-column ×
    /// char-width) — exact for ASCII, close enough for scroll-follow otherwise.
    /// With word-wrap on, the buffer never scrolls horizontally.
    fn ensure_cursor_visible_horizontal(&mut self) {
        if self.word_wrap {
            self.h_scroll_offset = px(0.);
            return;
        }
        let cw = f32::from(self.char_width);
        let vw = f32::from(self.viewport_width);
        if cw <= 0.0 || vw <= 0.0 {
            return;
        }
        let line_start = self
            .active
            .buffer
            .line_range(self.active.buffer.line_index_at_byte(self.point()))
            .start;
        let cursor_x = (self.point() - line_start) as f32 * cw;
        let margin = cw; // keep a character's breathing room at the edge
        let mut h = f32::from(self.h_scroll_offset);
        if cursor_x < h + margin {
            h = (cursor_x - margin).max(0.0);
        } else if cursor_x > h + vw - margin {
            h = cursor_x - vw + margin;
        }
        self.h_scroll_offset = px(h.max(0.0));
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        // The *real* line height from the last paint. NOT `window.line_height()`:
        // inside a mouse-event handler that returns the unstyled default, which
        // differs from the editor's font-driven height and would mis-compute the
        // wrapped-row index (`position.y / line_height`) — so a click on a soft-
        // wrapped line lands on the wrong visual row (cursor "jumps" elsewhere).
        let line_height = self.line_height;
        // Map a click to a byte offset within a (possibly wrapped) line. `top` is
        // the line's top relative to the element; `wrapped` knows how to resolve a
        // local (x, y) — including which visual row y falls on — to a byte index.
        let hit = |line: &LineLayout| {
            let local = point(
                position.x - bounds.left() + self.h_scroll_offset - line.indent,
                position.y - bounds.top() - line.top,
            );
            let idx = match line.wrapped.closest_index_for_position(local, line_height) {
                Ok(i) | Err(i) => i,
            };
            // `idx` is a display index; map it back past any hidden ranges.
            line.range.start
                + line
                    .from_display(idx)
                    .min(line.range.end - line.range.start)
        };
        for line in &self.lines {
            let bottom = bounds.top() + line.top + line.height;
            if position.y < bottom {
                return hit(line);
            }
        }
        self.lines.last().map_or(0, hit)
    }

    /// Byte offset at the start of the line a click at vertical position `y`
    /// falls on in the selected pane — computed straight from the pane's painted
    /// bounds and scroll, *not* `self.lines`. Unlike [`offset_for_position`], this
    /// stays correct on the very frame a pane's buffer is swapped in (which clears
    /// `self.lines`), so a click that also focuses a special pane lands on the
    /// right line. Column is the line start; special-buffer commands act per-line.
    fn line_start_for_click_y(&self, y: Pixels, line_height: Pixels) -> Option<usize> {
        let top = self
            .pane_bounds
            .iter()
            .find(|(id, _)| *id == self.selected_pane)
            .map(|(_, b)| b.top())?;
        let rel = f32::from(y - top) + f32::from(self.active.scroll_offset);
        let row = (rel.max(0.0) / f32::from(line_height).max(1.0)) as usize;
        let last = self.active.buffer.line_count().saturating_sub(1);
        Some(self.active.buffer.line_range(row.min(last)).start)
    }

    /// Index of the cached visible line containing `offset`, if it's on screen.
    fn line_index_at(&self, offset: usize) -> Option<usize> {
        self.lines
            .iter()
            .position(|l| offset >= l.range.start && offset <= l.range.end)
    }

    // --- Editing primitives (all edits funnel through `edit` for undo) -----

    /// Shift the active buffer's decorations to track an edit that replaced
    /// `[start, old_end)` with `new_len` bytes (`line_delta` net newlines, the
    /// edit beginning on `start_line`): its [`Overlays`] (see
    /// [`Overlays::adjust_for_edit`]) plus the line-keyed gutter dots and
    /// inline images, which otherwise stay on stale line numbers — a
    /// breakpoint dot or an Org inline image would sit beside the wrong line
    /// after a line is added above it, and nothing repaints them on edit.
    fn adjust_overlays(
        &mut self,
        start: usize,
        old_end: usize,
        new_len: usize,
        line_delta: isize,
        start_line: usize,
    ) {
        // Newlines inserted at column 0 push `start_line`'s own text down, so
        // its line decorations go with it — else Enter at the start of a
        // heading leaves the heading's scale/chevron on the now-empty line
        // above.
        let carry = line_delta != 0
            && old_end == start
            && self.active.buffer.line_range(start_line).start == start;
        let doc = &mut self.active;
        doc.overlays
            .adjust_for_edit(start, old_end, new_len, line_delta, start_line, carry);
        shift_line_map(&mut doc.gutter_dots, start_line, line_delta, carry);
        shift_line_map(&mut doc.line_images, start_line, line_delta, carry);
    }

    /// Whether replacing bytes `[a, b)` would touch a `read-only` text property
    /// range `[s, e)`: it may not overlap it, insert strictly inside or at its
    /// start (`s <= a < e`), or delete the char just before it (`b == s`, the
    /// newline that holds a protected line prefix in place). Inserting at `e`
    /// — right after a protected prefix — is allowed. All three cases reduce
    /// to `a < e && b >= s`.
    fn edit_hits_read_only(&self, a: usize, b: usize) -> bool {
        self.active
            .overlays
            .props
            .iter()
            .any(|(r, k, _)| k == "read-only" && a < r.end && b >= r.start)
    }

    /// Apply a buffer edit and record its inverse for undo. Returns the byte
    /// offset just past the inserted text. Highlighting is updated
    /// **incrementally**: the edit's positions are captured before the change,
    /// then tree-sitter reuses the unchanged subtrees on reparse.
    fn edit(&mut self, range: Range<usize>, new_text: &str) -> usize {
        // `read-only` text properties (occur-edit / wgrep's `file:line:`
        // prefixes) refuse any edit touching them; point stays put.
        if self.edit_hits_read_only(range.start, range.end) {
            self.status = "Text is read-only".into();
            return self.point();
        }
        // A ghost suggestion never survives a buffer change — the text it was
        // computed against is gone. Eager clearing here (every mutation funnels
        // through `edit`) is what keeps the ghost free of any byte bookkeeping.
        self.active.ghost = None;
        // Normalize once, up front, so the undo record, the highlighter's
        // InputEdit, LSP sync, and overlay shifts all agree with what the rope
        // actually does (it repairs bad ranges itself — see `Buffer::replace`).
        let range = self.active.buffer.clamp_range(range);
        let range_start = range.start;
        self.record_undo(UndoRecord {
            at: range.start,
            removed: self.active.buffer.slice_to_string(range.clone()),
            inserted: new_text.to_string(),
        });

        // Capture pre-edit byte/point positions while the old text still exists.
        let positions = self.active.highlighter.is_some().then(|| {
            (
                range.start,
                range.end,
                self.active.buffer.point_at(range.start),
                self.active.buffer.point_at(range.end),
            )
        });

        let cursor = self.apply_tracked_replace(range, new_text);
        self.active.modified = true;
        if let Some((start_byte, old_end_byte, start_point, old_end_point)) = positions {
            let new_end_byte = start_byte + new_text.len();
            let new_end_point = self.active.buffer.point_at(new_end_byte);
            let highlighter = self.active.highlighter.as_mut().unwrap();
            highlighter.apply_edit(&Edit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_point,
                old_end_point,
                new_end_point,
            });
            highlighter.reparse(self.active.buffer.rope());
        }
        // The change marks `[ ] .`.
        self.set_auto_mark('[', range_start);
        // `]` sits ON the last inserted char — its start byte, never mid-codepoint
        // (`len - 1` would land inside a multibyte char like `é`).
        let last_char = new_text.char_indices().next_back().map_or(0, |(i, _)| i);
        self.set_auto_mark(']', range_start + last_char);
        self.set_auto_mark('.', range_start);
        // Emacs `after-change-functions`. Guarded, so it costs nothing unless a
        // hook is registered (then it opts into a dispatch per edit).
        self.run_hook("after-change-functions");
        cursor
    }

    /// Replace the (already normalized) byte `range` with `new_text` and keep
    /// everything that tracks buffer positions in step — LSP incremental sync,
    /// Scheme markers, Vim marks / jump list, and byte-range overlays — but
    /// record nothing for undo. Shared by [`Self::edit`] and undo/redo replay
    /// (an undo is an ordinary document change as far as all of these are
    /// concerned). Returns the byte offset just past the inserted text.
    fn apply_tracked_replace(&mut self, range: Range<usize>, new_text: &str) -> usize {
        let (start, old_end) = (range.start, range.end);
        // Capture pre-change LSP positions BEFORE mutating the rope: skipping
        // this leaves an incremental-sync server (rust-analyzer, gopls, …)
        // permanently out of step with the buffer.
        let lsp_change = self.lsp_change_positions(start, old_end);
        // Line breaks removed (the editor's lines are `\n`-only, so this is a
        // line-index difference — no slice needed).
        let removed_newlines = self.active.buffer.line_index_at_byte(old_end)
            - self.active.buffer.line_index_at_byte(start);
        let cursor = self.active.buffer.replace(range, new_text);
        if let Some(change) = lsp_change {
            self.lsp_did_change(change, new_text);
        }
        // Move any markers tracking positions in this buffer, and the Vim
        // marks / jump list.
        self.script.adjust_markers(start, old_end, new_text.len());
        if let Some(input) = self.active.repl_input {
            self.active.repl_input = Some(anvil_repl::shift_repl_input(
                input,
                start,
                old_end,
                new_text.len(),
            ));
        }
        self.shift_vim_positions(start, old_end, new_text.len());
        // Shift byte-range overlays the same way so they stay aligned until the
        // debounced re-fontify runs — otherwise hidden ranges / faces below the
        // edit point at stale bytes for a frame and visibly flicker (e.g. an Org
        // heading's hidden stars momentarily reappear and its indent jumps), and
        // after an undo (with no after-change hook fired) nothing repaints them.
        let line_delta = new_text.matches('\n').count() as isize - removed_newlines as isize;
        let start_line = self.active.buffer.byte_to_position(start).0;
        self.adjust_overlays(start, old_end, new_text.len(), line_delta, start_line);
        cursor
    }

    /// Reparse the whole buffer from scratch. Used on initial load and after
    /// undo, where edit deltas aren't tracked.
    fn parse_full(&mut self) {
        if let Some(highlighter) = self.active.highlighter.as_mut() {
            highlighter.set_text(self.active.buffer.rope());
        }
    }

    /// Replace the active buffer's entire text with `new` (one undoable edit),
    /// reparse, and put point at `point` clamped to the new length. The shared
    /// "revert / reformat in place, preserving point" primitive used by
    /// revert-buffer, auto-revert, LSP formatting, workspace edits, and
    /// completion accepts.
    fn replace_whole_buffer(&mut self, new: &str, point: usize, cx: &mut Context<Self>) {
        let len = self.active.buffer.len();
        self.edit(0..len, new);
        self.parse_full();
        // Clamp AND floor to a char boundary: the old point can land inside a
        // multibyte char of the new text.
        let point = self.active.buffer.snap(point);
        self.move_to(point, cx);
    }

    /// Insert `text`, replacing the active region if any.
    fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let cursor = self.edit(self.active.selected_range.clone(), text);
        self.collapse_to(cursor);
        // Follow the cursor: plain self-insert, newlines, and tab-indent all route
        // through here, so without this the viewport won't scroll when typing past
        // the bottom of the view (the other edit helpers already do this).
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// electric-pair-mode handling for a single typed delimiter `ch` over
    /// `range`. Returns true if it fully handled the insertion (so the caller
    /// should not also self-insert): wraps an active region, types over an
    /// auto-inserted closer, or inserts an opener's partner and sits between.
    fn electric_pair_insert(
        &mut self,
        range: &Range<usize>,
        ch: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let c = ch.chars().next().unwrap();
        // In Lisp, `'` and `` ` `` are quote prefixes, never paired delimiters:
        // `'(` must not become `'()'`.
        if matches!(c, '\'' | '`') && self.is_lisp_buffer() {
            return false;
        }
        // Active region: wrap it in the delimiter pair.
        if !range.is_empty() {
            let Some(close) = open_delim_partner(c) else {
                return false;
            };
            let inner = self.active.buffer.slice_to_string(range.clone());
            let cursor = self.edit(range.clone(), &format!("{ch}{inner}{close}"));
            self.collapse_to(cursor);
            self.ensure_cursor_visible();
            cx.notify();
            return true;
        }
        let at = range.start;
        // Type over an auto-inserted closer rather than doubling it.
        if is_close_delim(c) {
            let nxt = self.active.buffer.next_grapheme(at);
            if self.active.buffer.slice_to_string(at..nxt) == ch {
                self.collapse_to(nxt);
                self.ensure_cursor_visible();
                cx.notify();
                return true;
            }
        }
        // After an opener, insert its partner and place point between the two —
        // but not right before a word char (e.g. an apostrophe in "don't").
        if let Some(close) = open_delim_partner(c) {
            let nxt = self.active.buffer.next_grapheme(at);
            let next_is_word = self
                .active
                .buffer
                .slice_to_string(at..nxt)
                .chars()
                .next()
                .is_some_and(|n| n.is_alphanumeric() || n == '_');
            if !next_is_word {
                let before = self.active.buffer.len();
                let cursor = self.edit(at..at, &format!("{ch}{close}"));
                // A refused edit (read-only text) returns point unchanged — only
                // step back between the pair when it was actually inserted.
                if self.active.buffer.len() != before {
                    self.collapse_to(cursor - close.len());
                }
                self.ensure_cursor_visible();
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Delete `range`, collapsing point to its start.
    fn delete_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range.is_empty() {
            return;
        }
        let cursor = self.edit(range, "");
        self.collapse_to(cursor);
        // Follow the cursor: backspace/delete/delete-word/kill all route through
        // here, so without this the viewport won't scroll to the new point — e.g.
        // backspacing the last (off-screen) line deletes it but the view stays put,
        // so it looks like nothing happened.
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Like `delete_range` but saves the deleted text to the kill ring.
    fn kill_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range.is_empty() {
            return;
        }
        // Vim `"_d`: the black hole register swallows the text.
        let reg = self.take_kill_register();
        if reg != Some('_') {
            // C-k/C-w/M-d also feed the OS clipboard.
            let killed = self.active.buffer.slice_to_string(range.clone());
            self.file_kill(registers::KillKind::Delete, reg, killed, false, cx);
        }
        self.delete_range(range, cx);
    }

    fn collapse_to(&mut self, offset: usize) {
        self.active.selected_range = offset..offset;
        self.active.selection_reversed = false;
        self.active.marked_range = None;
        self.active.mark_active = false;
    }

    fn region(&self) -> Option<Range<usize>> {
        let r = self.visual_region();
        (!r.is_empty()).then_some(r)
    }

    /// The byte range an operation acts on and the renderer highlights. In evil
    /// Visual state this is *inclusive* of the grapheme under the block cursor
    /// (vim semantics), so the emacs-exclusive `selected_range` is extended one
    /// grapheme to the right. With the cursor ON the line end (after `$`, or on
    /// an empty line) that grapheme is the line break, as in Vim: `v$d` joins
    /// the lines and `vd` on an empty line deletes it. Normal state never rests
    /// there (see `evil_clamp_cursor`), so a plain `vd` can't join by accident.
    fn visual_region(&self) -> Range<usize> {
        let r = self.active.selected_range.clone();
        match self.evil {
            EvilState::Visual => {
                let end = self
                    .active
                    .buffer
                    .next_grapheme(r.end)
                    .min(self.active.buffer.len());
                r.start..end.max(r.start)
            }
            // Line-wise Visual (`V`): round the raw selection out to whole lines —
            // from the start of the first line through the start of the line
            // *after* the last, so the trailing newline is included and a
            // line-wise `d`/`y` takes the whole line. At EOF (no trailing
            // newline) stop at the buffer end.
            EvilState::VisualLine => {
                let (lo, hi) = (r.start.min(r.end), r.start.max(r.end));
                let start = self.active.buffer.line_range_at(lo).start;
                let last = self.active.buffer.line_range_at(hi);
                let end = if last.end < self.active.buffer.len() {
                    self.active.buffer.next_grapheme(last.end)
                } else {
                    last.end
                };
                start..end
            }
            _ => r,
        }
    }

    /// The rectangle of a block-wise Visual selection (`C-v`) as
    /// `(top_row, bottom_row, left_col, right_col)` in 0-based line indices and
    /// character columns — the bounding box of the two corners
    /// `selected_range.start`/`.end`. `None` unless in [`EvilState::VisualBlock`].
    /// `right_col` is the cursor's own column (inclusive, vim semantics).
    fn visual_block_rect(&self) -> Option<(usize, usize, usize, usize)> {
        if !matches!(self.evil, EvilState::VisualBlock) {
            return None;
        }
        let row_col = |off: usize| -> (usize, usize) {
            let row = self.active.buffer.line_index_at_byte(off);
            let line_start = self.active.buffer.line_range(row).start;
            let col =
                self.active.buffer.byte_to_char(off) - self.active.buffer.byte_to_char(line_start);
            (row, col)
        };
        let (ra, ca) = row_col(self.active.selected_range.start);
        let (rb, cb) = row_col(self.active.selected_range.end);
        Some((ra.min(rb), ra.max(rb), ca.min(cb), ca.max(cb)))
    }

    /// Block-selection rectangle for pane `id`, for the renderer (selected pane
    /// only; `None` otherwise or when not in block-wise Visual).
    pub fn pane_visual_block(&self, id: usize) -> Option<(usize, usize, usize, usize)> {
        (id == self.selected_pane)
            .then(|| self.visual_block_rect())
            .flatten()
    }

    // --- Command implementations ------------------------------------------

    fn delete_char_forward(&mut self, cx: &mut Context<Self>) {
        let range = self.region().unwrap_or_else(|| {
            let p = self.point();
            p..self.active.buffer.next_grapheme(p)
        });
        self.delete_range(range, cx);
    }

    /// Vim `r<char>`: overwrite the single grapheme under the cursor with `ch`,
    /// then keep the cursor on the replaced character (Normal mode unchanged).
    /// A no-op — vim's "beep" — when point sits on the line break or past the
    /// end of the buffer, since there is no character there to replace.
    fn evil_replace_char(&mut self, ch: &str, cx: &mut Context<Self>) {
        let p = self.point();
        let line = self.active.buffer.line_range_at(p);
        if p >= line.end {
            return;
        }
        // `3rx` overwrites 3 chars — but, like vim, nothing when the line has
        // fewer than that many left.
        let n = std::mem::replace(&mut self.evil_replace_count, 1).max(1);
        let mut end = p;
        for _ in 0..n {
            if end >= line.end {
                return;
            }
            end = self.active.buffer.next_grapheme(end);
        }
        let graphemes = self
            .active
            .buffer
            .slice_to_string(p..end)
            .graphemes(true)
            .count();
        self.replace_region(p, end, &ch.repeat(graphemes), cx);
        // Vim leaves the cursor on the last replaced char.
        let last = self.active.buffer.prev_grapheme(p + ch.len() * graphemes);
        self.move_to(last.max(p), cx);
    }

    fn delete_char_backward(&mut self, cx: &mut Context<Self>) {
        if self.has_extra_cursors() && self.region().is_none() {
            self.multi_delete_back(cx);
            return;
        }
        let range = self.region().unwrap_or_else(|| {
            let p = self.point();
            self.active.buffer.prev_grapheme(p)..p
        });
        self.delete_range(range, cx);
    }

    fn newline(&mut self, cx: &mut Context<Self>) {
        if self.has_extra_cursors() {
            // Multi-cursor RET stays a plain split; per-cursor reindent would
            // need each cursor's own structural context.
            self.multi_insert("\n", cx);
            return;
        }
        if !cx.global::<crate::settings::Settings>().electric_indent() {
            self.insert_text("\n", cx);
            return;
        }
        let p = self.active.selected_range.start;

        // Lisp: dedicated paren-aligning indenter (column alignment, which the
        // step-based tree-sitter model can't express).
        if self
            .active_language()
            .as_deref()
            .is_some_and(is_lisp_language)
        {
            let tab = self.structural_indent_width(cx);
            // Inside a multi-line string the new line is string content: no
            // indentation is added to it.
            let indent = self
                .lisp_indent_context(p)
                .map(|text| " ".repeat(lisp_indent_for(&text, tab, &self.lisp_indent_rules)))
                .unwrap_or_default();
            self.insert_text(&format!("\n{indent}"), cx);
            return;
        }

        // Tree-sitter structural indent, when the grammar carries an indent query.
        // `tab` is the literal-tab display width; `step` is the columns added per
        // structural level (per-language — e.g. OCaml is 2 regardless of a user's
        // tab-width, matching ocamlformat).
        let tab = cx.global::<crate::settings::Settings>().tab_width();
        let step = self.structural_indent_width(cx);
        let target_row = self.active.buffer.byte_to_position(p).0 + 1;
        let anchor = self.prev_nonspace(p);
        // Column alignment (`@align`/`@anchor`) wins over the step model when a
        // query defines it — continuation lines align under a delimiter (e.g. call
        // arguments under the first one) rather than stepping in a fixed amount.
        if let Some(col) = self.tree_align(anchor, p, target_row) {
            self.insert_text(&format!("\n{}", " ".repeat(col)), cx);
            return;
        }
        if let Some(level) = self.tree_indent(anchor, p, target_row, Some(p)) {
            // Cursor sitting just before a closing bracket: open a body line and
            // push the closer down to its own (dedented) line.
            let next = self
                .active
                .buffer
                .slice_to_string(p..self.active.buffer.next_grapheme(p));
            if matches!(next.as_str(), "}" | ")" | "]")
                && let Some(body) = self.tree_indent(anchor, p, target_row, None)
                && body > level
            {
                let first = " ".repeat(body * step);
                let second = " ".repeat(level * step);
                self.edit(
                    self.active.selected_range.clone(),
                    &format!("\n{first}\n{second}"),
                );
                self.collapse_to(p + 1 + first.len());
                self.ensure_cursor_visible();
                cx.notify();
                return;
            }
            // Structural indent — but a plain RET must never fall to column 0
            // when the current line is itself indented: that only happens when
            // the query couldn't place us (e.g. `match … with` before its first
            // `|` arm exists, whose bodyless node can't attach to the enclosing
            // binding). Inherit the current line's indent so the caret lands at
            // the construct's column. Only rescues the level-0 case, so a genuine
            // structural dedent (a line that closed brackets → level ≥ 1) is
            // untouched.
            let mut indent = level * step;
            if level == 0 {
                let cur = self.line_leading_width(p, tab);
                if cur > 0 {
                    indent = cur;
                }
            }
            // Just after an opening bracket, the body goes one step past the
            // opener's line. The tree normally says so already, but not while the
            // bracket is still unclosed (typing fresh JSON without electric-pair):
            // the parse is an error node the query can't place, so the result
            // above would leave the body flush with the opener.
            if matches!(
                self.active
                    .buffer
                    .slice_to_string(anchor..self.active.buffer.next_grapheme(anchor))
                    .as_str(),
                "{" | "[" | "("
            ) {
                indent = indent.max(self.line_leading_width(anchor, tab) + step);
            }
            // Trailing-token rules (the token-level layer): a line ending in an
            // `indent-after` token (OCaml `->`) opens a body one step past the
            // current line — the tree can't distinguish a dangling `->` from a
            // satisfied one. A line ending in an `align-after` token (`in`/`with`)
            // keeps the next line at the current column. `max`/exact-set so they
            // compose with the structural result instead of double-counting.
            let cur = self.line_leading_width(p, tab);
            let lang = self.active_language();
            let (indent_after, align_after) = {
                let s = cx.global::<crate::settings::Settings>();
                (
                    s.indent_after(lang.as_deref()),
                    s.align_after(lang.as_deref()),
                )
            };
            if let Some(tok) = self.trailing_token(p) {
                indent = apply_trailing_rule(indent, cur, step, &tok, &indent_after, &align_after);
            }
            self.insert_text(&format!("\n{}", " ".repeat(indent)), cx);
            return;
        }

        // Fallback (no indent query): copy the current line's leading whitespace
        // up to point, so splitting inside the indentation never adds extra.
        let line = self.active.buffer.line_range_at(p);
        let indent: String = self
            .active
            .buffer
            .slice_to_string(line.start..p)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        self.insert_text(&format!("\n{indent}"), cx);
    }

    /// The structural indent depth (in `tab_width` steps) for a line, via the
    /// active buffer's tree-sitter indent query. `anchor` is the byte whose
    /// syntax the line follows (previous non-blank for a newline, the line's own
    /// first non-blank for reindent); `ref_byte` is the line's logical start.
    /// `None` when there is no highlighter or no indent query for the language.
    /// See [`crate::highlight::Highlighter::indent_level`].
    fn tree_indent(
        &self,
        anchor: usize,
        ref_byte: usize,
        target_row: usize,
        outdent_byte: Option<usize>,
    ) -> Option<usize> {
        self.active.highlighter.as_ref()?.indent_level(
            self.active.buffer.rope(),
            anchor,
            ref_byte,
            target_row,
            outdent_byte,
        )
    }

    /// Column-alignment indent for the target line (tree-sitter `@align`/`@anchor`),
    /// in spaces. `None` when no `@align` scope applies. See
    /// [`crate::highlight::Highlighter::align_column`].
    fn tree_align(&self, anchor: usize, ref_byte: usize, target_row: usize) -> Option<usize> {
        self.active.highlighter.as_ref()?.align_column(
            self.active.buffer.rope(),
            anchor,
            ref_byte,
            target_row,
        )
    }

    /// Byte ranges to highlight for matching-delimiter display (Emacs
    /// `show-paren-mode`): the bracket at point (block-cursor style) or just
    /// before it, together with its match — each one grapheme wide. Empty when
    /// point isn't on a bracket, or the bracket has no match. Computed fresh per
    /// frame by the painter, so it always tracks the cursor.
    pub(super) fn show_paren_ranges(&self) -> Vec<Range<usize>> {
        let p = self.point();
        let at = |b: usize, e: usize| self.active.buffer.slice_to_string(b..e);
        // Prefer the bracket the cursor sits on (the char "after" point); else
        // the closer the cursor just moved past.
        let delim = if p < self.active.buffer.len()
            && matches!(
                at(p, self.active.buffer.next_grapheme(p)).as_str(),
                "(" | "[" | "{"
            ) {
            Some(p)
        } else {
            let prev = self.active.buffer.prev_grapheme(p);
            (prev < p && matches!(at(prev, p).as_str(), ")" | "]" | "}")).then_some(prev)
        };
        let Some(d) = delim else {
            return Vec::new();
        };
        match self.bracket_match(d) {
            Some(m) => vec![
                d..self.active.buffer.next_grapheme(d),
                m..self.active.buffer.next_grapheme(m),
            ],
            None => Vec::new(),
        }
    }

    /// The matching delimiter for the bracket at `byte` — structurally via the
    /// parse tree when the buffer has one, else a balanced text scan (plain-text
    /// buffers, or a grammar where the tree lookup misses).
    fn bracket_match(&self, byte: usize) -> Option<usize> {
        if let Some(m) = self
            .active
            .highlighter
            .as_ref()
            .and_then(|h| h.bracket_match(byte))
        {
            return Some(m);
        }
        // Lisp: a reader-aware match, so a paren in a string, a `;` comment or a
        // `#\(` char literal never pairs. Bounded like the plain scan.
        if self.is_lisp_buffer() && self.active.buffer.len() <= LISP_MATCH_LIMIT {
            return crate::sexp::matching_bracket(&self.active.buffer.full_text(), byte);
        }
        self.scan_bracket_match(byte)
    }

    /// Whether the active buffer's language gets Lisp (reader-aware) editing.
    fn is_lisp_buffer(&self) -> bool {
        self.active_language()
            .as_deref()
            .is_some_and(is_lisp_language)
    }

    /// The opener matching the closing bracket at `closer`, by scanning the
    /// text backwards with nesting — skipping brackets inside single-line
    /// `"…"` / `'…'` strings and `//` comments, which a plain balanced scan
    /// would count. For an error-laden parse, where the tree's pairing can't be
    /// trusted. Bounded to a few thousand lines back.
    fn opener_by_text(&self, closer: usize) -> Option<usize> {
        let close = self.active.buffer.slice_to_string(closer..closer + 1);
        let (open, close) = match close.as_str() {
            "}" => (b'{', b'}'),
            "]" => (b'[', b']'),
            ")" => (b'(', b')'),
            _ => return None,
        };
        let first = self.active.buffer.line_index_at_byte(closer);
        let mut depth = 0usize;
        for row in (first.saturating_sub(5000)..=first).rev() {
            let lr = self.active.buffer.line_range(row);
            let end = if row == first { closer } else { lr.end };
            let line = self.active.buffer.slice_to_string(lr.start..end);
            // This line's code-level brackets of our kind, left to right.
            let mut brackets = Vec::new();
            let mut quote: Option<u8> = None;
            let bytes = line.as_bytes();
            let mut k = 0;
            while k < bytes.len() {
                let c = bytes[k];
                match quote {
                    Some(q) => {
                        if c == b'\\' {
                            k += 1;
                        } else if c == q {
                            quote = None;
                        }
                    }
                    // A quote opens a string only if it closes on this line, so a
                    // Rust lifetime (`&'a str`) isn't taken for one.
                    None if (c == b'"' || c == b'\'') && bytes[k + 1..].contains(&c) => {
                        quote = Some(c);
                    }
                    None if c == b'/' && bytes.get(k + 1) == Some(&b'/') => break,
                    None if c == open || c == close => brackets.push((k, c)),
                    None => {}
                }
                k += 1;
            }
            for &(k, c) in brackets.iter().rev() {
                if c == close {
                    depth += 1;
                } else if depth == 0 {
                    return Some(lr.start + k);
                } else {
                    depth -= 1;
                }
            }
        }
        None
    }

    /// Balanced bracket scan over buffer text (counting only the delimiter's own
    /// kind), bounded so a keypress can't scan an enormous buffer. Not string- or
    /// comment-aware — that's what the tree-sitter path in [`Self::bracket_match`]
    /// is for; this is the no-grammar fallback.
    fn scan_bracket_match(&self, byte: usize) -> Option<usize> {
        let rope = self.active.buffer.rope();
        if byte >= rope.len_bytes() {
            return None;
        }
        let total = rope.len_chars();
        let mut ci = rope.byte_to_char(byte);
        let (open, close, forward) = match rope.char(ci) {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => ('(', ')', false),
            ']' => ('[', ']', false),
            '}' => ('{', '}', false),
            _ => return None,
        };
        const LIMIT: usize = 200_000;
        let mut depth = 0i32;
        for _ in 0..LIMIT {
            let c = rope.char(ci);
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
            }
            if depth == 0 {
                return Some(rope.char_to_byte(ci));
            }
            if forward {
                ci += 1;
                if ci >= total {
                    return None;
                }
            } else if ci == 0 {
                return None;
            } else {
                ci -= 1;
            }
        }
        None
    }

    /// Columns to add per structural indent level for the active buffer's
    /// language: a per-language convention (e.g. OCaml is 2, matching ocamlformat)
    /// when one exists, otherwise the global `tab-width`. This is the indent
    /// *step*, distinct from how wide a literal tab renders.
    fn structural_indent_width(&self, cx: &mut Context<Self>) -> usize {
        let lang = self.active_language();
        cx.global::<crate::settings::Settings>()
            .tab_width_for(lang.as_deref())
    }

    /// The last whitespace-delimited token on the line containing `byte`, up to
    /// `byte` (so a mid-line split sees the token before the cursor). `None` if
    /// the line is blank before the point. Powers the trailing-token indent rules
    /// (`indent-after` / `align-after`) — e.g. detecting a line that ends in `->`.
    fn trailing_token(&self, byte: usize) -> Option<String> {
        let line = self.active.buffer.line_range_at(byte);
        let text = self.active.buffer.slice_to_string(line.start..byte);
        text.split_whitespace().next_back().map(str::to_string)
    }

    /// The display width (in columns) of the leading whitespace on the line
    /// containing `byte`, counting a tab as one `tab` step. Used as an indent
    /// floor so a plain RET never dedents below the current line.
    fn line_leading_width(&self, byte: usize, tab: usize) -> usize {
        let line = self.active.buffer.line_range_at(byte);
        self.active
            .buffer
            .slice_to_string(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .map(|c| if c == '\t' { tab } else { 1 })
            .sum()
    }

    /// Byte where the nearest non-whitespace grapheme before `from` begins, or 0
    /// if only whitespace precedes it. Anchors a fresh line's indentation to the
    /// previous line's syntax (e.g. a compound-statement header) rather than the
    /// blank position just past the newline.
    fn prev_nonspace(&self, from: usize) -> usize {
        let mut a = from;
        while a > 0 {
            let prev = self.active.buffer.prev_grapheme(a);
            if self
                .active
                .buffer
                .slice_to_string(prev..a)
                .chars()
                .all(char::is_whitespace)
            {
                a = prev;
            } else {
                return prev;
            }
        }
        0
    }

    /// Replace the current line's leading whitespace with `target` spaces,
    /// keeping point in the same place relative to the line's text (Emacs TAB
    /// reindent). A no-op move when the line is already correctly space-indented.
    fn reindent_current_line(&mut self, target: usize, cx: &mut Context<Self>) {
        let p = self.point();
        let line = self.active.buffer.line_range_at(p);
        let line_text = self.active.buffer.slice_to_string(line.clone());
        let cur: String = line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let cur_bytes = cur.len();
        let indent_end = line.start + cur_bytes;
        if target == cur.chars().count() && cur.bytes().all(|b| b == b' ') {
            if p < indent_end {
                self.move_to(indent_end, cx);
            }
            return;
        }
        self.edit(line.start..indent_end, &" ".repeat(target));
        let new_indent_end = line.start + target;
        let cursor = if p <= indent_end {
            new_indent_end
        } else {
            (p + target).saturating_sub(cur_bytes)
        };
        self.collapse_to(cursor);
        cx.notify();
    }

    /// electric-indent for a just-typed closing bracket: when it is the first
    /// thing on its line, reindent the line so the closer lines up with its
    /// opener's line (`{` … `}`), rather than staying at the body's depth until
    /// a TAB. Matching via the opener — not the indent query — keeps this right
    /// while an outer bracket is still unclosed and the tree can't place the
    /// line. Only for languages with a structural indent query.
    fn electric_closer_reindent(&mut self, cx: &mut Context<Self>) {
        if !cx.global::<crate::settings::Settings>().electric_indent()
            || self.has_extra_cursors()
            || self
                .active_language()
                .as_deref()
                .is_some_and(is_lisp_language)
            || !self
                .active
                .highlighter
                .as_ref()
                .is_some_and(|h| h.has_indent_query())
        {
            return;
        }
        let p = self.point();
        let closer = self.active.buffer.prev_grapheme(p);
        let line = self.active.buffer.line_range_at(closer);
        if !self
            .active
            .buffer
            .slice_to_string(line.start..closer)
            .chars()
            .all(|c| c == ' ' || c == '\t')
        {
            return;
        }
        let tab = cx.global::<crate::settings::Settings>().tab_width();
        // Mid-edit the parse usually has an error, and then the tree's pairing
        // is a guess: `{ "a": {…}, {` ⏎ `}` reads that `}` as closing the
        // OUTER object. Trust the tree only when it parsed cleanly.
        let clean = self
            .active
            .highlighter
            .as_ref()
            .and_then(|h| h.tree())
            .is_some_and(|t| !t.root_node().has_error());
        let open = if clean {
            self.bracket_match(closer)
        } else {
            self.opener_by_text(closer)
        };
        match open {
            Some(open) if open < line.start => {
                let target = self.line_leading_width(open, tab);
                self.reindent_current_line(target, cx);
            }
            _ => {
                let step = self.structural_indent_width(cx);
                self.structural_reindent_line(step, cx);
            }
        }
    }

    fn open_line(&mut self, cx: &mut Context<Self>) {
        let start = self.active.selected_range.start;
        self.edit(self.active.selected_range.clone(), "\n");
        self.move_to(start, cx);
    }

    fn indent_for_tab(&mut self, cx: &mut Context<Self>) {
        let electric = cx.global::<crate::settings::Settings>().electric_indent();
        let step = self.structural_indent_width(cx);
        // With a region active, TAB shifts the lines it spans one step right
        // (Emacs `indent-for-tab-command` hands off to `indent-region`). The
        // self-insert fallback below REPLACES the selection, so without this a
        // stray TAB over a mouse-drag selection silently swallows it.
        if let Some(region) = self.region() {
            self.indent_region(region, step, cx);
            return;
        }
        if electric && !self.has_extra_cursors() && self.structural_reindent_line(step, cx) {
            return;
        }
        self.insert_text(&" ".repeat(step), cx);
    }

    /// Reindent point's line from the language's structure: the Lisp aligner,
    /// or the tree-sitter indent / align queries. False when neither knows
    /// (no grammar or indent query) — the line is left alone.
    fn structural_reindent_line(&mut self, step: usize, cx: &mut Context<Self>) -> bool {
        {
            // Lisp: paren-aligning reindent.
            if self
                .active_language()
                .as_deref()
                .is_some_and(is_lisp_language)
            {
                self.lisp_reindent_line(cx);
                return true;
            }
            // Tree-sitter structural reindent of the current line.
            let line = self.active.buffer.line_range_at(self.point());
            let line_text = self.active.buffer.slice_to_string(line.clone());
            let lead_bytes: usize = line_text
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(char::len_utf8)
                .sum();
            let ref_byte = if lead_bytes < line_text.len() {
                line.start + lead_bytes
            } else {
                line.start
            };
            let target_row = self.active.buffer.byte_to_position(line.start).0;
            // Reindent anchors on the line's own content (no scan back), so a
            // line that belongs outside an enclosing block isn't pulled into it.
            // Column alignment (`@align`/`@anchor`) wins over the step model, the
            // same precedence the newline path uses.
            if let Some(col) = self.tree_align(ref_byte, ref_byte, target_row) {
                self.reindent_current_line(col, cx);
                return true;
            }
            if let Some(level) = self.tree_indent(ref_byte, ref_byte, target_row, Some(ref_byte)) {
                self.reindent_current_line(level * step, cx);
                return true;
            }
        }
        false
    }

    /// Vim `=` over (0-based) lines `first..=last`: each non-blank line is
    /// reindented structurally, or — with no grammar to ask — to the previous
    /// non-blank line's indentation (`autoindent`). The cursor lands on the
    /// first line's first non-blank.
    pub(super) fn reindent_lines(&mut self, first: usize, last: usize, cx: &mut Context<Self>) {
        let step = self.structural_indent_width(cx).max(1);
        let last = last.min(self.active.buffer.line_count().saturating_sub(1));
        self.begin_undo_group();
        for line in first..=last {
            let lr = self.active.buffer.line_range(line);
            let text = self.active.buffer.slice_to_string(lr.clone());
            if text.trim().is_empty() {
                continue;
            }
            self.collapse_to(lr.start);
            if self.structural_reindent_line(step, cx) {
                continue;
            }
            let prev = (0..line).rev().find_map(|l| {
                let t = self
                    .active
                    .buffer
                    .slice_to_string(self.active.buffer.line_range(l));
                (!t.trim().is_empty()).then(|| {
                    t.chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .map(|c| if c == '\t' { step } else { 1 })
                        .sum::<usize>()
                })
            });
            if let Some(col) = prev {
                self.reindent_current_line(col, cx);
            }
        }
        self.end_undo_group();
        let start = self.active.buffer.line_range(first).start;
        self.collapse_to(self.first_non_blank(start));
        cx.notify();
    }

    /// Shift every line `region` touches `step` columns right, as one undo unit,
    /// leaving the (now wider) lines selected so repeated TABs keep indenting.
    /// Blank lines are skipped, matching Emacs's `indent-region`.
    fn indent_region(&mut self, region: Range<usize>, step: usize, cx: &mut Context<Self>) {
        let first = self.active.buffer.line_index_at_byte(region.start);
        // `region.end` is exclusive: a selection that stops at a line's start
        // must not indent that line, so anchor on the last byte inside it.
        let last_byte = region.end.saturating_sub(1).max(region.start);
        let last = self.active.buffer.line_index_at_byte(last_byte);
        let pad = " ".repeat(step);
        self.begin_undo_group();
        // Bottom-up, so an earlier line's insertion can't shift the offsets of
        // the lines still to come.
        for line in (first..=last).rev() {
            let lr = self.active.buffer.line_range(line);
            if lr.end > lr.start {
                self.edit(lr.start..lr.start, &pad);
            }
        }
        self.end_undo_group();
        self.active.selected_range =
            self.active.buffer.line_range(first).start..self.active.buffer.line_range(last).end;
        self.active.selection_reversed = false;
        self.active.marked_range = None;
        self.active.mark_active = true;
        self.ensure_cursor_visible();
        cx.notify();
    }

    /// Shift (0-based) lines `first..=last` one `step` right (`dir` >= 0) or
    /// left, skipping blank lines; outdenting removes up to `step` columns of
    /// leading whitespace (a tab counts as the whole step). No undo grouping —
    /// callers group.
    fn shift_line_range(&mut self, first: usize, last: usize, dir: isize, step: usize) {
        let pad = " ".repeat(step);
        // Bottom-up, so an earlier line's edit can't shift the offsets of the
        // lines still to come.
        for line in (first..=last).rev() {
            let lr = self.active.buffer.line_range(line);
            if lr.end <= lr.start {
                continue; // blank line: vim leaves it alone
            }
            if dir >= 0 {
                self.edit(lr.start..lr.start, &pad);
            } else {
                let text = self.active.buffer.slice_to_string(lr.clone());
                let mut cols = 0;
                let mut bytes = 0;
                for c in text.chars() {
                    if cols >= step {
                        break;
                    }
                    match c {
                        ' ' => cols += 1,
                        '\t' => cols = step,
                        _ => break,
                    }
                    bytes += c.len_utf8();
                }
                if bytes > 0 {
                    self.edit(lr.start..lr.start + bytes, "");
                }
            }
        }
    }

    /// Shift the lines spanned by the region (or, with no region, the current
    /// line) one indent step right (`dir` +1) or left (`dir` -1) — vim `>>` /
    /// `<<` and visual `>` / `<`, Helix `>` / `<`. Outdenting removes up to
    /// `step` columns of leading whitespace, counting a tab as a full step, and
    /// never touches a line that has none. The cursor lands on the first
    /// non-blank of the first shifted line, the way vim leaves it.
    ///
    /// Unlike [`Self::indent_region`] (Emacs `indent-for-tab-command`'s
    /// helper, which re-establishes the region) this drops the selection when
    /// there wasn't one, so `>>` in Normal state doesn't leave a highlight.
    fn shift_lines(&mut self, dir: isize, cx: &mut Context<Self>) {
        let step = self.structural_indent_width(cx).max(1);
        let had_region = self.region().is_some();
        let region = self
            .region()
            .unwrap_or_else(|| self.active.buffer.line_range_at(self.point()));
        let first = self.active.buffer.line_index_at_byte(region.start);
        // `region.end` is exclusive: a selection stopping at a line's start must
        // not shift that line.
        let last_byte = region.end.saturating_sub(1).max(region.start);
        let mut last = self.active.buffer.line_index_at_byte(last_byte);
        // `3>>`: a count (count-aware dispatch) shifts that many lines.
        if !had_region && self.prefix_count > 1 {
            last = (first + self.prefix_count - 1)
                .min(self.active.buffer.line_count().saturating_sub(1));
        }
        self.begin_undo_group();
        self.shift_line_range(first, last, dir, step);
        self.end_undo_group();
        if had_region {
            self.active.selected_range =
                self.active.buffer.line_range(first).start..self.active.buffer.line_range(last).end;
            self.active.selection_reversed = false;
            self.active.marked_range = None;
            self.active.mark_active = true;
        } else {
            let start = self.active.buffer.line_range(first).start;
            self.collapse_to(self.first_non_blank(start));
        }
        self.ensure_cursor_visible();
        cx.notify();
    }

    fn set_mark(&mut self, cx: &mut Context<Self>) {
        let p = self.point();
        self.active.selected_range = p..p;
        self.active.selection_reversed = false;
        self.active.mark_active = true;
        self.echo("Mark set", cx);
    }

    fn exchange_point_and_mark(&mut self, cx: &mut Context<Self>) {
        if self.active.selected_range.is_empty() {
            self.echo("No mark set", cx);
            return;
        }
        self.active.selection_reversed = !self.active.selection_reversed;
        self.active.mark_active = true;
        self.ensure_cursor_visible();
        cx.notify();
    }

    fn mark_whole_buffer(&mut self, cx: &mut Context<Self>) {
        self.active.selected_range = 0..self.active.buffer.len();
        self.active.selection_reversed = false;
        self.active.mark_active = true;
        self.ensure_cursor_visible();
        cx.notify();
    }

    // The explicitly-addressed primitives below take offsets straight from
    // Scheme, so they normalize (swap inverted ends, clamp, floor to a char
    // boundary) rather than trust them — Emacs accepts `(delete-region 9 3)`.
    fn delete_region(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        let range = self.active.buffer.clamp_range(start..end);
        if !range.is_empty() {
            let cursor = self.edit(range, "");
            self.collapse_to(cursor);
            cx.notify();
        }
    }

    fn replace_region(&mut self, start: usize, end: usize, text: &str, cx: &mut Context<Self>) {
        let range = self.active.buffer.clamp_range(start..end);
        let cursor = self.edit(range, text);
        self.collapse_to(cursor);
        cx.notify();
    }

    fn insert_at(&mut self, pos: usize, text: &str, cx: &mut Context<Self>) {
        let pos = self.active.buffer.snap(pos);
        let cursor = self.edit(pos..pos, text);
        self.collapse_to(cursor);
        cx.notify();
    }

    fn scroll_page(&mut self, dir: isize, cx: &mut Context<Self>) {
        let lh = f32::from(self.line_height);
        let page = if lh <= 0.0 {
            1
        } else {
            ((f32::from(self.viewport_height) / lh).floor() as isize - 2).max(1)
        };
        self.move_to(self.line_offset(dir * page), cx);
    }

    /// Scroll half a page (evil `C-d` / `C-u`): `dir` +1 down, -1 up.
    fn scroll_half(&mut self, dir: isize, cx: &mut Context<Self>) {
        let lh = f32::from(self.line_height);
        let half = if lh <= 0.0 {
            1
        } else {
            ((f32::from(self.viewport_height) / lh).floor() as isize / 2).max(1)
        };
        // `goto` extends an active selection, so Visual `C-d`/`C-u` grow it.
        self.goto(self.line_offset(dir * half), cx);
    }

    /// `C-l` — Emacs `recenter-top-bottom`. Successive presses cycle the line at
    /// point through the middle, top, then bottom of the window; the cycle resets
    /// whenever point has moved since the last press. (On a buffer that already
    /// fits the window there's nothing to scroll, just as in Emacs.)
    fn recenter(&mut self, cx: &mut Context<Self>) {
        let lh = f32::from(self.line_height);
        let vh = f32::from(self.viewport_height);
        if lh <= 0.0 || vh <= 0.0 {
            return;
        }
        let point = self.point();
        if point == self.recenter_point {
            self.recenter_cycle = (self.recenter_cycle + 1) % 3;
        } else {
            self.recenter_cycle = 0;
        }
        self.recenter_point = point;

        // Scroll space is display rows (folds collapsed, image/scaled rows
        // counted), as in `ensure_cursor_visible` — not buffer line indices.
        let line = self.display_row_of_line(self.active.buffer.line_index_at_byte(point)) as f32;
        let top = line * lh; // point's line flush with the window top
        let desired = match self.recenter_cycle {
            0 => top - vh / 2.0 + lh / 2.0, // middle
            1 => top,                       // top
            _ => top - vh + lh,             // bottom
        };
        let max = self.max_scroll_offset(vh);
        self.active.scroll_offset = px(desired.clamp(0.0, max));
        cx.notify();
    }

    fn keyboard_quit(&mut self, cx: &mut Context<Self>) {
        if self.minibuffer.is_some() {
            self.mb_abort(cx);
            return;
        }
        let p = self.point();
        self.collapse_to(p);
        self.pending_keys.clear();
        self.avy_hints.clear(); // cancel any pending avy jump
        self.active.ghost = None; // C-g dismisses an inline suggestion
        if self.has_extra_cursors() {
            self.mc_clear(cx); // C-g collapses back to a single cursor
        }
        // Emacs C-g also quits running Lisp: a runaway `M-:` / C-x C-e loop,
        // or a command stuck in a loop, unwinds instead of wedging scripting.
        if self.script.interrupt_running() {
            self.echo("Quit (interrupted the running evaluation)", cx);
            return;
        }
        self.echo("Quit", cx);
    }

    /// Public entry point for logging a status message to the echo area and the
    /// `*Messages*` buffer. Used by the launch-time auto-update flow, which runs
    /// from the workspace and has no direct access to the private `echo`.
    pub fn message(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.echo(message, cx);
    }

    fn echo(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = message.into();
        let line = self.status.to_string();
        self.messages.push(line.clone());
        self.echo_count += 1;
        // Append live to `*Messages*` if it's open (active or any pane/inactive),
        // so the buffer accumulates like Emacs's `*Messages*`.
        self.append_to_log_buffer("*Messages*", &line);
        cx.notify();
    }

    /// Flip a boolean `Settings` option (stored as `"true"`/`"false"`) given its
    /// `current` value, then echo `"<label> enabled/disabled"`. Used by the
    /// `toggle-*` commands so the set-then-report dance lives in one place.
    fn toggle_setting(
        &mut self,
        key: impl Into<String>,
        current: bool,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let key = key.into();
        cx.update_global::<crate::settings::Settings, _>(|s, _| {
            s.set(key, crate::settings::OptionValue::Bool(!current));
        });
        self.script
            .set_options(cx.global::<crate::settings::Settings>().snapshot());
        self.echo(
            format!("{label} {}", if current { "disabled" } else { "enabled" }),
            cx,
        );
    }

    /// Recompute the active buffer's git-gutter (diff-hl) marks: its working-tree
    /// text (the live rope, so marks track unsaved edits) diffed against HEAD.
    /// Cleared when the gutter is disabled or the buffer isn't visiting a file.
    /// Only repaints when the marks actually changed, so it's cheap to call from
    /// frequent hooks (save / find-file). Driven by `magit-gutter-refresh`.
    fn refresh_git_gutter(&mut self, cx: &mut Context<Self>) {
        let enabled = cx.global::<crate::settings::Settings>().magit_gutter();
        let new_marks: HashMap<usize, DiffMark> = if enabled {
            match self.active.path.clone() {
                Some(path) => {
                    let text = self.active.buffer.full_text();
                    crate::script::git_diff_marks(&path, &text)
                        .into_iter()
                        .collect()
                }
                None => HashMap::new(),
            }
        } else {
            HashMap::new()
        };
        if new_marks != self.active.diff_marks {
            self.active.diff_marks = new_marks;
            cx.notify();
        }
    }

    /// Append a line to a log buffer, creating it (as an inactive editable
    /// buffer) the first time so it shows up in the buffer list. Never switches
    /// to it or steals focus.
    fn append_to_log_buffer(&mut self, name: &str, line: &str) {
        let entry = format!("{line}\n");
        let lh = f32::from(self.line_height);
        if self.active.name == name {
            let end = self.active.buffer.len();
            self.active.buffer.replace(end..end, &entry);
            self.lines.clear();
        } else if let Some(doc) = self.inactive_doc_mut(name) {
            let end = doc.buffer.len();
            doc.buffer.replace(end..end, &entry);
            // Bound growth (only while not being viewed): keep the last N lines.
            let lines = doc.buffer.line_count();
            let mut trimmed = None;
            if lines > LOG_BUFFER_LINES_CAP {
                let dropped = lines - LOG_BUFFER_LINES_CAP;
                let cut = doc.buffer.line_range(dropped).start;
                doc.buffer.replace(0..cut, "");
                doc.reset_undo(); // unrecorded edit: old records are stale
                // Shift point/mark back with the text they sat on (a position
                // inside the dropped head pins to the new start), and the view
                // up by the dropped rows so it keeps showing the same lines.
                let r = doc.selected_range.clone();
                doc.selected_range = doc
                    .buffer
                    .clamp_range(r.start.saturating_sub(cut)..r.end.saturating_sub(cut));
                let scroll = f32::from(doc.scroll_offset) - dropped as f32 * lh;
                doc.scroll_offset = px(scroll.max(0.0));
                trimmed = Some((cut, dropped, doc.buffer.len()));
            }
            // A non-selected window showing the log keeps its own point and
            // scroll: shift those the same way, or its view drifts a line per
            // message (and its point can run past the shortened text).
            if let Some((cut, dropped, len)) = trimmed {
                for pane in self.panes.iter_mut().filter(|p| p.buffer == name) {
                    pane.point = pane.point.saturating_sub(cut).min(len);
                    let scroll = f32::from(pane.scroll) - dropped as f32 * lh;
                    pane.scroll = px(scroll.max(0.0));
                }
            }
        } else {
            self.inactive
                .push(make_log_document(name.to_string(), &entry));
            self.sync_buffers(); // now visible in `(buffer-names)` / helm-buffers
        }
    }

    /// Switch to the editable log buffer `name`, creating it from the
    /// scrollback ring `ring` selects if it doesn't exist yet (`C-h e`:
    /// `*Messages*`; `C-h l`: `*lsp-log*`, the captured server stderr). New
    /// lines append live (see [`Self::append_to_log_buffer`]).
    fn open_log_buffer(
        &mut self,
        name: &str,
        ring: fn(&Self) -> &BoundedRing,
        cx: &mut Context<Self>,
    ) {
        if self.active.name == name {
            return;
        }
        if let Some(index) = self.inactive_index(name) {
            self.activate(index, true, cx);
        } else {
            let ring = ring(self);
            let text = if ring.is_empty() {
                String::new()
            } else {
                format!("{}\n", ring.join("\n"))
            };
            self.switch_to_doc(make_log_document(name.to_string(), &text));
            self.move_to(self.active.buffer.len(), cx);
        }
    }

    /// Pop `*lsp-log*` into a split below the current pane (focus stays on the
    /// file), unless it's already shown. Called when an external server starts.
    fn open_lsp_log_split(&mut self, cx: &mut Context<Self>) {
        if self.panes.iter().any(|p| p.buffer == "*lsp-log*") {
            return;
        }
        // The file keeps this share of the split (on top); the log gets the rest.
        const LSP_LOG_SPLIT_RATIO: f32 = 0.75;
        let origin = self.selected_pane;
        let new_id = self.next_pane_id;
        self.split_window_ratio(SplitAxis::Vertical, LSP_LOG_SPLIT_RATIO, cx);
        self.select_pane(new_id, cx);
        self.open_log_buffer("*lsp-log*", |e| &e.lsp_log, cx); // render into the new (selected) pane
        self.select_pane(origin, cx); // hand focus back to the file
    }

    // --- Minibuffer -------------------------------------------------------

    /// Whether `hook` has at least one Scheme function registered — lets edit
    /// coalescing avoid changing observable behaviour for hooks that watch every
    /// individual change (e.g. `after-change-functions`).
    fn has_active_hook(&self, hook: &str) -> bool {
        self.hooks.get(hook).is_some_and(|fns| !fns.is_empty())
    }

    /// Run every Scheme function registered on `hook` (Emacs lifecycle hooks like
    /// `after-save-hook`, `find-file-hook`, `after-change-functions`). No-op —
    /// and no thread hop — when nothing is registered.
    fn run_hook(&mut self, hook: &str) {
        let Some(functions) = self.hooks.get(hook) else {
            return;
        };
        if functions.is_empty() {
            return;
        }
        let functions = functions.clone();
        self.publish_point();
        for function in functions {
            self.script.dispatch_action(&function, None);
        }
    }

    /// Publish the active buffer name, point line, and directory so
    /// `(buffer-line)` / `(current-buffer)` / `(buffer-directory)` see the buffer
    /// as of command invocation. The directory is the visited file's parent
    /// (Emacs's buffer-local `default-directory`), or the process cwd when the
    /// buffer visits no file — what magit resolves the repo from.
    fn publish_point(&self) {
        let point = self.point();
        let directory = self
            .active
            .path
            .as_deref()
            .and_then(|p| p.parent())
            .map_or_else(
                || self.process_cwd.clone(),
                |d| d.to_string_lossy().into_owned(),
            );
        let region = self.region().map(|r| (r.start, r.end));
        let selections = self.all_selections();
        let props = self
            .active
            .overlays
            .props
            .iter()
            .map(|(r, k, v)| (r.start, r.end, k.clone(), v.clone()))
            .collect();
        let locals = self
            .active
            .locals
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Diagnostics as flat records "line\tcol\tseverity\tmessage" (message
        // tabs/newlines flattened so each diagnostic stays one record).
        let diagnostics = self
            .active_diagnostics()
            .into_iter()
            .map(|d| {
                let (line, col) = self.active.buffer.byte_to_position(d.range.start);
                let sev = d.severity.as_str();
                let msg = d.message.replace(['\t', '\n'], " ");
                format!("{line}\t{col}\t{sev}\t{msg}")
            })
            .collect();
        let windows = self.layout.leaves();
        self.script.set_snapshot(crate::script::EditorSnapshot {
            name: self.active.name.clone(),
            line: self.active.buffer.line_index_at_byte(point),
            directory,
            path: self
                .active
                .path
                .as_deref()
                .map_or_else(String::new, |p| p.to_string_lossy().into_owned()),
            point,
            // O(1) copy-on-write clone of the rope; read bridges materialize
            // only the slice they need, off the UI thread.
            text: self.active.buffer.rope().clone(),
            tick: self.active.buffer.tick(),
            region,
            selections,
            props,
            locals,
            diagnostics,
            window_buffers: windows
                .iter()
                .map(|&id| {
                    // The selected pane shows the live active buffer; others
                    // show whatever their `pane.buffer` records.
                    let name = if id == self.selected_pane {
                        self.active.name.clone()
                    } else {
                        self.panes
                            .iter()
                            .find(|p| p.id == id)
                            .map_or_else(|| self.active.name.clone(), |p| p.buffer.clone())
                    };
                    (id, name)
                })
                .collect(),
            windows,
            selected_window: self.selected_pane,
            lsp_ready: self.active_lsp_ready(),
            visible_lines: self.visible_line_range(),
            cursor_screen_row: self.cursor_screen_row(),
            evil_enabled: self.evil != EvilState::Disabled,
            helix_enabled: self.modal_helix,
            kbd_macro_recording: self.macro_recording.is_some(),
            prefix_count: self.prefix_count,
            evil_state: self.evil.as_str().to_string(),
            search_query: self.search_query.clone().unwrap_or_default(),
            search_forward: self.evil_search_forward,
            marks: self.active_marks(),
            language: self
                .active
                .modes
                .active_language()
                .unwrap_or("")
                .to_string(),
            tree: self
                .active
                .highlighter
                .as_ref()
                .and_then(|h| h.tree().cloned()),
            major_mode: self
                .active
                .modes
                .names()
                .last()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            grid: self.active.grid.as_deref().map(|g| g.snapshot()),
        });
    }

    /// Publish a snapshot built from the inactive buffer at `idx` (its text, name,
    /// path, and buffer-locals) so a `call_action` run sees that buffer rather than
    /// the active one. Only the fields the Org fontifier reads are filled — point,
    /// region, windows, and the rest stay at their defaults, since `org--fontify`
    /// never reads them. Restore with [`publish_point`](Self::publish_point).
    fn publish_inactive_snapshot(&self, idx: usize) {
        let doc = &self.inactive[idx];
        self.script.set_snapshot(crate::script::EditorSnapshot {
            name: doc.name.clone(),
            path: doc
                .path
                .as_deref()
                .map_or_else(String::new, |p| p.to_string_lossy().into_owned()),
            // O(1) copy-on-write clone of the rope, as in `publish_point`.
            text: doc.buffer.rope().clone(),
            tick: doc.buffer.tick(),
            locals: doc
                .locals
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            ..Default::default()
        });
    }

    /// The cursor's row within the viewport, in display rows (folds collapsed) —
    /// the distance from the first visible row to the cursor's row. Paired with
    /// [`scroll_cursor_to_row`](Self::scroll_cursor_to_row) to keep a buffer
    /// visually stable across an in-place re-render.
    fn cursor_screen_row(&self) -> usize {
        let lh = f32::from(self.line_height).max(1.0);
        let first = (f32::from(self.active.scroll_offset) / lh).floor().max(0.0) as usize;
        let cursor = self.display_row_of_line(self.active.buffer.line_index_at_byte(self.point()));
        cursor.saturating_sub(first)
    }

    /// Scroll so the cursor's row sits `row` rows below the top of the viewport
    /// (clamped to the scrollable range), undoing the jump a re-render + cursor
    /// move would otherwise cause.
    fn scroll_cursor_to_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let lh = f32::from(self.line_height);
        let vh = f32::from(self.viewport_height);
        if lh <= 0.0 {
            return;
        }
        let cursor = self.display_row_of_line(self.active.buffer.line_index_at_byte(self.point()));
        let first = cursor.saturating_sub(row);
        let max = self.max_scroll_offset(vh);
        self.active.scroll_offset = px((first as f32 * lh).clamp(0.0, max));
        cx.notify();
    }

    /// Approximate first/last buffer line currently on screen, from the scroll
    /// offset and viewport height. An approximation under word-wrap/folds, but
    /// fine for scoping avy's jump hints to the visible region.
    fn visible_line_range(&self) -> (usize, usize) {
        let lh = f32::from(self.line_height).max(1.0);
        let first = (f32::from(self.active.scroll_offset) / lh).floor().max(0.0) as usize;
        let rows = (f32::from(self.viewport_height) / lh).ceil() as usize + 1;
        let last = (first + rows).min(self.active.buffer.line_count());
        (first, last)
    }

    /// Whether the active buffer has a ready, opened LSP server (for the Scheme
    /// `(lsp-server-ready?)` guard).
    fn active_lsp_ready(&self) -> bool {
        let (Some(language), Some(uri)) = (self.active_language(), self.active_uri()) else {
            return false;
        };
        self.servers
            .get(&language)
            .is_some_and(|s| s.ready && s.open.contains(&uri))
    }

    /// Names of the LSP servers that are running and ready, for the mode-line
    /// `lsp …` indicator. Keyed by language id; sorted for a stable order since
    /// `servers` is a `HashMap`.
    pub fn active_lsp_servers(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .servers
            .iter()
            .filter(|(_, server)| server.ready)
            .map(|(key, _)| key.clone())
            .collect();
        names.sort();
        names
    }

    // --- Overlays / text properties / folds (active buffer) ---------------

    /// The buffer a decoration command (line icons/indents/scales/suffixes,
    /// images, overlay faces/hides) should land on: the redirected inactive
    /// [`Document`] while [`render_redirect`](field) is set — a quiet re-render
    /// of a buffer shown in a *non-selected* pane, whose same-batch decoration
    /// writes must follow it — otherwise the active buffer. EVERY decoration
    /// command arm must resolve its stores through this, so a redirected batch
    /// can never write into the wrong buffer.
    fn redirect_target(&mut self) -> &mut Document {
        // A redirect whose buffer is no longer inactive falls back to the
        // active buffer.
        match self.redirect_index() {
            Some(i) => &mut self.inactive[i],
            None => &mut self.active,
        }
    }

    /// Whether the cursor should currently be painted: always, unless blinking
    /// is on and we're in the hidden half of the blink cycle.
    pub fn cursor_blink_visible(&self) -> bool {
        self.blink.visible
    }

    /// Mark cursor activity (a keystroke): show the cursor and restart the blink
    /// cycle from "shown", so it never blinks off mid-typing. Bumping the token
    /// kills any in-flight blink timer from the previous cycle.
    fn reset_blink(&mut self, cx: &mut Context<Self>) {
        if !self.blink.visible {
            self.blink.visible = true;
            cx.notify();
        }
        self.blink.token = self.blink.token.wrapping_add(1);
        self.schedule_blink(cx);
    }

    /// React to the OS window gaining/losing focus. While inactive the cursor
    /// stops blinking and holds steady (shown); on reactivation it resumes.
    fn set_window_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.blink.window_active == active {
            return;
        }
        self.blink.window_active = active;
        if active {
            self.reset_blink(cx); // shown + restart the blink chain
        } else {
            // Kill the in-flight chain and leave the cursor steady.
            self.blink.token = self.blink.token.wrapping_add(1);
            if !self.blink.visible {
                self.blink.visible = true;
            }
            cx.notify();
        }
    }

    /// Schedule the next blink toggle, if `cursor-blink` is enabled. Each tick
    /// re-checks the setting and the token (so a disabled cursor or a fresh
    /// keystroke stops the chain), toggles visibility, and reschedules itself.
    fn schedule_blink(&mut self, cx: &mut Context<Self>) {
        let settings = cx.global::<crate::settings::Settings>();
        if !settings.cursor_blink() {
            return;
        }
        let interval = Duration::from_millis(settings.cursor_blink_interval());
        let token = self.blink.token;
        let timer = cx.background_executor().timer(interval);
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |editor, cx| {
                if editor.blink.token != token {
                    return; // superseded by a keystroke (newer chain owns blinking)
                }
                // Stop (leaving the cursor shown) when blinking is disabled or the
                // window is inactive; the chain restarts on enable / reactivation.
                if !cx.global::<crate::settings::Settings>().cursor_blink()
                    || !editor.blink.window_active
                {
                    if !editor.blink.visible {
                        editor.blink.visible = true;
                        cx.notify();
                    }
                    return;
                }
                editor.blink.visible = !editor.blink.visible;
                cx.notify();
                editor.schedule_blink(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Call Steel fn `callback` after `ms` milliseconds (Emacs `run-with-timer`).
    fn run_with_timer(&mut self, ms: u64, callback: String, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(Duration::from_millis(ms));
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |editor, _| {
                // Publish the active buffer so a timer callback reading
                // `(current-buffer)` sees the real focus, not a stale snapshot
                // (mirrors `run_script`/`run_hook`). Agent-shell's coalesced
                // write flush relies on this to avoid stealing focus.
                editor.publish_point();
                editor.script.dispatch_action(&callback, None)
            })
            .ok();
        })
        .detach();
    }

    fn open_transient(
        &mut self,
        title: String,
        columns: Vec<crate::script::TransientColumn>,
        sticky: bool,
        cx: &mut Context<Self>,
    ) {
        self.transient = Some(TransientState {
            title,
            columns,
            sticky,
        });
        cx.notify();
    }

    /// Whether the open transient stays put after a row is selected.
    fn transient_is_sticky(&self) -> bool {
        self.transient.as_ref().is_some_and(|t| t.sticky)
    }

    fn transient_quit(&mut self, cx: &mut Context<Self>) {
        if self.transient.take().is_some() {
            cx.notify();
        }
    }

    /// The command bound to `chord` in the open transient, if any (any column).
    fn transient_command(&self, chord: &str) -> Option<String> {
        self.transient
            .as_ref()?
            .columns
            .iter()
            .flat_map(|c| c.rows.iter())
            .find(|r| r.key == chord)
            .map(|r| r.command.clone())
    }

    /// A mouse click on a transient row: close the popup and run its command,
    /// mirroring the keyboard dispatch in `on_key_down`.
    pub fn transient_click(&mut self, command: &str, cx: &mut Context<Self>) {
        // A sticky popup stays open on selection (the command re-opens to refresh
        // or calls `transient-quit`); a normal one closes (`take`) before its
        // command runs. Either way, bail if there's nothing open.
        if !self.transient_is_sticky() && self.transient.take().is_none() {
            return;
        }
        cx.notify();
        // Transient rows store a bare command name (e.g. `magit-do-commit`); wrap
        // it in parens so it's *called*, matching the keyboard path.
        self.run_script(&format!("({command})"), cx);
    }

    /// A render snapshot of the open transient popup.
    pub fn transient_view(&self) -> Option<TransientView> {
        self.transient.as_ref().map(|t| TransientView {
            title: t.title.clone(),
            columns: t
                .columns
                .iter()
                .map(|c| TransientColumnView {
                    title: c.title.clone(),
                    rows: c
                        .rows
                        .iter()
                        .map(|r| TransientRow {
                            key: pretty_key(&r.key),
                            label: r.label.clone(),
                            command: r.command.clone(),
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    // --- Title-bar dropdowns (perspectives switcher + Scheme menus) ----------

    /// The rows surviving `filter` as `(original_index, label)` — the shared
    /// definition of "visible" for rendering, arrow keys, and Enter.
    fn titlebar_dropdown_filtered(d: &TitlebarDropdownState) -> Vec<(usize, SharedString)> {
        let needle = d.filter.to_lowercase();
        d.items
            .iter()
            .enumerate()
            .filter(|(_, label)| needle.is_empty() || label.to_lowercase().contains(&needle))
            .map(|(index, label)| (index, label.clone()))
            .collect()
    }

    /// The unfolded title-bar menu, if any (at most one is open at a time).
    fn titlebar_dropdown_open_mut(&mut self) -> Option<&mut TitlebarDropdownState> {
        self.titlebar_dropdowns.iter_mut().find(|d| d.open)
    }

    /// Fold `d` and clear its transient search state.
    fn titlebar_dropdown_fold(d: &mut TitlebarDropdownState) {
        d.open = false;
        d.filter.clear();
        d.cursor = 0;
        d.selected = None;
    }

    /// Whether a title-bar dropdown menu is currently unfolded (and thus
    /// modal: editing keys belong to its search field, not the buffer).
    pub(super) fn titlebar_dropdown_open(&self) -> bool {
        self.titlebar_dropdowns.iter().any(|d| d.open)
    }

    /// Byte offset of char index `cursor` in `filter` (caret ops are
    /// char-indexed; `String` mutation wants bytes).
    fn titlebar_dropdown_byte_at(filter: &str, cursor: usize) -> usize {
        filter
            .char_indices()
            .nth(cursor)
            .map(|(byte, _)| byte)
            .unwrap_or(filter.len())
    }

    /// Render snapshots of the visible (non-empty) title-bar menus, left to
    /// right.
    pub fn titlebar_dropdown_views(&self) -> Vec<TitlebarDropdownView> {
        self.titlebar_dropdowns
            .iter()
            .filter(|d| !d.items.is_empty())
            .map(|d| {
                let items = Self::titlebar_dropdown_filtered(d);
                TitlebarDropdownView {
                    key: d.key.clone(),
                    trigger: if d.active.is_empty() {
                        d.trigger.clone()
                    } else {
                        d.active.clone()
                    },
                    section: d.section.clone(),
                    placeholder: d.placeholder.clone(),
                    selected: d.selected.map(|s| s.min(items.len().saturating_sub(1))),
                    items,
                    active: d.active.clone(),
                    open: d.open,
                    filter: d.filter.clone(),
                    cursor: d.cursor.min(d.filter.chars().count()),
                }
            })
            .collect()
    }

    /// Insert or replace the menu keyed `state.key`, keeping its position
    /// and — when it is unfolded (a find-file-hook re-publishing mid-browse) —
    /// its open state, filter, caret, and selection. A menu emptied of items
    /// stays in the list (hidden, and folded) so it keeps its slot.
    pub(super) fn titlebar_dropdown_publish(&mut self, mut state: TitlebarDropdownState) {
        match self
            .titlebar_dropdowns
            .iter()
            .position(|d| d.key == state.key)
        {
            Some(i) => {
                let old = &self.titlebar_dropdowns[i];
                if !state.items.is_empty() {
                    state.open = old.open;
                    state.filter = old.filter.clone();
                    state.cursor = old.cursor;
                    state.selected = old.selected;
                }
                self.titlebar_dropdowns[i] = state;
            }
            None => self.titlebar_dropdowns.push(state),
        }
    }

    /// A click on the trigger for dropdown `key`: unfold or fold its menu,
    /// folding any other. The search field starts empty on every unfold, and
    /// no row is highlighted until an arrow key summons the keyboard selection.
    pub fn titlebar_dropdown_toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        for d in &mut self.titlebar_dropdowns {
            let open = d.key == key && !d.open;
            Self::titlebar_dropdown_fold(d);
            d.open = open;
        }
        cx.notify();
    }

    /// Fold the open menu without selecting (backdrop click, Escape, C-g).
    pub fn titlebar_dropdown_close(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut() {
            Self::titlebar_dropdown_fold(d);
            cx.notify();
        }
    }

    /// A click on the row for item `index` (an index into the FULL item list,
    /// as carried by the filtered view) of dropdown `key`: fold the menu, then
    /// run the row's own command if it has one, else hand the row's label to
    /// the dropdown's callback (e.g. `(perspective-select "macros")`),
    /// mirroring `transient_click`'s close-then-dispatch shape.
    pub fn titlebar_dropdown_click(&mut self, key: &str, index: usize, cx: &mut Context<Self>) {
        let Some(d) = self.titlebar_dropdowns.iter_mut().find(|d| d.key == key) else {
            return;
        };
        let Some(item) = d.items.get(index).cloned() else {
            return;
        };
        Self::titlebar_dropdown_fold(d);
        let command = d.commands.get(index).filter(|c| !c.is_empty()).cloned();
        let callback = d.callback.clone();
        cx.notify();
        match command {
            Some(command) => self.script.dispatch_action(&command, None),
            None => self.script.dispatch_action(&callback, Some(item.as_ref())),
        }
    }

    /// Typed text while a menu is open (routed from the IME, like Helm's
    /// filter): insert at the caret. The keyboard selection resets — the
    /// narrowed list has no highlight until arrows summon one (Enter still
    /// picks the top match).
    pub(super) fn titlebar_dropdown_input(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut() {
            let at = Self::titlebar_dropdown_byte_at(&d.filter, d.cursor);
            d.filter.insert_str(at, text);
            d.cursor += text.chars().count();
            d.selected = None;
            cx.notify();
        }
    }

    /// Backspace in the search field: delete the char before the caret.
    pub(super) fn titlebar_dropdown_backspace(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut()
            && d.cursor > 0
        {
            d.cursor -= 1;
            let at = Self::titlebar_dropdown_byte_at(&d.filter, d.cursor);
            d.filter.remove(at);
            d.selected = None;
            cx.notify();
        }
    }

    /// Forward delete in the search field: delete the char under the caret.
    pub(super) fn titlebar_dropdown_delete(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut()
            && d.cursor < d.filter.chars().count()
        {
            let at = Self::titlebar_dropdown_byte_at(&d.filter, d.cursor);
            d.filter.remove(at);
            d.selected = None;
            cx.notify();
        }
    }

    /// Move the search-field caret by `delta` chars (Left/Right), clamped.
    pub(super) fn titlebar_dropdown_cursor_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut() {
            let max = d.filter.chars().count();
            d.cursor = d.cursor.min(max).saturating_add_signed(delta).min(max);
            cx.notify();
        }
    }

    /// Place the search-field caret at char index `pos` (a click on the field,
    /// or Home/End via 0 / `usize::MAX`), clamped to the text.
    pub fn titlebar_dropdown_cursor_set(&mut self, pos: usize, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut() {
            d.cursor = pos.min(d.filter.chars().count());
            cx.notify();
        }
    }

    /// Move the keyboard selection by `delta` within the filtered rows. The
    /// first press summons the highlight at the top row.
    pub(super) fn titlebar_dropdown_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(d) = self.titlebar_dropdown_open_mut() {
            let len = Self::titlebar_dropdown_filtered(d).len();
            if len == 0 {
                return;
            }
            let max = len - 1;
            d.selected = Some(match d.selected {
                None => 0,
                Some(s) => s.min(max).saturating_add_signed(delta).min(max),
            });
            cx.notify();
        }
    }

    /// Enter: pick the keyboard-selected row, or — with no selection but a
    /// non-empty filter — the top match (type-and-enter, the picker idiom).
    pub(super) fn titlebar_dropdown_select_current(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.titlebar_dropdowns.iter().find(|d| d.open) else {
            return;
        };
        let items = Self::titlebar_dropdown_filtered(d);
        let row = match d.selected {
            Some(s) => Some(s.min(items.len().saturating_sub(1))),
            None if !d.filter.is_empty() => Some(0),
            None => None,
        };
        if let Some(row) = row
            && let Some((index, _)) = items.get(row)
        {
            let (key, index) = (d.key.clone(), *index);
            self.titlebar_dropdown_click(&key, index, cx);
        }
    }

    // --- LSP --------------------------------------------------------------

    /// The URI identifying the active buffer for LSP (see [`document_uri`]).
    fn active_uri(&self) -> Option<String> {
        Some(document_uri(&self.active))
    }

    /// The LSP language id for the active buffer: the Scheme-configured mapping
    /// for its file extension, or — when there's no file — the active mode's
    /// language (so `*scratch*` in steel-mode resolves to `scheme`).
    fn active_language(&self) -> Option<String> {
        // An explicitly-entered `macros-steel` mode (`M-x macros-steel-mode`) forces
        // the editor-aware `macros-scheme` server even for a plain `.scm` file whose
        // path wouldn't otherwise match — the escape hatch's whole purpose. Without
        // this the extension map below would resolve `.scm` to `scheme` and the
        // mode's declared language would be silently ignored.
        if self.active.modes.active_language() == Some("macros-scheme") {
            return Some("macros-scheme".to_string());
        }
        if let Some(language) = self
            .active
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(|ext| self.extensions.get(ext))
        {
            // Editor config / extension Scheme uses a distinct `macros-scheme`
            // server (seeded with the editor API); plain `.scm` stays on the
            // vanilla `scheme` server. The split is by path, so it can't live in
            // the extension map — apply it here.
            if language == "scheme"
                && self
                    .active
                    .path
                    .as_deref()
                    .is_some_and(crate::runtime::is_macros_scheme_path)
            {
                return Some("macros-scheme".to_string());
            }
            return Some(language.clone());
        }
        self.active.modes.active_language().map(str::to_string)
    }

    /// The wire `languageId` for the active buffer's `didOpen`. Resolved from the
    /// file extension via the `language_ids` map (`set-language-id`), falling
    /// back to the server key (`active_language`) when no override is set. This
    /// is what lets a `.tsx` file open as `typescriptreact` on the shared
    /// `typescript` server, so JSX isn't flagged as syntax errors.
    fn active_language_id(&self, server_key: &str) -> String {
        self.active
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(|ext| self.language_ids.get(ext))
            .cloned()
            .unwrap_or_else(|| server_key.to_string())
    }

    /// Run the active buffer's mode hooks once (Emacs `run-mode-hooks`). For each
    /// active mode, every Scheme function registered with `add-hook` on the
    /// mode's name or on `<mode>-hook` is called
    /// (deduplicated, so a function on both `fundamental` and `steel` runs once).
    /// This is where steel-mode's `(add-hook "steel" "enable-lsp")` brings up the
    /// LSP — entirely Scheme-driven, with the default set in `steel-mode.scm`.
    pub fn run_mode_hooks_once(&mut self, cx: &mut Context<Self>) {
        if self.active.hooks_ran {
            return;
        }
        self.active.hooks_ran = true;
        let mut seen = std::collections::HashSet::new();
        let mut to_run = Vec::new();
        for mode in self.active.modes.names() {
            // `(add-hook "grid-mode" f)` and Emacs's `(add-hook "grid-mode-hook" f)`
            // both run.
            let emacs_name = format!("{mode}-hook");
            for key in [mode, emacs_name.as_str()] {
                if let Some(functions) = self.hooks.get(key) {
                    for function in functions {
                        if seen.insert(function.clone()) {
                            to_run.push(function.clone());
                        }
                    }
                }
            }
        }
        for function in to_run {
            // Dispatch to the engine thread; results apply via the drain task.
            self.script.dispatch_action(&function, None);
        }
        let _ = cx;
    }

    /// Diagnostics for the active buffer, mapped to byte offsets.
    pub fn active_diagnostics(&self) -> Vec<Diagnostic> {
        let Some(uri) = self.active_uri() else {
            return Vec::new();
        };
        let Some(raw) = self.diagnostics.get(&uri) else {
            return Vec::new();
        };
        raw.iter()
            .map(|d| {
                let start = self
                    .active
                    .buffer
                    .position_to_byte(d.start.line, d.start.character);
                let end = self
                    .active
                    .buffer
                    .position_to_byte(d.end.line, d.end.character)
                    .max(start);
                Diagnostic {
                    range: start..end,
                    severity: d.severity,
                    message: d.message.clone(),
                }
            })
            .collect()
    }

    /// `E:n W:n` summary for the mode line, or `None` if clean.
    pub fn diagnostic_summary(&self) -> Option<String> {
        let raw = self
            .active_uri()
            .and_then(|uri| self.diagnostics.get(&uri))?;
        if raw.is_empty() {
            return None;
        }
        let errors = raw
            .iter()
            .filter(|d| d.severity == lsp::Severity::Error)
            .count();
        let warnings = raw
            .iter()
            .filter(|d| d.severity == lsp::Severity::Warning)
            .count();
        Some(format!("E:{errors} W:{warnings}"))
    }

    /// The diagnostic message at point, if any (shown in the echo area). Collapsed
    /// to a single line — the echo row shapes it with `shape_line`, which panics
    /// on a newline, and LSP diagnostics (e.g. ocamllsp type errors) are often
    /// multi-line.
    fn diagnostic_at_offset(&self, offset: usize) -> Option<String> {
        self.active_diagnostics()
            .into_iter()
            .find(|d| d.range.contains(&offset) || d.range.start == offset)
            .map(|d| crate::util::single_line(&d.message))
    }

    fn diagnostic_at_point(&self) -> Option<String> {
        self.diagnostic_at_offset(self.point())
    }

    /// `completion-at-point`: try the Scheme CAPFs first (on the engine thread,
    /// results arrive via `apply_capf_result`); if none are registered, go
    /// straight to LSP.
    fn completion_at_point(&mut self, cx: &mut Context<Self>) {
        // The ANVIL REPL completes against its *live image* (its actual
        // bindings, including everything you've defined this session), not the
        // main engine's CAPFs or LSP — SLIME-style symbol completion. Uses a
        // Lisp-symbol boundary so `string->number` completes as one token.
        if self.active.modes.names().last().copied() == Some("anvil-repl-mode") {
            let point = self.point();
            let start = self.active.buffer.backward_symbol(point);
            let prefix = self.active.buffer.slice_to_string(start..point);
            self.stamp_completion_request();
            self.anvil.complete(&prefix, start, point);
            return;
        }
        let capf_enabled = cx.global::<crate::settings::Settings>().completion_capf();
        if capf_enabled && !self.capf_functions.is_empty() {
            let point = self.point();
            let start = self.active.buffer.word_start(point);
            let prefix = self.active.buffer.slice_to_string(start..point);
            self.publish_point();
            self.stamp_completion_request();
            self.script
                .dispatch_capf(self.capf_functions.clone(), &prefix, start, point);
            return;
        }
        self.completion_at_point_lsp(cx);
    }

    /// Where the completion prefix ending at `point` starts. Lisp buffers use a
    /// symbol boundary, so `plot-b` or `string->` is one token — what Scheme
    /// servers (the builtin one included) match labels against; a word-only
    /// prefix (`b`) would filter every returned `plot-…` label out. Elsewhere
    /// it's `word_start`, not `backward_word`: right after `self.` the prefix
    /// must be empty, or a motion-style word jump would filter every member
    /// candidate out.
    pub(super) fn completion_prefix_start(&self, point: usize) -> usize {
        if self.is_lisp_buffer() {
            self.active.buffer.backward_symbol(point)
        } else {
            self.active.buffer.word_start(point)
        }
    }

    /// Request completions from the active buffer's language server.
    fn completion_at_point_lsp(&mut self, cx: &mut Context<Self>) {
        // A full-sync server (the builtin Steel one) gets edits debounced: send
        // the pending text first, or it completes against the line as it was
        // before the last keystrokes (wrong prefix, wrong scope).
        self.flush_lsp_fullsync();
        let (Some(language), Some(uri)) = (self.active_language(), self.active_uri()) else {
            // Same contract as the !sent path below: drop the auto flag (or it
            // jams auto-completion for the session) and only echo for explicit
            // requests.
            if !std::mem::take(&mut self.completion_auto_pending) {
                self.echo(NO_LSP_FOR_BUFFER, cx);
            }
            return;
        };
        let point = self.point();
        let (line, character) = self.active.buffer.byte_to_position(point);
        // The range becomes the popup anchor AND the live filter prefix, so it
        // must match what the server treats as the prefix — see
        // `completion_prefix_start`.
        let word_range = self.completion_prefix_start(point)..point;
        let sent = if let Some(server) = self.servers.get_mut(&language) {
            if server.ready && server.open.contains(&uri) {
                // A newer request supersedes the in-flight one: its response
                // would be dropped anyway, so let the server stop working on it.
                if let Some(old) = server.pending_completion.take() {
                    server.client.cancel(old.id);
                }
                let id = server.client.completion(&uri, line, character);
                server.pending_completion = Some(PendingCompletion {
                    id,
                    range: word_range,
                });
                true
            } else {
                false
            }
        } else {
            false
        };
        if sent {
            self.stamp_completion_request();
        } else {
            // Nothing will answer this round, so the auto flag must drop here or
            // auto-completion is dead for the whole session (one keystroke in a
            // serverless buffer whose CAPFs decline would jam it). Auto requests
            // also stay quiet — no "No LSP" echo per keystroke in text buffers.
            let was_auto = std::mem::take(&mut self.completion_auto_pending);
            if !was_auto {
                self.echo(NO_LSP_FOR_BUFFER, cx);
            }
        }
    }

    /// Open the completion popup from Scheme CAPF results. An empty item list
    /// means no CAPF produced anything, so fall back to LSP (so a buffer with a
    /// language server still completes when its CAPFs decline).
    fn apply_capf_result(
        &mut self,
        start: usize,
        end: usize,
        items: Vec<(String, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        // A reply computed against since-changed text (or another buffer) is
        // dropped outright — its range addresses bytes that no longer exist.
        let Some(range) = self.completion_reply_range(start, end) else {
            return;
        };
        if items.is_empty() {
            self.completion_at_point_lsp(cx);
            return;
        }
        let items = items
            .into_iter()
            .map(|(label, annotation)| CompletionItem {
                insert_text: label.clone(),
                label,
                detail: annotation,
                kind: None,
                documentation: None,
                additional_edits: Vec::new(),
                data: serde_json::Value::Null,
                resolved: true,
            })
            .collect();
        self.show_completion(items, range, cx);
    }

    fn show_completion(
        &mut self,
        items: Vec<CompletionItem>,
        word_range: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let was_auto = std::mem::take(&mut self.completion_auto_pending);
        // A reply that lands after the user left the buffer's text — Esc to
        // evil Normal, or a prompt opened (`:`, M-x) — is dropped: the popup
        // is consulted before the minibuffer and the modal keymap for
        // Enter/Tab/arrows, so it would steal e.g. the RET of `:w`.
        if self.evil_intercepts() || self.minibuffer.is_some() || self.helm.is_some() {
            return;
        }
        if items.is_empty() {
            if !was_auto {
                self.echo("No completions", cx); // stay quiet for auto-triggers
            }
            return;
        }
        // Presentation is Scheme-configurable: an in-buffer popup (default) or
        // the minibuffer.
        let style = cx.global::<crate::settings::Settings>().completion_style();
        if style == crate::settings::CompletionStyle::Minibuffer {
            let prefix = self.active.buffer.slice_to_string(word_range.clone());
            self.completing_candidates = items.iter().map(|i| i.label.clone()).collect();
            self.completion_range = Some(word_range);
            self.open_minibuffer(
                "Complete: ",
                MinibufferPurpose::CompleteAtPoint,
                &prefix,
                cx,
            );
        } else {
            self.completion_popup = Some(CompletionPopup {
                items,
                anchor: word_range.start,
                selected: 0,
            });
            // Nothing survives the live prefix filter (the server matched a
            // different prefix than the anchor): the popup would render nothing
            // yet still capture Tab/RET/arrows, so don't keep it.
            if self.popup_filtered().is_empty() {
                self.completion_popup = None;
                if !was_auto {
                    self.echo("No completions", cx);
                }
                return;
            }
            self.completion_resolve_selected();
            cx.notify();
        }
    }

    // --- Command dispatch -------------------------------------------------

    /// Run a tree-sitter query over the active buffer's live parse tree and hand
    /// the captures to Steel `callback` as `"capture\tstart\tend\tkind"` records
    /// (one per line). The tree lives here on the UI thread; only the serialized
    /// result crosses to the engine thread. No grammar / uncompilable query →
    /// empty string (the callback decides what that means).
    fn tree_sitter_query(&mut self, query: String, callback: String) {
        let records = self
            .active
            .highlighter
            .as_ref()
            .and_then(|h| h.query_captures(self.active.buffer.rope(), &query))
            .map(|caps| {
                caps.iter()
                    .map(|(name, start, end, kind)| format!("{name}\t{start}\t{end}\t{kind}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        self.script.dispatch_action(&callback, Some(&records));
    }

    /// Hand the node chain covering `byte` (innermost first) to Steel `callback`
    /// as `"kind\tstart\tend"` records — lets a script ask "what am I inside?".
    fn tree_sitter_node_at(&mut self, byte: usize, callback: String) {
        let records = self
            .active
            .highlighter
            .as_ref()
            .map(|h| {
                h.node_at(byte)
                    .iter()
                    .map(|(kind, start, end)| format!("{kind}\t{start}\t{end}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        self.script.dispatch_action(&callback, Some(&records));
    }

    /// Run a keybinding/interactive Steel expression. A bare `(builtin)` applies
    /// synchronously (instant motion, etc.); anything else (user functions,
    /// argument calls) is dispatched to the engine thread so a slow script can't
    /// freeze the UI — its commands arrive via `apply_script_outcome`.
    fn run_script(&mut self, source: &str, cx: &mut Context<Self>) {
        // Vim-grammar commands with string arguments (`(evil-use-register
        // "a")`, …) apply synchronously, like builtins: the next keystroke or
        // replayed command depends on their state.
        if let Some(command) = vim::native_command(source) {
            self.apply_command(command, cx);
            return;
        }
        if let Some(commands) = self.script.builtins_for(source) {
            // One builtin: apply directly (motions/edits manage their own undo).
            // A sequence: group it so the compound binding undoes as one unit.
            match <[EditorCommand; 1]>::try_from(commands) {
                Ok([command]) => self.apply_command(command, cx),
                Err(commands) => {
                    self.begin_undo_group();
                    for command in commands {
                        self.apply_command(command, cx);
                    }
                    self.end_undo_group();
                }
            }
            self.evil_clamp_cursor();
        } else {
            // Publish point so a command reading `(buffer-line)` /
            // `(current-buffer)` sees it as of this invocation. A pending
            // `C-u` reaches the Scheme command as `(prefix-count)` (Emacs's
            // `current-prefix-arg`: `C-u M-!` inserts the output) instead of
            // leaking onto the first builtin the command queues.
            let saved_count = self.prefix_count;
            if let Some(n) = self.uarg.pending.take() {
                self.prefix_count = usize::try_from(n).unwrap_or(0);
            }
            self.publish_point();
            self.prefix_count = saved_count;
            self.script.dispatch(source, false);
            self.vim.went_async = true;
        }
    }

    /// Run a key-resolved command and, if recording a keyboard macro, append it
    /// (skipping the macro-control commands themselves).
    fn dispatch_recorded(&mut self, expr: &str, cx: &mut Context<Self>) {
        self.record_event(MacroEvent::Command(expr.to_string()));
        self.run_script(expr, cx);
    }

    /// Run a modal binding under a typed count (`3dd`, `5G`, `3w`). Three cases:
    /// - a count-aware command (`set-count-aware!`) runs ONCE and reads the
    ///   count as `(prefix-count)` — `5G` means line 5, not "G five times";
    /// - a builtin repeats synchronously (each run sees the last one's result);
    /// - any other Scheme command repeats as a chain, each run dispatched after
    ///   the previous one applied (see [`CountRepeat`]).
    ///
    /// Either repeat form squashes its edits into ONE undo step, and kills
    /// after the first append, so `3dd` then `p` / `u` restore all three lines.
    fn dispatch_counted(&mut self, expr: &str, count: Option<usize>, cx: &mut Context<Self>) {
        let Some(n) = count else {
            self.dispatch_recorded(expr, cx);
            return;
        };
        // Record the count with the command, so replay re-runs `3dd`, not `dd`.
        self.record_event(MacroEvent::Counted(expr.to_string(), n));
        self.run_counted(expr, n, cx);
    }

    /// The unrecorded body of [`Self::dispatch_counted`] (macro replay calls
    /// it directly).
    fn run_counted(&mut self, expr: &str, n: usize, cx: &mut Context<Self>) {
        // The command name: the call's first token (`(evil-macro-play "a")`).
        if self.count_aware.contains(vim::expr_head(expr)) {
            self.prefix_count = n;
            self.run_script(expr, cx);
            self.prefix_count = 0;
            return;
        }
        if n <= 1 {
            self.run_script(expr, cx);
            return;
        }
        let base = self.undo_base();
        if self.script.builtins_for(expr).is_some() {
            for i in 0..n {
                self.kill_append = i > 0;
                self.run_script(expr, cx);
            }
            self.kill_append = false;
            self.sync_appended_kill(cx);
            self.squash_undo_since(&base);
            return;
        }
        let source = format!("(begin {expr} (count-repeat-step))");
        self.count_repeat = Some(CountRepeat {
            source: source.clone(),
            total: n,
            done: 0,
            undo_base: base,
        });
        self.run_script(&source, cx);
    }

    /// `(count-repeat-step)` — the tail of each chained run: dispatch the next
    /// run against a fresh snapshot, or finish the chain.
    fn count_repeat_step(&mut self, _cx: &mut Context<Self>) {
        let Some(r) = self.count_repeat.as_mut() else {
            return;
        };
        r.done += 1;
        if r.done >= r.total {
            let base = r.undo_base.clone();
            self.count_repeat = None;
            self.squash_undo_since(&base);
            return;
        }
        let source = r.source.clone();
        self.publish_point();
        self.script.dispatch(&source, false);
    }

    /// After kills were appended (`kill_append`), push the combined kill to the
    /// OS clipboard — otherwise the clipboard holds only the last piece, and
    /// the next `p` would adopt that piece as an "external" copy.
    fn sync_appended_kill(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.kill_appended)
            && let Some(text) = self.kill_ring.last().cloned()
        {
            self.set_clipboard(&text, cx);
        }
    }

    /// Vim Normal state never rests the cursor past the last char of a line
    /// (only an empty line puts it on the line end). Motions like `$`/`l` and
    /// deletes at EOL can leave it there; pull it back one grapheme, so `$x`,
    /// `$a`, `p` act on the line's last char instead of the line break.
    fn evil_clamp_cursor(&mut self) {
        if self.evil != EvilState::Normal
            || self.modal_helix
            || self.active.special.is_some()
            || self.active.mark_active
            || !self.active.selected_range.is_empty()
            || self.has_extra_cursors()
        {
            return;
        }
        let p = self.point();
        let line = self.active.buffer.line_range_at(p);
        if p == line.end && line.start < line.end {
            let prev = self.active.buffer.prev_grapheme(p);
            self.collapse_to(prev);
        }
    }

    // --- Keyboard macros --------------------------------------------------

    fn start_kbd_macro(&mut self, cx: &mut Context<Self>) {
        self.vim.macro_register = None;
        self.macro_recording = Some(Vec::new());
        self.echo("Recording keyboard macro…", cx);
    }

    fn end_kbd_macro(&mut self, cx: &mut Context<Self>) {
        if let Some(rec) = self.macro_recording.take() {
            // `qa … q` stores it in register a too (`qA` appends).
            if let Some(r) = self.vim.macro_register.take() {
                let lower = r.to_ascii_lowercase();
                let stored = self.vim.macros.entry(lower).or_default();
                if r.is_ascii_uppercase() {
                    stored.extend(rec.iter().cloned());
                } else {
                    *stored = rec.clone();
                }
                self.vim.last_macro_register = Some(lower);
            }
            self.last_macro = rec;
            self.echo("Keyboard macro defined", cx);
        }
    }

    fn call_last_kbd_macro(&mut self, cx: &mut Context<Self>) {
        if self.last_macro.is_empty() {
            self.echo("No keyboard macro defined", cx);
            return;
        }
        // Replayed one command at a time, each against a fresh snapshot (see
        // `replay_events`).
        self.replay_events(self.last_macro.clone(), false, cx);
    }

    /// Mark startup config as fully applied, so plain keystrokes stop being
    /// swallowed (see `config_pending`). Called once the bundled scripts and the
    /// user's `init.scm` have been evaluated — including when that path errors,
    /// so a broken config can never wedge the keyboard.
    pub fn mark_config_loaded(&mut self, cx: &mut Context<Self>) {
        if self.config_pending {
            self.config_pending = false;
            cx.notify();
        }
    }

    /// Evaluate Steel synchronously and apply its commands in order. Used at
    /// startup, where config (keymaps, options, themes) must take effect before
    /// the editor is driven. Blocks the caller — fine before the UI is live.
    pub fn run_script_blocking(&mut self, source: &str, cx: &mut Context<Self>) {
        // Apply the commands accumulated before any error too (Emacs `init.el`
        // semantics) — a typo late in `init.scm` must not discard the
        // `(evil-mode)` / keymaps that ran before it.
        let (commands, error) = self.script.eval_collecting(source);
        for command in commands {
            self.apply_command(command, cx);
        }
        if let Some(err) = error {
            self.echo(format!("error: {err}"), cx);
        }
    }

    /// What `eval-last-sexp` evaluates: the active region (all its forms), else
    /// the complete datum before point. `None` when there is nothing.
    fn region_or_last_sexp(&self) -> Option<String> {
        if let Some(region) = self.region() {
            return Some(self.active.buffer.slice_to_string(region));
        }
        let text = self.active.buffer.slice_to_string(0..self.point());
        last_sexp(&text).map(|range| text[range].to_string())
    }

    /// [`Self::run_script_blocking`] with a deadline, for the user's `init.scm`:
    /// startup waits on it from the UI thread, so a loop in the config must be
    /// interrupted rather than hang the editor.
    pub fn run_script_blocking_within(
        &mut self,
        source: &str,
        timeout: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let (commands, error) = self.script.eval_collecting_within(source, timeout);
        for command in commands {
            self.apply_command(command, cx);
        }
        if let Some(err) = error {
            self.echo(format!("error in init.scm: {err}"), cx);
        }
    }

    /// Evaluate Steel source interactively (steel-mode): dispatch to the engine
    /// thread and echo the result (handled in `apply_script_outcome`).
    fn eval_interactive(&mut self, source: &str, _cx: &mut Context<Self>) {
        self.script.dispatch(source, true);
    }

    // --- Key handling -----------------------------------------------------
}

/// The byte offset in `s` of UTF-16 offset `u16` (clamped to `s`'s end).
fn utf16_to_byte_in(s: &str, u16: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        if units >= u16 {
            return i;
        }
        units += c.len_utf16();
    }
    s.len()
}

/// The grapheme boundary immediately before `cursor` in `s`.
fn prev_grapheme(s: &str, cursor: usize) -> usize {
    s[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// The grapheme boundary immediately after `cursor` in `s`.
fn next_grapheme(s: &str, cursor: usize) -> usize {
    s[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |g| cursor + g.len())
}

/// Compose the trailing-token indent rules with the structural result. A line
/// ending in an `indent-after` token opens the next line at least one `step` past
/// the current line (`max`, so it never *reduces* a deeper structural indent). A
/// line ending in an `align-after` token pins the next line to the current line's
/// column. Neither list matching leaves the structural result untouched.
fn apply_trailing_rule(
    structural: usize,
    cur: usize,
    step: usize,
    tok: &str,
    indent_after: &str,
    align_after: &str,
) -> usize {
    if indent_after.split_whitespace().any(|t| t == tok) {
        structural.max(cur + step)
    } else if align_after.split_whitespace().any(|t| t == tok) {
        cur
    } else {
        structural
    }
}

/// Expand a leading `~/` to the home directory.
fn expand_path(input: &str) -> String {
    crate::util::expand_tilde(input)
}

/// Decode an image file into a GPUI [`RenderImage`] (RGBA → BGRA, the byte order
/// the renderer's atlas expects). Returns `None` if the file is missing or not a
/// decodable image. Runs on a background thread (it can be slow for big files).
fn load_inline_image(path: &Path) -> Option<CachedImage> {
    let bytes = std::fs::read(path).ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?;
    let mut rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    // GPUI's sprite atlas samples BGRA; swap the red/blue channels in place.
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let frame = image::Frame::new(rgba);
    let data = Arc::new(RenderImage::new(vec![frame]));
    Some(CachedImage {
        data,
        width,
        height,
    })
}

/// The highlighting language for a file, by exact basename then extension.
/// The basename check (the filenames column of `highlight::LANGUAGES`) is how
/// extensionless config files like Dune's `dune-project` match. On the
/// extension side, bundled grammars win; otherwise a grammar a user installed
/// at runtime (M-x install-language) may claim the extension — checked so
/// opening the file highlights without a restart.
fn language_for_path(path: &Path) -> Option<&'static str> {
    if let Some(lang) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(crate::highlight::language_for_filename)
    {
        return Some(lang);
    }
    let ext = path.extension().and_then(|e| e.to_str())?;
    crate::highlight::language_for_extension(ext)
        .or_else(|| crate::grammar::language_for_extension(ext))
}

/// A blank document with the shared default editing state (empty selection, a
/// fresh undo tree, no overlays/locals/faces). The `make_*` constructors layer
/// the buffer contents, mode stack, and any special/hooks flags on top via
/// struct-update syntax, so the ~15 invariant fields live in one place.
fn blank_document(
    name: String,
    path: Option<PathBuf>,
    contents: &str,
    modes: ModeStack,
) -> Document {
    Document {
        name,
        path,
        buffer: Buffer::new(contents),
        selected_range: 0..0,
        selection_reversed: false,
        marked_range: None,
        mark_active: false,
        undo: UndoTree::default(),
        scroll_offset: px(0.),
        modes,
        highlighter: None,
        modified: false,
        hooks_ran: false,
        special: None,
        line_faces: HashMap::new(),
        line_icons: HashMap::new(),
        gutter_dots: HashMap::new(),
        line_images: HashMap::new(),
        overlays: Overlays::default(),
        locals: HashMap::new(),
        diff_marks: HashMap::new(),
        ghost: None,
        grid: None,
        grid_stash: None,
        repl_input: None,
    }
}

/// The mode stack of a Scheme buffer. Scheme keeps the `steel` major mode
/// (keymaps, hooks, built-in LSP); config / extension Scheme (`macros`: under
/// the config dir, or `*.macros.scm`) instead gets the editor-aware
/// `macros-steel` mode and `macros-scheme` server — completion/hover/
/// diagnostics over the editor API. Both highlight with Scheme's grammar.
fn scheme_mode_stack(macros: bool) -> ModeStack {
    ModeStack::with_major(if macros {
        Mode::new("macros-steel").with_language("macros-scheme")
    } else {
        Mode::new("steel").with_language("scheme")
    })
}

/// Create the filesystem watcher, or `None` if it can't start (auto-revert /
/// live-refresh then no-op). In test builds the watcher is off BY DEFAULT —
/// its thread fires real (nondeterministic) events and dropping/sending from
/// that thread would wake the drain task on a foreign thread — but the real
/// `new_debouncer` path is still compiled in and reachable: a test that wants
/// to exercise watcher setup / `reconcile_watches` / the debouncer callback
/// clears the suppression with [`set_fs_watcher_suppressed`] (gpui-ce's
/// foreign-thread assert is already disabled by `test_context`'s
/// `allow_parking`). Previously the whole thing was `#[cfg(test)]`'d out,
/// which made those paths untestable.
fn new_fs_debouncer(
    fs_tx: futures::channel::mpsc::UnboundedSender<Vec<PathBuf>>,
) -> Option<Debouncer<RecommendedWatcher>> {
    #[cfg(test)]
    if FS_WATCHER_SUPPRESSED.load(std::sync::atomic::Ordering::SeqCst) {
        drop(fs_tx); // closes the channel; the drain task just ends
        return None;
    }
    new_debouncer(
        Duration::from_millis(400),
        move |res: DebounceEventResult| {
            if let Ok(events) = res {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                if !paths.is_empty() {
                    let _ = fs_tx.unbounded_send(paths);
                }
            }
        },
    )
    .ok()
}

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
static FS_WATCHER_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Opt tests into (or out of) the real filesystem watcher: `Editor::new` calls
/// [`new_fs_debouncer`], which consults this flag. `test_context` suppresses
/// it; a watcher test clears the flag before creating its editor and can
/// restore it afterwards. NOTE: process-global — while the flag is cleared,
/// every editor created by concurrently running tests gets a live watcher.
#[cfg(test)]
pub(crate) fn set_fs_watcher_suppressed(suppressed: bool) {
    FS_WATCHER_SUPPRESSED.store(suppressed, Ordering::SeqCst);
}

/// Shared test-context bootstrap for every `#[gpui::test]` setup helper:
/// allows foreign-thread wakes (the Scheme engine thread sends outcomes from
/// its own thread; gpui-ce's test scheduler asserts on that unless parking is
/// allowed) and installs default globals. Also suppresses the real filesystem
/// watcher — real events are nondeterministic, and tests that want them opt
/// back in via `set_fs_watcher_suppressed(false)`. Call this before creating
/// the test editor.
#[cfg(test)]
pub(crate) fn test_context(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    set_fs_watcher_suppressed(true);
    cx.update(|cx| {
        cx.set_global(crate::theme::Theme::default());
        cx.set_global(crate::settings::Settings::default());
    });
}

/// Build a fresh document for `contents`, choosing modes/highlighter by file type.
fn make_document(name: String, path: Option<PathBuf>, contents: &str) -> Document {
    let language = path.as_deref().and_then(language_for_path);
    let (modes, highlighter) = match language {
        Some("scheme") => {
            let macros = path
                .as_deref()
                .is_some_and(crate::runtime::is_macros_scheme_path);
            (scheme_mode_stack(macros), Highlighter::scheme())
        }
        // Other languages: a major mode named after the language, with its
        // bundled tree-sitter grammar (if any) for highlighting.
        Some(lang) => (
            ModeStack::with_major(Mode::new(lang).with_language(lang)),
            Highlighter::for_language(lang),
        ),
        None => (ModeStack::fundamental(), None),
    };
    Document {
        highlighter,
        ..blank_document(name, path, contents, modes)
    }
}

/// A normal (editable) buffer for logs like `*Messages*` — `special: None` so
/// it can be edited/copied, `messages-mode` (no language) so no LSP/highlighter
/// runs on it, `hooks_ran` so it never tries to start a server.
fn make_log_document(name: String, contents: &str) -> Document {
    let modes = ModeStack::with_major(Mode::new("messages-mode"));
    Document {
        hooks_ran: true,
        ..blank_document(name, None, contents, modes)
    }
}

/// A read-only program-generated buffer (the magit pattern): no file, no
/// highlighter, major mode `mode` stacked on `fundamental` so its keymap
/// resolves. Hooks are marked run (these buffers don't take mode hooks).
fn make_special_document(name: String, mode: &str, contents: &str) -> Document {
    let modes = ModeStack::with_major(Mode::new(mode));
    Document {
        hooks_ran: true,
        special: Some(mode.to_string()),
        ..blank_document(name, None, contents, modes)
    }
}

/// Filesystem completions for a (possibly partial) path: entries in the
/// directory part whose names start with the filename part, as full paths.
fn file_completions(input: &str) -> Vec<String> {
    let expanded = expand_path(input);
    let (dir, prefix) = if expanded.ends_with('/') {
        (PathBuf::from(&expanded), String::new())
    } else {
        let path = Path::new(&expanded);
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let prefix = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        (dir, prefix)
    };
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let mut full = dir.join(&name).to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    full.push('/');
                }
                out.push(full);
            }
        }
    }
    out.sort();
    out
}

/// Render completion candidates for the echo area, capped so a long list
/// doesn't overflow the line.
fn format_candidates(candidates: &[String]) -> String {
    const MAX_SHOWN: usize = 12;
    let shown = candidates.len().min(MAX_SHOWN);
    let mut out = format!(
        "{} matches: {}",
        candidates.len(),
        candidates[..shown].join("  ")
    );
    if candidates.len() > shown {
        out.push_str("  …");
    }
    out
}

/// The trailing Scheme symbol token of `s`, as a `(start, token)` pair where
/// `token == &s[start..]`. The token runs back to the previous delimiter —
/// whitespace or one of `()[]{}'`\``,";` — so completing it leaves the rest of
/// the expression (open parens, earlier args) untouched.
fn scheme_symbol_token(s: &str) -> (usize, &str) {
    let is_delim = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '(' | ')' | '[' | ']' | '{' | '}' | '\'' | '`' | ',' | '"' | ';'
            )
    };
    let start = s
        .char_indices()
        .rev()
        .find(|(_, c)| is_delim(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    (start, &s[start..])
}

/// Longest common byte prefix of `items`, trimmed to a char boundary.
fn longest_common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.len();
    for item in &items[1..] {
        let common = first
            .bytes()
            .zip(item.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        end = end.min(common);
    }
    first[..first.floor_char_boundary(end)].to_string()
}

/// The startup `default-directory` for fileless buffers: the process cwd, except
/// that a cwd of `/` (what macOS gives apps launched from the Dock or Finder) or
/// an unreadable one falls back to the home directory — as Emacs.app does.
fn startup_directory(cwd: Option<PathBuf>, home: Option<PathBuf>) -> String {
    let dir = match cwd {
        Some(d) if d.parent().is_some() => Some(d),
        cwd => home.or(cwd),
    };
    dir.map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The symbol (identifier) under `point`, for `anvil-doc`. A run of non-delimiter
/// characters around point; `None` if point sits on whitespace or a delimiter.
fn symbol_at(text: &str, point: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let is_sym = |c: u8| {
        !c.is_ascii_whitespace()
            && !matches!(
                c,
                b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';' | b'\'' | b'`' | b','
            )
    };
    let p = point.min(text.len());
    let mut start = p;
    while start > 0 && is_sym(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = p;
    while end < bytes.len() && is_sym(bytes[end]) {
        end += 1;
    }
    (start < end).then(|| text[start..end].to_string())
}

/// Prettify a chord for display in a transient menu, Emacs-style: `shift-p` →
/// `P`, `ctrl-x` → `C-x`, `alt-a` → `M-a`. Matching still uses the raw chord.
fn pretty_key(key: &str) -> String {
    if let Some(rest) = key.strip_prefix("shift-")
        && rest.chars().count() == 1
    {
        return rest.to_uppercase();
    }
    key.replace("ctrl-", "C-")
        .replace("alt-", "M-")
        .replace("shift-", "S-")
}

/// Prettify a whole space-separated chord sequence (e.g. `"ctrl-x ctrl-t"` →
/// `"C-x C-t"`) for the pending-chord echo. Each chord is run through
/// [`pretty_key`] so the per-chord shift→uppercase rule still applies.
fn pretty_sequence(seq: &str) -> String {
    seq.split(' ').map(pretty_key).collect::<Vec<_>>().join(" ")
}

/// A shifted letter as the literal character it types (shift+f → `"F"`), so a
/// transient row can be declared with the glyph users see (`"F"`, `"G"`).
/// Only bare shift qualifies — ctrl/alt/cmd chords keep their prefixed form.
fn shifted_letter(ks: &Keystroke) -> Option<String> {
    if ks.modifiers.shift
        && !ks.modifiers.control
        && !ks.modifiers.alt
        && !ks.modifiers.platform
        && ks.key.len() == 1
        && ks
            .key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
    {
        Some(ks.key.to_ascii_uppercase())
    } else {
        None
    }
}

/// Render a [`Keystroke`] as a chord string like `"ctrl-g"` or `"alt-shift-,"`.
/// Order is fixed (control, alt, shift, platform) so keymap entries can match.
fn chord_string(ks: &Keystroke) -> String {
    let mut s = String::new();
    if ks.modifiers.control {
        s.push_str("ctrl-");
    }
    if ks.modifiers.alt {
        s.push_str("alt-");
    }
    if ks.modifiers.shift {
        s.push_str("shift-");
    }
    if ks.modifiers.platform {
        s.push_str("cmd-");
    }
    s.push_str(&ks.key);
    s
}

/// Ceiling on how many times a universal-argument repeats a command. A repeat
/// is a foreground loop (each iteration reparses, re-lays-out, and re-reveals
/// the cursor), so an unbounded count is a hung window, not a slow command —
/// and the counts that get that large are never typed on purpose. `C-u` held
/// down on key-repeat stacks the ×4 default into the billions within a second;
/// this is the valve that keeps that recoverable.
const MAX_PREFIX_REPEAT: i64 = 100_000;

/// The numeric value of a universal-argument from its parts: an explicit typed
/// number wins; otherwise each `C-u` multiplies the default by 4 (`C-u` = 4,
/// `C-u C-u` = 16, …); with neither, the count is 1. Clamped to
/// [`MAX_PREFIX_REPEAT`] — without it a long `C-u` chain saturates to
/// `i64::MAX` and the repeat loop never returns.
fn prefix_arg_value(digits: Option<i64>, cu: u32) -> i64 {
    let n = match digits {
        Some(n) => n,
        None if cu > 0 => 4i64.saturating_pow(cu.min(31)),
        None => 1,
    };
    n.min(MAX_PREFIX_REPEAT)
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.window_handle = Some(window.window_handle());
        // Observe window focus once, so the cursor only blinks while the window
        // is active. Seed the current state here; the observer handles later
        // activate/deactivate changes.
        if self.blink.observer.is_none() {
            self.blink.window_active = window.is_window_active();
            self.blink.observer =
                Some(cx.observe_window_activation(window, |editor, window, cx| {
                    editor.set_window_active(window.is_window_active(), cx);
                }));
        }
        let theme = cx.global::<Theme>();
        let background = theme.background();
        let foreground = theme.foreground();
        // Font size / line height are Scheme-configurable via `(set-option ...)`;
        // the custom element inherits them from `window.text_style()`.
        let settings = cx.global::<crate::settings::Settings>();
        let font_size = settings.font_size();
        let line_height = settings.line_height();
        let font_family = settings.font_family().to_string();
        div()
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .size_full()
            .p(px(8.))
            .bg(background)
            .text_color(foreground)
            .font_family(font_family)
            .text_size(px(font_size))
            .line_height(px(line_height))
            .on_action(cx.listener(Self::act_backspace))
            .on_action(cx.listener(Self::act_delete))
            .on_action(cx.listener(Self::act_left))
            .on_action(cx.listener(Self::act_right))
            .on_action(cx.listener(Self::act_up))
            .on_action(cx.listener(Self::act_down))
            .on_action(cx.listener(Self::act_home))
            .on_action(cx.listener(Self::act_end))
            .on_action(cx.listener(Self::act_newline))
            .on_action(cx.listener(Self::act_tab))
            .on_action(cx.listener(Self::act_select_left))
            .on_action(cx.listener(Self::act_select_right))
            .on_action(cx.listener(Self::act_select_all))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            // `on_mouse_up` only fires when the release lands inside the editor's
            // hitbox (gpui gates it on `hitbox.is_hovered`). A drag that ends
            // past the text edge, over the mode line/another pane, or outside the
            // window would otherwise never clear `is_selecting`, so highlighting
            // would keep following the pointer after release. `on_mouse_up_out`
            // catches exactly those releases (macOS keeps delivering the up to
            // the capturing window even outside its bounds).
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            // Open files dragged onto the window from the OS. gpui turns an
            // external file drop into an `ExternalPaths` drag, so a plain
            // `on_drop` of that type fires once the user releases over us.
            .on_drop::<ExternalPaths>(cx.listener(Self::on_drop_files))
            .child(self.render_layout(&self.layout, &[], cx))
    }
}

impl Editor {
    /// Typed text (the IME's `replace_text_in_range`, minus the window) —
    /// also how `:normal` replays printable keys in Insert state.
    pub(super) fn type_text(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        cx: &mut Context<Self>,
    ) {
        if self.titlebar_dropdown_open() {
            self.titlebar_dropdown_input(new_text, cx);
            return;
        }
        if self.helm.is_some() {
            self.helm_input(new_text, cx);
            return;
        }
        if self.minibuffer.is_some() {
            self.mb_insert(new_text, cx);
            return;
        }
        if self.grid_type_text(new_text, cx) {
            return; // grid: the cell editor (or spreadsheet typing) took it
        }
        if self.evil_intercepts() {
            return; // Normal/Visual: plain keys are commands, not text
        }
        if self.transient.is_some() || self.active.special.is_some() {
            return; // modal transient / read-only special buffer: never insert
        }
        if !self.pending_keys.is_empty() {
            return; // mid key-sequence; ignore stray text the platform may emit
        }
        // universal-argument (Emacs `C-u N` then a literal char): insert it N
        // times. Repeat up front so a recorded keyboard macro replays the full
        // text and electric-pair / completion see the final string.
        let repeated;
        let new_text = match self.uarg.pending.take() {
            Some(n) if n > 1 => {
                self.status = SharedString::default();
                repeated = new_text.repeat(n as usize);
                repeated.as_str()
            }
            _ => new_text,
        };
        // While avy hints are showing, the next printable key picks a hint: hand
        // it to Scheme's `avy-pick` and consume it (never self-insert).
        if !self.avy_hints.is_empty() {
            self.avy_hints.clear();
            cx.notify();
            self.script.dispatch_action("avy-pick", Some(new_text));
            return;
        }
        // Record self-inserted text into an in-progress keyboard macro.
        self.record_event(MacroEvent::Text(new_text.to_string()));

        let range = range_utf16
            .as_ref()
            .map(|r| self.active.buffer.range_from_utf16(r))
            .or_else(|| self.active.marked_range.clone())
            .unwrap_or_else(|| self.active.selected_range.clone());

        // multiple-cursors: replicate the typed text at every cursor.
        if self.has_extra_cursors() && range.is_empty() {
            self.multi_insert(new_text, cx);
            return;
        }

        // electric-pair-mode: handle a single typed delimiter specially.
        if self.electric_pair
            && new_text.chars().count() == 1
            && self.electric_pair_insert(&range, new_text, cx)
        {
            return;
        }

        let cursor = self.edit(range, new_text);
        self.collapse_to(cursor);
        if matches!(new_text, "}" | "]" | ")") {
            self.electric_closer_reindent(cx);
        }
        self.ensure_cursor_visible();
        // A trigger character (e.g. `.`) always starts a fresh completion, even
        // mid-word with a popup already open — otherwise typing `List.` would just
        // refilter (and drop) the `List` popup instead of requesting members.
        let triggers = cx
            .global::<crate::settings::Settings>()
            .completion_trigger_chars(self.active_language().as_deref());
        let trigger = new_text.chars().count() == 1
            && new_text
                .chars()
                .next()
                .is_some_and(|c| self.lsp_trigger_char(c, &triggers));
        if trigger {
            self.completion_popup = None;
            self.completion_auto_pending = false;
            self.maybe_auto_complete(new_text, cx);
        } else if self.completion_popup.is_some() {
            self.popup_refilter(cx);
        } else {
            self.maybe_auto_complete(new_text, cx);
        }
        cx.notify();
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = clamp_range(
            self.active.buffer.range_from_utf16(&range_utf16),
            self.active.buffer.len(),
        );
        actual_range.replace(self.active.buffer.range_to_utf16(&range));
        Some(self.active.buffer.slice_to_string(range))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self
                .active
                .buffer
                .range_to_utf16(&self.active.selected_range),
            reversed: self.active.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.active
            .marked_range
            .as_ref()
            .map(|range| self.active.buffer.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.active.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.type_text(range_utf16, new_text, cx);
        self.anvil_eldoc_tick();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The same routing `replace_text_in_range` does. Composition (dead keys,
        // CJK IMEs) goes through THIS entry point, so a filter surface missing
        // here doesn't just lose the composed text — it lands the text in the
        // buffer underneath the popup. IME composition isn't tracked in any of
        // these single-line fields; treat it as a direct insert of the text so
        // far, matching the minibuffer's long-standing behavior.
        if self.titlebar_dropdown_open() {
            self.titlebar_dropdown_input(new_text, cx);
            return;
        }
        if self.helm.is_some() {
            self.helm_input(new_text, cx);
            return;
        }
        if self.minibuffer.is_some() {
            self.mb_insert(new_text, cx);
            return;
        }
        if self.grid_type_text(new_text, cx) {
            return;
        }
        // Surfaces where plain keys are never text: evil Normal/Visual (they're
        // commands), a modal transient, and read-only special buffers.
        if self.evil_intercepts() || self.transient.is_some() || self.active.special.is_some() {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.active.buffer.range_from_utf16(r))
            .or_else(|| self.active.marked_range.clone())
            .unwrap_or_else(|| self.active.selected_range.clone());

        let start = range.start;
        let before = self.active.buffer.len();
        self.edit(range.clone(), new_text);
        if self.active.buffer.len() + range.len() != before + new_text.len() {
            return; // refused (read-only text): no composition to track
        }
        self.active.marked_range = (!new_text.is_empty()).then(|| start..start + new_text.len());
        // The IME's selection is in UTF-16 units *within `new_text`*, not the
        // buffer: convert against the composed text itself.
        self.active.selected_range = new_selected_range_utf16.as_ref().map_or_else(
            || {
                let cursor = start + new_text.len();
                cursor..cursor
            },
            |r| {
                start + utf16_to_byte_in(new_text, r.start)
                    ..start + utf16_to_byte_in(new_text, r.end)
            },
        );
        self.active.mark_active = false;
        self.ensure_cursor_visible();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.active.buffer.range_from_utf16(&range_utf16);
        let row = self.line_index_at(range.start)?;
        let line = self.lines.get(row)?;
        // Real painted height (see `offset_for_position`): `window.line_height()`
        // is the unstyled default here and would put the caret rect on the wrong
        // wrapped row (`position_for_index` lays rows out at `ix * line_height`).
        let lh = self.line_height;
        let pos = line
            .wrapped
            .position_for_index(line.to_display(range.start - line.range.start), lh)
            .unwrap_or_default();
        let x = element_bounds.left() + pos.x - self.h_scroll_offset + line.indent;
        let top = element_bounds.top() + line.top + pos.y;
        Some(Bounds::from_corners(point(x, top), point(x, top + lh)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_position(point);
        Some(self.active.buffer.offset_to_utf16(offset))
    }
}

/// Clamp a byte range to `[0, len]` and keep it well-ordered.
fn clamp_range(range: Range<usize>, len: usize) -> Range<usize> {
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);
    start..end
}

#[cfg(test)]
mod tests {
    use super::{
        Buffer, EditorCommand, MAX_PREFIX_REPEAT, apply_trailing_rule, file_completions,
        language_for_path, line_span, prefix_arg_value, scheme_symbol_token, symbol_at,
    };

    #[test]
    fn trailing_token_rule_composes_with_structural() {
        // Exercise the composition with sample token lists (both branches),
        // independent of any language's actual config; step 2.
        let (ia, aa, step) = ("->", "in with", 2);
        // A dangling `->` opens the body one step past the current line (col 2),
        // even though the tree only placed us at the match column (col 2 → still 2).
        assert_eq!(apply_trailing_rule(2, 2, step, "->", ia, aa), 4);
        // …but it never *reduces* a deeper structural indent.
        assert_eq!(apply_trailing_rule(6, 2, step, "->", ia, aa), 6);
        // `in` / `with` pin the next line to the current column, ignoring a
        // spurious structural over-indent.
        assert_eq!(apply_trailing_rule(4, 2, step, "in", ia, aa), 2);
        assert_eq!(apply_trailing_rule(0, 2, step, "with", ia, aa), 2);
        // A token in neither list leaves the structural result untouched, and a
        // near-miss (`main` ends in "in") must not match `in`.
        assert_eq!(apply_trailing_rule(4, 2, step, "=", ia, aa), 4);
        assert_eq!(apply_trailing_rule(4, 2, step, "main", ia, aa), 4);
        // No rules configured (another language): structural is returned as-is.
        assert_eq!(apply_trailing_rule(4, 2, step, "->", "", ""), 4);
    }

    #[test]
    fn universal_argument_value() {
        // C-u alone defaults to 4; stacking C-u multiplies by 4 each time.
        assert_eq!(prefix_arg_value(None, 0), 1); // no argument
        assert_eq!(prefix_arg_value(None, 1), 4); // C-u
        assert_eq!(prefix_arg_value(None, 2), 16); // C-u C-u
        assert_eq!(prefix_arg_value(None, 3), 64); // C-u C-u C-u
        // An explicit typed number wins over the C-u default (C-u 10 == 10).
        assert_eq!(prefix_arg_value(Some(10), 1), 10);
        assert_eq!(prefix_arg_value(Some(0), 0), 0);
        // A long C-u chain (key repeat) must not saturate into an unbounded
        // repeat loop — 4^32 overflows i64, and i64::MAX iterations is a hang.
        assert_eq!(prefix_arg_value(None, 40), MAX_PREFIX_REPEAT);
        assert_eq!(prefix_arg_value(None, 8), 4i64.pow(8)); // still exact below the cap
        // A typed number is clamped the same way.
        assert_eq!(prefix_arg_value(Some(i64::MAX), 0), MAX_PREFIX_REPEAT);
    }

    #[test]
    fn prefix_arg_repeats_motions_and_edits_not_meta() {
        use EditorCommand::*;
        assert!(NextLine.is_repeatable());
        assert!(ForwardChar.is_repeatable());
        assert!(KillLine.is_repeatable());
        assert!(Yank.is_repeatable());
        // Meta/modal commands must run once regardless of the argument.
        assert!(!ExecuteExtendedCommand.is_repeatable());
        assert!(!Quit.is_repeatable());
        assert!(!SaveBuffer.is_repeatable());
    }

    #[test]
    fn scheme_symbol_token_extracts_trailing_symbol() {
        // Bare symbol: whole string is the token.
        assert_eq!(scheme_symbol_token("set-o"), (0, "set-o"));
        // After an open paren: the paren is the delimiter, token starts after it.
        assert_eq!(scheme_symbol_token("(set-o"), (1, "set-o"));
        // Nested call with earlier args: only the trailing token completes.
        assert_eq!(scheme_symbol_token("(foo (bar ba"), (10, "ba"));
        // Just after a delimiter (paren or space) → empty token, nothing to complete.
        assert_eq!(scheme_symbol_token("(foo "), (5, ""));
        assert_eq!(scheme_symbol_token("("), (1, ""));
        // A string literal opens a fresh delimiter boundary.
        assert_eq!(scheme_symbol_token("(load-theme \"mac"), (13, "mac"));
    }

    #[test]
    fn parse_snippet_extracts_fields_in_tab_order() {
        use super::parse_snippet;
        let (text, fields) = parse_snippet("fn ${1:name}($2) {\n    $0\n}");
        assert_eq!(text, "fn name() {\n    \n}");
        // $1 at "name" offset (len 4), $2 right after "(", $0 last.
        assert_eq!(fields, vec![(1, 3, 4), (2, 8, 0), (0, 16, 0)]);
        // A bare "$" with no field is literal.
        let (t2, f2) = parse_snippet("cost is $5 today");
        // "$5" parses as field 5 (no text), leaving the rest literal.
        assert_eq!(t2, "cost is  today");
        assert_eq!(f2, vec![(5, 8, 0)]);
    }

    #[test]
    fn line_span_maps_line_ranges_to_bytes() {
        // "a\nbb\nccc\n" -> lines: "a"(0..1) "bb"(2..4) "ccc"(5..8) ""(9)
        let buf = Buffer::new("a\nbb\nccc\n");
        assert_eq!(line_span(&buf, 0, 1), (0, 2)); // line 0 incl. its '\n'
        assert_eq!(line_span(&buf, 1, 2), (2, 5)); // line 1 incl. its '\n'
        assert_eq!(line_span(&buf, 0, 3), (0, 9)); // lines 0..3 = up to last '\n'
        assert_eq!(line_span(&buf, 1, 1), (2, 2)); // empty range (insert point)
        // End past the last line clamps to end-of-buffer.
        assert_eq!(line_span(&buf, 2, 99), (5, 9));
    }

    use std::path::Path;

    #[test]
    fn transient_keys_prettified() {
        use super::pretty_key;
        assert_eq!(pretty_key("shift-p"), "P");
        assert_eq!(pretty_key("shift-x"), "X");
        assert_eq!(pretty_key("ctrl-x"), "C-x");
        assert_eq!(pretty_key("alt-a"), "M-a");
        assert_eq!(pretty_key("c"), "c");
        assert_eq!(pretty_key("tab"), "tab");
    }

    #[test]
    fn pretty_sequence_prettifies_each_chord() {
        use super::pretty_sequence;
        assert_eq!(pretty_sequence("ctrl-x ctrl-t"), "C-x C-t");
        assert_eq!(pretty_sequence("alt-g n"), "M-g n");
        assert_eq!(pretty_sequence("ctrl-c shift-p"), "C-c P");
        assert_eq!(pretty_sequence("ctrl-x"), "C-x");
    }

    #[test]
    fn language_by_extension() {
        assert_eq!(language_for_path(Path::new("main.scm")), Some("scheme"));
        assert_eq!(language_for_path(Path::new("a.rkt")), Some("racket"));
        assert_eq!(language_for_path(Path::new("paper.tex")), Some("latex"));
        assert_eq!(language_for_path(Path::new("doc.typ")), Some("typst"));
        assert_eq!(language_for_path(Path::new("flow.mmd")), Some("mermaid"));
        assert_eq!(language_for_path(Path::new("query.sql")), Some("sql"));
        assert_eq!(language_for_path(Path::new("app.ex")), Some("elixir"));
        assert_eq!(language_for_path(Path::new("mix.exs")), Some("elixir"));
        assert_eq!(
            language_for_path(Path::new("build.ps1")),
            Some("powershell")
        );
        assert_eq!(language_for_path(Path::new("notes.txt")), None);
        assert_eq!(language_for_path(Path::new("Makefile")), None);
    }

    #[test]
    fn file_completions_list_directory_entries() {
        // Run from the crate root; complete the bundled font directory.
        let matches = file_completions("assets/fonts/Lilex");
        assert!(
            matches.iter().any(|m| m.ends_with("Lilex-Regular.ttf")),
            "expected Lilex-Regular.ttf in {matches:?}"
        );
        // Every candidate extends the typed prefix.
        assert!(matches.iter().all(|m| m.starts_with("assets/fonts/Lilex")));
    }

    #[test]
    fn startup_directory_falls_back_to_home_for_root_cwd() {
        use super::startup_directory;
        use std::path::PathBuf;
        let home = || Some(PathBuf::from("/Users/me"));
        // Dock / Finder launch: cwd is `/`.
        assert_eq!(startup_directory(Some("/".into()), home()), "/Users/me");
        // Unreadable cwd.
        assert_eq!(startup_directory(None, home()), "/Users/me");
        // Terminal launch keeps the shell's cwd.
        assert_eq!(
            startup_directory(Some("/Users/me/proj".into()), home()),
            "/Users/me/proj"
        );
        // No home either: keep `/` rather than nothing.
        assert_eq!(startup_directory(Some("/".into()), None), "/");
    }

    #[test]
    fn symbol_at_reads_identifier_under_point() {
        let src = "(foo-bar! baz)";
        assert_eq!(symbol_at(src, 3).as_deref(), Some("foo-bar!")); // inside foo-bar!
        assert_eq!(symbol_at(src, 11).as_deref(), Some("baz"));
        assert_eq!(symbol_at(src, 0), None); // on '('
        // Point right after a symbol still picks it up (point at end of word).
        assert_eq!(symbol_at("(a b)", 2).as_deref(), Some("a"));
        // A space flanked by non-symbol chars yields nothing.
        assert_eq!(symbol_at("a  b", 2), None);
    }
}
