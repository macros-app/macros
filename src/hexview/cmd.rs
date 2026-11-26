//! The hex buffer's command vocabulary: every key binding in `scheme/hex.scm`
//! names one of these. They are registered as nullary builtins (see
//! `nullary_commands` in `src/script/registry.rs`), so a keypress applies
//! synchronously, like the grid's commands.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::CopyFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexCmd {
    // Motion
    Left,
    Right,
    Up,
    Down,
    /// Forward / back one group (`M-f` / `M-b`, `w` / `b`).
    NextGroup,
    PreviousGroup,
    RowStart,
    RowEnd,
    PageDown,
    PageUp,
    BufferStart,
    BufferEnd,
    /// Switch focus between the hex and ASCII columns.
    TogglePane,
    // Selection
    SetMark,
    /// Drop the selection (and, under evil, leave typing).
    Cancel,
    SelectAll,
    // Editing
    /// Evil `i` / `R`: arm typing.
    EditStart,
    /// Evil Escape: disarm typing.
    EditEnd,
    /// Overwrite ↔ insert (Emacs's Insert key).
    ToggleInsert,
    /// Delete the byte at point (or the selection).
    Delete,
    /// Delete the byte before point (or the selection).
    DeleteBackward,
    /// Insert a zero byte at point.
    InsertNull,
    Undo,
    Redo,
    /// Paste the clipboard at point (hex text becomes those bytes).
    Paste,
    Copy(CopyFormat),
    /// Cut the selection (as hex).
    Cut,
    // Search
    SearchNext,
    SearchPrevious,
    // View
    ToggleInspector,
    ToggleEndian,
    WiderRows,
    NarrowerRows,
    CycleGroup,
    Recenter,
    /// Show this buffer as text (only when the bytes are UTF-8).
    ViewAsText,
    /// `M-x hex-mode`: show the current text buffer as hex.
    HexMode,
}

/// Steel name → command, for builtin registration.
pub const HEX_COMMANDS: &[(&str, HexCmd)] = {
    use HexCmd::*;
    &[
        ("hex-backward-byte", Left),
        ("hex-forward-byte", Right),
        ("hex-previous-row", Up),
        ("hex-next-row", Down),
        ("hex-forward-group", NextGroup),
        ("hex-backward-group", PreviousGroup),
        ("hex-row-start", RowStart),
        ("hex-row-end", RowEnd),
        ("hex-page-down", PageDown),
        ("hex-page-up", PageUp),
        ("hex-beginning-of-buffer", BufferStart),
        ("hex-end-of-buffer", BufferEnd),
        ("hex-toggle-column", TogglePane),
        ("hex-set-mark", SetMark),
        ("hex-cancel", Cancel),
        ("hex-select-all", SelectAll),
        ("hex-edit-start", EditStart),
        ("hex-edit-end", EditEnd),
        ("hex-toggle-insert", ToggleInsert),
        ("hex-delete-byte", Delete),
        ("hex-delete-backward-byte", DeleteBackward),
        ("hex-insert-null", InsertNull),
        ("hex-undo", Undo),
        ("hex-redo", Redo),
        ("hex-paste", Paste),
        ("hex-copy", Copy(CopyFormat::Hex)),
        ("hex-copy-as-c-array", Copy(CopyFormat::CArray)),
        ("hex-copy-as-base64", Copy(CopyFormat::Base64)),
        ("hex-copy-as-text", Copy(CopyFormat::Text)),
        ("hex-cut", Cut),
        ("hex-search-next", SearchNext),
        ("hex-search-previous", SearchPrevious),
        ("hex-toggle-inspector", ToggleInspector),
        ("hex-toggle-endian", ToggleEndian),
        ("hex-wider-rows", WiderRows),
        ("hex-narrower-rows", NarrowerRows),
        ("hex-cycle-group", CycleGroup),
        ("hex-recenter", Recenter),
        ("hex-view-as-text", ViewAsText),
        ("hex-mode", HexMode),
    ]
};

impl HexCmd {
    pub fn from_name(name: &str) -> Option<Self> {
        static BY_NAME: OnceLock<HashMap<&'static str, HexCmd>> = OnceLock::new();
        BY_NAME
            .get_or_init(|| HEX_COMMANDS.iter().copied().collect())
            .get(name)
            .copied()
    }

    /// Changes the bytes (refused in a read-only buffer).
    pub fn edits(self) -> bool {
        use HexCmd::*;
        matches!(
            self,
            Delete | DeleteBackward | InsertNull | Undo | Redo | Paste | Cut
        )
    }
}

/// A hex operation with arguments, from the Scheme API (`hex.scm`'s prompts).
#[derive(Debug, Clone)]
pub enum HexOp {
    /// `(hex-goto-offset "0x1f0")`.
    Goto(String),
    /// `(hex-search "de ad ?? ef" forward?)`.
    Search { pattern: String, forward: bool },
    /// `(hex-fill "00")`: repeat the bytes over the selection.
    Fill(String),
    /// `(hex-insert "de ad")`: insert bytes at point.
    Insert(String),
    /// `(hex-type "de")`: type as if from the keyboard (keyboard macros
    /// replay typing through this).
    Type(String),
    /// `(hex-set-row-width 16)` (`0` fits the window).
    RowWidth(usize),
    /// `(hex-set-group-size 4)`.
    GroupSize(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for &(name, cmd) in HEX_COMMANDS {
            assert_eq!(HexCmd::from_name(name), Some(cmd), "{name}");
        }
        assert_eq!(HexCmd::from_name("hex-nope"), None);
    }
}
