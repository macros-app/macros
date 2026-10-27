use std::sync::{Arc, LazyLock, OnceLock};

use ropey::Rope;
use steel::rvals::{IntoSteelVal, SteelVal};
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

use crate::editor::StructuralOp;

use super::*;

/// The nullary commands: each Steel name pushes its variant. Shared by engine
/// registration (on the thread), `(command-list)`, and the handle's fast-path
/// table; built once.
pub(crate) fn nullary_commands() -> &'static [(&'static str, EditorCommand)] {
    static COMMANDS: LazyLock<Vec<(&'static str, EditorCommand)>> = LazyLock::new(build_nullary);
    &COMMANDS
}

fn build_nullary() -> Vec<(&'static str, EditorCommand)> {
    use EditorCommand::*;
    let mut out = vec![
        ("forward-char", ForwardChar),
        ("backward-char", BackwardChar),
        ("next-line", NextLine),
        ("previous-line", PreviousLine),
        ("forward-word", ForwardWord),
        ("forward-word-start", ForwardWordStart),
        ("backward-word", BackwardWord),
        ("backward-word-start", BackwardWordStart),
        ("forward-word-end", ForwardWordEnd),
        ("forward-bigword-start", ForwardBigwordStart),
        ("backward-bigword-start", BackwardBigwordStart),
        ("forward-bigword-end", ForwardBigwordEnd),
        ("indent-region", IndentRegion),
        ("outdent-region", OutdentRegion),
        ("reindent-region", ReindentRegion),
        ("json-pretty-print-buffer", JsonPrettyPrintBuffer),
        ("json-pretty-print", JsonPrettyPrintRegion),
        ("move-beginning-of-line", BeginningOfLine),
        ("move-end-of-line", EndOfLine),
        ("beginning-of-buffer", BeginningOfBuffer),
        ("end-of-buffer", EndOfBuffer),
        ("set-mark", SetMark),
        ("exchange-point-and-mark", ExchangePointAndMark),
        ("mark-whole-buffer", MarkWholeBuffer),
        ("newline", Newline),
        ("open-line", OpenLine),
        ("indent-for-tab-command", IndentForTab),
        ("delete-char", DeleteChar),
        ("delete-backward-char", DeleteBackwardChar),
        ("transpose-chars", TransposeChars),
        ("kill-line", KillLine),
        ("kill-word", KillWord),
        ("backward-kill-word", BackwardKillWord),
        ("kill-region", KillRegion),
        ("kill-ring-save", KillRingSave),
        ("yank", Yank),
        ("yank-pop", YankPop),
        ("clipboard-copy", ClipboardCopy),
        ("clipboard-cut", ClipboardCut),
        ("clipboard-paste", ClipboardPaste),
        ("undo", Undo),
        ("redo", Redo),
        ("scroll-up", ScrollUp),
        ("scroll-down", ScrollDown),
        ("recenter", Recenter),
        ("keyboard-quit", KeyboardQuit),
        ("execute-extended-command", ExecuteExtendedCommand),
        ("list-messages", ListMessages),
        ("list-lsp-log", ListLspLog),
        ("check-for-updates", CheckForUpdates),
        ("eval-buffer", EvalBuffer),
        ("eval-last-sexp", EvalLastSexp),
        ("eval-defun", EvalDefun),
        ("forward-sexp", Structural(StructuralOp::ForwardSexp)),
        ("backward-sexp", Structural(StructuralOp::BackwardSexp)),
        ("up-list", Structural(StructuralOp::UpList)),
        ("backward-up-list", Structural(StructuralOp::BackwardUpList)),
        ("down-list", Structural(StructuralOp::DownList)),
        ("kill-sexp", Structural(StructuralOp::KillSexp)),
        (
            "paredit-forward-slurp-sexp",
            Structural(StructuralOp::SlurpForward),
        ),
        (
            "paredit-forward-barf-sexp",
            Structural(StructuralOp::BarfForward),
        ),
        ("paredit-raise-sexp", Structural(StructuralOp::Raise)),
        ("paredit-splice-sexp", Structural(StructuralOp::Splice)),
        ("paredit-wrap-round", Structural(StructuralOp::WrapRound)),
        ("eval-expression", EvalExpression),
        ("find-file", FindFile),
        ("switch-buffer", SwitchBuffer),
        ("next-buffer", NextBuffer),
        ("previous-buffer", PreviousBuffer),
        ("save-buffer", SaveBuffer),
        ("save-some-buffers", SaveSomeBuffers),
        ("revert-buffer", RevertBuffer),
        ("line-numbers-mode", ToggleLineNumbers),
        ("scroll-bar-mode", ToggleScrollBar),
        ("word-wrap-mode", ToggleWordWrap),
        ("magit-gutter-mode", ToggleGitGutter),
        ("magit-gutter-refresh", GitGutterRefresh),
        ("electric-pair-mode", ToggleElectricPair),
        ("snippet-next-field", SnippetNextField),
        ("snippet-prev-field", SnippetPrevField),
        ("mc-add-cursor-below", McAddBelow),
        ("mc-add-cursor-above", McAddAbove),
        ("mc-mark-next-like-this", McMarkNext),
        ("mc-clear", McClear),
        ("kill-buffer", KillBuffer),
        ("mcp-server-info", McpServerInfo),
        ("kill-buffer-and-window", KillBufferAndWindow),
        ("quit", Quit),
        ("lsp-find-definition", LspDefinition),
        ("lsp-find-declaration", LspDeclaration),
        ("lsp-find-type-definition", LspTypeDefinition),
        ("lsp-find-implementation", LspImplementation),
        ("lsp-find-references", LspReferences),
        ("lsp-hover", LspHover),
        ("lsp-signature-help", LspSignatureHelp),
        ("lsp-document-symbols", LspDocumentSymbols),
        ("lsp-code-action", LspCodeAction),
        ("lsp-format-buffer", LspFormatBuffer),
        ("lsp-restart", LspRestart),
        ("lsp-call-hierarchy-incoming", LspCallHierarchyIncoming),
        ("lsp-call-hierarchy-outgoing", LspCallHierarchyOutgoing),
        ("lsp-rename", LspPrepareRename),
        ("lsp-code-lens", LspCodeLens),
        ("lsp-xref-visit", LspXrefVisit),
        ("lsp-apply-code-action", LspApplyCodeAction),
        ("xref-push-marker", XrefPushMarker),
        ("xref-pop-marker", XrefPopMarker),
        ("clear-overlay-faces", ClearOverlayFaces),
        ("clear-overlay-hides", ClearOverlayHides),
        ("clear-line-suffixes", ClearLineSuffixes),
        ("completion-at-point", CompletionAtPoint),
        ("completion-next", CompletionNext),
        ("completion-previous", CompletionPrevious),
        ("completion-accept", CompletionAccept),
        ("completion-abort", CompletionAbort),
        ("helm-M-x", HelmMx),
        ("helm-next", HelmNext),
        ("helm-previous", HelmPrevious),
        ("helm-accept", HelmAccept),
        ("helm-abort", HelmAbort),
        ("helm-actions", HelmActions),
        ("helm-mark", HelmToggleMark),
        ("enable-lsp", EnableLsp),
        ("macros-steel-mode", EnterMacrosSteel),
        ("helix-mode", HelixMode),
        ("helix-disable", HelixDisable),
        ("helix-normal-state", HelixNormalState),
        ("evil-mode", EvilMode),
        ("evil-normal-state", EvilNormalState),
        ("evil-insert-state", EvilInsertState),
        ("evil-visual-state", EvilVisualState),
        ("evil-visual-line-state", EvilVisualLineState),
        ("evil-visual-insert", EvilVisualInsert),
        ("evil-visual-append", EvilVisualAppend),
        ("evil-visual-block-state", EvilVisualBlockState),
        ("evil-block-insert", EvilBlockInsert),
        ("evil-block-append", EvilBlockAppend),
        ("evil-block-delete", EvilBlockDelete),
        ("evil-block-change", EvilBlockChange),
        ("evil-block-yank", EvilBlockYank),
        ("evil-disable", EvilDisable),
        ("evil-scroll-down", EvilScrollDown),
        ("evil-scroll-up", EvilScrollUp),
        ("evil-search-forward", EvilSearchForward),
        ("evil-search-backward", EvilSearchBackward),
        ("evil-search-next", EvilSearchNext),
        ("evil-search-previous", EvilSearchPrevious),
        ("evil-ex-command", EvilExCommand),
        ("evil-paste-after", EvilPasteAfter),
        ("evil-paste-before", EvilPasteBefore),
        ("evil-replace-char", EvilReplaceChar),
        ("evil-set-linewise", EvilSetLinewise),
        ("evil-set-charwise", EvilSetCharwise),
        ("count-repeat-step", CountRepeatStep),
        ("replay-step", ReplayStep),
        ("evil-repeat", EvilRepeat),
        ("evil-reselect-visual", EvilReselectVisual),
        ("evil-jump-backward", EvilJumpBackward),
        ("evil-jump-forward", EvilJumpForward),
        ("windmove-left", WindmoveLeft),
        ("windmove-down", WindmoveDown),
        ("windmove-up", WindmoveUp),
        ("windmove-right", WindmoveRight),
        ("previous-window-any-frame", PreviousWindow),
        ("evil-window-mru", LastWindow),
        ("evil-window-top-left", TopLeftWindow),
        ("evil-window-bottom-right", BottomRightWindow),
        ("balance-windows", BalanceWindows),
        ("winner-undo", WinnerUndo),
        ("winner-redo", WinnerRedo),
        ("evil-window-set-height", MaximizeWindowHeight),
        ("evil-window-set-width", MaximizeWindowWidth),
        ("evil-window-move-far-left", MoveWindowFarLeft),
        ("evil-window-move-very-bottom", MoveWindowFarDown),
        ("evil-window-move-very-top", MoveWindowFarUp),
        ("evil-window-move-far-right", MoveWindowFarRight),
        ("evil-window-exchange", ExchangeWindow),
        ("evil-window-rotate-downwards", RotateWindowsDown),
        ("evil-window-rotate-upwards", RotateWindowsUp),
        ("evil-window-new", NewWindowBelow),
        ("evil-window-vnew", NewWindowRight),
        ("helix-paste-after", HelixPasteAfter),
        ("helix-paste-before", HelixPasteBefore),
        ("isearch-forward", IsearchForward),
        ("isearch-backward", IsearchBackward),
        ("split-window-below", SplitWindowBelow),
        ("split-window-right", SplitWindowRight),
        ("other-window", OtherWindow),
        ("delete-window", DeleteWindow),
        ("delete-other-windows", DeleteOtherWindows),
        ("enlarge-window", EnlargeWindow),
        ("shrink-window", ShrinkWindow),
        ("enlarge-window-horizontally", EnlargeWindowHorizontally),
        ("shrink-window-horizontally", ShrinkWindowHorizontally),
        ("start-kbd-macro", StartKbdMacro),
        ("end-kbd-macro", EndKbdMacro),
        ("call-last-kbd-macro", CallLastKbdMacro),
    ];
    // Grid buffer commands (scheme/grid.scm binds them) — one table so the
    // command set lives with the grid code.
    out.extend(
        crate::grid::cmd::GRID_COMMANDS
            .iter()
            .map(|&(name, cmd)| (name, Grid(cmd.into()))),
    );
    out
}

/// Build the Steel engine and register every command/bridge function. Runs on
/// the engine thread (the engine is `!Send`).
pub(crate) fn build_engine(shared: &EngineShared) -> (Engine, Vec<String>) {
    let EngineShared { queue, .. } = shared;
    let mut engine = Engine::new();
    let mut names: Vec<String> = Vec::new();
    let mut api: Vec<ApiEntry> = Vec::new();
    // Filled once registration finishes; `api-reference` reads it lazily.
    let api_cell: Arc<OnceLock<Vec<ApiEntry>>> = Arc::new(OnceLock::new());
    let mut reg = Registrar {
        engine: &mut engine,
        names: &mut names,
        api: &mut api,
        section: Section::Commands,
    };
    for (name, command) in nullary_commands() {
        register_nullary(&mut reg, queue, name, command.clone());
    }
    prims::editing::register(&mut reg, shared);
    prims::grid::register(&mut reg, shared);
    prims::prompts::register(&mut reg, shared);
    prims::keymaps::register(&mut reg, shared);
    prims::languages::register(&mut reg, shared);
    prims::faces::register(&mut reg, shared);
    prims::help::register(&mut reg, shared, &api_cell);
    prims::text::register(&mut reg, shared);
    prims::plotting::register(&mut reg, shared);
    prims::spell::register(&mut reg, shared);
    prims::modal::register(&mut reg, shared);
    prims::tree_sitter::register(&mut reg, shared);
    prims::lsp::register(&mut reg, shared);
    prims::options::register(&mut reg, shared);
    prims::buffers::register(&mut reg, shared);
    prims::decorations::register(&mut reg, shared);
    prims::files::register(&mut reg, shared);
    prims::hooks::register(&mut reg, shared);
    prims::special_buffers::register(&mut reg, shared);
    prims::ui::register(&mut reg, shared);
    prims::overlays::register(&mut reg, shared);
    prims::time::register(&mut reg, shared);
    prims::search::register(&mut reg, shared);
    prims::git::register(&mut reg, shared);
    prims::process::register(&mut reg, shared);
    prims::dap::register(&mut reg, shared);
    prims::http::register(&mut reg, shared);
    prims::crypto::register(&mut reg, shared);
    prims::sockets::register(&mut reg, shared);
    prims::sqlite::register(&mut reg, shared);
    prims::anvil::register(&mut reg, shared);
    prims::buffer_reads::register(&mut reg, shared);
    prims::markers::register(&mut reg, shared);
    prims::motions::register(&mut reg, shared);
    prims::regexp::register(&mut reg, shared);
    // Session persistence (savehist / desktop) — see src/script/session.rs.
    reg.section(Section::Session);
    register_session(&mut reg, shared);
    // The unlocked authinfo.gpg store behind scheme/secrets.scm.
    reg.section(Section::Secrets);
    crate::authinfo::register(&mut reg);
    let _ = api_cell.set(api);
    (engine, names)
}

/// Registers Steel functions into the engine while recording every registered
/// name. The global-name list (consumed by the built-in Steel LSP and the
/// minibuffer symbol completion via [`ScriptEngine::global_names`]) is thus
/// derived from registration itself and can never drift from it.
///
/// It also records each primitive's category (the current [`Registrar::section`])
/// and its argument/return types for the package-API reference ([`super::api`]).
pub(crate) struct Registrar<'a> {
    engine: &'a mut Engine,
    names: &'a mut Vec<String>,
    api: &'a mut Vec<ApiEntry>,
    section: Section,
}

