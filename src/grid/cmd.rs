//! The grid's command vocabulary: every key binding in `scheme/grid.scm`
//! names one of these. They are registered as nullary builtins (see
//! `nullary_commands` in `src/script/registry.rs`), so a keypress applies
//! synchronously without a trip through the Scheme engine thread.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::formula::refs::Axis;

/// A direction on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    /// `(rows, cols)` one step this way moves.
    pub fn delta(self) -> (isize, isize) {
        match self {
            Dir::Left => (0, -1),
            Dir::Right => (0, 1),
            Dir::Up => (-1, 0),
            Dir::Down => (1, 0),
        }
    }

    /// Across columns (left/right) or rows (up/down).
    pub fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::Cols,
            Dir::Up | Dir::Down => Axis::Rows,
        }
    }

    /// Toward higher indices (right or down).
    pub fn forward(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

/// One grid command. Applied by `Editor::grid_command` against the active
/// grid buffer; a no-op (with a message) anywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridCmd {
    // Motion
    /// One cell (`grid-left`, …).
    Move(Dir),
    NextCell,
    PreviousCell,
    /// Excel's Ctrl+Arrow: to the end of the filled run (`grid-edge-left`, …).
    Edge(Dir),
    RowStart,
    RowFirstNonEmpty,
    RowEnd,
    GotoFirstRow,
    GotoLastRow,
    GotoColumn,
    GotoFirstCell,
    GotoLastCell,
    PageDown,
    PageUp,
    HalfPageDown,
    HalfPageUp,
    // Selection
    /// Extend the selection one cell (`grid-extend-left`, …).
    Extend(Dir),
    SelectCells,
    SelectRows,
    SelectColumns,
    SelectAll,
    SelectCancel,
    // Editing
    EditStart,
    EditEnd,
    EditReplace,
    EditPopup,
    Clear,
    Cut,
    Yank,
    Paste,
    DeleteRow,
    DeleteColumn,
    InsertRowBelow,
    InsertRowAbove,
    InsertColumnRight,
    InsertColumnLeft,
    SortAscending,
    SortDescending,
    Undo,
    Redo,
    Repeat,
    // View
    WidenColumn,
    NarrowColumn,
    AutofitColumn,
    AutofitColumnFull,
    TallerRow,
    ShorterRow,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Goto,
    SearchForward,
    SearchNext,
    SearchPrevious,
    ToggleHeader,
    /// Freeze the rows above the cursor (row 1 at the top), or unfreeze.
    ToggleFreeze,
    FormulaBar,
    ViewAsText,
    NextProblem,
    /// `M-x grid-mode`: reopen the current text buffer as a grid.
    GridMode,
    /// `grid-recalculate` (F9): recalculate every formula.
    Recalculate,
    /// Toggle cells between results and formula text (Excel's Ctrl+`).
    ShowFormulas,
    /// Outline the cells the cursor's formula reads.
    TracePrecedents,
    /// Outline the formulas that read the cursor cell.
    TraceDependents,
    /// `M-x grid-toggle-view`: switch this buffer between grid and raw text.
    ToggleView,
    /// In a `ge` popup (`*cell C7*`): write the text back to the cell.
    CellPopupCommit,
    /// In a `ge` popup: discard it.
    CellPopupAbort,
    // Emacs-style additions
    /// `C-x C-x`: swap the cursor and the selection's other corner.
    ExchangeAnchor,
    /// `C-l`: scroll the cursor row to the middle, top, then bottom.
    Recenter,
    SearchBackward,
    /// Jump to a column by its header name, with completion.
    JumpToColumn,
    /// `C-k`: kill the cells from the cursor to the end of the row.
    KillToRowEnd,
    Upcase,
    Downcase,
    Capitalize,
    /// `S-RET`: copy the cell down one row, continuing a series (org-table's
    /// copy-down); over a multi-row selection, fill it down.
    CopyDown,
    /// `M-y` after a paste: replace it with the previous kill.
    YankPop,
    /// Complete the cell's value from the column's choices or its values.
    CompleteCell,
    /// `M-arrows`: move the cursor's row (up/down) or column (left/right),
    /// or the selected ones (`grid-move-row-up`, …).
    MoveLine(Dir),
    HideColumn,
    ShowColumns,
    /// `C-x n n`: show only the selected rows.
    NarrowToSelection,
    /// Show only rows whose cursor-column value passes a typed test.
    Filter,
    /// Show only rows whose cursor-column value equals the cursor cell's.
    FilterByValue,
    /// `C-x n w`: drop narrowing and filters.
    Widen,
    CopyAsOrg,
    CopyAsMarkdown,
    CopyAsJson,
    /// In a text buffer: open the org/Markdown table at point as a grid.
    OrgTableToGrid,
    // Formatting by hand (`src/grid/cellfmt.rs`)
    /// Make the selected cells bold, or not bold when the cursor's is.
    ToggleBold,
    /// Give the selected cells the text color after the cursor cell's.
    CycleTextColor,
    /// Give the selected cells the fill color after the cursor cell's.
    CycleFillColor,
    /// Remove bold, colors and number formats from the selected cells.
    ClearFormat,
    /// Number formats (display only — the stored values are untouched).
    FormatGeneral,
    FormatNumber,
    FormatCurrency,
    FormatPercent,
    FormatDate,
    FormatText,
    /// One decimal place more / fewer in the selection's number format.
    IncreaseDecimals,
    DecreaseDecimals,
    // Helix multi-cursor (`src/editor/grid_cursors.rs`)
    /// `C`: a cursor on the cell below each cursor; the new one below the
    /// primary becomes the primary (helix's copy-selection-on-next-line).
    AddCursorBelow,
    /// `M-C`: the same, above.
    AddCursorAbove,
    /// `,`: drop every cursor but the primary.
    KeepPrimaryCursor,
    /// `M-,`: drop the primary cursor; the next one becomes the primary.
    RemovePrimaryCursor,
    /// `)` / `(`: make the next / previous cursor (reading order) primary.
    RotateCursorsForward,
    RotateCursorsBackward,
    /// `s`: put a cursor on every cell of the selection matching a regex.
    SelectMatching,
}

