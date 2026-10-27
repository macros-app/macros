/// A command requested by a script, applied to the editor by Rust.
///
/// Steel runs without direct access to GPUI's `Context`, so scripts never mutate
/// the editor in place. Instead, registered functions enqueue `EditorCommand`s
/// that the editor drains and applies after evaluation — the same decoupling
/// Emacs gets from its command loop. Most commands are nullary; a few carry a
/// string payload.
/// Payload for [`EditorCommand::InstallGrammar`], boxed in the variant to keep
/// `EditorCommand` small. Fields carry the raw Steel-side strings; `symbol` and
/// `subdir` are `""` for "use the default" (symbol = id, grammar at repo root),
/// and `extensions` is whitespace-separated.
#[derive(Debug, Clone)]
pub struct GrammarInstall {
    pub id: String,
    pub repo: String,
    pub rev: String,
    pub extensions: String,
    pub symbol: String,
    pub subdir: String,
}

/// Payload of [`EditorCommand::DapStart`]: spawn a debug adapter under `id`.
/// Every incoming message (response, event, or reverse request) is delivered
/// to Steel fn `on_message` as raw JSON text; `on_exit` (if present) gets the
/// exit code as a string. The transport is protocol-free — Scheme stamps
/// `seq`, correlates responses, and owns the session state machine (dap.scm).
#[derive(Debug, Clone)]
pub struct DapStartArgs {
    pub id: u64,
    pub program: String,
    pub args: Vec<String>,
    /// Adapter env (any `with-process-env` extent open at `dap-start`).
    pub env: EnvOverrides,
    /// Working directory; "" = inherit the editor's.
    pub cwd: String,
    pub on_message: String,
    pub on_exit: Option<String>,
}

/// Payload for [`EditorCommand::HttpStream`]. Boxed in the enum because it's the
/// largest variant by far (a `Vec` plus several `String`s); see the variant doc.
#[derive(Debug, Clone)]
pub struct HttpStreamArgs {
    /// Stream id (for `http-stream-cancel`).
    pub id: u64,
    /// Request URL.
    pub url: String,
    /// Request headers as `(key, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// JSON request body.
    pub body: String,
    /// Steel fn called with each response line as it arrives.
    pub on_line: String,
    /// Steel fn called with the stream id once the body closes.
    pub on_done: String,
}

/// Payload for [`EditorCommand::HttpRequest`]. A one-shot request of any method
/// (the streaming [`HttpStreamArgs`] is POST-only and has no status code); the
/// whole response is delivered to one Steel callback. Boxed for the same reason
/// as `HttpStreamArgs`.
#[derive(Debug, Clone)]
pub struct HttpRequestArgs {
    /// HTTP method verb: `GET`, `POST`, `PATCH`, `DELETE`, …
    pub method: String,
    /// Request URL.
    pub url: String,
    /// Request headers as `(key, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Request body; empty means no body (e.g. a bare `GET`).
    pub body: String,
    /// Steel fn called once with the response. When `full` is false:
    /// `"<status>\n<body>"` — status on the first line, text body after. When
    /// `full` is true: `"<status>\n<Header: value>\n…\n\n<hex-body>"` — status,
    /// then each response header on its own line, a blank line, then the body
    /// hex-encoded (so binary downloads survive). Status `0` marks a transport
    /// failure, with the error text as the body in both modes.
    pub on_response: String,
    /// Deliver the rich header+hex-body payload (`http-request-full`) rather
    /// than the plain status+text payload (`http-request`).
    pub full: bool,
}

/// Payload for [`EditorCommand::HttpServe`]. A minimal local HTTP server: it
/// accepts connections, writes back one fixed response, and hands each request's
/// target (`/path?query`) to a Steel callback. The transport for an OAuth
/// loopback redirect, but not OAuth-specific. Boxed to keep the enum small.
#[derive(Debug, Clone)]
pub struct HttpServeArgs {
    /// The already-bound listener, moved to the worker thread on first drain.
    /// Bound in the registry so the caller learns the (possibly OS-assigned)
    /// port synchronously; wrapped so the `Debug`/`Clone` enum can carry it.
    pub listener: std::sync::Arc<std::sync::Mutex<Option<std::net::TcpListener>>>,
    /// The bound port — the handle for `http-server-stop`.
    pub port: u16,
    /// `Content-Type` header written on every response.
    pub content_type: String,
    /// Response body written on every request (e.g. "you may close this tab").
    pub body: String,
    /// Steel fn called with each request target string as it arrives.
    pub on_request: String,
}

pub use crate::process::EnvOverrides;

/// Payload for [`EditorCommand::MakeTerminal`] (boxed: keeps the enum small).
#[derive(Debug, Clone)]
pub struct MakeTerminalArgs {
    pub id: u64,
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// Working directory to launch the shell in; empty = the active buffer's
    /// directory (or the process cwd for a fileless buffer).
    pub directory: String,
    /// Shell env (any `with-process-env` extent open at `make-terminal`).
    pub env: EnvOverrides,
}

/// Payload for [`EditorCommand::MakeProcess`] (boxed: keeps the enum small).
#[derive(Debug, Clone)]
pub struct MakeProcessArgs {
    pub id: u64,
    pub program: String,
    pub args: Vec<String>,
    /// Child-only env (`make-process-env` + any `with-process-env` extent).
    pub env: EnvOverrides,
    pub on_output: String,
    pub on_exit: Option<String>,
}

/// Payload for [`EditorCommand::SetTitlebarMenu`] (boxed: keeps the enum small).
#[derive(Debug, Clone)]
pub struct TitlebarMenuArgs {
    pub title: String,
    pub items: Vec<String>,
    pub commands: Vec<String>,
    pub active: String,
    pub section: String,
    pub callback: String,
}

