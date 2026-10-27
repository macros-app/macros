//! Primitives that mutate the current buffer: insertion, goto, regions and
//! selections, the clipboard, snippets and undo grouping.

use steel::rvals::SteelVal;

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { queue, .. } = ctx;
    reg.section(Section::Editing);
    // Commands carrying a payload.
    register_unary(reg, queue, "insert", EditorCommand::Insert);
    register_unary(reg, queue, "clipboard-set", EditorCommand::ClipboardSet);
    // `(reindent-lines first last)` — Vim `=` over 0-based lines.
    register_binary(reg, queue, "reindent-lines", EditorCommand::ReindentLines);
    // `(lisp-indent-rule "name" "spec")` — how Lisp forms headed by `name`
    // indent (Emacs `lisp-indent-function`): "body" (definer: body one step),
    // "function" (align under the first argument), "clauses" (cond-style), or
    // a count like "1" of distinguished arguments before the body.
    register_binary(
        reg,
        queue,
        "lisp-indent-rule",
        |name: String, spec: String| EditorCommand::SetLispIndent { name, spec },
    );
    register_unary(reg, queue, "snippet-expand", EditorCommand::SnippetExpand);
    register_unary(reg, queue, "goto-char", |n: isize| {
        EditorCommand::GotoChar(scheme_pos(n))
    });
    register_binary(reg, queue, "delete-region", |start: isize, end: isize| {
        EditorCommand::DeleteRegion {
            start: scheme_pos(start),
            end: scheme_pos(end),
        }
    });
    register_binary(
        reg,
        queue,
        "kill-region-between",
        |start: isize, end: isize| EditorCommand::KillRegionBetween {
            start: scheme_pos(start),
            end: scheme_pos(end),
        },
    );
    // `(buffer-narrow start end)`: restrict the buffer to `[start, end)` — the
    // primitive behind `narrow-to-region` (scheme/narrow.scm). `widen` undoes it.
    register_binary(reg, queue, "buffer-narrow", |start: isize, end: isize| {
        EditorCommand::NarrowToRegion {
            start: scheme_pos(start),
            end: scheme_pos(end),
        }
    });
    register_binary(reg, queue, "set-region", |start: isize, end: isize| {
        EditorCommand::SetRegion {
            start: scheme_pos(start),
            end: scheme_pos(end),
        }
    });
    {
        // `(set-selections '(a0 h0 a1 h1 …))` — replace the whole selection set
        // from a flat list of alternating anchor/head byte offsets (the first
        // pair is the primary). The Scheme reader returns selections in the same
        // flat shape (see the `selections` read), so a motion is read → map →
        // set-selections. An odd trailing element is ignored.
        let q = queue.clone();
        reg.register_fn("set-selections", move |flat: SteelVal| {
            let v = steel_usize_list(&flat);
            let pairs = v.as_chunks::<2>().0.iter().map(|c| (c[0], c[1])).collect();
            q.push(EditorCommand::SetSelections(pairs));
        });
    }
    register_ternary(
        reg,
        queue,
        "replace-region",
        |start: isize, end: isize, text| EditorCommand::ReplaceRegion {
            start: scheme_pos(start),
            end: scheme_pos(end),
            text,
        },
    );
    register_binary(reg, queue, "insert-at", |pos: isize, text| {
        EditorCommand::InsertAt {
            pos: scheme_pos(pos),
            text,
        }
    });
    register_nullary(reg, queue, "undo-boundary", EditorCommand::UndoBoundary);
    // `(undo-amalgamate-begin)` … `(undo-amalgamate-end)` — fold every edit
    // made between them, across any number of commands, into ONE undo step.
    register_nullary(
        reg,
        queue,
        "undo-amalgamate-begin",
        EditorCommand::UndoAmalgamateBegin,
    );
    register_nullary(
        reg,
        queue,
        "undo-amalgamate-end",
        EditorCommand::UndoAmalgamateEnd,
    );
    register_unary(reg, queue, "undo-tree-dump", EditorCommand::UndoTreeDump);
    register_binary(reg, queue, "append-to-buffer", |name, text| {
        EditorCommand::AppendToBuffer { name, text }
    });
}