/// One run of a grid command: the command and the prefix typed before the
/// key that ran it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridInvocation {
    pub cmd: GridCmd,
    /// The typed count (0 = none): count-aware commands read it, others
    /// repeat.
    pub count: usize,
    /// A `C-u` typed before a command whose prefix is a flag (see
    /// [`GridCmd::prefix_is_flag`]).
    pub flag: bool,
}

impl From<GridCmd> for GridInvocation {
    /// The command with no prefix.
    fn from(cmd: GridCmd) -> Self {
        Self {
            cmd,
            count: 0,
            flag: false,
        }
    }
}

/// Steel name → command, for builtin registration.
pub const GRID_COMMANDS: &[(&str, GridCmd)] = {
    use Dir::*;
    use GridCmd::*;
    &[
        ("grid-left", Move(Left)),
        ("grid-right", Move(Right)),
        ("grid-up", Move(Up)),
        ("grid-down", Move(Down)),
        ("grid-next-cell", NextCell),
        ("grid-previous-cell", PreviousCell),
        ("grid-edge-left", Edge(Left)),
        ("grid-edge-right", Edge(Right)),
        ("grid-edge-up", Edge(Up)),
        ("grid-edge-down", Edge(Down)),
        ("grid-row-start", RowStart),
        ("grid-row-first-nonempty", RowFirstNonEmpty),
        ("grid-row-end", RowEnd),
        ("grid-goto-first-row", GotoFirstRow),
        ("grid-goto-last-row", GotoLastRow),
        ("grid-goto-column", GotoColumn),
        ("grid-goto-first-cell", GotoFirstCell),
        ("grid-goto-last-cell", GotoLastCell),
        ("grid-page-down", PageDown),
        ("grid-page-up", PageUp),
        ("grid-half-page-down", HalfPageDown),
        ("grid-half-page-up", HalfPageUp),
        ("grid-extend-left", Extend(Left)),
        ("grid-extend-right", Extend(Right)),
        ("grid-extend-up", Extend(Up)),
        ("grid-extend-down", Extend(Down)),
        ("grid-select-cells", SelectCells),
        ("grid-select-rows", SelectRows),
        ("grid-select-columns", SelectColumns),
        ("grid-select-all", SelectAll),
        ("grid-select-cancel", SelectCancel),
        ("grid-edit-start", EditStart),
        ("grid-edit-end", EditEnd),
        ("grid-edit-replace", EditReplace),
        ("grid-edit-popup", EditPopup),
        ("grid-clear", Clear),
        ("grid-cut", Cut),
        ("grid-yank", Yank),
        ("grid-paste", Paste),
        ("grid-delete-row", DeleteRow),
        ("grid-delete-column", DeleteColumn),
        ("grid-insert-row-below", InsertRowBelow),
        ("grid-insert-row-above", InsertRowAbove),
        ("grid-insert-column-right", InsertColumnRight),
        ("grid-insert-column-left", InsertColumnLeft),
        ("grid-sort-ascending", SortAscending),
        ("grid-sort-descending", SortDescending),
        ("grid-undo", Undo),
        ("grid-redo", Redo),
        ("grid-repeat", Repeat),
        ("grid-widen-column", WidenColumn),
        ("grid-narrow-column", NarrowColumn),
        ("grid-autofit-column", AutofitColumn),
        ("grid-autofit-column-full", AutofitColumnFull),
        ("grid-taller-row", TallerRow),
        ("grid-shorter-row", ShorterRow),
        ("grid-zoom-in", ZoomIn),
        ("grid-zoom-out", ZoomOut),
        ("grid-zoom-reset", ZoomReset),
        ("grid-goto", Goto),
        ("grid-search-forward", SearchForward),
        ("grid-search-next", SearchNext),
        ("grid-search-previous", SearchPrevious),
        ("grid-toggle-header", ToggleHeader),
        ("grid-toggle-freeze", ToggleFreeze),
        ("grid-formula-bar", FormulaBar),
        ("grid-view-as-text", ViewAsText),
        ("grid-next-problem", NextProblem),
        ("grid-mode", GridMode),
        ("grid-recalculate", Recalculate),
        ("grid-show-formulas", ShowFormulas),
        ("grid-trace-precedents", TracePrecedents),
        ("grid-trace-dependents", TraceDependents),
        ("grid-toggle-view", ToggleView),
        ("grid-cell-edit-commit", CellPopupCommit),
        ("grid-cell-edit-abort", CellPopupAbort),
        ("grid-exchange-point-and-mark", ExchangeAnchor),
        ("grid-recenter", Recenter),
        ("grid-search-backward", SearchBackward),
        ("grid-jump-to-column", JumpToColumn),
        ("grid-kill-to-row-end", KillToRowEnd),
        ("grid-upcase", Upcase),
        ("grid-downcase", Downcase),
        ("grid-capitalize", Capitalize),
        ("grid-copy-down", CopyDown),
        ("grid-yank-pop", YankPop),
        ("grid-complete-cell", CompleteCell),
        ("grid-move-row-up", MoveLine(Up)),
        ("grid-move-row-down", MoveLine(Down)),
        ("grid-move-column-left", MoveLine(Left)),
        ("grid-move-column-right", MoveLine(Right)),
        ("grid-hide-column", HideColumn),
        ("grid-show-columns", ShowColumns),
        ("grid-narrow-to-selection", NarrowToSelection),
        ("grid-filter", Filter),
        ("grid-filter-by-value", FilterByValue),
        ("grid-widen", Widen),
        ("grid-copy-as-org", CopyAsOrg),
        ("grid-copy-as-markdown", CopyAsMarkdown),
        ("grid-copy-as-json", CopyAsJson),
        ("org-table-to-grid", OrgTableToGrid),
        ("grid-toggle-bold", ToggleBold),
        ("grid-cycle-text-color", CycleTextColor),
        ("grid-cycle-fill-color", CycleFillColor),
        ("grid-clear-format", ClearFormat),
        ("grid-format-general", FormatGeneral),
        ("grid-format-number", FormatNumber),
        ("grid-format-currency", FormatCurrency),
        ("grid-format-percent", FormatPercent),
        ("grid-format-date", FormatDate),
        ("grid-format-text", FormatText),
        ("grid-increase-decimals", IncreaseDecimals),
        ("grid-decrease-decimals", DecreaseDecimals),
        ("grid-add-cursor-below", AddCursorBelow),
        ("grid-add-cursor-above", AddCursorAbove),
        ("grid-keep-primary-cursor", KeepPrimaryCursor),
        ("grid-remove-primary-cursor", RemovePrimaryCursor),
        ("grid-rotate-cursors-forward", RotateCursorsForward),
        ("grid-rotate-cursors-backward", RotateCursorsBackward),
        ("grid-select-matching", SelectMatching),
    ]
};

