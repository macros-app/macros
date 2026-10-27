//! shell-command.scm (M-! / M-| / M-&) and grep.scm (grep / rgrep / lgrep):
//! the commands run against a real engine with the bundled library loaded.

use ropey::Rope;

use super::*;

/// An engine with every startup script loaded and a snapshot of `text` with
/// point at 0, the region `region` and a `C-u` count of `prefix`.
fn engine_with(text: &str, region: Option<(usize, usize)>, prefix: usize) -> ScriptEngine {
    let (engine, _rx) = ScriptEngine::new();
    for src in crate::runtime::startup_scripts() {
        engine.eval(src).unwrap();
    }
    engine.set_snapshot(EditorSnapshot {
        name: "src.txt".into(),
        text: Rope::from_str(text),
        directory: std::env::temp_dir().to_string_lossy().into_owned(),
        region,
        prefix_count: prefix,
        ..Default::default()
    });
    engine
}

fn commands(engine: &ScriptEngine, q: &str) -> Vec<EditorCommand> {
    engine
        .eval(q)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .commands
}

fn messages(cmds: &[EditorCommand]) -> Vec<String> {
    cmds.iter()
        .filter_map(|c| match c {
            EditorCommand::Message(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

/// Run an interactive command, then answer its minibuffer prompt with `input`.
fn answer(engine: &ScriptEngine, command: &str, input: &str) -> Vec<EditorCommand> {
    let callback = match commands(engine, command).as_slice() {
        [EditorCommand::ReadString { callback, .. }] => callback.clone(),
        other => panic!("{command}: expected one ReadString, got {other:?}"),
    };
    commands(engine, &format!("({callback} {input:?})"))
}

fn rendered<'a>(cmds: &'a [EditorCommand], buffer: &str) -> Option<&'a str> {
    cmds.iter().find_map(|c| match c {
        EditorCommand::RenderBuffer { name, text, .. } if name == buffer => Some(text.as_str()),
        _ => None,
    })
}

#[cfg(unix)]
mod shell_command {
    use super::*;

    #[test]
    fn one_line_goes_to_the_echo_area() {
        let engine = engine_with("", None, 0);
        let cmds = answer(&engine, "(shell-command)", "echo hello");
        assert_eq!(messages(&cmds), vec!["hello"]);
    }

    #[test]
    fn runs_in_the_buffer_directory() {
        let engine = engine_with("", None, 0);
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let cmds = answer(&engine, "(shell-command)", "pwd -P");
        assert_eq!(messages(&cmds), vec![dir.to_string_lossy().into_owned()]);
    }

    #[test]
    fn long_output_goes_to_its_buffer_and_failures_report_status() {
        let engine = engine_with("", None, 0);
        let cmds = answer(
            &engine,
            "(shell-command)",
            "printf 'a\\nb\\n'; echo oops >&2; exit 2",
        );
        assert_eq!(
            rendered(&cmds, "*Shell Command Output*"),
            Some("a\nb\noops\n")
        );
        assert_eq!(messages(&cmds), vec!["Shell command exited with code 2"]);

        let cmds = commands(&engine, r#"(shell-command-run "" "true" #f)"#);
        assert_eq!(
            messages(&cmds),
            vec!["(Shell command succeeded with no output)"]
        );
        let cmds = commands(&engine, r#"(shell-command-run "" "echo bad; exit 1" #f)"#);
        assert_eq!(
            messages(&cmds),
            vec!["bad  [Shell command exited with code 1]"]
        );
    }

    #[test]
    fn prefix_inserts_output_at_point() {
        let engine = engine_with("", None, 4);
        let cmds = answer(&engine, "(shell-command)", "echo inserted");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::Insert(t) if t == "inserted\n")),
            "{cmds:?}"
        );
    }

    #[test]
    fn trailing_ampersand_runs_async() {
        let engine = engine_with("", None, 0);
        let cmds = answer(&engine, "(shell-command)", "sleep 1 &");
        let spawned = cmds.iter().any(|c| match c {
            EditorCommand::MakeProcess(p) => p.args.last().is_some_and(|a| a == "sleep 1"),
            _ => false,
        });
        assert!(spawned, "{cmds:?}");
    }

    #[test]
    fn region_is_piped_and_replaced_with_prefix() {
        let text = "pear\napple\nfig\n";
        let engine = engine_with(text, Some((0, text.len())), 4);
        let cmds = answer(&engine, "(shell-command-on-region)", "sort");
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                EditorCommand::ReplaceRegion { start: 0, end, text }
                    if *end == 15 && text == "apple\nfig\npear\n"
            )),
            "{cmds:?}"
        );

        // Without C-u the output is only shown.
        let engine = engine_with(text, Some((0, text.len())), 0);
        let cmds = answer(&engine, "(shell-command-on-region)", "wc -l | tr -d ' '");
        assert_eq!(messages(&cmds), vec!["3"]);
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, EditorCommand::ReplaceRegion { .. }))
        );
    }

    #[test]
    fn region_command_needs_a_region() {
        let engine = engine_with("x", None, 0);
        let cmds = commands(&engine, "(shell-command-on-region)");
        assert_eq!(
            messages(&cmds),
            vec!["The mark is not set now, so there is no region"]
        );
    }

    #[test]
    fn async_streams_into_its_buffer() {
        let engine = engine_with("", None, 0);
        let cmds = answer(&engine, "(async-shell-command)", "echo hi");
        assert_eq!(
            rendered(&cmds, "*Async Shell Command*"),
            Some("$ echo hi\n")
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::MakeProcess(p) if p.on_output == "async-shell-command-output"))
        );
        let cmds = commands(&engine, r#"(async-shell-command-output "hi")"#);
        assert!(cmds.iter().any(|c| matches!(
            c,
            EditorCommand::AppendToBuffer { name, text }
                if name == "*Async Shell Command*" && text == "hi\n"
        )));
        let cmds = commands(&engine, r#"(async-shell-command-done "3")"#);
        assert_eq!(
            messages(&cmds),
            vec!["Shell command exited abnormally with code 3: echo hi"]
        );
    }
}

