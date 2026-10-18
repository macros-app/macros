//! Grid buffer data model: delimited text (CSV/TSV/…) as a spreadsheet.
//!
//! This is the pure model layer — no UI. See `docs/plans/grid-buffer.md`.
//!
//! - [`cell`]: [`CellRef`], the packed 8-byte cell (Empty / Span into the
//!   source bytes / Inline ≤7 bytes / Interned id).
//! - [`intern`]: [`StringPool`], one stored copy per distinct string.
//! - [`sheet`]: [`Sheet`], column-major chunked storage plus per-row source
//!   metadata that makes byte-exact saves possible.
//! - [`model`]: [`Workbook`], the owner of sheets and the pool, and the editing
//!   API with undo/redo and saved-state tracking.
//! - [`infer`]: column type inference ([`ColType`]).
//! - [`dialect`]: encoding / delimiter / quoting / header sniffing.
//! - [`parse`]: the loader. [`write`]: the saver.
//! - [`cellfmt`]: bold, colors, number formats and wrap set by hand on cells.
//! - [`wrap`]: Wrap Text line breaking and row heights.
//! - [`scrollbar`]: scrollbar thumb geometry over the infinite sheet.
//! - [`series`]: continuing a series down a column (copy-down / fill).
//! - [`dedup`]: Remove duplicates — which rows repeat an earlier one.
//! - [`export`]: cells as org / Markdown / JSON / CSV, and org tables back.
//! - [`replace`]: find & replace over cell text.
//! - [`sidecar`]: per-file view settings saved beside the config.
//!
//! The core invariant: a row loaded from the file and never edited ("clean")
//! is written back by copying its original bytes, so saving an untouched file
//! reproduces it exactly and saving an edited one changes only edited records.

pub mod cell;
pub mod cellfmt;
pub mod cmd;
pub mod dedup;
pub mod dialect;
pub mod export;
pub mod infer;
pub mod intern;
pub mod model;
pub mod parse;
#[cfg(test)]
mod regression_tests;
pub mod replace;
pub mod scrollbar;
pub mod series;
pub mod sheet;
pub mod sidecar;
pub mod style;
#[cfg(test)]
mod testgen;
pub mod view;
pub mod wrap;
pub mod write;

/// A fast content fingerprint (auto-revert, sidecar names; not a security
/// hash).
pub fn bytes_hash(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write(bytes);
    h.write_usize(bytes.len());
    h.finish()
}