impl GridCmd {
    /// The command a Steel name (`grid-left`) names.
    pub fn from_name(name: &str) -> Option<GridCmd> {
        static BY_NAME: OnceLock<HashMap<&'static str, GridCmd>> = OnceLock::new();
        BY_NAME
            .get_or_init(|| GRID_COMMANDS.iter().copied().collect())
            .get(name)
            .copied()
    }

    /// Whether this command changes cell data or formatting (so `.` repeats
    /// it, and it is refused while the file is still loading).
    pub fn edits(self) -> bool {
        use GridCmd::*;
        matches!(
            self,
            Clear
                | Cut
                | Paste
                | DeleteRow
                | DeleteColumn
                | InsertRowBelow
                | InsertRowAbove
                | InsertColumnRight
                | InsertColumnLeft
                | SortAscending
                | SortDescending
                | KillToRowEnd
                | Upcase
                | Downcase
                | Capitalize
                | CopyDown
                | YankPop
                | MoveLine(_)
                | ToggleBold
                | CycleTextColor
                | CycleFillColor
                | ClearFormat
                | FormatGeneral
                | FormatNumber
                | FormatCurrency
                | FormatPercent
                | FormatDate
                | FormatText
                | IncreaseDecimals
                | DecreaseDecimals
        )
    }

