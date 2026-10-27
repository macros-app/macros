//! Subprocess primitives (sync and streaming), the process environment, and
//! embedded terminals.

use crate::util::HideConsole;

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { queue, .. } = ctx;
    let async_ids = &ctx.ids;
    reg.section(Section::Processes);
    // `(executable-find name)` → the full path of `name` on `PATH`, or "" when
    // it isn't installed (Emacs `executable-find`).
    reg.register_fn("executable-find", |name: String| -> String {
        crate::util::executable_find(&name)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    // `(call-process program args)` — run a subprocess, return stdout. Blocks on
    // the engine thread (never the UI), the same model as `list-directory`.
    reg.register_fn(
        "call-process",
        move |program: String, StringList(args): StringList| -> String {
            let program = crate::util::expand_tilde(&program);
            let mut cmd = std::process::Command::new(&program);
            cmd.args(&args).hide_console();
            crate::process::current().apply(&mut cmd);
            cmd.output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        },
    );
    // `(call-process-full program args input)` → `(exit-code stdout stderr)`,
    // writing `input` to stdin (empty = none). Used for `git apply -` (hunk
    // staging) and to surface git errors (stderr + non-zero code).
    //
    // Every spawner (call-process*, make-process*, make-terminal, dap-start)
    // also applies the `with-process-env` overrides in effect — see
    // `crate::process`.
    reg.register_fn(
        "call-process-full",
        move |program: String, StringList(args): StringList, input: String| -> Vec<String> {
            let env = crate::process::current();
            crate::process::call_full(&program, &args, input, &env)
        },
    );
    // `(shell-command-argv dir command merge-stderr)` → `(program arg …)` that
    // runs `command` through the user's shell in `dir` (Emacs
    // `shell-file-name` + `shell-command-switch`): `$SHELL -c`, or `cmd /C` on
    // Windows. `merge-stderr` folds stderr into stdout (`2>&1`), for
    // `make-process`, which streams stdout only. Feed the list to
    // `call-process-full` / `make-process` (car = program, cdr = args).
    reg.register_fn(
        "shell-command-argv",
        |dir: String, command: String, merge: bool| -> Vec<String> {
            shell_argv(
                &dir,
                &command,
                merge,
                crate::process::getenv("SHELL").as_deref(),
            )
        },
    );
    // `(call-process-env program args input env)` — `call-process-full` with
    // extra environment for the child only. `env` is a list of "KEY=VALUE"
    // strings layered over the inherited environment; a bare "KEY" removes
    // the variable. The way to hand a subprocess a secret (PGPASSWORD, …)
    // without putting it in argv, where `ps` shows it to every local user.
    reg.register_fn(
        "call-process-env",
        move |program: String,
              StringList(args): StringList,
              input: String,
              StringList(env): StringList|
              -> Vec<String> {
            let env = crate::process::current_then(&env);
            crate::process::call_full(&program, &args, input, &env)
        },
    );
    // `(process-env-push! env)` / `(process-env-pop!)` — the primitives under
    // core.scm's `(with-process-env env thunk)`: every process spawned while
    // an extent is open gets `env` (same list shape as `call-process-env`).
    // The engine clears the stack before each job, so an extent left by an
    // error never outlives it.
    reg.register_fn("process-env-push!", |StringList(env): StringList| {
        crate::process::push_dynamic(crate::process::EnvOverrides::parse(&env));
    });
    reg.register_fn("process-env-pop!", crate::process::pop_dynamic);
    // `(getenv name)` → the variable's value, or "" if unset. Sees `setenv`
    // and any open `with-process-env` extent — what a child spawned now gets.
    reg.register_fn("getenv", |name: String| -> String {
        crate::process::getenv(&name).unwrap_or_default()
    });
    // `(setenv name value)` → set an env var for every subsequently spawned
    // subprocess (LSP servers, call-process, make-process, terminals). Lets a
    // config extend PATH or pin a toolchain, e.g.
    //   (setenv "PATH" (string-append (getenv "PATH") ":" ...))
    // Recorded in an overlay applied at spawn time, never via
    // `std::env::set_var`: other threads read the environment concurrently.
    reg.register_fn("setenv", |name: String, value: String| {
        crate::process::set_global(&name, &value);
    });
    {
        // `(make-process program args on-output on-exit)` → a process id (used by
        // process-send-string / kill-process). on-exit "" = none; it receives the
        // exit code as a string.
        // `(make-process-env program args on-output on-exit env)` — the same,
        // with `env` ("KEY=VALUE" / bare "KEY" = unset) for the child only (see
        // `call-process-env`). Both capture the `with-process-env` overrides
        // NOW: the spawn itself happens later, on the UI thread.
        let make = |ids: IdGen, q: Queue| {
            move |program: String,
                  StringList(args): StringList,
                  on_output: Callback,
                  on_exit: Callback,
                  env: crate::process::EnvOverrides|
                  -> usize {
                let id = ids.next();
                q.push(EditorCommand::MakeProcess(Box::new(MakeProcessArgs {
                    id,
                    program,
                    args,
                    env,
                    on_output: on_output.0,
                    on_exit: Some(on_exit.0).filter(|s| !s.is_empty()),
                })));
                id as usize
            }
        };
        let spawn = make(async_ids.clone(), queue.clone());
        reg.register_fn(
            "make-process",
            move |program: String, args: StringList, on_output: Callback, on_exit: Callback| {
                spawn(program, args, on_output, on_exit, crate::process::current())
            },
        );
        let spawn = make(async_ids.clone(), queue.clone());
        reg.register_fn(
            "make-process-env",
            move |program: String,
                  args: StringList,
                  on_output: Callback,
                  on_exit: Callback,
                  StringList(env): StringList| {
                let env = crate::process::current_then(&env);
                spawn(program, args, on_output, on_exit, env)
            },
        );
    }
    register_binary(reg, queue, "process-send-string", |id, text| {
        EditorCommand::SendProcess { id, text }
    });
    register_unary(reg, queue, "kill-process", |id| {
        EditorCommand::KillProcess { id }
    });
    // `(set-process-buffer id name)`: killing buffer `name` terminates process
    // `id` (its whole process group), like Emacs's process buffer.
    register_binary(reg, queue, "set-process-buffer", |id, name| {
        EditorCommand::SetProcessBuffer { id, name }
    });

    reg.section(Section::Terminal);
    {
        // `(make-terminal name program args directory)` → a terminal id. Empty
        // `program` runs the user's login shell; empty `directory` uses the
        // active buffer's directory. The terminal is shown in a `term-mode`
        // buffer named `name`; switching to that buffer later reuses it.
        let q = queue.clone();
        let ids = async_ids.clone();
        reg.register_fn(
            "make-terminal",
            move |name: String,
                  program: String,
                  StringList(args): StringList,
                  directory: String|
                  -> usize {
                let id = ids.next();
                q.push(EditorCommand::MakeTerminal(Box::new(MakeTerminalArgs {
                    id,
                    name,
                    program,
                    args,
                    directory,
                    env: crate::process::current(),
                })));
                id as usize
            },
        );
    }
}

/// The argv that runs `command` through the user's shell in `dir` (see
/// `shell-command-argv`). On Unix the user's `$SHELL` (else `/bin/sh`) gets
/// the command verbatim via `-c`; a fixed `/bin/sh` wrapper does the `cd` and
/// the stderr merge, taking `dir`, the shell and the command as positional
/// parameters, so nothing is spliced into shell source and fish/nu users'
/// syntax still works. An empty `dir` stays in the inherited cwd.
fn shell_argv(dir: &str, command: &str, merge: bool, shell: Option<&str>) -> Vec<String> {
    let redirect = if merge { " 2>&1" } else { "" };
    if cfg!(windows) {
        let cd = if dir.is_empty() {
            String::new()
        } else {
            format!("cd /d \"{dir}\" && ")
        };
        let comspec = crate::process::getenv("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        return vec![
            comspec,
            "/D".into(),
            "/C".into(),
            format!("{cd}{command}{redirect}"),
        ];
    }
    let shell = shell.filter(|s| !s.is_empty()).unwrap_or("/bin/sh");
    vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "if [ -n \"$1\" ]; then cd -- \"$1\" || exit 1; fi; exec \"$2\" -c \"$3\"{redirect}"
        ),
        "sh".into(),
        dir.into(),
        shell.into(),
        command.into(),
    ]
}