#[derive(Debug, Clone)]
pub enum EditorCommand {
    /// A grid-buffer command (scheme/grid.scm), with the count or `C-u`
    /// flag its key carried; see [`crate::grid::cmd`].
    Grid(crate::grid::cmd::GridInvocation),
    /// `(grid-extensions '("csv" …))`: extensions that open as a grid.
    GridExtensions(Vec<String>),
    /// `(grid-goto-address "C7")`: move the grid cursor (or select a range).
    GridGotoAddress(String),
    /// A grid operation with arguments from the Scheme API.
    GridOp(crate::grid::cmd::GridOp),
    /// An editor-wide grid setting (`grid-default-column-width`, …).
    GridOption(crate::grid::cmd::GridOption),
    /// `define-grid-function` / `define-grid-helper`: share a definition with
    /// the grid-function interpreter; a function also becomes callable from
    /// formulas under its upper-cased name.
    GridRegisterFunction {
        name: String,
        source: String,
        helper: bool,
    },
    // Motion
    ForwardChar,
    BackwardChar,
    NextLine,
    PreviousLine,
    ForwardWord,
    ForwardWordStart,
    BackwardWord,
    /// Vim `b` — the start of the previous word, class-aware (the exact mirror
    /// of [`Self::ForwardWordStart`]). `BackwardWord` is Emacs `M-b`, which
    /// treats punctuation as a separator and so is NOT vim's `b`.
    BackwardWordStart,
    /// Vim `e` — ON the last character of the next word, class-aware.
    ForwardWordEnd,
    /// Vim `W` / `B` / `E` — whitespace-delimited WORD motions.
    ForwardBigwordStart,
    BackwardBigwordStart,
    ForwardBigwordEnd,
    /// Shift the region (or the current line) one indent step right / left —
    /// the primitive behind vim `>>` / `<<` and visual `>` / `<`.
    IndentRegion,
    OutdentRegion,
    /// Emacs `indent-region` (`C-M-\`): reindent the region's lines (the whole
    /// buffer with no region) from the language's structure.
    ReindentRegion,
    /// Pretty-print the buffer / the region as indented JSON.
    JsonPrettyPrintBuffer,
    JsonPrettyPrintRegion,
    BeginningOfLine,
    EndOfLine,
    BeginningOfBuffer,
    EndOfBuffer,
    // Region / mark
    SetMark,
    ExchangePointAndMark,
    MarkWholeBuffer,
    ExtendBackwardChar,
    ExtendForwardChar,
    // Insertion
    Insert(String),
    Newline,
    OpenLine,
    IndentForTab,
    // Deletion / killing
    DeleteChar,
    DeleteBackwardChar,
    TransposeChars,
    KillLine,
    KillWord,
    BackwardKillWord,
    KillRegion,
    KillRingSave,
    Yank,
    YankPop,
    /// System-clipboard copy / cut / paste (macOS cmd-c/x/v), kept in sync with
    /// the kill ring.
    ClipboardCopy,
    ClipboardCut,
    ClipboardPaste,
    /// Put an arbitrary string on the system clipboard (Emacs `kill-new`).
    ClipboardSet(String),
    // Undo
    Undo,
    /// Redo the most recently created branch off the current undo-tree node.
    Redo,
    /// Dump the undo tree as text to Steel `callback` (one node per line) — the
    /// vundo visualizer reads this to draw the branch structure.
    UndoTreeDump(String),
    // View
    ScrollUp,
    ScrollDown,
    Recenter,
    // Misc
    KeyboardQuit,
    ExecuteExtendedCommand,
    /// Show the echo-area scrollback (`*Messages*`).
    ListMessages,
    /// Show captured language-server stderr (`*lsp-log*`).
    ListLspLog,
    /// Force an update check against macros.dev and install a newer release if
    /// the user confirms (Emacs-style, echo-area driven). See `src/update.rs`.
    CheckForUpdates,
    // Steel interaction (steel-mode)
    EvalBuffer,
    EvalLastSexp,
    /// `(lisp-indent-rule name spec)`: how forms headed by `name` indent
    /// (`"body"`, `"function"`, `"clauses"`, or a distinguished-arg count).
    SetLispIndent {
        name: String,
        spec: String,
    },
    /// A structural (s-expression) motion or paredit edit.
    Structural(crate::editor::StructuralOp),
    /// Evaluate the top-level form at point in the live editor engine (Emacs
    /// `eval-defun`, C-M-x in config code).
    EvalDefun,
    /// Read a Steel expression in the minibuffer and evaluate it (Emacs `M-:`).
    EvalExpression,
    // Buffers / files
    FindFile,
    /// Open a file by path (a Helm action / programmatic open).
    OpenFile(String),
    SwitchBuffer,
    /// Switch to an open buffer by name (a Helm action / programmatic switch).
    SwitchToBufferNamed(String),
    /// Kill a buffer by name (active or inactive).
    KillBufferNamed(String),
    /// A file or directory moved on disk from `.0` to `.1` (a dired rename):
    /// buffers visiting it, or a file under it, now visit the new path.
    RetargetFileBuffers(String, String),
    /// Set the active (editable) buffer's major mode by name, so its keymap
    /// applies (e.g. shell-mode rebinding RET). Does not make the buffer special.
    SetMajorMode(String),
    /// Switch the active buffer into the editor-aware `macros-steel` major mode:
    /// stack it on `fundamental`, attach the `macros-scheme` language (Scheme
    /// highlighting plus the built-in editor-API server), and re-run mode hooks
    /// so its LSP comes up. The interactive escape hatch for editing this
    /// editor's own Scheme outside the auto-detected config paths — unlike
    /// `set-major-mode`, it re-attaches the language, not just the keymap.
    EnterMacrosSteel,
    NextBuffer,
    PreviousBuffer,
    SaveBuffer,
    /// Save every modified file-visiting buffer (Emacs `save-some-buffers`).
    SaveSomeBuffers,
    RevertBuffer,
    ToggleLineNumbers,
    ToggleScrollBar,
    ToggleWordWrap,
    /// Toggle the git-gutter / diff-hl change bars (`magit-gutter` option) and
    /// recompute the active buffer's marks. Bound to `magit-gutter-mode`.
    ToggleGitGutter,
    /// Recompute the active buffer's working-tree-vs-HEAD gutter marks
    /// (`magit-gutter-refresh`); Scheme wires this to save / find-file hooks.
    GitGutterRefresh,
    /// Toggle electric-pair-mode: auto-insert the closing delimiter on a typed
    /// opener (and wrap an active region). Off by default.
    ToggleElectricPair,
    SelectWindow(usize),
    OpenFileOtherWindow(String),
    /// Emacs `write-file`: save the active buffer as this path and visit it.
    WriteFile(String),
    /// Emacs `rename-buffer`: rename the active buffer.
    RenameBuffer(String),
    /// Emacs `insert-file`: insert a file's contents at point.
    InsertFile(String),
    /// `C-x 4 f`: visit a file in another window.
    FindFileOtherWindow(String),
    /// `C-x 4 b`: switch to a buffer in another window.
    SwitchToBufferOtherWindow(String),
    /// `C-x 4 C-o`: show a buffer in another window, keeping the selection.
    DisplayBuffer(String),
    /// `C-x C-q`: toggle the active buffer read-only.
    ToggleReadOnly,
    SetLineIcon(usize, usize, String),
    /// `(add-line-icon line col name face)`: like `SetLineIcon` but appends
    /// (several icons per row) and tints by theme face `face` ("" = the
    /// line's face).
    AddLineIcon(usize, usize, String, String),
    ClearLineIcons,
    /// Put a colored dot (the diagnostic-dot shape) in the gutter of 0-based
    /// `line` of the active buffer, tinted by theme face `String` — breakpoint
    /// signs (dap.scm). Mirrors `SetLineIcon`'s per-buffer storage.
    SetGutterDot(usize, String),
    ClearGutterDots,
    /// Display a raster image (decoded from `path`) on rows reserved below buffer
    /// `line` — the Org inline-image primitive.
    SetLineImage {
        line: usize,
        path: String,
        max_width: usize,
    },
    ClearLineImages,
    /// Abort the rest of this command batch unless the decoration target buffer
    /// (the render-redirect target, else the active buffer) is still named
    /// `name` with edit tick `tick` — both captured from the snapshot when the
    /// Scheme call ran. A debounced repaint computed while the user kept typing
    /// would otherwise land byte-offset decorations at stale positions (org /
    /// markdown headings below the edit visibly blink until the next repaint).
    /// On abort, `on_stale` (a global fn name; "" for none) is dispatched so the
    /// caller can reschedule its repaint.
    GuardBufferUnchanged {
        name: String,
        tick: u64,
        on_stale: String,
    },
    /// Virtual left indentation (in columns) for buffer `line` — display-only, the
    /// Org `org-indent-mode` primitive. The buffer text is untouched.
    SetLineIndent {
        line: usize,
        cols: usize,
    },
    ClearLineIndents,
    /// Whole-line font-size multiplier for buffer `line` (1.0 = unscaled) —
    /// markdown/org headings render larger. Display-only.
    SetLineScale {
        line: usize,
        scale: f32,
    },
    ClearLineScales,
    /// Copilot-style inline suggestion at point (`ghost-text-show`): dim virtual
    /// text rendered after the cursor, occupying no buffer bytes. Multi-line
    /// text reserves display rows below point's line. Cleared by any edit;
    /// hidden whenever point moves off the byte it was shown at.
    GhostShow {
        text: String,
    },
    GhostClear,
    /// Insert the visible ghost suggestion at point (one undoable edit) — the
    /// `ghost` keymap binds TAB to this.
    GhostAccept,
    KillBuffer,
    /// Show the MCP server's status, connection command, and log.
    McpServerInfo,
    /// Kill the active buffer and close its window (Emacs `kill-buffer-and-window`).
    KillBufferAndWindow,
    Quit,
    // LSP navigation / editing
    LspDefinition,
    LspDeclaration,
    LspTypeDefinition,
    LspImplementation,
    LspReferences,
    LspHover,
    LspSignatureHelp,
    LspDocumentSymbols,
    LspCodeAction,
    LspFormatBuffer,
    /// Kill the active buffer's language server (if any) and spawn a fresh one.
    LspRestart,
    LspCallHierarchyIncoming,
    LspCallHierarchyOutgoing,
    /// Validate + prompt rename (`prepareRename` then `rename`).
    LspPrepareRename,
    /// Request + render code lenses (resolving those missing a command).
    LspCodeLens,
    /// RET in an `*lsp-xref*` / `*lsp-symbols*` list: visit the row's target.
    LspXrefVisit,
    /// RET in `*lsp-actions*`: apply the row's code action.
    LspApplyCodeAction,
    /// Rename the symbol at point to the given name (read via `read-string`).
    LspRename(String),
    /// `workspace/symbol` for the given query (read via `read-string`).
    LspWorkspaceSymbol(String),
    /// Generic LSP bridge: send `method` with params built from `kind`
    /// (parsed from "position"|"document"|"range"|"region"|"positions"|"raw")
    /// merged with `params` (a JSON object string), delivering the result JSON
    /// to Steel fn `callback`. This exposes the whole protocol to Scheme.
    LspRequest {
        method: String,
        kind: crate::lsp::ParamsKind,
        params: String,
        callback: String,
    },
    /// Jump to `uri` at `(line, character)` (a Scheme handler visiting a result).
    LspGoto {
        uri: String,
        line: usize,
        character: usize,
    },
    /// Apply a `WorkspaceEdit` (JSON) across files (rename / code action).
    LspApplyWorkspaceEdit(String),
    /// Apply `TextEdit[]` (JSON) to the active buffer (formatting).
    LspApplyTextEdits(String),
    /// Push the current location onto the jump-back ring.
    XrefPushMarker,
    /// Pop the jump-back ring and return there (Emacs `pop-tag-mark`).
    XrefPopMarker,
    /// Enable/disable LSP format-on-save for a language.
    SetLspFormatOnSave(String, bool),
    /// Request `textDocument/semanticTokens/full`, decode it, and deliver the
    /// tokens (one `"line\tcol\tlength\ttype"` record per line) to Steel `callback`.
    LspSemanticTokens(String),
    /// Like `LspSemanticTokens` but only the viewport window (`/range`).
    LspSemanticTokensRange(String),
    /// Like `LspSemanticTokens` but via `/full/delta` when possible (live).
    LspSemanticTokensDelta(String),
    // Tree-sitter (the live parse tree, queried on the UI thread).
    /// Run tree-sitter query `query` over the active buffer's parse tree and
    /// deliver the captures to Steel fn `callback` as one
    /// `"capture\tstart\tend\tkind"` record per line (byte offsets). Empty when
    /// there's no grammar/tree or the query won't compile. Gives Scheme the whole
    /// parse tree — find brackets, strings, comments, functions, etc.
    TreeSitterQuery {
        query: String,
        callback: String,
    },
    /// Deliver the node chain covering `byte` (innermost first) to Steel fn
    /// `callback`, one `"kind\tstart\tend"` record per line — "what am I inside?".
    TreeSitterNodeAt {
        byte: usize,
        callback: String,
    },
    /// Download, compile, and load a user tree-sitter grammar (see
    /// [`crate::grammar`]). Runs on the background executor; on success the active
    /// buffer re-highlights if it visits a matching file. Boxed so its six fields
    /// don't make this the largest `EditorCommand` variant (which would bloat the
    /// `Result<(), EditorCommand>` every dispatcher threads — `result_large_err`).
    InstallGrammar(Box<GrammarInstall>),
    // LSP / completion popup
    CompletionAtPoint,
    CompletionNext,
    CompletionPrevious,
    CompletionAccept,
    CompletionAbort,
    // Helm (selection framework)
    /// Open a Helm session over Scheme-provided sources.
    Helm(Vec<HelmSourceSpec>),
    /// Like `Helm`, but with an initial input pre-filled (e.g. a directory for
    /// the file navigator, so top-level fuzzy + TAB-descend work).
    HelmFrom(String, Vec<HelmSourceSpec>),
    /// Built-in: Helm over all command names, running the chosen command.
    HelmMx,
    HelmNext,
    HelmPrevious,
    HelmAccept,
    HelmAbort,
    /// Open the action menu for the selected candidate (multi-action sources).
    HelmActions,
    /// Toggle the selected candidate's multi-select mark (`C-SPC`).
    HelmToggleMark,
    // Helix (selection-first modal editing, scheme/helix.scm)
    /// Enter Helix modal editing: turn on the shared Normal/Insert state machine
    /// and route plain keys against the `helix-*` keymaps.
    HelixMode,
    /// Leave Helix modal editing (back to plain Emacs keys).
    HelixDisable,
    /// Return to Helix Normal from Insert, *keeping* any secondary cursors (so
    /// `c`/`i` across multiple selections survive Escape, unlike evil's Normal).
    HelixNormalState,
    /// Replace the whole selection set with these `(anchor, head)` pairs (first =
    /// primary). The workhorse Helix selection-first motions drive through this.
    SetSelections(Vec<(usize, usize)>),
    // Evil (modal editing)
    EvilMode,
    EvilNormalState,
    EvilInsertState,
    EvilVisualState,
    /// Vim line-wise Visual (`V`): the selection always covers whole lines.
    EvilVisualLineState,
    /// Block insert/append (`I`/`A` in Visual line): drop a cursor at the start
    /// (or end) of every selected line and switch to Insert, so one edit
    /// replicates to all of them.
    EvilVisualInsert,
    EvilVisualAppend,
    /// Vim block-wise Visual (`C-v`): a rectangular column selection between two
    /// corners; motions move one corner. `I`/`A` insert/append down the block's
    /// left/right column; `d`/`x` delete and `y` yanks the rectangle.
    EvilVisualBlockState,
    EvilBlockInsert,
    EvilBlockAppend,
    EvilBlockDelete,
    /// Block-wise change (`c` in Visual Block): delete the rectangle, then drop a
    /// cursor at its left column on every row and enter Insert.
    EvilBlockChange,
    EvilBlockYank,
    EvilDisable,
    /// Scroll half a page down / up (evil `C-d` / `C-u`).
    EvilScrollDown,
    EvilScrollUp,
    EvilSearchForward,
    EvilSearchBackward,
    EvilSearchNext,
    EvilSearchPrevious,
    EvilExCommand,
    /// Vim paste after (`p`) / before (`P`) the cursor, honoring the linewise
    /// flag set by the most recent yank/delete.
    EvilPasteAfter,
    EvilPasteBefore,
    /// Vim `r`: arm replace-pending so the next printable key overwrites the
    /// single character under the cursor (cursor stays put, mode stays Normal).
    EvilReplaceChar,
    /// Arm a one-keystroke capture: the next printable key is passed to the
    /// Scheme `callback` as its sole argument; any other chord (Escape, a
    /// Ctrl/Alt combo) cancels. Backs vim's `f`/`t`/`F`/`T` — and any Scheme
    /// feature that needs "read one character" — the way `EvilReplaceChar`'s
    /// hardcoded pending flag backs `r`.
    ReadChar {
        prompt: String,
        callback: String,
    },
    /// Arm a STICKY key capture (`read-key-start`): every keystroke — printable
    /// keys as their character, everything else (RET, DEL, Escape, chords) as
    /// its chord name (`"enter"`, `"backspace"`, `"ctrl-g"`, …) — is passed to
    /// the Scheme `callback` until `ReadKeyStop`. Unlike `ReadChar` it stays
    /// armed across keys, so a multi-key Scheme dialog (query-replace's
    /// y/n/!/q loop) can't lose a fast keystroke to the normal keymap between
    /// callbacks. Escape / C-g are delivered and ALSO disarm, so a Scheme error
    /// can never strand the capture. Re-arming just updates the prompt.
    ReadKeyStart {
        prompt: String,
        callback: String,
    },
    /// Disarm the `ReadKeyStart` capture.
    ReadKeyStop,
    /// Extend the selection to byte `pos` — the extend-flavored `goto-char`,
    /// for visual-mode motion targets computed in Scheme.
    SelectTo(usize),
    /// Store a search query + direction without prompting (evil `*`/`#`), so
    /// `evil-search-next`/`-previous` repeat it like a `/` search.
    SetSearchQuery {
        query: String,
        forward: bool,
    },
    /// Mark the most recent yank/delete as line-wise / character-wise, so the
    /// next `p`/`P` knows whether to open a new line or insert inline.
    EvilSetLinewise,
    EvilSetCharwise,
    /// Register a command name as count-aware (see `set-count-aware!`).
    SetCountAware(String),
    /// Appended by the editor to each run of a counted Scheme command: marks
    /// the batch as one repetition and triggers the next (with a fresh
    /// snapshot), so `3w`/`3dd` see the previous run's result.
    CountRepeatStep,
    /// Sentinel queued behind each async command of a replay (macro, `.`):
    /// the previous command's batch has applied, so the next may run.
    ReplayStep,
    /// Vim `.` — repeat the last change (count-aware: `3.`).
    EvilRepeat,
    /// `"x` — the next yank/delete/paste uses register x.
    EvilUseRegister(String),
    /// Insert-state `C-r x` — insert register x's text.
    EvilInsertRegister(String),
    /// `qx` — start recording a keyboard macro into register x.
    EvilMacroRecord(String),
    /// `@x` — replay the macro in register x (`@@` the last one, `@:` the
    /// last ex command); count-aware.
    EvilMacroPlay(String),
    /// `mx` — set mark x at point.
    EvilSetMark(String),
    /// `` `x `` (exact = true) / `'x` (line) — jump to mark x.
    EvilGotoMark(String, bool),
    /// Mark a command name as a jump (`set-jump-command!`): running it from a
    /// key records the jump list and the `''` mark first.
    SetJumpCommand(String),
    /// `gv` — reselect the last Visual area.
    EvilReselectVisual,
    /// `r x` resolved: overwrite `(prefix-count)` chars with x (what a macro or
    /// `.` replays for `r`).
    EvilReplaceWith(String),
    /// Vim `=`: reindent (0-based) lines first..=last.
    ReindentLines(usize, usize),
    /// Run an ex command line (`(evil-ex "%s/a/b/g")`; `&` / `g&` use it).
    EvilEx(String),
    /// Move to the start of (1-based) line N — a `:normal` replay step.
    EvilExGotoLine(String),
    /// C-o / C-i — older / newer jump-list position.
    EvilJumpBackward,
    EvilJumpForward,
    /// windmove / `C-w h/j/k/l`: select the window in a direction.
    WindmoveLeft,
    WindmoveDown,
    WindmoveUp,
    WindmoveRight,
    /// `C-w W` / `C-w p` / `C-w t` / `C-w b`.
    PreviousWindow,
    LastWindow,
    TopLeftWindow,
    BottomRightWindow,
    /// `C-w =` (Emacs `C-x +`).
    BalanceWindows,
    /// `C-w _` / `C-w |`.
    MaximizeWindowHeight,
    MaximizeWindowWidth,
    /// `C-w H/J/K/L`: move the window to the far side.
    MoveWindowFarLeft,
    MoveWindowFarDown,
    MoveWindowFarUp,
    MoveWindowFarRight,
    /// `C-w x` / `C-w r` / `C-w R`.
    ExchangeWindow,
    RotateWindowsDown,
    RotateWindowsUp,
    /// `C-w n` / `:new` (stacked) and `:vnew` (side by side).
    NewWindowBelow,
    NewWindowRight,
    /// Visual-block `r<char>`: overwrite every char of the rectangle.
    EvilBlockReplace(String),
    /// Helix `p` / `P`: paste after / before every selection.
    HelixPasteAfter,
    HelixPasteBefore,
    /// Emacs incremental search (`C-s` / `C-r`).
    IsearchForward,
    IsearchBackward,
    // Modes / hooks
    /// Start the language server for the current buffer (mode-hook friendly).
    EnableLsp,
    /// Run a named function when a mode is enabled: `(add-hook "steel" "enable-lsp")`.
    AddHook(String, String),
    /// Remove a previously added mode hook.
    RemoveHook(String, String),
    /// Run every function registered on a named hook from Scheme:
    /// `(run-hook "bookmark-save-hook")` — for hooks whose trigger lives in
    /// Scheme (the editor runs its own lifecycle hooks itself).
    RunHook(String),
    /// Register a Scheme completion-at-point function (tried by `M-x
    /// completion-at-point` before LSP): `(add-completion-at-point-function "f")`.
    AddCapf(String),
    /// Replace the set of directories watched on behalf of `owner` with `dirs`
    /// (`(watch-directories "dired-sidebar" (list ...))`). Owner-namespaced with
    /// replace semantics so a view re-declares its full watch set each render and
    /// never has to unwatch individual paths; pass `'()` to drop them all (e.g. on
    /// quit). A change under any watched dir fires `directory-changed-hook`.
    WatchDirectories(String, Vec<String>),
    /// Like [`WatchDirectories`](Self::WatchDirectories) but RECURSIVE: a change
    /// anywhere in the subtree under a watched root fires `directory-changed-hook`
    /// with the changed root. For magit watching a whole repo. Replace semantics.
    WatchTree(String, Vec<String>),
    /// Configure a language server: `(set-lsp "rust" "rust-analyzer")`. The
    /// env overrides in effect at the call (`with-process-env`) are captured
    /// and applied whenever the server is spawned, like `dap-start`.
    SetLsp(String, String, EnvOverrides),
    /// Map a file extension to a server key / config language:
    /// `(set-extension "rs" "rust")`.
    SetExtension(String, String),
    /// Map a file extension to the wire `languageId` sent in `didOpen`:
    /// `(set-language-id "tsx" "typescriptreact")`. Lets several extensions
    /// share one server yet announce distinct ids.
    SetLanguageId(String, String),
    /// Set a tunable option: `(set-option "tab-width" 4)`. The value may be a
    /// number, boolean, or string; it is coerced to the option's declared type
    /// when applied (see [`crate::settings::Settings::set`]).
    SetOption(String, crate::settings::OptionValue),
    Message(String),
    /// Bind a key sequence to a command in a named mode's keymap:
    /// `(define-key "steel" "ctrl-c ctrl-e" "eval-last-sexp")`.
    DefineKey(String, String, String),
    // Theming
    /// Set a face's color: `(set-face "comment" "#6a9955")`.
    SetFace(String, String),
    /// Set a face's background color, painted behind overlay text that uses
    /// it: `(set-face-background "plot-heat-3-bg" "#f4a261")`.
    SetFaceBackground(String, String),
    /// Set the font family text under a face shapes with:
    /// `(set-face-font "plot-series-0" "Macros Plot")`.
    SetFaceFont(String, String),
    /// Toggle a face's underline attribute: `(set-face-underline "link" #t)`.
    SetFaceUnderline(String, bool),
    /// Toggle a face's bold attribute: `(set-face-bold "feeds-unread" #t)`.
    SetFaceBold(String, bool),
    /// Toggle a face's wavy-underline attribute (spellcheck squiggles):
    /// `(set-face-wavy "spell-error" #t)`. Wavy implies underline.
    SetFaceWavy(String, bool),
    // Native spell checking (macOS NSSpellChecker; no-ops elsewhere). These
    // run as commands (not read primitives) because each call XPCs to the
    // AppleSpell service — the handler runs it on a worker thread (blocking
    // the main thread deadlocks the XPC reply) and delivers the result to
    // the named Scheme callback asynchronously.
    /// Spell-check `text`: the callback receives `"start\tend\tword"` rows
    /// (UTF-8 byte offsets into `text`) joined by newlines, or `""` if clean.
    SpellCheck {
        text: String,
        callback: String,
    },
    /// Correction candidates for a misspelled word: the callback receives one
    /// suggestion per line, or `""` if there are none.
    SpellSuggest {
        word: String,
        callback: String,
    },
    /// Add a word to the OS user dictionary (persists across sessions).
    SpellLearnWord(String),
    /// Load a built-in theme by name. Queued by the `load-theme-impl` primitive,
    /// which the core.scm `load-theme` wrapper calls for `(load-theme 'macros-light)`
    /// or `(load-theme "macros-light")` (a symbol or string).
    LoadTheme(String),
    // Special (read-only, program-generated) buffers — the magit pattern.
    /// Render `text` into a read-only buffer `name` with major mode `mode` and
    /// per-line faces `(line, face)`, switching to it (creating it if needed,
    /// refreshing in place — point preserved — if already shown).
    ///
    /// With `quiet`, refresh the buffer wherever it already lives without
    /// switching to it or moving focus — the auto-refresh path for the
    /// dired-sidebar/dired view when a watched directory changes on disk.
    RenderBuffer {
        name: String,
        mode: String,
        text: String,
        line_faces: Vec<(usize, String)>,
        quiet: bool,
    },
    /// Move point to the start of (0-based) line `n` in the active buffer.
    GotoLine(usize),
    /// Scroll so the cursor sits `n` display-rows below the viewport top — pairs
    /// with `(window-cursor-row)` to keep a buffer stable across a re-render.
    ScrollCursorToRow(usize),
    /// Move point to byte offset `n` (Emacs `goto-char`).
    GotoChar(usize),
    /// Read a line in the minibuffer, then call Steel fn `callback` with it
    /// (Emacs `read-string`, callback-style to fit the async engine). `initial`
    /// pre-fills the input (empty for none).
    ReadString {
        prompt: String,
        initial: String,
        callback: String,
    },
    /// Read a secret in the minibuffer — echoed as `•`, kept out of history,
    /// not trimmed — then call Steel fn `callback` with it (Emacs `read-passwd`).
    ReadPasswd {
        prompt: String,
        callback: String,
        /// Called with "" if the read is aborted (C-g) or displaced by another.
        on_cancel: Option<String>,
    },
    /// Append `text` to the end of buffer `name`, creating it if needed and
    /// switching to it (Emacs `append-to-buffer`; an output-buffer primitive).
    AppendToBuffer {
        name: String,
        text: String,
    },
    /// Evaluate `(callback path text)` with `text` what `path` holds now: the
    /// text of the buffer visiting it when one does, else the file's ("" if it
    /// doesn't exist). The callback answers with [`Self::SetFileText`].
    EditFileText {
        path: String,
        callback: String,
    },
    /// Replace `path`'s text with `text`, answering [`Self::EditFileText`]:
    /// into the buffer visiting it (an undoable edit, left unsaved) when one
    /// does, else by rewriting the file atomically. If the buffer or file
    /// changed since the callback was given its text, the callback is run
    /// again on the new text instead.
    SetFileText {
        path: String,
        text: String,
    },
    /// Re-run Org fontification against the buffer `name`, even when it is not the
    /// active buffer — e.g. agent-shell streaming a reply into a buffer the user
    /// is viewing in a non-selected pane. The editor publishes a snapshot of that
    /// buffer and redirects the resulting overlay writes into it, so the existing
    /// `org--fontify` colorizes it in place without disturbing focus.
    FontifyOrgBuffer(String),
    /// Diff two buffers by name (the `similar` crate) and call Steel fn
    /// `callback` with one "tag\tas\tae\tbs\tbe\tabs\tabe\tbbs\tbbe" record per
    /// non-equal hunk — line ranges + byte ranges for each side. Powers the
    /// interactive ediff two-pane merge (cross-buffer, so it can't be done from
    /// the active-buffer snapshot in Scheme).
    DiffBuffers {
        a: String,
        b: String,
        callback: String,
    },
    /// Replace lines [dst_start,dst_end) of buffer `dst` with lines
    /// [src_start,src_end) of buffer `src` (ediff "copy this hunk A->B").
    BufferCopyLines {
        src: String,
        src_start: usize,
        src_end: usize,
        dst: String,
        dst_start: usize,
        dst_end: usize,
    },
    /// Paint a face over lines [start,end) of buffer `name`, whether or not it's
    /// the active buffer (ediff hunk highlight in the non-selected pane).
    BufferHighlightLines {
        name: String,
        start: usize,
        end: usize,
        face: String,
    },
    /// Clear all face overlays from buffer `name` (active or inactive).
    BufferClearHighlights {
        name: String,
    },
    /// Expand a yasnippet-style template at point: insert it, drop tab-stop field
    /// markers ($1/$2/${n:default}/$0), and jump to the first field.
    SnippetExpand(String),
    /// Jump to the next / previous snippet field in the active session.
    SnippetNextField,
    SnippetPrevField,
    /// Show avy jump hints: one "byte\tlabel" record per line; the labels paint
    /// over the buffer at those byte offsets until cleared.
    AvyShowHints(String),
    /// Clear all avy jump hints.
    AvyClearHints,
    /// multiple-cursors: add a secondary cursor below / above the primary, at the
    /// next occurrence of the word at point, or clear all secondary cursors.
    McAddBelow,
    McAddAbove,
    McMarkNext,
    McClear,
    /// Set the mode-line project register (back-compat sugar for slot "project").
    SetModelineProject(String),
    /// Set the mode-line git register (back-compat sugar for slot "git").
    SetModelineGit(String),
    /// Write/clear a named mode-line register: `(key, value)`, empty value clears.
    SetModeLineSlot(String, String),
    /// Replace the mode-line format with tab-encoded segment tokens (see
    /// `parse_mode_line_format`).
    SetModeLineFormat(Vec<String>),
    /// Publish the title-bar menu named the payload's `title` (see scheme/titlebar-menu.scm):
    /// one row per `items` label, each running the parallel `commands` entry
    /// by name (a late-bound call, so package-defined commands resolve). A row
    /// with an empty command hands its label to `callback` instead. `active`
    /// (if non-empty) is checkmarked and shown on the trigger; `section` heads
    /// the rows. An empty `items` hides the menu but keeps its position.
    SetTitlebarMenu(Box<TitlebarMenuArgs>),
    // Transient popup (the magit / `transient.el` pattern).
    /// Open a transient popup: a title plus `(key, label, command)` rows. When
    /// `sticky`, selecting a row keeps the popup open (the row command re-opens
    /// to refresh, or calls `transient-quit`) instead of auto-closing.
    Transient {
        title: String,
        columns: Vec<TransientColumn>,
        sticky: bool,
    },
    /// Close any open transient popup.
    TransientQuit,
    // Windows (split panes).
    SplitWindowBelow,
    SplitWindowRight,
    /// Split the selected window side by side, the existing pane keeping this
    /// fraction of the width — a narrow sidebar (e.g. 0.2 for dired-sidebar).
    SplitWindowRightRatio(f64),
    OtherWindow,
    DeleteWindow,
    DeleteOtherWindows,
    EnlargeWindow,
    ShrinkWindow,
    EnlargeWindowHorizontally,
    ShrinkWindowHorizontally,
    // Keyboard macros.
    StartKbdMacro,
    EndKbdMacro,
    CallLastKbdMacro,
    // Overlays / text properties / folds.
    /// Layer `face` over the byte range — sub-line highlight (Emacs overlay).
    OverlayFace {
        start: usize,
        end: usize,
        face: String,
    },
    /// Hide a byte range from display without changing buffer text.
    OverlayHide {
        start: usize,
        end: usize,
    },
    /// Hide buffer lines `[start_line, end_line)` (folding / invisibility).
    FoldLines {
        start_line: usize,
        end_line: usize,
    },
    /// Unfold everything.
    UnfoldAll,
    /// Attach a fold chevron to buffer `line`'s gutter. `open` picks the glyph
    /// (down = expanded, right = collapsed); the click hit-test calls back into
    /// Scheme's `fold-toggle-at-line`. Policy (which lines, what range) is Scheme.
    SetFoldMarker {
        line: usize,
        open: bool,
    },
    /// Remove every fold chevron from the active buffer's gutter.
    ClearFoldMarkers,
    /// Remove all overlays/folds/properties from the active buffer.
    ClearOverlays,
    /// Remove only the sub-line overlay faces (keeps folds/props/suffixes/hides)
    /// — for repainting semantic tokens / document highlights without nuking the
    /// rest, or another feature's concealment.
    ClearOverlayFaces,
    /// Remove only the sub-line overlay faces whose face name equals the given
    /// one — lets a feature (e.g. spellcheck squiggles) repaint its own faces
    /// without wiping co-resident overlays from org/rainbow/LSP.
    ClearOverlayFacesMatching(String),
    /// Remove only the `overlay-hide` concealment ranges (keeps faces/folds) —
    /// for refreshing markdown/org heading-marker hiding.
    ClearOverlayHides,
    /// Remove only the line suffixes (virtual text) — for refreshing inlay hints.
    ClearLineSuffixes,
    /// Attach a text property `(key, value)` to a byte range.
    PutTextProperty {
        start: usize,
        end: usize,
        key: String,
        value: String,
    },
    /// Virtual text appended after `line` (inlay hint / blame / inline note).
    SetLineSuffix {
        line: usize,
        text: String,
        face: String,
    },
    // Async.
    /// Spawn `program` async under `id`; call Steel fn `on_output` per stdout
    /// line, and `on_exit` (if non-empty) with the exit code (Emacs `make-process`).
    MakeProcess(Box<MakeProcessArgs>),
    /// Spawn a debug adapter (see [`DapStartArgs`]): Content-Length-framed DAP
    /// JSON on stdio. Boxed like [`HttpStream`](Self::HttpStream): its payload
    /// would otherwise bloat every `Result<(), EditorCommand>` (clippy
    /// `result_large_err`).
    DapStart(Box<DapStartArgs>),
    /// Like [`DapStart`](Self::DapStart) but connect to `host:port` for
    /// adapters that serve DAP over a socket instead of stdio (delve).
    DapConnect {
        id: u64,
        host: String,
        port: u16,
        on_message: String,
        on_exit: Option<String>,
    },
    /// Frame one complete DAP JSON message to session `id`'s adapter.
    DapSend {
        id: u64,
        message: String,
    },
    /// Kill session `id`'s adapter (its process group) / close its socket.
    DapKill {
        id: u64,
    },
    /// Write `text` to process `id`'s stdin (Emacs `process-send-string`).
    SendProcess {
        id: u64,
        text: String,
    },
    /// Kill process `id` (Emacs `kill-process`).
    KillProcess {
        id: u64,
    },
    /// Bind process `id` to buffer `name`: killing the buffer terminates the
    /// process (Emacs `set-process-buffer`).
    SetProcessBuffer {
        id: u64,
        name: String,
    },
    /// Open a streaming HTTP POST (see [`HttpStreamArgs`]); the transport for
    /// talking directly to a model provider's SSE endpoint (e.g. Anthropic) from
    /// Steel — the HTTP analogue of `make-process`. Boxed: its payload is far
    /// larger than any other variant, and unboxed it would bloat every
    /// `Result<(), EditorCommand>` (clippy `result_large_err`).
    HttpStream(Box<HttpStreamArgs>),
    /// Cancel in-flight HTTP stream `id`: the reader stops forwarding lines and
    /// drops the connection (the chat client's "cancel turn").
    HttpStreamCancel {
        id: u64,
    },
    /// One-shot HTTP request of any method (see [`HttpRequestArgs`]); the whole
    /// response goes to a single callback. Used for an OAuth token exchange and
    /// authenticated non-streaming API calls.
    HttpRequest(Box<HttpRequestArgs>),
    /// Start a local HTTP server (see [`HttpServeArgs`]) — the OAuth loopback
    /// redirect target, or any local callback endpoint.
    HttpServe(Box<HttpServeArgs>),
    /// Stop the local HTTP server bound to `port` (its accept loop exits on its
    /// next poll and the socket is dropped).
    HttpServerStop {
        port: u16,
    },
    /// Open a long-lived async socket under `id` (`socket-connect-async`): a
    /// worker thread connects (TLS when `tls`), then forwards inbound chunks —
    /// hex-encoded — to Steel fn `on_data`, and calls `on_close` with the id
    /// when the peer closes or errors. The socket analogue of `make-process`.
    SocketConnectAsync {
        id: u64,
        host: String,
        port: u16,
        tls: bool,
        on_data: String,
        on_close: String,
    },
    /// Send `hex`-decoded bytes on async socket `id` (`socket-async-send`).
    SocketAsyncSend {
        id: u64,
        hex: String,
    },
    /// Close async socket `id` (`socket-async-close`); its reader stops and the
    /// connection drops.
    SocketAsyncClose {
        id: u64,
    },
    /// Open a WebSocket under `id` (`ws-connect`): a worker thread performs the
    /// handshake to `url` (ws:// or wss://), forwards each inbound text message
    /// to Steel fn `on_message`, and calls `on_close` with the id when the
    /// socket closes.
    WsConnect {
        id: u64,
        url: String,
        on_message: String,
        on_close: String,
    },
    /// Send a text message on WebSocket `id` (`ws-send`).
    WsSend {
        id: u64,
        text: String,
    },
    /// Close WebSocket `id` (`ws-close`).
    WsClose {
        id: u64,
    },
    // ANVIL REPL — a second, independent Steel interpreter (see `crate::anvil`).
    /// Evaluate `source` in the ANVIL REPL engine; deliver the printed result to
    /// the Scheme callback `on_result` (SLIME's `slime-eval-async`).
    AnvilEval {
        source: String,
        on_result: String,
    },
    /// Cancel the ANVIL REPL's current evaluation (SLIME's `C-c C-c`).
    AnvilInterrupt,
    /// Open (or switch to) the `*anvil-repl*` buffer.
    AnvilReplOpen,
    /// RET in the REPL: submit complete input, or continue it on a new line.
    AnvilSendInput,
    /// M-p (`true`, older) / M-n (`false`, newer) through the REPL's input history.
    AnvilHistory(bool),
    /// Rebuild the ANVIL REPL engine — a fresh image (SLIME's restart).
    AnvilReset,
    /// Load the active buffer's file into a *fresh* ANVIL image, rendering its
    /// output and result in the REPL — the Steel test runner without a `steel`
    /// CLI.
    AnvilTestFile,
    /// `eval-last-sexp` into the ANVIL image: the region, else the sexp before
    /// point; the result is echoed.
    AnvilEvalLastSexp,
    /// Evaluate the top-level form at point in the ANVIL image (SLIME `C-M-x`).
    /// The editor computes the form's bounds from the buffer and point.
    AnvilEvalDefun,
    /// `(load …)` the active buffer's file into the ANVIL image (SLIME `C-c C-l`).
    AnvilLoadFile,
    /// Show the ANVIL image's docstring for the symbol at point (SLIME `C-c C-d`).
    AnvilDoc,
    /// Macroexpand the region (or the form at point) against the image's macros
    /// (SLIME `C-c RET`).
    AnvilMacroexpand,
    /// List live image bindings whose name contains `pattern` (SLIME `apropos`).
    AnvilApropos(String),
    /// Echo the image's one-line signature for the symbol at point — eldoc.
    AnvilSignature,
    // Terminal emulator.
    /// Open a terminal running `program` (login shell if empty) in a new
    /// `term-mode` buffer named `name`, backed by terminal `id`.
    MakeTerminal(Box<MakeTerminalArgs>),
    /// Call Steel fn `callback` after `ms` milliseconds (Emacs `run-with-timer`).
    RunWithTimer {
        ms: u64,
        callback: String,
    },
    // Programmatic editing (arbitrary ranges in the active buffer).
    /// Delete bytes `[start, end)`.
    DeleteRegion {
        start: usize,
        end: usize,
    },
    /// Kill bytes `[start, end)` into the kill ring (like `kill-region` but on an
    /// explicit range rather than the active region).
    KillRegionBetween {
        start: usize,
        end: usize,
    },
    /// Replace bytes `[start, end)` with `text`.
    ReplaceRegion {
        start: usize,
        end: usize,
        text: String,
    },
    /// Insert `text` at byte `pos`.
    InsertAt {
        pos: usize,
        text: String,
    },
    /// Select the byte range `[start, end)` (activates the mark). Lets Scheme
    /// set an arbitrary region, e.g. expand-region / LSP selection ranges.
    SetRegion {
        start: usize,
        end: usize,
    },
    /// Close the current undo group so the next edit starts a new one.
    UndoBoundary,
    /// Remember the undo state (Emacs `prepare-change-group`); a later
    /// `UndoAmalgamateEnd` folds every undo node added since into one, so a
    /// multi-command operation (query-replace across many keystrokes) undoes
    /// as a single step.
    UndoAmalgamateBegin,
    /// Fold the undo nodes recorded since `UndoAmalgamateBegin` into one
    /// (Emacs `undo-amalgamate-change-group`). No-op without a begin.
    UndoAmalgamateEnd,
    /// Toggle the active buffer between a read-only special buffer driven by
    /// `mode`'s keymap and ordinary editable text (`""`) — occur-edit / wgrep
    /// flip a result buffer editable in place and back.
    SetSpecialMode(String),
    /// Set a buffer-local variable on the active buffer.
    BufferLocalSet {
        key: String,
        value: String,
    },
    // Minibuffer reads that call back into Scheme.
    /// `completing-read`: prompt with completion, then call `callback`.
    /// `candidates` is the static list; `dynamic` is a Scheme fn name re-queried
    /// per keystroke when the caller passed a function. `initial` pre-fills input.
    CompletingRead {
        prompt: String,
        candidates: Vec<String>,
        dynamic: Option<String>,
        initial: String,
        callback: String,
    },
    /// `read-file-name`: prompt for a file path, then call `callback`.
    ReadFileName {
        prompt: String,
        callback: String,
    },
    /// `y-or-n-p`: yes/no prompt, then call `callback` with "yes"/"no". When
    /// `sticky`, the prompt can't be aborted (C-g/Escape) and stays until the
    /// user answers — for prompts that must resolve a pending request.
    YesOrNo {
        prompt: String,
        callback: String,
        sticky: bool,
    },
    // --- Session persistence (scheme/session.scm: savehist + desktop) -----
    /// Publish the [`SessionState`] (rings, buffers, layout), then call
    /// `callback` so it reads fresh values.
    SessionRefresh(String),
    /// Replace one history ring wholesale (`"minibuffer"`, `"kill"`,
    /// `"search"`, `"command"`), oldest entry first — savehist's restore.
    SetSessionRing {
        ring: String,
        items: Vec<String>,
    },
    /// Visit `(path, point)` files as background buffers (no switch, no
    /// find-file-hook); missing/unreadable files are skipped silently, and an
    /// already-open file just gets its point moved.
    RestoreFileBuffers(Vec<(String, usize)>),
    /// Rebuild the window split tree (see [`LayoutSpec`]).
    SetWindowLayout(LayoutSpec),
}