    /// Whether `C-u` is a flag for this command (changes what it does)
    /// rather than a repeat count: `C-u C-d` clears whole rows, `C-u C-c C-s`
    /// asks how to sort, `C-u S-RET` copies without incrementing.
    pub fn prefix_is_flag(self) -> bool {
        use GridCmd::*;
        matches!(self, Clear | SortAscending | SortDescending | CopyDown)
    }

    /// Whether helix's extra cursors survive this command. Motions move them
    /// all, edits spread to them, and view tweaks leave them be; anything
    /// else (a jump, a structural edit, undo, Escape) keeps only the primary.
    pub fn keeps_cursors(self) -> bool {
        use GridCmd::*;
        matches!(
            self,
            Move(_)
                | EditStart
                | EditEnd
                | EditReplace
                | Clear
                | Cut
                | Yank
                | Repeat
                | WidenColumn
                | NarrowColumn
                | AutofitColumn
                | AutofitColumnFull
                | TallerRow
                | ShorterRow
                | ZoomIn
                | ZoomOut
                | ZoomReset
                | FormulaBar
                | ShowFormulas
                | Recenter
                | AddCursorBelow
                | AddCursorAbove
                | RemovePrimaryCursor
                | RotateCursorsForward
                | RotateCursorsBackward
                | SelectMatching
        )
    }
}

/// Editor-wide grid settings (`grid-default-column-width`, …).
#[derive(Debug, Clone)]
pub enum GridOption {
    /// Width (100% px) of columns without their own width, for grids opened
    /// from now on.
    DefaultColumnWidth(f64),
    /// Widest an auto-fit (`=`, double-click a border) makes a column.
    MaxAutofitWidth(f64),
    /// Whether row 1 is a header: `None` sniffs it from the data.
    AutoHeader(Option<bool>),
    /// Drawn at the end of text cut off by its cell ("" draws nothing).
    TruncateIndicator(String),
    /// Seconds a Scheme function called from a formula may run.
    FunctionTimeout(f64),
}