#[cfg(all(test, unix))]
mod shell_argv_tests {
    use super::shell_argv;

    fn run(dir: &str, command: &str, merge: bool, shell: Option<&str>) -> Vec<String> {
        let argv = shell_argv(dir, command, merge, shell);
        crate::process::call_full(
            &argv[0],
            &argv[1..],
            String::new(),
            &crate::process::EnvOverrides::default(),
        )
    }

    #[test]
    fn runs_in_dir_through_the_given_shell() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let dir = dir.to_str().unwrap();
        let r = run(dir, "pwd -P; echo \"$0\"", false, Some("/bin/sh"));
        assert_eq!(r[0], "0");
        assert_eq!(r[1], format!("{dir}\n/bin/sh\n"));
    }

    #[test]
    fn quotes_in_dir_and_command_are_data() {
        let base = std::env::temp_dir().join("macros shell'arg \"test\"");
        std::fs::create_dir_all(&base).unwrap();
        let dir = base.canonicalize().unwrap();
        let r = run(
            dir.to_str().unwrap(),
            "echo 'it''s' \"$(pwd -P)\"",
            false,
            None,
        );
        assert_eq!(r[1], format!("its {}\n", dir.display()));
    }

    #[test]
    fn stderr_and_exit_status() {
        let r = run("", "echo out; echo err >&2; exit 3", false, Some(""));
        assert_eq!(
            (r[0].as_str(), r[1].as_str(), r[2].as_str()),
            ("3", "out\n", "err\n")
        );
        let r = run("", "echo out; echo err >&2", true, None);
        assert_eq!((r[1].as_str(), r[2].as_str()), ("out\nerr\n", ""));
        let r = run("/nonexistent-macros-dir", "echo hi", false, None);
        assert_eq!(r[0], "1");
        assert!(!r[2].is_empty());
    }
}
