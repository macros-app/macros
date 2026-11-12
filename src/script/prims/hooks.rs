//! Hooks, completion-at-point functions and timers.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared {
        queue, snapshot, ..
    } = ctx;
    reg.section(Section::HooksTimers);
    register_binary(reg, queue, "add-hook", EditorCommand::AddHook);
    register_binary(reg, queue, "remove-hook", EditorCommand::RemoveHook);
    // Fire a named hook from Scheme (the editor runs its own lifecycle hooks;
    // Scheme-side triggers — e.g. bookmarks.scm — go through this).
    register_unary(reg, queue, "run-hook", EditorCommand::RunHook);
    // The names of every hook with registered functions, sorted (the live
    // registry is the editor's string-keyed hook table — describe-variable
    // offers each as a C-h v target).
    {
        let shared = snapshot.clone();
        reg.register_fn("hook-names", move || -> Vec<String> {
            let mut names: Vec<String> = {
                let snap = shared.load();
                snap.hooks.iter().map(|(k, _)| k.clone()).collect()
            };
            names.sort();
            names
        });
    }
    // Names of the functions registered on a hook key, in registration order.
    // Empty for an unknown hook.
    {
        let shared = snapshot.clone();
        reg.register_fn(
            "hook-registered-functions",
            move |hook: String| -> Vec<String> {
                let snap = shared.load();
                snap.hooks
                    .iter()
                    .find(|(k, _)| *k == hook)
                    .map(|(_, fns)| fns.clone())
                    .unwrap_or_default()
            },
        );
    }
    register_unary(
        reg,
        queue,
        "add-completion-at-point-function",
        EditorCommand::AddCapf,
    );
    register_binary(reg, queue, "run-with-timer", |ms, callback| {
        EditorCommand::RunWithTimer { ms, callback }
    });
}
