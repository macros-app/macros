use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

/// The embedded Steel runtime — everything under `scheme/`, baked into the
/// binary at compile time. Top-level `*.scm` files are evaluated at startup;
/// `themes/*.scm` are loaded on demand by `load-theme`.
static SCHEME: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/scheme");

/// One top-level `scheme/*.scm` startup script: its file stem, the stems it
/// declares in its `;; requires:` header, and its source.
#[derive(Debug, Clone)]
pub struct StartupScript {
    pub name: &'static str,
    pub requires: Vec<&'static str>,
    pub source: &'static str,
}

/// The stems named by a script's `;; requires: a b c` header lines. Only the
/// leading comment block counts (blank and `;` lines up to the first form), so
/// a "requires:" mentioned in later prose is never mistaken for a dependency.
/// Several header lines accumulate; `;; requires:` with nothing after it
/// declares no dependencies.
pub fn parse_requires(src: &str) -> Option<Vec<&str>> {
    let mut found = None;
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(comment) = line.strip_prefix(';') else {
            break;
        };
        if let Some(deps) = comment
            .trim_start_matches(';')
            .trim()
            .strip_prefix("requires:")
        {
            found
                .get_or_insert_with(Vec::new)
                .extend(deps.split_whitespace());
        }
    }
    found
}

/// Every top-level `scheme/*.scm` file (subdirectories like `themes/` are not
/// included), alphabetical by stem, with its declared dependencies.
pub fn bundled_scripts() -> Vec<StartupScript> {
    let mut scripts: Vec<_> = SCHEME
        .files()
        .filter(|f| f.path().extension().is_some_and(|e| e == "scm"))
        .filter_map(|f| {
            let name = f.path().file_stem()?.to_str()?;
            let source = f.contents_utf8()?;
            Some(StartupScript {
                name,
                requires: parse_requires(source).unwrap_or_default(),
                source,
            })
        })
        .collect();
    scripts.sort_by_key(|s| s.name);
    scripts
}

/// Order `scripts` so each comes after everything it `requires` (Steel rejects
/// a reference to a global that isn't defined yet, even inside a function
/// body). Kahn's algorithm, always taking the alphabetically first ready
/// script, so the order is deterministic and — when every dependency already
/// sorts earlier — identical to plain alphabetical order. Errs on a dependency
/// that names no script or on a cycle.
pub fn load_order(scripts: &[StartupScript]) -> Result<Vec<&StartupScript>, String> {
    use std::collections::{BTreeMap, BTreeSet};
    let by_name: BTreeMap<&str, &StartupScript> = scripts.iter().map(|s| (s.name, s)).collect();
    let mut pending: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for s in scripts {
        for dep in &s.requires {
            if !by_name.contains_key(dep) {
                return Err(format!(
                    "scheme/{}.scm requires `{dep}`, but there is no scheme/{dep}.scm",
                    s.name
                ));
            }
        }
        pending.insert(s.name, s.requires.iter().copied().collect());
    }
    let mut order = Vec::with_capacity(scripts.len());
    while !pending.is_empty() {
        // BTreeMap iterates in name order: the first ready script is the
        // alphabetically first one.
        let Some(next) = pending
            .iter()
            .find(|(_, deps)| deps.is_empty())
            .map(|(name, _)| *name)
        else {
            let stuck: Vec<String> = pending
                .iter()
                .map(|(name, deps)| {
                    let deps: Vec<&str> = deps.iter().copied().collect();
                    format!("{name} (waiting on {})", deps.join(", "))
                })
                .collect();
            return Err(format!(
                "dependency cycle among scheme scripts: {}",
                stuck.join("; ")
            ));
        };
        pending.remove(next);
        for deps in pending.values_mut() {
            deps.remove(next);
        }
        order.push(by_name[next]);
    }
    Ok(order)
}

/// The contents of the top-level `scheme/*.scm` files, in dependency order, for
/// startup evaluation. The bundle is fixed at compile time and its graph is
/// checked by tests, so a bad `requires:` header is a build bug: panic loudly.
pub fn startup_scripts() -> Vec<&'static str> {
    let scripts = bundled_scripts();
    load_order(&scripts)
        .unwrap_or_else(|e| panic!("{e}"))
        .into_iter()
        .map(|s| s.source)
        .collect()
}