mod grep {
    use super::*;

    /// A fresh tree: a.txt and sub/b.rs mention "needle".
    fn tree() -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("macros-grep-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "one\nneedle here\n").unwrap();
        std::fs::write(dir.join("sub/b.rs"), "fn needle() {}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    fn grep_text(engine: &ScriptEngine, q: &str) -> String {
        let cmds = commands(engine, q);
        rendered(&cmds, "*grep*")
            .unwrap_or_else(|| panic!("no *grep* render: {cmds:?}"))
            .to_string()
    }

    #[test]
    fn rgrep_and_lgrep_honour_glob_and_depth() {
        let dir = tree();
        let d = dir.to_string_lossy();
        let engine = engine_with("", None, 0);
        let all = grep_text(&engine, &format!(r#"(grep-run "{d}" "needle" "" #t)"#));
        assert!(all.contains("a.txt:2:needle here"), "{all}");
        assert!(all.contains("sub/b.rs:1:fn needle() {}"), "{all}");

        let rs = grep_text(&engine, &format!(r#"(grep-run "{d}" "needle" "*.rs" #t)"#));
        assert!(rs.contains("sub/b.rs:1:"), "{rs}");
        assert!(!rs.contains("a.txt"), "{rs}");

        let local = grep_text(&engine, &format!(r#"(grep-run "{d}/" "needle" "*" #f)"#));
        assert!(local.contains("a.txt:2:"), "{local}");
        assert!(!local.contains("b.rs"), "{local}");
    }

    #[test]
    fn hits_feed_next_error() {
        let dir = tree();
        let d = dir.to_string_lossy();
        let engine = engine_with("", None, 0);
        commands(&engine, &format!(r#"(grep-run "{d}" "needle" "*.txt" #t)"#));
        let cmds = commands(&engine, "(next-error)");
        let a = dir.join("a.txt").to_string_lossy().into_owned();
        assert!(
            cmds.iter()
                .any(|c| matches!(c, EditorCommand::OpenFileOtherWindow(p) if *p == a)),
            "{cmds:?}"
        );
        assert_eq!(
            engine
                .eval("(car *visit-pending*)")
                .unwrap()
                .value
                .as_deref(),
            Some("\"\"")
        );
        assert_eq!(
            engine
                .eval("(caddr *visit-pending*)")
                .unwrap()
                .value
                .as_deref(),
            Some("1")
        );
        let cmds = commands(&engine, "(next-error)");
        assert_eq!(messages(&cmds), vec!["No more errors"]);
    }

    #[test]
    fn grep_prompts_and_searches_the_buffer_directory() {
        let dir = tree();
        let (engine, _rx) = ScriptEngine::new();
        for src in crate::runtime::startup_scripts() {
            engine.eval(src).unwrap();
        }
        engine.set_snapshot(EditorSnapshot {
            directory: dir.to_string_lossy().into_owned(),
            ..Default::default()
        });
        let cmds = answer(&engine, "(grep)", "needle");
        let text = rendered(&cmds, "*grep*").unwrap();
        assert!(
            text.starts_with("2 matches for \"needle\" under "),
            "{text}"
        );
    }

    #[test]
    fn no_match_and_bad_regexp() {
        let dir = tree();
        let d = dir.to_string_lossy();
        let engine = engine_with("", None, 0);
        let cmds = commands(&engine, &format!(r#"(grep-run "{d}" "zzz" "" #t)"#));
        assert_eq!(
            messages(&cmds),
            vec![format!("No matches for \"zzz\" in {d}")]
        );
        let Err(err) = engine.eval(&format!(r#"(grep-run "{d}" "(" "" #t)"#)) else {
            panic!("an invalid regexp must be an error");
        };
        assert!(err.contains("Invalid regexp"), "{err}");
    }
}