/// A grid operation with arguments, queued by the Scheme API (`grid-set-cell`,
/// `grid-format-column`, …). Data edits from one Scheme command batch undo as
/// one step. Column/row arguments are 0-based; `None` marks an argument
/// Scheme passed that isn't a valid index, reported instead of applied.
#[derive(Debug, Clone)]
pub enum GridOp {
    /// `(row, col, text)` triples, applied as one edit.
    SetCells(Vec<(Option<usize>, Option<usize>, String)>),
    InsertRows {
        at: Option<usize>,
        n: usize,
    },
    DeleteRows {
        at: Option<usize>,
        n: usize,
    },
    InsertCols {
        at: Option<usize>,
        n: usize,
    },
    DeleteCols {
        at: Option<usize>,
        n: usize,
    },
    Sort {
        col: Option<usize>,
        descending: bool,
        kind: crate::grid::model::SortKind,
    },
    MoveTo {
        row: Option<usize>,
        col: Option<usize>,
    },
    Select {
        r0: Option<usize>,
        c0: Option<usize>,
        r1: Option<usize>,
        c1: Option<usize>,
    },
    ColumnWidth {
        col: Option<usize>,
        px: f64,
    },
    RowHeight {
        row: Option<usize>,
        px: f64,
    },
    Freeze(usize),
    SetHeader(bool),
    /// Zoom in percent (10–400).
    Zoom(f64),
    /// Call Scheme fn `callback` with the cells of `range` (`"A1:C10"`) as a
    /// list of rows, each a list of strings.
    ReadRange {
        range: String,
        callback: String,
    },
    ColumnFace {
        col: Option<usize>,
        face: String,
    },
    FormatColumn {
        col: Option<usize>,
        format: String,
    },
    /// `col: None` applies to every column. A `row` rule colors the whole
    /// row when `col`'s value passes.
    HighlightRule {
        col: Option<usize>,
        op: String,
        value: String,
        face: String,
        row: bool,
    },
    ClearStyles,
    CellFace {
        row: Option<usize>,
        col: Option<usize>,
        face: String,
    },
    ClearCellFaces,
    /// Hand-set formatting of one cell (`grid-set-cell-bold`, …): each
    /// `Some` field is changed; a color of `Some(None)` removes it.
    CellFormat {
        row: Option<usize>,
        col: Option<usize>,
        bold: Option<bool>,
        text: Option<Option<u8>>,
        fill: Option<Option<u8>>,
        /// A number format code or preset name (`"General"` removes it).
        num: Option<String>,
        /// The color name given, when it isn't one of the palette's.
        bad_color: Option<String>,
    },
    ClearCellFormats,
    /// Show the workbook's sheet with this name (`grid-open-sheet`).
    OpenSheet(String),
    /// Add an empty sheet with this name (`""`: the next SheetN).
    AddSheet(String),
    /// Rename sheet `from` (`""`: the one shown) to `to`.
    RenameSheet {
        from: String,
        to: String,
    },
    /// Delete the sheet with this name (`""`: the one shown).
    DeleteSheet(String),
    /// Move the sheet shown this many places (negative: left).
    MoveSheet(isize),
    /// Give the selection (or the cursor cell) this number format: a code
    /// or a preset name (`grid-set-number-format`).
    NumberFormat(String),
    /// Zebra-stripe rows with this face's background (`None` turns it off).
    Stripe(Option<String>),
    HeaderFace(String),
    Align {
        col: Option<usize>,
        align: String,
    },
    HideColumn {
        col: Option<usize>,
        hidden: bool,
    },
    /// Show only rows whose `col` value passes `(op value)`; filters stack.
    Filter {
        col: Option<usize>,
        op: String,
        value: String,
    },
    /// Values offered by `grid-complete-cell` for `col` (empty: none).
    Choices {
        col: Option<usize>,
        values: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_and_are_unique() {
        assert_eq!(
            GridCmd::from_name("grid-left"),
            Some(GridCmd::Move(Dir::Left))
        );
        assert_eq!(
            GridCmd::from_name("grid-move-column-right"),
            Some(GridCmd::MoveLine(Dir::Right))
        );
        assert_eq!(GridCmd::from_name("grid-nope"), None);
        let mut names: Vec<&str> = GRID_COMMANDS.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), GRID_COMMANDS.len(), "a name listed twice");
        for &(name, cmd) in GRID_COMMANDS {
            assert_eq!(GridCmd::from_name(name), Some(cmd), "{name}");
        }
    }

    #[test]
    fn directions() {
        assert_eq!(Dir::Left.delta(), (0, -1));
        assert_eq!(Dir::Down.delta(), (1, 0));
        assert_eq!(Dir::Up.axis(), Axis::Rows);
        assert!(Dir::Right.forward() && !Dir::Up.forward());
    }
}