/// The sources needed to load the bundled script `name` on its own: its
/// transitive `requires:` closure, then `name` itself, in startup order. Lets a
/// test load one module without hand-listing its dependency graph.
#[cfg(test)]
pub fn script_with_deps(name: &str) -> Vec<&'static str> {
    let scripts = bundled_scripts();
    let order = load_order(&scripts).unwrap_or_else(|e| panic!("{e}"));
    let by_name: std::collections::HashMap<&str, &StartupScript> =
        scripts.iter().map(|s| (s.name, s)).collect();
    assert!(by_name.contains_key(name), "no bundled scheme/{name}.scm");
    let mut needed = std::collections::HashSet::new();
    let mut stack = vec![name];
    while let Some(n) = stack.pop() {
        if needed.insert(n) {
            stack.extend(by_name[n].requires.iter().copied());
        }
    }
    order
        .into_iter()
        .filter(|s| needed.contains(s.name))
        .map(|s| s.source)
        .collect()
}

/// Names defined at top level by the bundled `scheme/*.scm` library — every
/// `(define (foo …))`, `(define foo …)`, and `(define-syntax foo …)`. These are
/// real bindings once the runtime loads, but the built-in Scheme analyzer (a
/// separate compile-only engine) doesn't see them, so it would flag a user
/// config that calls `lsp-language`, `evil-define-key`, etc. as "unknown
/// function". Seeding the analyzer with these names fixes that.
pub fn defined_names() -> Vec<String> {
    // Top-level forms start at column 0 in the bundled files.
    startup_scripts()
        .iter()
        .flat_map(|src| src.lines())
        .filter_map(define_name)
        .collect()
}

/// The name bound by a top-level `(define name …)`, `(define (name …) …)`,
/// `(define-syntax name …)`, or `(define-command (name …) …)` line, if the line
/// is such a define. Shared by [`defined_names`] and [`function_docs`].
pub(crate) fn define_name(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("(define-syntax ")
        .or_else(|| line.strip_prefix("(define-command "))
        .or_else(|| line.strip_prefix("(define "))?;
    // `(define (name …)` — skip the opening paren of the arg list.
    let token = rest
        .trim_start()
        .strip_prefix('(')
        .unwrap_or(rest.trim_start());
    let name: String = token
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '(' && *c != ')')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The head of a define line `define_name` accepted: the balanced `(name …)`
/// form when it's a procedure (cut at end of line if it continues), else the
/// bare name.
fn define_signature(line: &str, name: &str) -> String {
    if line.starts_with("(define-syntax ") {
        return format!("{name} (macro)");
    }
    let rest = line
        .split_once(' ')
        .map(|(_, r)| r.trim_start())
        .unwrap_or("");
    if !rest.starts_with('(') {
        return name.to_string();
    }
    let mut depth = 0;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..=i].to_string();
                }
            }
            _ => {}
        }
    }
    format!("{} …)", rest.trim_end())
}

/// One documented top-level definition: its name, the `;;` comment block
/// immediately preceding it (markers stripped), the source file it lives in, and
/// the 1-based line of its `define`. Steel has no runtime docstrings, so the
/// source comment IS the documentation; the `file`/`line` let `describe-function`
/// / `helm-apropos` jump to the implementation.
#[derive(Debug, Clone)]
pub struct DocEntry {
    pub name: String,
    pub doc: String,
    pub file: String,
    pub line: usize,
    /// The define's head as written: `(name a b)` for a procedure, `name` for
    /// a value, `name (macro)` for a `define-syntax`.
    pub sig: String,
}

