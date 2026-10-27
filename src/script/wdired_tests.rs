//! wdired.scm: parsing the edited listing, the rename plan (chains, swaps,
//! cycles, conflicts) and running it — with a fake file system and on disk,
//! including a failure partway that must roll back.

use super::*;

fn engine() -> ScriptEngine {
    let (engine, _rx) = ScriptEngine::new();
    for src in crate::runtime::startup_scripts() {
        engine.eval(src).unwrap();
    }
    // A fake file system: `fs` lists the paths that exist.
    engine
        .eval(
            r#"(begin
                 (define fs '("/d" "/d/a" "/d/b" "/d/c" "/d/sub"))
                 (define (ex? p) (if (member p fs) #t #f)))"#,
        )
        .unwrap();
    engine
}

fn v(engine: &ScriptEngine, q: &str) -> String {
    engine.eval(q).unwrap().value.unwrap()
}

#[test]
fn plan_simple_rename_and_chain() {
    let e = engine();
    assert_eq!(
        v(&e, r#"(wdired-plan "/d" (list (cons "/d/a" "x")) ex?)"#),
        r#"((mv "/d/a" "/d/x"))"#
    );
    // a → b while b → x: b must move out of the way first.
    assert_eq!(
        v(
            &e,
            r#"(wdired-plan "/d" (list (cons "/d/a" "b") (cons "/d/b" "x")) ex?)"#
        ),
        r#"((mv "/d/b" "/d/x") (mv "/d/a" "/d/b"))"#
    );
    // A directory's trailing "/" is ignored on both sides.
    assert_eq!(
        v(
            &e,
            r#"(wdired-plan "/d" (list (cons "/d/sub/" "lib/")) ex?)"#
        ),
        r#"((mv "/d/sub" "/d/lib"))"#
    );
}

#[test]
fn plan_swaps_and_cycles_go_through_temp_names() {
    let e = engine();
    assert_eq!(
        v(
            &e,
            r#"(wdired-plan "/d" (list (cons "/d/a" "b") (cons "/d/b" "a")) ex?)"#
        ),
        r#"((mv "/d/a" "/d/.wdired-0-a") (mv "/d/b" "/d/a") (mv "/d/.wdired-0-a" "/d/b"))"#
    );
    assert_eq!(
        v(
            &e,
            r#"(wdired-plan "/d" (list (cons "/d/a" "b") (cons "/d/b" "c") (cons "/d/c" "a")) ex?)"#
        ),
        r#"((mv "/d/a" "/d/.wdired-0-a") (mv "/d/c" "/d/a") (mv "/d/b" "/d/c") (mv "/d/.wdired-0-a" "/d/b"))"#
    );
    // A case-only rename steps aside too (case-folding file systems).
    assert_eq!(
        v(&e, r#"(wdired-plan "/d" (list (cons "/d/a" "A")) ex?)"#),
        r#"((mv "/d/a" "/d/.wdired-0-a") (mv "/d/.wdired-0-a" "/d/A"))"#
    );
}

#[test]
fn plan_creates_missing_directories() {
    let e = engine();
    assert_eq!(
        v(
            &e,
            r#"(wdired-plan "/d" (list (cons "/d/a" "new/deep/a") (cons "/d/b" "new/b")) ex?)"#
        ),
        r#"((mkdir "/d/new") (mkdir "/d/new/deep") (mv "/d/a" "/d/new/deep/a") (mv "/d/b" "/d/new/b"))"#
    );
}

#[test]
fn plan_refuses_conflicts() {
    let e = engine();
    assert_eq!(
        v(&e, r#"(wdired-plan "/d" (list (cons "/d/a" "c")) ex?)"#),
        r#""/d/c already exists""#
    );
    assert_eq!(
        v(
            &e,
            r#"(wdired-plan "/d" (list (cons "/d/a" "x") (cons "/d/b" "x")) ex?)"#
        ),
        r#""Two files would be renamed to /d/x""#
    );
    assert_eq!(
        v(&e, r#"(wdired-plan "/d" (list (cons "/d/a" "")) ex?)"#),
        r#""Empty file name for a""#
    );
}

#[test]
fn execute_rolls_back_on_failure() {
    let e = engine();
    // A fake runner: the second move fails; everything done is undone.
    let out = v(
        &e,
        r#"(begin
             (define calls '())
             (define tries 0)
             (define (mv a b)
               (set! tries (+ tries 1))
               (if (= tries 2)
                   "mv: permission denied"
                   (begin (set! calls (cons (list 'mv a b) calls)) #t)))
             (define (mk d) (set! calls (cons (list 'mkdir d) calls)) #t)
             (define (rm d) (set! calls (cons (list 'rmdir d) calls)) #t)
             (list (wdired-execute '((mkdir "/d/n") (mv "/d/a" "/d/n/a") (mv "/d/b" "/d/x"))
                                   mv mk rm (lambda (p) #f))
                   (reverse calls)))"#,
    );
    assert_eq!(
        out,
        r#"((failed "mv: permission denied" ()) ((mkdir "/d/n") (mv "/d/a" "/d/n/a") (mv "/d/n/a" "/d/a") (rmdir "/d/n")))"#
    );
    // A move never overwrites: an existing target is a failure.
    assert_eq!(
        v(
            &e,
            r#"(wdired-execute '((mv "/d/a" "/d/b")) (lambda (a b) #t) (lambda (d) #t)
                                (lambda (d) #t) ex?)"#
        ),
        r#"(failed "/d/b already exists" ())"#
    );
    // A rollback that itself fails reports where the file was left.
    assert_eq!(
        v(
            &e,
            r#"(let ((n 0))
                 (wdired-execute '((mv "/d/a" "/d/x") (mv "/d/b" "/d/y"))
                                 (lambda (a b) (set! n (+ n 1)) (if (= n 1) #t "boom"))
                                 (lambda (d) #t) (lambda (d) #t) (lambda (p) #f)))"#
        ),
        r#"(failed "boom" (("/d/a" "/d/x")))"#
    );
}

#[test]
fn parse_reads_changed_names_and_rejects_new_lines() {
    let e = engine();
    e.eval(r#"(define spec '((2 "  " "a" "/d/a") (3 "D " "sub/" "/d/sub/") (4 "  " "c" "/d/c")))"#)
        .unwrap();
    assert_eq!(
        v(&e, r#"(wdired-parse spec "/d:\n\n  a\nD lib/\n  see")"#),
        r#"(("/d/sub/" . "lib") ("/d/c" . "see"))"#
    );
    assert_eq!(
        v(&e, r#"(wdired-parse spec "/d:\n\n  a\nD sub\n  c")"#),
        "()"
    );
    assert!(
        v(&e, r#"(wdired-parse spec "/d:\n\n  a\nD sub/")"#).contains("line breaks"),
        "a removed line is an error"
    );
}

#[test]
fn read_only_ranges_cover_all_but_the_names() {
    let e = engine();
    // "/d:" (3 bytes) + \n, "" + \n, then "  a\n", "D é\n" (é is 2 bytes).
    assert_eq!(
        v(
            &e,
            r#"(wdired-read-only-ranges '("/d:" "" "  a" "D é" "  b")
                                        '((2 "  " "a" "/d/a") (3 "D " "é" "/d/é") (4 "  " "b" "/d/b")))"#
        ),
        "((0 7) (9 11) (14 16))"
    );
}

#[test]
fn renames_on_disk_including_a_swap_and_rolls_back_a_failure() {
    let dir = std::env::temp_dir().join(format!("macros-wdired-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body) in [("a", "A"), ("b", "B"), ("c", "C")] {
        std::fs::write(dir.join(name), body).unwrap();
    }
    let d = dir.to_str().unwrap();
    let e = engine();
    let run = |renames: &str| {
        v(
            &e,
            &format!(
                r#"(let ((plan (wdired-plan "{d}" {renames} file-exists?)))
                     (if (string? plan) plan
                         (wdired-execute plan wdired-fs-mv wdired-fs-mkdir wdired-fs-rmdir
                                         file-exists?)))"#
            ),
        )
    };
    // Swap a and b, and move c into a new directory.
    let out = run(&format!(
        r#"(list (cons "{d}/a" "b") (cons "{d}/b" "a") (cons "{d}/c" "sub/c"))"#
    ));
    assert_eq!(out, "(ok)");
    let read = |p: &str| std::fs::read_to_string(dir.join(p)).unwrap();
    assert_eq!(read("a"), "B");
    assert_eq!(read("b"), "A");
    assert_eq!(read("sub/c"), "C");
    // A target that appears after planning: the second move fails and the
    // first is undone, so every file is back where it was.
    let out = v(
        &e,
        &format!(
            r#"(let ((plan (wdired-plan "{d}" (list (cons "{d}/a" "x") (cons "{d}/b" "y"))
                                       file-exists?)))
                 (write-file "{d}/y" "intruder")
                 (wdired-execute plan wdired-fs-mv wdired-fs-mkdir wdired-fs-rmdir file-exists?))"#
        ),
    );
    assert!(out.starts_with("(failed"), "{out}");
    assert_eq!(read("a"), "B");
    assert_eq!(read("b"), "A");
    assert_eq!(read("y"), "intruder");
    assert!(!dir.join("x").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keys_are_dired_local() {
    let (engine, _rx) = ScriptEngine::new();
    let mut cmds = Vec::new();
    for src in crate::runtime::startup_scripts() {
        cmds.extend(engine.eval(src).unwrap().commands);
    }
    let bound = |mode: &str, key: &str, cmd: &str| {
        cmds.iter().any(|c| {
            matches!(c, EditorCommand::DefineKey(m, k, x)
                if m == mode && k == key && x.contains(cmd))
        })
    };
    assert!(bound(
        "dired-mode",
        "ctrl-x ctrl-q",
        "wdired-change-to-wdired-mode"
    ));
    assert!(bound(
        "evil-normal:dired-mode",
        "i",
        "wdired-change-to-wdired-mode"
    ));
    assert!(bound("wdired-mode", "ctrl-c ctrl-c", "wdired-finish-edit"));
    assert!(bound(
        "wdired-mode",
        "ctrl-c escape",
        "wdired-abort-changes"
    ));
    assert!(bound(
        "wdired-mode",
        "ctrl-c ctrl-k",
        "wdired-abort-changes"
    ));
    assert!(bound("fundamental", "ctrl-c left", "winner-undo"));
    assert!(bound("fundamental", "ctrl-c right", "winner-redo"));
}
