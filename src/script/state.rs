use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use ropey::Rope;

use super::EditorCommand;

/// The engine thread's outbox: the [`EditorCommand`]s Scheme queued during the
/// current job, handed to the editor (in order) when the job finishes.
#[derive(Clone, Default)]
pub(crate) struct Queue(Arc<Mutex<Vec<EditorCommand>>>);

impl Queue {
    /// Queue one command.
    pub(crate) fn push(&self, cmd: EditorCommand) {
        self.0.lock().unwrap().push(cmd);
    }

    /// Queue several commands under one lock (a batch primitive's output).
    pub(crate) fn extend(&self, cmds: impl IntoIterator<Item = EditorCommand>) {
        self.0.lock().unwrap().extend(cmds);
    }

    /// Take everything queued so far, oldest first.
    pub(crate) fn drain(&self) -> Vec<EditorCommand> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

pub(crate) type BufferNames = Arc<Mutex<Vec<String>>>;
/// Buffer name → absolute file path, for buffers visiting a file. Lets Scheme
/// resolve a non-active buffer's path (`buffer-file-name-of`) — the snapshot
/// bridges only expose the active buffer. Published alongside `BufferNames`.
pub(crate) type BufferPaths = Arc<Mutex<HashMap<String, String>>>;
/// A snapshot of the active buffer, published by the editor before it dispatches
/// a command so the read bridges (`buffer-string`, `point`, `region-text`, …)
/// see the buffer as of the moment the command was invoked — Emacs's command
/// semantics. The text is held as a [`Rope`] handle, so publishing is an O(1)
/// copy-on-write clone rather than a full-buffer copy, and still gives Scheme
/// the full read surface Emacs Lisp expects.
#[derive(Default, Clone)]
pub struct EditorSnapshot {
    /// Active buffer name.
    pub name: String,
    /// Point's line (0-based).
    pub line: usize,
    /// Buffer's directory — visited file's parent, else the process cwd
    /// (Emacs's buffer-local `default-directory`).
    pub directory: String,
    /// The visited file's full path, or "" for a buffer with no file (Emacs's
    /// `buffer-file-name`). Lets Scheme reveal the current file in dired etc.
    pub path: String,
    /// Point as a byte offset.
    pub point: usize,
    /// The full buffer text. A [`Rope`] handle, not an owned `String`: the editor
    /// publishes it with an O(1) copy-on-write clone (no per-command full-text
    /// copy), and the read bridges materialize only the slice they need, outside
    /// the snapshot lock.
    pub text: Rope,
    /// The published buffer's edit tick (see `Buffer::tick`): captured by
    /// `(guard-buffer-unchanged …)` so a decoration batch computed from this
    /// snapshot can be dropped if the buffer was edited before it applied.
    pub tick: u64,
    /// The active region's byte range, if the mark is active.
    pub region: Option<(usize, usize)>,
    /// Every selection as an `(anchor, head)` pair — primary first, then any
    /// secondary (multiple-cursor / Helix) selections. For `(selections)`, which
    /// scheme/helix.scm reads to move and edit each selection independently.
    pub selections: Vec<(usize, usize)>,
    /// Text properties `(start, end, key, value)`, for `get-text-property`.
    pub props: Vec<(usize, usize, String, String)>,
    /// Buffer-local variables `(key, value)`, for `buffer-local-get`.
    pub locals: Vec<(String, String)>,
    /// Active-buffer LSP diagnostics, one record each as
    /// `"line\tcol\tseverity\tmessage"` (line/col 0-based), for `(diagnostics)`.
    pub diagnostics: Vec<String>,
    /// Pane (window) ids in cycle order, for `(window-list)`.
    pub windows: Vec<usize>,
    /// Each pane's `(id, buffer-name)`, for `(get-buffer-window name)` — lets
    /// Scheme find which window (if any) is already showing a buffer.
    pub window_buffers: Vec<(usize, String)>,
    /// The selected pane (window) id, for `(selected-window)`.
    pub selected_window: usize,
    /// Whether the active buffer has a ready, opened LSP server, for
    /// `(lsp-server-ready?)` — lets Scheme guard LSP calls.
    pub lsp_ready: bool,
    /// Approximate first/last buffer line currently on screen, for `(visible-lines)`
    /// — avy scopes its jump hints to what the user can see.
    pub visible_lines: (usize, usize),
    /// The cursor's row within the viewport (display rows, folds collapsed), for
    /// `(window-cursor-row)` — magit captures it before a refresh and restores it
    /// after so the buffer doesn't visually jump.
    pub cursor_screen_row: usize,
    /// Whether evil (modal) editing is currently active — true in any evil state
    /// (Normal/Insert/Visual/…), false when disabled. For `(evil-enabled?)`, so
    /// Scheme can branch on the active keybinding style (e.g. open the vim or the
    /// emacs tutorial to match what the user is using).
    pub evil_enabled: bool,
    /// Whether Helix (selection-first) modal editing is active. Distinct from
    /// `evil_enabled`, which is also true under Helix (the Normal/Insert state
    /// machine is shared) — `(helix-enabled?)` lets Scheme branch on the style,
    /// e.g. open the Helix tutorial instead of the Vim one.
    pub helix_enabled: bool,
    /// Whether a keyboard macro is being recorded right now. For
    /// `(kbd-macro-recording?)`: evil's `q` toggle must read the editor's real
    /// state, not a Scheme-side mirror that desyncs the moment the user reaches
    /// for the Emacs chords (`C-x (` / `C-x )`) instead.
    pub kbd_macro_recording: bool,
    /// The Vim/Helix count typed before a count-aware command (`3` in `3dd`),
    /// or 0 when none was typed. For `(prefix-count)`; see `set-count-aware!`.
    pub prefix_count: usize,
    /// The most recent buffer edit as `(beg end old-len)` — byte offsets and
    /// the pre-change span length, Emacs's `after-change-functions` triple.
    /// `old-len == 0` means a pure insertion; `(0, 0, 0)` is the no-change
    /// startup state. For `(last-change)`.
    pub last_change: (usize, usize, usize),
    /// The modal state name ("normal", "insert", "visual", "visual-line",
    /// "visual-block"; "" when modal editing is off), for `(evil-state)`.
    pub evil_state: String,
    /// The active `/` search regex ("" when none) and its direction, for
    /// `(search-query)` / `(search-forward?)` — the `n`/`N` motions.
    pub search_query: String,
    pub search_forward: bool,
    /// The buffer's major mode (top of its mode stack), for `(major-mode)`.
    pub major_mode: String,
    /// The buffer's Vim marks `(name, byte)`, for `(mark-position "a")`.
    pub marks: Vec<(String, usize)>,
    /// The buffer's highlighting language ("rust", "python", …; "" when none),
    /// for `(buffer-language)`.
    pub language: String,
    /// The buffer's current syntax tree (a refcounted copy), for the
    /// synchronous `ts-*` reads behind tree-sitter text objects and motions.
    pub tree: Option<tree_sitter::Tree>,
    /// The active grid buffer's state, for the `grid-*` reads; `None` when
    /// the active buffer isn't a grid.
    pub grid: Option<GridSnapshot>,
}

/// What Scheme can read about the active grid without a round trip: small,
/// bounded values only (cell *data* in bulk goes through `grid-read-range`).
#[derive(Default, Clone)]
pub struct GridSnapshot {
    pub cursor: (usize, usize),
    /// Selected rectangle `(r0, c0, r1, c1)`, inclusive; the cursor cell
    /// alone when nothing is selected.
    pub selection: (usize, usize, usize, usize),
    pub rows: usize,
    pub cols: usize,
    /// The cursor cell's value (the text being typed while editing).
    pub value: String,
    pub header: bool,
    /// Row 1's values when it is a header, else empty.
    pub header_names: Vec<String>,
    /// Inferred type per used column: "int", "float", "bool", "date", "text".
    pub col_types: Vec<String>,
    pub frozen_rows: usize,
    pub frozen_cols: usize,
    /// Zoom in percent.
    pub zoom: f64,
    pub editing: bool,
    /// The cursor cell's formula (`=SUM(A1:A3)`), if it holds one.
    pub formula: Option<String>,
    /// Every cell, for the synchronous `grid-cell` / `grid-column-values`
    /// reads; `None` when the grid is too large to copy per change (those
    /// reads then report an error and `grid-read-range` remains).
    pub cells: Option<Arc<crate::grid::model::CellsSnapshot>>,
    /// The active row filter, for `(grid-row-visible? r)`.
    pub filter: Option<Arc<crate::grid::view::Only>>,
    /// Rows shown (all rows when unfiltered).
    pub visible_rows: usize,
    /// An `.xlsx`'s sheet names (empty for CSV), and the one shown.
    pub sheets: Vec<String>,
    pub sheet: Option<String>,
}

/// The published editor snapshot. An [`ArcSwap`] rather than a `Mutex`: the
/// editor publishes a new snapshot with a single atomic store and the Scheme
/// read bridges `load()` it without ever taking a lock, so a read bridge and a
/// publish never block each other (and neither blocks the UI thread).
pub(crate) type SharedSnapshot = Arc<ArcSwap<EditorSnapshot>>;

/// Markers: byte positions that move as the buffer is edited (Emacs markers).
/// Shared so `make-marker` can allocate an id and return it synchronously from
/// the engine thread, while the editor adjusts positions on every edit.
#[derive(Default)]
pub(crate) struct MarkerStore {
    pub(crate) next: u64,
    pub(crate) positions: HashMap<u64, usize>,
}

impl MarkerStore {
    /// Allocate a fresh id for a marker at `pos` — the single id source shared
    /// by the Rust-side [`ScriptEngine::add_marker`] and Scheme's
    /// `(make-marker pos)`, so the two sides can never hand out colliding ids
    /// (ids start at 1).
    pub(crate) fn alloc(&mut self, pos: usize) -> u64 {
        self.next += 1;
        let id = self.next;
        self.positions.insert(id, pos);
        id
    }
}
pub(crate) type Markers = Arc<Mutex<MarkerStore>>;

/// The applied editor options, published by the editor after every option change
/// so Scheme can read them back via `(get-option "name")`.
pub(crate) type Options = Arc<Mutex<HashMap<String, crate::settings::OptionValue>>>;

/// A monotonic id source shared among the `make-process` / `make-terminal`
/// bridges on the engine thread. Each lives in its own editor-side map keyed by
/// id, so they don't need to share the marker counter (which is for markers
/// only) — a dedicated atomic keeps the concern separate.
#[derive(Clone, Default)]
pub(crate) struct IdGen(Arc<std::sync::atomic::AtomicU64>);
impl IdGen {
    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }
}