/// Scrape every top-level `(define …)` / `(define-syntax …)` in `src`, pairing
/// each with its preceding `;;` comment block (markers stripped) and attributing
/// it to `file` with its 1-based line. Shared by the startup seed over the
/// bundled library ([`builtin_docs`]) and the runtime indexer over dynamically
/// loaded packages (`index-doc-file`), so built-ins and packages are documented
/// by exactly the same rules. A define with no preceding comment gets an empty
/// doc string.
pub fn scrape_docs(src: &str, file: &str) -> Vec<DocEntry> {
    let mut docs = Vec::new();
    // Accumulate consecutive comment lines; they document the next define.
    let mut comment: Vec<&str> = Vec::new();
    for (i, line) in src.lines().enumerate() {
        if let Some(rest) = line.trim_start().strip_prefix(";;") {
            comment.push(rest.strip_prefix(' ').unwrap_or(rest));
            continue; // keep the block growing — don't clear
        }
        if let Some(name) = define_name(line) {
            docs.push(DocEntry {
                sig: define_signature(line, &name),
                name,
                doc: comment.join("\n").trim().to_string(),
                file: file.to_string(),
                line: i + 1, // 1-based, like an editor's line numbers
            });
        }
        comment.clear(); // a non-comment line breaks contiguity
    }
    docs
}

/// Every bundled built-in definition with its doc and source location. The file
/// is the definition's path in the compile-time source tree
/// (`$CARGO_MANIFEST_DIR/scheme/…`): present when running from a checkout (so a
/// jump opens the real file) and gracefully absent for an installed binary,
/// where the caller falls back to showing the doc.
pub fn builtin_docs() -> Vec<DocEntry> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/scheme");
    let mut files: Vec<_> = SCHEME
        .files()
        .filter(|f| f.path().extension().is_some_and(|e| e == "scm"))
        .collect();
    files.sort_by_key(|f| f.path());
    let mut docs = Vec::new();
    for f in files {
        if let Some(src) = f.contents_utf8() {
            let file = format!("{root}/{}", f.path().display());
            docs.extend(scrape_docs(src, &file));
        }
    }
    docs
}