impl Registrar<'_> {
    pub(crate) fn register_fn<FN, ARGS, RET>(&mut self, name: &'static str, func: FN)
    where
        Engine: RegisterFn<FN, ARGS, RET>,
    {
        self.names.push(name.to_string());
        self.api.push(ApiEntry::new(
            name,
            self.section,
            std::any::type_name::<ARGS>(),
            std::any::type_name::<RET>(),
        ));
        self.engine.register_fn(name, func);
    }

    /// [`Self::register_fn`] for a closure that takes raw `SteelVal`s and
    /// converts them itself: the API reference records `arg_types` (the types
    /// it converts to) rather than `SteelVal`.
    fn register_fn_typed<FN, ARGS, RET>(&mut self, name: &'static str, func: FN, arg_types: &[&str])
    where
        Engine: RegisterFn<FN, ARGS, RET>,
    {
        self.names.push(name.to_string());
        self.api
            .push(ApiEntry::with_types(name, self.section, arg_types, "()"));
        self.engine.register_fn(name, func);
    }

    /// File every following registration under `section` in the API reference.
    pub(crate) fn section(&mut self, section: Section) {
        self.section = section;
    }
}

/// Register a Steel function `name` that, when called, enqueues a clone of `cmd`.
pub(crate) fn register_nullary(
    reg: &mut Registrar,
    queue: &Queue,
    name: &'static str,
    cmd: EditorCommand,
) {
    let q = queue.clone();
    reg.register_fn(name, move || q.push(cmd.clone()));
}

/// Convert one argument of a command primitive. Steel converts an integer to
/// `usize` / `u64` / `u32` with a bare `as` cast, so a Scheme `-1` silently
/// becomes `usize::MAX` (a line, count or timeout of 2^64); a negative integer
/// for an unsigned parameter is rejected with a conversion error instead.
fn command_arg<T: steel::rvals::FromSteelVal + 'static>(val: &SteelVal) -> steel::rvals::Result<T> {
    use std::any::TypeId;
    let unsigned = [
        TypeId::of::<usize>(),
        TypeId::of::<u64>(),
        TypeId::of::<u32>(),
        TypeId::of::<u16>(),
        TypeId::of::<u8>(),
    ];
    if let SteelVal::IntV(n) = val
        && *n < 0
        && unsigned.contains(&TypeId::of::<T>())
    {
        return Err(steel::SteelErr::new(
            steel::rerrs::ErrorKind::ConversionError,
            format!("expected a non-negative integer, found: {n}"),
        ));
    }
    T::from_steelval(val)
}