/// Editor state that session persistence (scheme/session.scm's savehist +
/// desktop) reads: the history rings and the window/buffer layout. Unlike the
/// per-command [`EditorSnapshot`] it is published ON DEMAND — by
/// `(session-refresh callback)` just before the callback runs, and right before
/// `kill-emacs-hook` — so the hot command path never clones the kill ring.
#[derive(Default, Clone)]
pub struct SessionState {
    /// Shared minibuffer input history, oldest first.
    pub minibuffer_history: Vec<String>,
    /// Kill ring, oldest first.
    pub kill_ring: Vec<String>,
    /// Completed search strings (isearch + evil `/` `?`), oldest first.
    pub search_ring: Vec<String>,
    /// Commands run via `M-x` (minibuffer or Helm), oldest first.
    pub command_history: Vec<String>,
    /// File-visiting buffers as `(path, point)`, in buffer-ring order with the
    /// active buffer last.
    pub file_buffers: Vec<(String, usize)>,
    /// The window split tree, or `None` before the first publish.
    pub layout: Option<super::LayoutSpec>,
}

/// The published [`SessionState`], shared with the engine thread.
pub(crate) type SharedSession = Arc<Mutex<SessionState>>;

/// Every handle the engine thread's primitives share with the editor (and
/// with each other), bundled so they travel as one value: from
/// [`ScriptEngine::new`] into the engine thread and through registration.
#[derive(Clone, Default)]
pub(crate) struct EngineShared {
    pub(crate) queue: Queue,
    pub(crate) buffers: BufferNames,
    pub(crate) buffer_paths: BufferPaths,
    pub(crate) snapshot: SharedSnapshot,
    pub(crate) markers: Markers,
    pub(crate) options: Options,
    pub(crate) session: SharedSession,
    /// The id source shared by `make-process`, `make-terminal`, `dap-*`,
    /// `make-http-stream` and the async sockets (see [`IdGen`]). Each lives in
    /// its own editor-side map keyed by id, so it needn't share the marker
    /// counter.
    pub(crate) ids: IdGen,
}
