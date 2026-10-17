//! Mode line and title bar primitives.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { queue, .. } = ctx;
    reg.section(Section::Ui);
    register_unary(
        reg,
        queue,
        "set-modeline-project",
        EditorCommand::SetModelineProject,
    );
    register_unary(
        reg,
        queue,
        "set-modeline-git",
        EditorCommand::SetModelineGit,
    );
    // `(set-mode-line-slot key value)` → write/clear a named mode-line register
    // (empty value clears). The general form behind project/git indicators.
    register_binary(
        reg,
        queue,
        "set-mode-line-slot",
        EditorCommand::SetModeLineSlot,
    );
    {
        // `(set-mode-line-format segs)` — replace the mode-line layout. `segs` is
        // a list of segment tokens (bare strings = literals, or `ml-field`/
        // `ml-slot`/`ml-text` tab-encoded tokens). Defined from scheme/modeline.scm.
        let q = queue.clone();
        reg.register_fn(
            "set-mode-line-format",
            move |StringList(segs): StringList| {
                q.push(EditorCommand::SetModeLineFormat(segs));
            },
        );
    }
    {
        // `(set-titlebar-menu title labels commands active section callback)` —
        // publish the title-bar menu named `title`. `labels` and `commands` are
        // parallel lists; clicking a row calls its command (a function name)
        // with no arguments, or — when that entry is "" — `(callback label)`.
        // `active` is checkmarked ("" for none);
        // `section` heads the rows. An empty list hides the menu. Driven from
        // scheme/titlebar-menu.scm, which is the API to use.
        let q = queue.clone();
        reg.register_fn(
            "set-titlebar-menu",
            move |title: String,
                  StringList(items): StringList,
                  StringList(commands): StringList,
                  active: String,
                  section: String,
                  callback: Callback| {
                q.push(EditorCommand::SetTitlebarMenu(Box::new(
                    crate::script::TitlebarMenuArgs {
                        title,
                        items,
                        commands,
                        active,
                        section,
                        callback: callback.0,
                    },
                )));
            },
        );
    }
}