/// Register a one-argument Steel command: `name` enqueues the command built by
/// `make` from its argument. Collapses the `{ let q = queue.clone(); ... push }`
/// boilerplate for the many single-payload commands.
pub(crate) fn register_unary<T>(
    reg: &mut Registrar,
    queue: &Queue,
    name: &'static str,
    make: impl Fn(T) -> EditorCommand + Send + Sync + 'static,
) where
    T: steel::rvals::FromSteelVal + 'static,
{
    let q = queue.clone();
    reg.register_fn_typed(
        name,
        move |a: SteelVal| -> steel::rvals::Result<()> {
            q.push(make(command_arg(&a)?));
            Ok(())
        },
        &[std::any::type_name::<T>()],
    );
}

/// Register a two-argument Steel command (see [`register_unary`]).
pub(crate) fn register_binary<T, U>(
    reg: &mut Registrar,
    queue: &Queue,
    name: &'static str,
    make: impl Fn(T, U) -> EditorCommand + Send + Sync + 'static,
) where
    T: steel::rvals::FromSteelVal + 'static,
    U: steel::rvals::FromSteelVal + 'static,
{
    let q = queue.clone();
    reg.register_fn_typed(
        name,
        move |a: SteelVal, b: SteelVal| -> steel::rvals::Result<()> {
            q.push(make(command_arg(&a)?, command_arg(&b)?));
            Ok(())
        },
        &[std::any::type_name::<T>(), std::any::type_name::<U>()],
    );
}