/// The names of the bundled themes — the stems of `scheme/themes/*.scm`, sorted.
/// Scheme reads this via `(theme-names)` to offer completion for `load-theme`
/// (both the minibuffer prompt and the in-buffer argument CAPF).
pub fn theme_names() -> Vec<String> {
    let mut names: Vec<String> = SCHEME
        .get_dir("themes")
        .into_iter()
        .flat_map(|d| d.files())
        .filter(|f| f.path().extension().is_some_and(|e| e == "scm"))
        .filter_map(|f| {
            f.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .collect();
    names.sort();
    names
}

/// A theme definition by name — the contents of `scheme/themes/<name>.scm`.
/// Every face name some bundled theme sets (`(set-face "name" …)` and the
/// `set-face-*` attribute setters). Loading a theme clears exactly these, so
/// switching themes leaves no stale theme colors while faces that only a
/// module or the user's config defines (ediff's `diff-added`, rainbow's
/// `rainbow-N`) survive — they used to be wiped by every `load-theme`.
pub fn theme_face_names() -> &'static std::collections::HashSet<String> {
    static NAMES: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let re = regex::Regex::new(r#"\(set-face[a-z-]*\s+"([^"]+)""#).expect("valid regex");
        theme_names()
            .iter()
            .filter_map(|n| theme_script(n))
            .flat_map(|src| {
                re.captures_iter(src)
                    .map(|c| c[1].to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    })
}

pub fn theme_script(name: &str) -> Option<&'static str> {
    SCHEME
        .get_file(format!("themes/{name}.scm"))
        .and_then(|f| f.contents_utf8())
}

/// Import the login shell's `PATH` into our own environment, so subprocesses
/// (LSP servers, `call-process`, `make-process`) can find tools installed via
/// nvm/pyenv/cargo/brew/asdf/mise/etc.
///
/// Why this is needed: on macOS (and some Linux desktops) a GUI app launched
/// from Finder/Dock/`.desktop` inherits a minimal `launchd` environment — not
/// the user's shell `PATH` — so `typescript-language-server`, `rust-analyzer`,
/// and friends are invisible. We recover the real `PATH` by asking the login
/// shell. Windows has no such stripping (GUI apps inherit the registry `PATH`),
/// so this is a no-op there.
///
/// Must be called at the very start of `main`, before any threads spawn, since
/// it mutates the process environment (`set_var` is unsound once other threads
/// may be reading the environment).
#[cfg(unix)]
pub fn import_shell_env() {
    use std::io::{IsTerminal, Read};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // Long enough for a heavy rc file; short enough that one which blocks
    // (waits on a network mount, a prompt, …) can't hang the launch.
    const TIMEOUT: Duration = Duration::from_secs(5);
    // Bracket the value so whatever else the rc files print (banners, MOTDs,
    // "nvm is out of date") can't leak into `PATH`.
    const START: &str = "__MACROS_PATH_START__";
    const END: &str = "__MACROS_PATH_END__";

    // Launched from a terminal? Then we already inherited a real shell `PATH`;
    // skip the ~50-150ms subshell cost.
    if std::io::stdout().is_terminal() {
        return;
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // `-l -i` so login + interactive rc files run — nvm/pyenv/asdf typically
    // inject `PATH` from `.zshrc`/`.bashrc` (interactive), not just profile.
    // A null stdin means an interactive shell can't block waiting for input.
    let Ok(mut child) = Command::new(&shell)
        .args([
            "-l",
            "-i",
            "-c",
            &format!("command -v printf >/dev/null && printf '{START}%s{END}' \"$PATH\""),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    // Read on a helper thread so the wait can time out. It only touches the
    // pipe, never the environment. It stops at the end marker rather than EOF:
    // a daemon the rc files started (ssh-agent, …) can hold the pipe open
    // long after the shell exits. On timeout it's abandoned.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut chunk = [0; 4096];
        while let Ok(n @ 1..) = stdout.read(&mut chunk) {
            out.extend_from_slice(&chunk[..n]);
            if out.windows(END.len()).any(|w| w == END.as_bytes()) {
                break;
            }
        }
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(TIMEOUT);
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let Ok(out) = out else {
        return;
    };
    let out = String::from_utf8_lossy(&out);
    let Some(path) = out
        .split_once(START)
        .and_then(|(_, rest)| rest.split_once(END))
        .map(|(path, _)| path.trim())
    else {
        return;
    };
    if !path.is_empty() {
        // SAFETY: called at the top of `main`, before any thread that could
        // read the environment exists (the reader above only reads a pipe).
        unsafe { std::env::set_var("PATH", path) };
    }
}

/// No-op on Windows: GUI apps already inherit the registry-assembled `PATH`.
#[cfg(not(unix))]
pub fn import_shell_env() {}

/// The user's home directory, from `$HOME` (Unix) or `%USERPROFILE%` (Windows).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Candidate config directories, most-specific first:
///   1. `$XDG_CONFIG_HOME/macros` (explicit override, any OS)
///   2. `~/.config/macros`        (macOS + Linux; the documented location)
///   3. `%APPDATA%/macros`        (native Windows config)
fn config_dirs() -> Vec<PathBuf> {
    // A non-empty env var joined with the app sub-directory.
    let from_env = |key: &str| {
        std::env::var_os(key)
            .filter(|x| !x.is_empty())
            .map(|v| PathBuf::from(v).join("macros"))
    };
    let mut dirs = Vec::new();
    dirs.extend(from_env("XDG_CONFIG_HOME"));
    // Tests must never read the developer's real init.scm or write into their
    // real config dir (MCP token, grammar trust list, packages/): swap the
    // home/APPDATA candidates for a per-process sandbox.
    #[cfg(test)]
    dirs.push(test_sandbox("config"));
    #[cfg(not(test))]
    {
        if let Some(home) = home_dir() {
            dirs.push(home.join(".config").join("macros"));
        }
        dirs.extend(from_env("APPDATA"));
    }
    dirs
}

/// A test-only sandbox directory under the system temp dir, unique to this
/// process and `kind` (created on demand by whoever writes into it).
#[cfg(test)]
fn test_sandbox(kind: &str) -> PathBuf {
    std::env::temp_dir().join(format!("macros-test-{kind}-{}", std::process::id()))
}

/// Where session state persists — saveplace, recentf, savehist, desktop,
/// perspectives, projects, dap-breakpoints — exposed to Scheme as
/// `(persistence-directory)`. Always `~/.config/macros` (deliberately NOT
/// `$XDG_CONFIG_HOME`), matching where that state has always lived.
///
/// Under `cfg(test)` it is instead a fresh temp dir per thread. Each
/// `ScriptEngine` runs on its own thread, so every test engine (and every
/// test `Editor`) gets an isolated, initially-empty state dir: `cargo test`
/// never touches the real config, and parallel tests can't leak state into
/// each other.
pub fn persistence_directory() -> PathBuf {
    #[cfg(test)]
    {
        thread_local! {
            static DIR: PathBuf = {
                static SEQ: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let dir = test_sandbox("persist").join(n.to_string());
                let _ = std::fs::remove_dir_all(&dir);
                dir
            };
        }
        DIR.with(PathBuf::clone)
    }
    #[cfg(not(test))]
    {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("macros")
    }
}

/// The user's `init.scm` and the directory it lives in, if present. Absent =
/// `None` (the editor just uses its built-in defaults, no error).
pub fn user_config() -> Option<(PathBuf, String)> {
    config_dirs().into_iter().find_map(|dir| {
        std::fs::read_to_string(dir.join("init.scm"))
            .ok()
            .map(|src| (dir, src))
    })
}

/// Working trees of locally-developed packages (`package-install-local`), whose
/// `*.scm` are editor-extension code even though they live outside the config dir.
/// Registered at config eval via `register-macros-scheme-dir`; consulted by
/// [`is_macros_scheme_path`]. `Mutex::new` is const, so no lazy-init dance.
static LOCAL_SCHEME_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Register a local package working tree so its `*.scm` resolve to `macros-scheme`
/// (see [`is_macros_scheme_path`]). Idempotent. Must run before the file is opened
/// — `package-install-local` calls it at config-eval time, the file is edited later.
pub fn register_local_scheme_dir(dir: PathBuf) {
    let mut dirs = LOCAL_SCHEME_DIRS.lock().unwrap();
    if !dirs.contains(&dir) {
        dirs.push(dir);
    }
}

/// Whether `path` is editor-aware Scheme — config / extension code that should
/// use the `macros-scheme` language (a built-in Steel server seeded with the
/// editor API, plus the `macros-steel` major mode) rather than the vanilla
/// `scheme` server. True when the file lives under the user's config directory, a
/// registered local-package dir, or is named `*.macros.scm` (an explicit opt-in
/// usable from anywhere). Plain `.scm` files are left as "just the interpreter".
pub fn is_macros_scheme_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".macros.scm")
        || config_dirs().iter().any(|dir| path.starts_with(dir))
        || LOCAL_SCHEME_DIRS
            .lock()
            .unwrap()
            .iter()
            .any(|dir| path.starts_with(dir))
}

/// Whether `path` is Scheme the user already runs — it lives under the config
/// directory or a local package dir their config registered — so it is safe to
/// hand to Steel's compiler. Compiling is *not* inert: macro expansion runs
/// `defmacro` bodies, and Steel's expansion kernel can spawn processes. A
/// `*.macros.scm` name alone doesn't qualify (any cloned repo can ship one).
/// Both sides are canonicalized, so `..` or a symlink can't fake the prefix.
pub fn is_trusted_scheme_path(path: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let under = |dir: &PathBuf| dir.canonicalize().is_ok_and(|dir| path.starts_with(dir));
    config_dirs().iter().any(under) || LOCAL_SCHEME_DIRS.lock().unwrap().iter().any(under)
}

/// The active config directory: where `init.scm` was found, else the primary
/// candidate location (so it resolves even before the user writes a config).
/// `package-install` clones into its `packages/` subdirectory — exposed to Scheme
/// as `(packages-directory)`.
/// Same precedence as [`user_config`], but only checks that `init.scm` exists
/// rather than reading it.
pub fn config_directory() -> PathBuf {
    let dirs = config_dirs();
    dirs.iter()
        .find(|d| d.join("init.scm").is_file())
        .or(dirs.first())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A small prelude, evaluated before `init.scm`, that exposes `*config-directory*`
/// and an `include` helper resolving paths against it — so a config can split
/// across files with `(include "keymaps.scm")`. (`load` remains available for
/// absolute paths.) Built on Steel's native `load`, which evaluates the file in
/// the current engine so its definitions land in the global scope.
pub fn include_prelude(dir: &Path) -> String {
    let escaped = dir
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "(define *config-directory* \"{escaped}\")\n\
         (define (include path) (load (string-append *config-directory* \"/\" path)))\n"
    )
}

#[cfg(test)]
mod theme_face_tests {
    #[test]
    fn theme_face_names_cover_theme_faces_only() {
        let names = super::theme_face_names();
        assert!(names.contains("background"));
        assert!(names.contains("comment"));
        // Module-owned faces survive a theme switch.
        assert!(!names.contains("diff-added"));
        assert!(!names.contains("rainbow-1"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `cargo test` once wrote test breakpoints into the
    /// developer's real ~/.config/macros/dap-breakpoints. Under `cfg(test)`
    /// neither the persistence dir nor the config dir may resolve under the
    /// real home config, and each thread (= each `ScriptEngine`) gets its own
    /// persistence dir.
    #[test]
    fn test_state_dirs_never_touch_real_home_config() {
        let real = home_dir().map(|h| h.join(".config").join("macros"));
        let mine = persistence_directory();
        let other = std::thread::spawn(persistence_directory).join().unwrap();
        assert_ne!(mine, other, "each engine thread gets its own state dir");
        assert_eq!(mine, persistence_directory(), "stable within a thread");
        for dir in [&mine, &other, &config_directory()] {
            assert!(
                dir.starts_with(std::env::temp_dir()),
                "{dir:?} not sandboxed"
            );
            if let Some(real) = &real {
                assert!(!dir.starts_with(real), "{dir:?} is the real config dir");
            }
        }
    }

    #[test]
    fn registered_local_dir_is_macros_scheme_path() {
        let dir = PathBuf::from("/tmp/macros-probe-pkg");
        // Before registration: a plain `.scm` under it is not editor-aware.
        assert!(!is_macros_scheme_path(&dir.join("foo.scm")));
        register_local_scheme_dir(dir.clone());
        // After: files under it resolve to macros-scheme…
        assert!(is_macros_scheme_path(&dir.join("foo.scm")));
        assert!(is_macros_scheme_path(&dir.join("sub/bar.scm")));
        // …but a SIBLING dir sharing a name prefix does not (component-wise check).
        assert!(!is_macros_scheme_path(&PathBuf::from(
            "/tmp/macros-probe-pkg-other/baz.scm"
        )));
        // `*.macros.scm` still works independent of registration.
        assert!(is_macros_scheme_path(&PathBuf::from(
            "/elsewhere/init.macros.scm"
        )));
    }

    fn script(name: &'static str, requires: &[&'static str]) -> StartupScript {
        StartupScript {
            name,
            requires: requires.to_vec(),
            source: "",
        }
    }

    fn names(order: Vec<&StartupScript>) -> Vec<&str> {
        order.into_iter().map(|s| s.name).collect()
    }

    #[test]
    fn parse_requires_reads_only_the_header() {
        assert_eq!(
            parse_requires(";; requires: core lib-section\n\n;; prose\n(define x 1)"),
            Some(vec!["core", "lib-section"])
        );
        // Empty header = no deps; several header lines accumulate.
        assert_eq!(parse_requires(";; requires:\n(define x 1)"), Some(vec![]));
        assert_eq!(
            parse_requires(";; requires: a\n;; requires: b\n"),
            Some(vec!["a", "b"])
        );
        // No header at all, or one after the first form, is not a declaration.
        assert_eq!(parse_requires(";; prose\n(define x 1)"), None);
        assert_eq!(parse_requires("(define x 1)\n;; requires: core"), None);
    }

    #[test]
    fn load_order_puts_deps_first_alphabetical_among_peers() {
        let scripts = [
            script("a", &["z"]),
            script("b", &[]),
            script("c", &["a"]),
            script("z", &[]),
        ];
        assert_eq!(names(load_order(&scripts).unwrap()), ["b", "z", "a", "c"]);
        // Already-consistent deps leave plain alphabetical order untouched.
        let scripts = [script("a", &[]), script("b", &["a"]), script("c", &["a"])];
        assert_eq!(names(load_order(&scripts).unwrap()), ["a", "b", "c"]);
    }

    #[test]
    fn load_order_rejects_missing_deps_and_cycles() {
        let err = load_order(&[script("a", &["nope"])]).unwrap_err();
        assert!(err.contains("scheme/a.scm requires `nope`"), "{err}");

        let scripts = [script("a", &["b"]), script("b", &["a"]), script("c", &[])];
        let err = load_order(&scripts).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
        assert!(
            err.contains("a (waiting on b)") && err.contains("b (waiting on a)"),
            "{err}"
        );
        assert!(!err.contains("c ("), "{err}");
    }

    /// The bundled graph sorts, and declaring dependencies kept startup order
    /// alphabetical except where a dependency forces a file later.
    #[test]
    fn bundled_load_order() {
        let scripts = bundled_scripts();
        let order = names(load_order(&scripts).unwrap());
        assert_eq!(order.len(), scripts.len());
        let pos = |n: &str| order.iter().position(|o| *o == n).unwrap();
        for s in &scripts {
            for dep in &s.requires {
                assert!(
                    pos(dep) < pos(s.name),
                    "{} loads before its dep {dep}",
                    s.name
                );
            }
        }
        assert_eq!(order.first(), Some(&"core"));
        assert!(pos("persistence") < pos("dap") && pos("dap") < pos("ui-dap"));
        // `script_with_deps` is the closure in startup order, ending with itself.
        let magit = script_with_deps("magit");
        let src = |n: &str| scripts.iter().find(|s| s.name == n).unwrap().source;
        assert_eq!(
            magit,
            ["core", "lib-async", "lib-section", "lib-transient", "magit"].map(src)
        );
    }

    #[test]
    fn embeds_startup_and_themes() {
        // Startup scripts are present and the keymap is among them.
        let scripts = startup_scripts();
        assert!(!scripts.is_empty());
        assert!(scripts.iter().any(|s| s.contains("forward-char")));

        // Themes resolve and contain set-face calls; unknown ones don't.
        assert!(theme_script("macros-dark").is_some_and(|s| s.contains("set-face")));
        assert!(theme_script("macros-light").is_some());
        assert!(theme_script("nope").is_none());

        // `theme-names` enumerates the bundled themes (stems, sorted, no .scm).
        let names = theme_names();
        assert_eq!(
            names,
            vec!["macros-dark".to_string(), "macros-light".to_string()]
        );
    }

    /// Every color literal in the bundled themes must parse (catches a bad hex
    /// or stray digit — including the 8-digit `#rrggbbaa` translucent values).
    #[test]
    fn bundled_theme_colors_parse() {
        for name in ["macros-dark", "macros-light"] {
            let src = theme_script(name).unwrap();
            let mut count = 0;
            // Each face line is `(set-face "name" "#hex")` — grab the quoted hex.
            for line in src.lines() {
                if let Some(rest) = line.trim().strip_prefix("(set-face ")
                    && let Some(hex_start) = rest.find("\"#")
                {
                    let hex = &rest[hex_start + 1..];
                    let hex = &hex[..hex.find('"').expect("closing quote")];
                    assert!(
                        crate::theme::parse_color(hex).is_some(),
                        "{name}: unparseable color {hex:?} in line {line:?}"
                    );
                    count += 1;
                }
            }
            assert!(count > 10, "{name}: expected many faces, found {count}");
        }
    }

    #[test]
    fn function_docs_scrape_preceding_comments() {
        let docs = builtin_docs();
        let entry = |name: &str| docs.iter().find(|e| e.name == name);
        let find = |name: &str| entry(name).map(|e| e.doc.clone());

        // Every define is captured…
        assert!(!docs.is_empty());
        // …and a documented one carries its preceding ;; block.
        let doc = find("save-excursion").expect("save-excursion should be scraped");
        assert!(doc.contains("restore point"), "got: {doc:?}");
        // The ;; prefix is stripped (doc text, not raw comment).
        assert!(
            !doc.contains(";;"),
            "comment markers should be stripped: {doc:?}"
        );

        // The tree-sitter rainbow demo's public setters are discoverable.
        assert!(find("rainbow-set-colors").is_some());

        // Markdown's override variables each carry their OWN docstring, so
        // apropos / describe-function explain them to someone writing overrides.
        let hide =
            find("*markdown-hide-heading-markers*").expect("hide-markers var should be scraped");
        assert!(hide.contains("init.scm"), "got: {hide:?}");
        let scale = find("*markdown-scale-headings*").expect("scale var should be scraped");
        assert!(
            scale.contains("larger font"),
            "the scale var needs its own doc, not the previous block: {scale:?}"
        );

        // Source location is captured: a real bundled file, on a sane 1-based
        // line. This is what lets helm-apropos jump to the implementation.
        let se = entry("save-excursion").unwrap();
        assert!(
            se.file.ends_with(".scm"),
            "file should be a .scm: {:?}",
            se.file
        );
        assert!(se.line >= 1, "line should be 1-based, got {}", se.line);
    }

    /// `scrape_docs` attributes defines to the file it is told, and counts lines
    /// from 1 — the contract `index-doc-file` relies on for packages.
    #[test]
    fn scrape_docs_records_file_and_line() {
        let src = "; not a doc comment\n\
                   ;; doc for foo\n\
                   (define (foo x) x)\n\
                   \n\
                   (define bar 1)\n";
        let docs = scrape_docs(src, "/pkg/thing.scm");
        let foo = docs.iter().find(|e| e.name == "foo").unwrap();
        assert_eq!(foo.doc, "doc for foo");
        assert_eq!(foo.file, "/pkg/thing.scm");
        assert_eq!(foo.line, 3);
        // A define with no preceding ;; block has an empty doc, still located.
        let bar = docs.iter().find(|e| e.name == "bar").unwrap();
        assert_eq!(bar.doc, "");
        assert_eq!(bar.line, 5);
    }

    #[test]
    fn include_prelude_escapes_and_resolves() {
        // The config dir becomes a valid Steel string literal (backslashes and
        // quotes escaped — important for Windows paths).
        let prelude = include_prelude(Path::new(r"C:\Users\me\macros"));
        assert!(prelude.contains(r#""C:\\Users\\me\\macros""#));
        assert!(prelude.contains("(define (include path)"));
        // A POSIX path passes through cleanly.
        let prelude = include_prelude(Path::new("/home/me/.config/macros"));
        assert!(prelude.contains(r#""/home/me/.config/macros""#));
    }
}

/// Serializes tests that mutate process-wide environment variables (e.g.
/// `XDG_CONFIG_HOME`): `cargo test` runs tests on parallel threads, and a
/// concurrent test resolving config directories would otherwise observe the
/// mutated environment mid-flight. Any test that sets/removes such a variable
/// must hold this for its whole body.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod config_integration {
    use super::*;
    #[test]
    fn user_config_with_include_roundtrip() {
        // Mutates XDG_CONFIG_HOME for the duration of the test — hold the env
        // lock so parallel tests can't observe the temporary override. A
        // poisoned lock (a prior holder panicked) is fine: the env var is
        // restored below regardless, so just take the guard.
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A temp config dir as XDG_CONFIG_HOME/macros with init.scm that includes
        // a second file — exactly the main.rs flow.
        let root = std::env::temp_dir().join(format!("macros-cfg-{}", std::process::id()));
        let dir = root.join("macros");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("init.scm"), "(include \"more.scm\")\n").unwrap();
        std::fs::write(dir.join("more.scm"), "(set-option \"font-size\" \"17\")\n").unwrap();
        // SAFETY: `TEST_ENV_LOCK` is held, so no other test reads or writes the
        // environment concurrently.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &root);
        }

        let (dir2, src) = user_config().expect("init.scm found via XDG_CONFIG_HOME");
        assert_eq!(dir2, dir);
        assert!(src.contains("include"));

        // Eval prelude + init through a real engine; the included set-option lands.
        let (engine, _rx) = crate::script::ScriptEngine::new();
        engine.eval(&include_prelude(&dir2)).unwrap();
        let result = engine.eval(&src).unwrap();
        let has_set = result.commands.iter().any(|c| {
            matches!(
            c, crate::script::EditorCommand::SetOption(k, v) if k == "font-size" && *v == "17")
        });
        assert!(
            has_set,
            "included file's set-option should apply, got {:?}",
            result.commands
        );

        // SAFETY: `TEST_ENV_LOCK` is still held (see above).
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
