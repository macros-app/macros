//! The hex buffer primitives (see `scheme/hex.scm` for the overview): reads
//! from the published [`HexSnapshot`](crate::script::HexSnapshot), and queued
//! [`HexOp`](crate::hexview::cmd::HexOp)s for the operations that take an
//! argument.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    use crate::hexview::cmd::HexOp;
    let EngineShared {
        queue, snapshot, ..
    } = ctx;
    reg.section(Section::Hex);
    register_snapshot_read(reg, snapshot, "hex-buffer?", |s| s.hex.is_some());
    register_snapshot_read(reg, snapshot, "hex-offset", |s| {
        s.hex.as_ref().map_or(0, |h| h.offset)
    });
    register_snapshot_read(reg, snapshot, "hex-size", |s| {
        s.hex.as_ref().map_or(0, |h| h.size)
    });
    register_snapshot_read(reg, snapshot, "hex-selection", |s| {
        s.hex
            .as_ref()
            .and_then(|h| h.selection)
            .map_or_else(Vec::new, |(a, b)| vec![a, b])
    });
    // The byte at point, or #f at the append slot / outside a hex buffer.
    register_snapshot_read(reg, snapshot, "hex-byte", |s| {
        s.hex.as_ref().and_then(|h| h.byte).map(|b| b as usize)
    });
    register_unary(reg, queue, "hex-goto-offset", |s: String| {
        EditorCommand::HexOp(HexOp::Goto(s))
    });
    register_unary(reg, queue, "hex-search-forward-for", |s: String| {
        EditorCommand::HexOp(HexOp::Search {
            pattern: s,
            forward: true,
        })
    });
    register_unary(reg, queue, "hex-search-backward-for", |s: String| {
        EditorCommand::HexOp(HexOp::Search {
            pattern: s,
            forward: false,
        })
    });
    register_unary(reg, queue, "hex-fill", |s: String| {
        EditorCommand::HexOp(HexOp::Fill(s))
    });
    register_unary(reg, queue, "hex-insert", |s: String| {
        EditorCommand::HexOp(HexOp::Insert(s))
    });
    register_unary(reg, queue, "hex-type", |s: String| {
        EditorCommand::HexOp(HexOp::Type(s))
    });
    register_unary(reg, queue, "hex-set-row-width", |n: usize| {
        EditorCommand::HexOp(HexOp::RowWidth(n))
    });
    register_unary(reg, queue, "hex-set-group-size", |n: usize| {
        EditorCommand::HexOp(HexOp::GroupSize(n))
    });
    register_unary(reg, queue, "hex-find-file", EditorCommand::HexFindFile);
}