/// Register a three-argument Steel command (see [`register_unary`]).
pub(crate) fn register_ternary<T, U, V>(
    reg: &mut Registrar,
    queue: &Queue,
    name: &'static str,
    make: impl Fn(T, U, V) -> EditorCommand + Send + Sync + 'static,
) where
    T: steel::rvals::FromSteelVal + 'static,
    U: steel::rvals::FromSteelVal + 'static,
    V: steel::rvals::FromSteelVal + 'static,
{
    let q = queue.clone();
    reg.register_fn_typed(
        name,
        move |a: SteelVal, b: SteelVal, c: SteelVal| -> steel::rvals::Result<()> {
            q.push(make(command_arg(&a)?, command_arg(&b)?, command_arg(&c)?));
            Ok(())
        },
        &[
            std::any::type_name::<T>(),
            std::any::type_name::<U>(),
            std::any::type_name::<V>(),
        ],
    );
}

/// Register a nullary Steel *read* bridge: `name` returns a value derived from
/// the editor's published snapshot. Collapses the repeated
/// `{ let shared = snapshot.clone(); engine.register_fn(name, move || …) }`
/// boilerplate for the many single-snapshot readers. `get` runs under the
/// snapshot lock and must produce an owned value (clone out anything borrowed).
pub(crate) fn register_snapshot_read<T: IntoSteelVal + 'static>(
    reg: &mut Registrar,
    snapshot: &SharedSnapshot,
    name: &'static str,
    get: impl Fn(&EditorSnapshot) -> T + Send + Sync + 'static,
) {
    let shared = snapshot.clone();
    reg.register_fn(name, move || get(&shared.load()));
}

/// Snap a byte index down to the nearest UTF-8 char boundary in `rope`, clamped
/// to its length. Slicing a `Rope` panics on a non-boundary index, so the
/// text-slicing bridges route caller-supplied offsets through this first — the
/// boundary safety the old `String`-slicing bridges did by hand.
pub(crate) fn snap_byte(rope: &Rope, byte: usize) -> usize {
    let byte = byte.min(rope.len_bytes());
    rope.char_to_byte(rope.byte_to_char(byte))
}