/// One row of a transient popup: a key, its description, and the Steel command
/// to run when it's pressed.
#[derive(Debug, Clone)]
pub struct TransientItem {
    pub key: String,
    pub label: String,
    pub command: String,
}

/// A column of transient rows, optionally headed by a title. A flat
/// `(transient title rows)` is one untitled column; the grouped form gives
/// several titled columns laid out side by side (magit-style).
#[derive(Debug, Clone)]
pub struct TransientColumn {
    pub title: Option<String>,
    pub rows: Vec<TransientItem>,
}

/// A Helm source's candidates: a fixed list (each candidate carrying an optional
/// marginalia-style annotation), or the name of a Steel function re-queried with
/// the current pattern as it changes (dynamic sources).
#[derive(Debug, Clone)]
pub enum HelmCandidates {
    Static(Vec<(String, Option<String>)>),
    /// A Scheme fn re-queried per keystroke; its result is then filtered/ranked
    /// by Rust against the pattern (the fn supplies the pool).
    Dynamic(String),
    /// Like [`HelmCandidates::Dynamic`], but the fn's result is shown verbatim —
    /// Rust does no further filtering. The fn is the sole filter authority, so a
    /// Helm picker can be driven entirely from Scheme (e.g. via `fuzzy-match`).
    DynamicRaw(String),
}

