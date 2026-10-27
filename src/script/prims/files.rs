//! File-system primitives (pure, run on the engine thread): read / write /
//! list, paths, directories and directory watches.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared { queue, .. } = ctx;
    reg.section(Section::Files);
    // State bridges for dynamic sources (run on the engine thread — blocking
    // I/O here never touches the UI). All pure value-returning functions.
    reg.register_fn("current-directory", || -> String {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    reg.register_fn("directory-of", |path: String| -> String {
        if path.ends_with('/') {
            return path;
        }
        std::path::Path::new(&path)
            .parent()
            .map(|d| {
                let mut s = d.to_string_lossy().into_owned();
                // A bare name ("foo.txt") has an empty parent: that's the
                // current directory, not the filesystem root.
                if s.is_empty() {
                    return "./".to_string();
                }
                if !s.ends_with('/') {
                    s.push('/');
                }
                s
            })
            .unwrap_or_default()
    });
    reg.register_fn("list-directory", |path: String| -> Vec<String> {
        // `/ssh:host:/dir` lists over ssh (cached briefly, so completion
        // doesn't relist on every keystroke); a failure is echoed.
        if let Some(remote) = crate::remote::parse(&path) {
            return crate::remote::list_dir(&remote).unwrap_or_else(|err| {
                report_remote(&err);
                Vec::new()
            });
        }
        let path = crate::util::expand_tilde(&path);
        let mut entries = Vec::new();
        if let Ok(read) = std::fs::read_dir(&path) {
            for entry in read.flatten() {
                let mut full = entry.path().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    full.push('/');
                }
                entries.push(full);
            }
        }
        entries.sort();
        entries
    });
    {
        // `(watch-directories owner dirs)` — replace `owner`'s watched dir set so
        // a change under any of them fires `directory-changed-hook` (dired-sidebar /
        // dired live-refresh). Replace semantics: pass `'()` to drop them.
        let q = queue.clone();
        reg.register_fn(
            "watch-directories",
            move |owner: String, StringList(dirs): StringList| {
                q.push(EditorCommand::WatchDirectories(owner, dirs));
            },
        );
    }
    {
        // `(watch-tree owner dirs)` — like `watch-directories` but recursive: a
        // change anywhere under `dirs` fires `directory-changed-hook` with the
        // watched root (magit watches its repo root). `'()` drops the watch.
        let q = queue.clone();
        reg.register_fn(
            "watch-tree",
            move |owner: String, StringList(dirs): StringList| {
                q.push(EditorCommand::WatchTree(owner, dirs));
            },
        );
    }
    // --- File-system primitives (pure, run on the engine thread) ----------
    // Every path argument has a leading `~` expanded, as `open-file` and the
    // process primitives do: a config path like "~/org/inbox.org" otherwise
    // read as empty or got written to a literal `./~/org/` directory.
    reg.register_fn("file-exists?", |path: String| -> bool {
        if let Some(remote) = crate::remote::parse(&path) {
            return remote_kind(&remote).is_some_and(|k| k != crate::remote::Kind::Missing);
        }
        let path = crate::util::expand_tilde(&path);
        std::path::Path::new(&path).exists()
    });
    reg.register_fn("file-directory?", |path: String| -> bool {
        if let Some(remote) = crate::remote::parse(&path) {
            return remote_kind(&remote) == Some(crate::remote::Kind::Dir);
        }
        let path = crate::util::expand_tilde(&path);
        std::path::Path::new(&path).is_dir()
    });
    // `(file-truename path)` → the canonical absolute path (symlinks and
    // `..` resolved — Emacs's `file-truename`), or `path` unchanged when it
    // doesn't exist. Lets dap.scm match an adapter's frame paths (e.g. macOS
    // `/private/tmp/…`) against the buffer that visits `/tmp/…`.
    reg.register_fn("file-truename", |path: String| -> String {
        if let Some(remote) = crate::remote::parse(&path) {
            return remote.to_string();
        }
        let path = crate::util::expand_tilde(&path);
        std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(path)
    });
    // `(packages-directory)` → where `package-install` clones git packages:
    // the active config dir's `packages/` subdirectory. Resolved in Rust at call
    // time (the same config-dir logic that finds init.scm), so Scheme needn't
    // thread the path through a global.
    reg.register_fn("packages-directory", || -> String {
        crate::runtime::config_directory()
            .join("packages")
            .to_string_lossy()
            .into_owned()
    });
    // `(persistence-directory)` → where session state (saveplace, recentf,
    // savehist, desktop, perspectives, dap-breakpoints, …) is saved. Resolved
    // in Rust so test builds can sandbox it per engine (see
    // `runtime::persistence_directory`).
    reg.register_fn("persistence-directory", || -> String {
        crate::runtime::persistence_directory()
            .to_string_lossy()
            .into_owned()
    });
    // `(register-macros-scheme-dir dir)` → treat `*.scm` under `dir` as editor
    // extension code (the `macros-steel` mode + editor-aware `macros-scheme` LSP),
    // even though it's outside the config dir. `package-install-local` calls this
    // so a local working tree's sources resolve correctly on open.
    reg.register_fn("register-macros-scheme-dir", |dir: String| {
        crate::runtime::register_local_scheme_dir(std::path::PathBuf::from(dir));
    });
    reg.register_fn("parent-directory", |path: String| -> String {
        // Parent of a path, trailing slash trimmed first so a directory's own
        // parent is returned (Emacs `(file-name-directory (directory-file-name …))`).
        let trimmed = path.trim_end_matches('/');
        // A remote path keeps its `/ssh:host:` prefix; its root has no parent.
        crate::remote::parent_of(std::path::Path::new(trimmed))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    // `(read-file path)` → the file's text, or "" when it can't be read for any
    // reason (missing, unreadable, not UTF-8). Right for scanners and previews;
    // wrong for read-modify-write, which would then write "" + its addition
    // over the original — use `read-file-strict` there.
    reg.register_fn("read-file", |path: String| -> String {
        if let Some(remote) = crate::remote::parse(&path) {
            return crate::remote::read_file(&remote)
                .ok()
                .and_then(|r| String::from_utf8(r.bytes).ok())
                .unwrap_or_default();
        }
        let path = crate::util::expand_tilde(&path);
        std::fs::read_to_string(&path).unwrap_or_default()
    });
    // `(read-file-strict path)` → the file's text, "" only when it doesn't exist,
    // and an error otherwise (permission denied, a directory, invalid UTF-8), so
    // a caller about to rewrite the file stops instead of truncating it.
    reg.register_fn(
        "read-file-strict",
        |path: String| -> Result<String, String> {
            if let Some(remote) = crate::remote::parse(&path) {
                return match crate::remote::read_file(&remote) {
                    Ok(read) => String::from_utf8(read.bytes).map_err(|e| format!("{path}: {e}")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                    Err(e) => Err(e.to_string()),
                };
            }
            let path = crate::util::expand_tilde(&path);
            match std::fs::read_to_string(&path) {
                Ok(text) => Ok(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
                Err(e) => Err(format!("{path}: {e}")),
            }
        },
    );
    reg.register_fn("write-file", |path: String, text: String| -> bool {
        let path = crate::util::expand_tilde(&path);
        // Create parent directories first (Emacs's `make-directory ... t` before
        // writing). Without this, the very first write to a fresh config dir like
        // ~/.config/macros/ fails silently and persistence (recent projects,
        // recentf, saveplace) never takes hold.
        if let Some(parent) = std::path::Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
            && !crate::remote::is_remote(parent)
        {
            let _ = std::fs::create_dir_all(parent);
        }
        // Atomic (remote paths too, over ssh) (temp file + rename), so a crash or full disk mid-write
        // leaves the old file rather than a truncated one — these are the
        // user's org files, bookmarks and desktop state.
        crate::save::atomic_write(std::path::Path::new(&path), text.as_bytes()).is_ok()
    });
    // `(delete-file path)` → #t if the file was removed (or already absent),
    // #f on a real error (permission, path is a non-empty directory). Emacs's
    // `delete-file`; a package managing its own on-disk queue (an outbox, a
    // cache) needs to retract entries, not just append.
    reg.register_fn("delete-file", |path: String| -> bool {
        if crate::remote::parse(&path).is_some() {
            return crate::remote::run_program("rm", &["-f".into(), path])
                .is_ok_and(|(code, ..)| code == 0);
        }
        let path = crate::util::expand_tilde(&path);
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        }
    });
    // Binary-safe file I/O, hex interchange (`read-file`/`write-file` are
    // UTF-8-only: a String can't round-trip arbitrary bytes). `read-file-hex`
    // → hex of the file's bytes, "" on error; `write-file-hex` decodes hex
    // and writes the bytes, creating parent dirs like `write-file`.
    reg.register_fn("read-file-hex", |path: String| -> String {
        let path = crate::util::expand_tilde(&path);
        std::fs::read(&path).map(hex::encode).unwrap_or_default()
    });
    reg.register_fn("write-file-hex", |path: String, hex_str: String| -> bool {
        let path = crate::util::expand_tilde(&path);
        let Ok(bytes) = hex::decode(hex_str.trim()) else {
            return false;
        };
        if let Some(parent) = std::path::Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        crate::save::atomic_write(std::path::Path::new(&path), &bytes).is_ok()
    });
    // `(file-remote? path)` → whether `path` is a `/ssh:host:/…` path (see
    // src/remote.rs). Lets a package skip local-only work for remote files.
    reg.register_fn("file-remote?", |path: String| -> bool {
        crate::remote::parse(&path).is_some()
    });
    // `(remote-call-process program args)` → `(exit-code stdout stderr)`, like
    // `call-process-full`, but run over ssh on the host the `/ssh:` arguments
    // name (prefixes stripped, every argument quoted). dired's copy / rename /
    // delete / mkdir use it for remote files. Fails (exit "-1", the reason in
    // stderr) when the remote arguments span hosts or mix in a local path.
    reg.register_fn(
        "remote-call-process",
        |program: String, StringList(args): StringList| -> Vec<String> {
            match crate::remote::run_program(&program, &args) {
                Ok((code, out, err)) => vec![code.to_string(), out, err],
                Err(err) => vec!["-1".into(), String::new(), err.to_string()],
            }
        },
    );
}

/// A remote path's kind, `None` (and the reason echoed) when it couldn't be
/// determined.
fn remote_kind(remote: &crate::remote::RemotePath) -> Option<crate::remote::Kind> {
    crate::remote::kind(remote)
        .map_err(|err| report_remote(&err))
        .ok()
}

/// Echo why a remote operation a Scheme primitive ran failed (unless the user
/// cancelled it with C-g, which says so itself).
fn report_remote(err: &std::io::Error) {
    if err.kind() != std::io::ErrorKind::Interrupted {
        crate::remote::report(err.to_string());
    }
}