/// A Helm source: a named group of candidates plus one or more actions. Each
/// action is `(label, function-name)`; the first is the default (RET), the rest
/// reachable via the TAB action menu. The chosen candidate string is passed to
/// the action function.
#[derive(Debug, Clone)]
pub struct HelmSourceSpec {
    pub name: String,
    pub candidates: HelmCandidates,
    pub actions: Vec<(String, String)>,
    /// Optional Scheme fn name; given the selected candidate it returns the
    /// preview as a list of lines, shown in a side pane. `None` = no preview.
    pub preview: Option<String>,
}

/// A window split tree as data — what `(window-layout)` returns and
/// `(set-window-layout tree)` rebuilds (desktop save/restore). Scheme sees it as
/// nested lists: `(leaf name path point selected?)` and
/// `(split vertical? ratio a b)`.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutSpec {
    Leaf {
        /// Buffer name shown in the pane. On restore, a leaf is resolved by
        /// `path` first (when non-empty), then by this name.
        buffer: String,
        /// The buffer's visited file, or "" for a fileless buffer.
        path: String,
        /// The pane's point (byte offset).
        point: usize,
        /// Whether this is the selected window.
        selected: bool,
    },
    Split {
        /// `true` = stacked (split below), `false` = side by side.
        vertical: bool,
        /// Fraction of the extent given to `a`.
        ratio: f32,
        a: Box<LayoutSpec>,
        b: Box<LayoutSpec>,
    },
}
