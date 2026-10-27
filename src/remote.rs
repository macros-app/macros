//! Remote files over ssh (TRAMP-style): `/ssh:[user@]host[#port]:/path`.
//!
//! A remote path is an ordinary [`Path`] spelled with the TRAMP prefix, so it
//! rides through buffers, `buffer-file-name` and Scheme unchanged; the file
//! operations that must touch it (visit, save, revert, list, dired's
//! copy/rename/delete) check [`parse_path`] and come here instead of `std::fs`.
//!
//! **Transport.** Every operation runs the system `ssh` (so `~/.ssh/config`,
//! the agent, keys, jump hosts and known-hosts all just work) with
//! `BatchMode=yes` — macros never sees or prompts for a password; a host that
//! needs one fails with a message saying to set up a key or agent. On Unix the
//! connection is shared (`ControlMaster=auto` + `ControlPersist`) through a
//! socket in a private per-user directory, so only the first operation pays
//! for the handshake.
//!
//! **No injection.** ssh hands its command line to the remote user's login
//! shell, which may be bash, zsh, fish or csh — each with its own quoting. So
//! the login shell only ever sees one fixed, quote-free string
//! ([`login_command`]): `/bin/sh` reads the real script from the first bytes
//! of stdin (`dd` of an exact length) and `eval`s it, and the file's bytes (on
//! a write) follow on the same stdin. Paths go into that script with POSIX
//! single-quote quoting ([`sh_quote`]), which `/bin/sh` parses the same way
//! everywhere.
//!
//! **Off the UI thread, cancellable.** Callers run these blocking functions on
//! a background thread (see [`spawn`]) or the Scheme engine thread. `C-g`
//! calls [`cancel_all`], which kills every ssh still running; the operation
//! then fails as [`io::ErrorKind::Interrupted`]. While a host has no live
//! master connection, "Connecting to host…" is published to every editor
//! window ([`subscribe`]).
//!
//! **Saves keep the local guarantees** (see `crate::save`): the write lands in
//! a temp file next to the target and is `mv`ed over it (keeping the mode;
//! written in place for a hard-linked file or one owned by someone else), and
//! it is refused when the file changed on the host since the buffer read it —
//! the check runs on the host, in the same script as the write.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::HideConsole;

/// The connection a remote path names: `[user@]host[#port]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Host {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// A parsed `/ssh:` (or `/scp:`) path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePath {
    /// `ssh` or `scp` — both are carried over ssh; kept so the path prints
    /// back the way it was written.
    pub method: &'static str,
    pub host: Host,
    /// The path on the host, normalized: absolute (`/…`) or home-relative
    /// (`~` / `~/…`), with `.`, empty and resolvable `..` segments removed.
    pub path: String,
}

const METHODS: [&str; 2] = ["ssh", "scp"];

impl Host {
    /// `user@host#port`, as it appears in a remote path.
    pub fn spec(&self) -> String {
        let mut s = String::new();
        if let Some(user) = &self.user {
            s.push_str(user);
            s.push('@');
        }
        if self.host.contains(':') {
            s.push('[');
            s.push_str(&self.host);
            s.push(']');
        } else {
            s.push_str(&self.host);
        }
        if let Some(port) = self.port {
            s.push('#');
            s.push_str(&port.to_string());
        }
        s
    }
}

impl Host {
    /// The interactive `ssh` command that reaches this host, for messages.
    fn ssh_invocation(&self) -> String {
        let port = self.port.map_or_else(String::new, |p| format!("-p {p} "));
        let user = self
            .user
            .as_ref()
            .map_or_else(String::new, |u| format!("{u}@"));
        format!("ssh {port}{user}{}", self.host)
    }
}

impl RemotePath {
    /// `/ssh:user@host#port:` — everything before the host-side path.
    pub fn prefix(&self) -> String {
        format!("/{}:{}:", self.method, self.host.spec())
    }

    /// The same host with a different host-side `path` (normalized).
    pub fn with_path(&self, path: &str) -> RemotePath {
        RemotePath {
            method: self.method,
            host: self.host.clone(),
            path: normalize(path),
        }
    }

    /// The containing directory, or `None` at the host's root.
    pub fn parent(&self) -> Option<RemotePath> {
        let path = &self.path;
        if path == "/" {
            return None;
        }
        if path == "~" {
            return Some(self.with_path("~/.."));
        }
        let cut = path.rfind('/')?;
        let parent = if cut == 0 { "/" } else { &path[..cut] };
        // `~/..` has no lexical parent: go one more level up through the shell.
        if path.ends_with("/..") {
            return Some(self.with_path(&format!("{path}/..")));
        }
        Some(self.with_path(parent))
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.prefix(), self.path)
    }
}

/// Parse `s` as a remote path, or `None` if it isn't one (or is malformed —
/// an empty host, a host or user starting with `-` that ssh would read as an
/// option, a bad port).
pub fn parse(s: &str) -> Option<RemotePath> {
    let rest = s.strip_prefix('/')?;
    let (method, rest) = rest.split_once(':')?;
    let method = METHODS.into_iter().find(|m| *m == method)?;
    // `user@` is optional; the user can't contain `@`, `:` or `/`.
    let (user, rest) = match rest.split_once('@') {
        Some((user, after)) if !user.contains([':', '/', '[']) => (Some(user), after),
        _ => (None, rest),
    };
    // `[v6::addr]` or a plain host, up to the `:` that starts the path.
    let (host, rest) = if let Some(bracketed) = rest.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        (host, after)
    } else {
        let end = rest.find([':', '#'])?;
        (&rest[..end], &rest[end..])
    };
    let (port, rest) = match rest.strip_prefix('#') {
        Some(after) => {
            let (port, after) = after.split_once(':')?;
            (Some(port.parse::<u16>().ok().filter(|p| *p != 0)?), after)
        }
        None => (None, rest.strip_prefix(':')?),
    };
    let ok_name = |n: &str, extra: &[char]| {
        !n.is_empty()
            && !n.starts_with('-')
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c) || extra.contains(&c))
    };
    if !ok_name(host, &[':', '%']) || user.is_some_and(|u| !ok_name(u, &['+'])) {
        return None;
    }
    Some(RemotePath {
        method,
        host: Host {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
        },
        path: normalize(rest),
    })
}

/// [`parse`] for a [`Path`].
pub fn parse_path(path: &Path) -> Option<RemotePath> {
    let s = path.to_str()?;
    // Cheap pre-check: nearly every path a caller asks about is local.
    if !s.starts_with("/ssh:") && !s.starts_with("/scp:") {
        return None;
    }
    parse(s)
}

/// Whether `path` names a file on another host.
pub fn is_remote(path: &Path) -> bool {
    parse_path(path).is_some()
}

/// `path` in its canonical spelling when it is remote (so two spellings of
/// one remote file share a buffer); unchanged otherwise.
pub fn canonical(path: &Path) -> Option<PathBuf> {
    parse_path(path).map(|r| PathBuf::from(r.to_string()))
}

/// The directory containing `path`, remote-aware: a remote file's parent keeps
/// its `/ssh:host:` prefix (`Path::parent` would cut `/ssh:host:/x` to
/// `/ssh:host:`). `None` at a root.
pub fn parent_of(path: &Path) -> Option<PathBuf> {
    match parse_path(path) {
        Some(remote) => remote.parent().map(|p| PathBuf::from(p.to_string())),
        None => path.parent().map(Path::to_path_buf),
    }
}

/// Lexically normalize a host-side path: empty → `~` (TRAMP's `/ssh:host:` is
/// the home directory), relative → under `~`, duplicate slashes and `.`
/// dropped, `..` folded where the parent is known.
fn normalize(path: &str) -> String {
    let (root, rest) = if let Some(rest) = path.strip_prefix('/') {
        ("/", rest)
    } else if path == "~" {
        ("~", "")
    } else if let Some(rest) = path.strip_prefix("~/") {
        ("~", rest)
    } else {
        ("~", path)
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                // `/..` is `/`; `~/..` needs the shell to know where home is.
                _ if root == "/" => {}
                _ => parts.push(".."),
            },
            seg => parts.push(seg),
        }
    }
    match (root, parts.is_empty()) {
        ("/", _) => format!("/{}", parts.join("/")),
        (_, true) => "~".to_string(),
        _ => format!("~/{}", parts.join("/")),
    }
}

// --- Quoting ---------------------------------------------------------------

/// Quote `s` as one POSIX shell word: single quotes, with each embedded `'`
/// written as `'\''`. Nothing inside single quotes is special to `/bin/sh`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A host-side path as a shell word: home-relative paths expand `$HOME`
/// (quoted `~` would not), everything else is [`sh_quote`]d.
pub fn sh_path(path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) if rest.starts_with('/') => format!("\"$HOME\"{}", sh_quote(rest)),
        _ => sh_quote(path),
    }
}

/// The command line ssh hands to the remote login shell for a script of
/// `script_len` bytes: `/bin/sh` reads exactly that many bytes of stdin as its
/// script (the rest of stdin is left for the script, e.g. a file to write).
/// Fixed text with no backslash, `!` or newline, so bash, zsh, fish and csh
/// all pass it through to `/bin/sh` unchanged.
pub fn login_command(script_len: usize) -> String {
    format!("exec /bin/sh -c 'eval \"$(dd bs=1 count={script_len} 2>/dev/null)\"'")
}

/// Marks the start of a script's real output: anything a login shell's rc
/// files print to stdout before it is skipped.
const SENTINEL: &[u8] = b"\x01macros-ok\n";

/// Shared prelude: C locale, and `st` printing a file's stamp (mtime, size,
/// inode) with GNU/busybox `stat -c` or BSD/macOS `stat -f`.
const PRELUDE: &str = "LC_ALL=C; export LC_ALL\n\
st() { stat -L -c '%Y %s %i' -- \"$1\" 2>/dev/null || stat -L -f '%m %z %i' -- \"$1\" 2>/dev/null; }\n";

/// Exit codes the scripts use (ssh itself uses 255).
const EXIT_NOT_FOUND: i32 = 2;
const EXIT_IS_DIR: i32 = 3;
const EXIT_FAILED: i32 = 4;
const EXIT_CHANGED: i32 = 9;

/// Read a file: its stamp line, then its bytes.
pub fn read_script(path: &str) -> String {
    format!(
        "{PRELUDE}f={}\n\
[ -e \"$f\" ] || exit {EXIT_NOT_FOUND}\n\
[ -d \"$f\" ] && exit {EXIT_IS_DIR}\n\
[ -r \"$f\" ] || {{ echo \"$f: Permission denied\" >&2; exit {EXIT_FAILED}; }}\n\
s=$(st \"$f\")\n\
printf '\\001macros-ok\\n%s\\n' \"$s\"\n\
exec cat -- \"$f\"\n",
        sh_path(path)
    )
}

/// Classify a path: `d` (directory), `f` (anything else that exists) or `-`.
pub fn kind_script(path: &str) -> String {
    format!(
        "f={}\nprintf '\\001macros-ok\\n'\n\
if [ -d \"$f\" ]; then echo d; elif [ -e \"$f\" ] || [ -L \"$f\" ]; then echo f; else echo -; fi\n",
        sh_path(path)
    )
}

/// List a directory: NUL-terminated `d/name` / `f/name` records.
pub fn list_script(path: &str) -> String {
    format!(
        "d={}\n\
cd -- \"$d\" 2>/dev/null || {{ [ -e \"$d\" ] || exit {EXIT_NOT_FOUND}; echo \"$d: cannot open directory\" >&2; exit {EXIT_FAILED}; }}\n\
printf '\\001macros-ok\\n'\n\
for f in .* *; do\n\
  case $f in .|..) continue;; esac\n\
  if [ -d \"$f\" ]; then t=d; elif [ -e \"$f\" ] || [ -L \"$f\" ]; then t=f; else continue; fi\n\
  printf '%s/%s\\000' \"$t\" \"$f\"\n\
done\n",
        sh_path(path)
    )
}

/// Write stdin to a file atomically (see the module doc). `expect` is the
/// stamp the caller's copy is based on, `none` (it didn't exist) or `*` (don't
/// check); on a mismatch the script prints the current stamp and exits
/// [`EXIT_CHANGED`] without writing. On success it prints the new stamp.
pub fn write_script(path: &str, expect: &str) -> String {
    format!(
        "{PRELUDE}f={path}; exp={expect}\n\
if [ -L \"$f\" ]; then t=$(readlink -f -- \"$f\" 2>/dev/null) && [ -n \"$t\" ] && f=$t; fi\n\
cur=; [ -e \"$f\" ] && cur=$(st \"$f\")\n\
if [ \"$exp\" != '*' ] && [ -n \"$cur\" ] && [ \"$cur\" != \"$exp\" ]; then printf '\\001macros-ok\\n%s\\n' \"$cur\"; exit {EXIT_CHANGED}; fi\n\
um=$(umask); inplace=\n\
if [ -e \"$f\" ]; then\n\
  [ -d \"$f\" ] && {{ echo \"$f: Is a directory\" >&2; exit {EXIT_FAILED}; }}\n\
  [ -w \"$f\" ] || {{ echo \"$f is not writable\" >&2; exit {EXIT_FAILED}; }}\n\
  set -- $(stat -L -c '%a %h %u' -- \"$f\" 2>/dev/null || stat -L -f '%Lp %l %u' -- \"$f\" 2>/dev/null)\n\
  mode=${{1:-644}}; links=${{2:-1}}; owner=${{3:-$(id -u)}}\n\
  if [ \"$links\" -gt 1 ] || [ \"$owner\" != \"$(id -u)\" ]; then inplace=1; fi\n\
else\n\
  mode=$(printf '%o' $(( 0666 & ~0$um )))\n\
fi\n\
if [ -z \"$inplace\" ]; then\n\
  tmp=\"$(dirname -- \"$f\")/.$(basename -- \"$f\").macros-save-$$\"\n\
  if (umask 077; set -C; : > \"$tmp\") 2>/dev/null; then\n\
    if cat > \"$tmp\" && chmod \"$mode\" \"$tmp\" && mv -f -- \"$tmp\" \"$f\"; then :; else rm -f -- \"$tmp\"; exit {EXIT_FAILED}; fi\n\
  elif [ -e \"$f\" ]; then inplace=1\n\
  else echo \"$f: cannot create file (missing or unwritable directory)\" >&2; exit {EXIT_FAILED}\n\
  fi\n\
fi\n\
if [ -n \"$inplace\" ]; then cat > \"$f\" || exit {EXIT_FAILED}; fi\n\
printf '\\001macros-ok\\n%s\\n' \"$(st \"$f\")\"\n",
        path = sh_path(path),
        expect = sh_quote(expect),
    )
}

/// Run `program` with `args` (each one word) in the host's home directory.
pub fn command_script(program: &str, args: &[String]) -> String {
    let mut line = sh_quote(program);
    for arg in args {
        line.push(' ');
        line.push_str(&sh_path(arg));
    }
    format!("cd || exit {EXIT_FAILED}\nprintf '\\001macros-ok\\n'\n{line}\n")
}

// --- Running ssh -------------------------------------------------------------

/// The `ssh` invocation for one operation on `host` (without stdin).
pub fn ssh_command(host: &Host, remote_command: &str) -> Command {
    let mut cmd = Command::new("ssh");
    for opt in [
        // Never prompt (passwords, passphrases, unknown host keys): fail with
        // a message instead — macros has no business handling credentials.
        "BatchMode=yes",
        "ConnectTimeout=15",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
    ] {
        cmd.arg("-o").arg(opt);
    }
    if let Some(path) = control_path(host) {
        cmd.arg("-o").arg("ControlMaster=auto");
        cmd.arg("-o").arg(format!("ControlPath={}", path.display()));
        cmd.arg("-o").arg("ControlPersist=600");
    }
    // No pty, no escape character: stdin is a byte stream (the file to write).
    cmd.args(["-T", "-e", "none"]);
    if let Some(port) = host.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &host.user {
        cmd.arg("-l").arg(user);
    }
    // `--` ends ssh's options, so no host can be read as one (parse also
    // rejects a leading `-`).
    cmd.arg("--").arg(&host.host).arg(remote_command);
    cmd
}

/// The shared-connection socket for `host`: a short hashed name (Unix socket
/// paths are limited to ~104 bytes) in a private, per-user directory. `None`
/// on Windows (its OpenSSH has no ControlMaster) or when the directory can't
/// be made safe.
fn control_path(host: &Host) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::hash::{Hash, Hasher};
        let dir = control_dir()?;
        let mut h = rustc_hash::FxHasher::default();
        host.hash(&mut h);
        Some(dir.join(format!("{:016x}", h.finish())))
    }
    #[cfg(not(unix))]
    {
        let _ = host;
        None
    }
}

#[cfg(unix)]
fn control_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        // `/tmp`, not `$TMPDIR`: macOS's per-user temp dir is long enough to
        // push the socket path past the limit.
        let dir = PathBuf::from(format!("/tmp/macros-ssh-{uid}"));
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
        // A shared /tmp: only use a directory that is ours and private.
        let meta = std::fs::symlink_metadata(&dir).ok()?;
        let private = meta.is_dir() && meta.uid() == uid && meta.permissions().mode() & 0o077 == 0;
        private.then_some(dir)
    })
    .clone()
}

/// Whether a master connection to `host` is (probably) up.
fn connected(host: &Host) -> bool {
    control_path(host).is_some_and(|p| p.exists())
}

/// The outcome of one script run.
#[derive(Debug)]
pub struct Output {
    pub code: i32,
    /// Stdout after the [`SENTINEL`] (rc-file noise before it dropped).
    pub stdout: Vec<u8>,
    pub stderr: String,
}

static NEXT_OP: AtomicU64 = AtomicU64::new(0);

/// A running ssh, killable by `C-g`.
struct Running {
    child: Arc<Mutex<Child>>,
    cancelled: Arc<AtomicBool>,
}

fn running() -> &'static Mutex<HashMap<u64, Running>> {
    static RUNNING: OnceLock<Mutex<HashMap<u64, Running>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// Kill every remote operation in flight (`C-g`). Returns how many there were.
pub fn cancel_all() -> usize {
    let running = running().lock().unwrap_or_else(|e| e.into_inner());
    for op in running.values() {
        op.cancelled.store(true, Ordering::SeqCst);
        let _ = op.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
    running.len()
}

/// A "host" whose commands run through a local `/bin/sh -c` (standing in for
/// the remote login shell) instead of ssh — tests of the whole remote path
/// without a server.
#[cfg(all(test, unix))]
pub(crate) const TEST_HOST: &str = "macros-test-local";

/// The command that runs a script on `host`. Tests run it through a local
/// `/bin/sh -c`, standing in for the remote login shell.
fn transport(host: &Host, remote_command: &str) -> Command {
    #[cfg(all(test, unix))]
    if host.host == TEST_HOST {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(remote_command);
        return cmd;
    }
    ssh_command(host, remote_command)
}

/// Run `script` on `host`, feeding `data` to it on stdin after the script.
pub fn exec(host: &Host, script: &str, data: &[u8]) -> io::Result<Output> {
    if !connected(host) {
        announce(host);
    }
    let mut cmd = transport(host, &login_command(script.len()));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .hide_console();
    let mut child = cmd.spawn().map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                "ssh not found — install OpenSSH to edit remote files",
            )
        } else {
            err
        }
    })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let child = Arc::new(Mutex::new(child));
    let cancelled = Arc::new(AtomicBool::new(false));
    let id = NEXT_OP.fetch_add(1, Ordering::Relaxed);
    running().lock().unwrap_or_else(|e| e.into_inner()).insert(
        id,
        Running {
            child: child.clone(),
            cancelled: cancelled.clone(),
        },
    );
    let mut input = Vec::with_capacity(script.len() + data.len());
    input.extend_from_slice(script.as_bytes());
    input.extend_from_slice(data);
    // A script that exits early (a refused write) closes its stdin: the
    // broken pipe is expected, not an error.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let mut out = Vec::new();
    let read = stdout.read_to_end(&mut out);
    let err = err_reader.join().unwrap_or_default();
    let _ = writer.join();
    let status = child.lock().unwrap_or_else(|e| e.into_inner()).wait();
    running()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    if cancelled.load(Ordering::SeqCst) {
        settle(host, false);
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    read?;
    let code = status?.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&err).trim().to_string();
    if code == 255 {
        settle(host, false);
        return Err(ssh_failure(host, &stderr));
    }
    settle(host, true);
    let stdout = match find(&out, SENTINEL) {
        Some(at) => out.split_off(at + SENTINEL.len()),
        None => Vec::new(),
    };
    Ok(Output {
        code,
        stdout,
        stderr,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// An ssh-level failure (exit 255), with the fix spelled out for the common
/// cases.
fn ssh_failure(host: &Host, stderr: &str) -> io::Error {
    let name = host.spec();
    let lower = stderr.to_ascii_lowercase();
    let message = if lower.contains("permission denied") || lower.contains("passphrase") {
        format!(
            "ssh {name}: authentication needs a password or passphrase, which macros never asks \
             for — add a key to ssh-agent (ssh-add) or ~/.ssh/config, then retry"
        )
    } else if lower.contains("host key verification failed") {
        format!(
            "ssh {name}: the host key is unknown or changed — run `{}` once in a terminal \
             to verify it, then retry",
            host.ssh_invocation()
        )
    } else if stderr.is_empty() {
        format!("ssh {name}: connection failed")
    } else {
        format!(
            "ssh {name}: {}",
            stderr.lines().last().unwrap_or(stderr).trim()
        )
    };
    io::Error::new(io::ErrorKind::ConnectionRefused, message)
}

/// The error a script's non-zero exit stands for.
fn script_error(out: &Output, what: &str) -> io::Error {
    let kind = match out.code {
        EXIT_NOT_FOUND => io::ErrorKind::NotFound,
        EXIT_IS_DIR => io::ErrorKind::IsADirectory,
        _ => io::ErrorKind::Other,
    };
    let detail = match out.code {
        EXIT_NOT_FOUND => "No such file or directory".to_string(),
        EXIT_IS_DIR => "Is a directory".to_string(),
        _ if out.stderr.is_empty() => format!("failed (exit {})", out.code),
        _ => out.stderr.lines().last().unwrap_or_default().to_string(),
    };
    io::Error::new(kind, format!("{what}: {detail}"))
}

// --- Status --------------------------------------------------------------------

type Subscribers = Vec<futures::channel::mpsc::UnboundedSender<String>>;

fn subscribers() -> &'static Mutex<Subscribers> {
    static SUBS: OnceLock<Mutex<Subscribers>> = OnceLock::new();
    SUBS.get_or_init(Default::default)
}

/// Status lines for the echo area ("Connecting to host…", failures of
/// background listings). Each editor window subscribes once.
pub fn subscribe() -> futures::channel::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    subscribers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tx);
    rx
}

/// Publish `message` to every subscribed window.
pub fn report(message: impl Into<String>) {
    let message = message.into();
    subscribers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|tx| tx.unbounded_send(message.clone()).is_ok());
}

/// Hosts a "Connecting…" was shown for: when, and whether the attempt has
/// since finished.
fn connecting() -> &'static Mutex<HashMap<Host, (Instant, bool)>> {
    static CONNECTING: OnceLock<Mutex<HashMap<Host, (Instant, bool)>>> = OnceLock::new();
    CONNECTING.get_or_init(Default::default)
}

fn announce(host: &Host) {
    let mut connecting = connecting().lock().unwrap_or_else(|e| e.into_inner());
    // Without a control socket (Windows) every operation reconnects; say so
    // once a minute, not on every keystroke of a completion.
    let fresh = connecting
        .get(host)
        .is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(60));
    if fresh {
        connecting.insert(host.clone(), (Instant::now(), false));
        report(format!("Connecting to {}… (C-g to cancel)", host.spec()));
    }
}

/// An operation on `host` finished (`ok`: the connection worked). Follows up
/// a "Connecting…" once; a failure forgets it, so a retry announces again.
fn settle(host: &Host, ok: bool) {
    let mut connecting = connecting().lock().unwrap_or_else(|e| e.into_inner());
    let Some((_, done)) = connecting.get_mut(host) else {
        return;
    };
    if !*done && ok {
        report(format!("Connected to {}", host.spec()));
    }
    *done = true;
    if !ok || connected(host) {
        connecting.remove(host);
    }
}

/// Run `work` on a fresh thread (network waits must not tie up a shared
/// executor's workers) and await its result.
pub fn spawn<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> impl std::future::Future<Output = Option<T>> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    async move { rx.await.ok() }
}

// --- File operations -------------------------------------------------------------

/// What a buffer's text is based on: the file's stamp and a hash of its bytes
/// when read or written (`None`: the file didn't exist).
type Baseline = Option<(String, u64)>;

fn baselines() -> &'static Mutex<HashMap<String, Baseline>> {
    static BASE: OnceLock<Mutex<HashMap<String, Baseline>>> = OnceLock::new();
    BASE.get_or_init(Default::default)
}

fn content_hash(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write(bytes);
    h.write_usize(bytes.len());
    h.finish()
}

/// A buffer now holds `contents`, which is what `remote` held at `stamp`
/// (`None`: it didn't exist). The next checked save refuses to overwrite a
/// file that has changed since.
pub fn note_synced(remote: &RemotePath, stamp: Option<&str>, contents: &[u8]) {
    let entry = stamp.map(|s| (s.to_string(), content_hash(contents)));
    baselines()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(remote.to_string(), entry);
}

/// Forget what `path`'s buffer was based on, so its next save overwrites
/// whatever is there (the user answered "save over it anyway").
pub fn forget_baseline(path: &Path) {
    if let Some(remote) = parse_path(path) {
        baselines()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&remote.to_string());
    }
}

/// The error a checked save fails with when the file changed on the host
/// since its buffer read it.
#[derive(Debug)]
pub struct ChangedOnDisk;

impl std::fmt::Display for ChangedOnDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("changed on the remote host since it was read")
    }
}

impl std::error::Error for ChangedOnDisk {}

/// Whether `err` is a refused save ([`ChangedOnDisk`]).
pub fn is_changed_on_disk(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|e| e.is::<ChangedOnDisk>())
}

/// A file's bytes and the stamp they were read at.
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub stamp: String,
}

/// Read a remote file. `NotFound` / `IsADirectory` errors say which.
pub fn read_file(remote: &RemotePath) -> io::Result<Fetched> {
    let out = exec(&remote.host, &read_script(&remote.path), &[])?;
    if out.code != 0 {
        return Err(script_error(&out, &remote.to_string()));
    }
    let mut stdout = out.stdout;
    let line_end = stdout
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| io::Error::other("unexpected output from the remote shell"))?;
    let bytes = stdout.split_off(line_end + 1);
    let stamp = String::from_utf8_lossy(&stdout[..line_end]).into_owned();
    Ok(Fetched { bytes, stamp })
}

/// Read a file and record it as a buffer's baseline (visit / revert).
pub fn read_for_buffer(remote: &RemotePath) -> io::Result<Vec<u8>> {
    match read_file(remote) {
        Ok(read) => {
            note_synced(remote, Some(&read.stamp), &read.bytes);
            Ok(read.bytes)
        }
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                note_synced(remote, None, &[]);
            }
            Err(err)
        }
    }
}

/// Write `contents` to a remote file atomically. With `checked`, refuse (with
/// [`ChangedOnDisk`]) when the file changed since the buffer's baseline, and
/// record the written text as the new baseline — a buffer save. Unchecked, the
/// write records nothing (a script's `write-file`, like the local one).
pub fn write_file(remote: &RemotePath, contents: &[u8], checked: bool) -> io::Result<()> {
    let key = remote.to_string();
    let baseline = || {
        baselines()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    };
    let mut expect = match checked.then(baseline).flatten() {
        None => "*".to_string(),
        Some(None) => "none".to_string(),
        Some(Some((stamp, _))) => stamp,
    };
    invalidate_listings(&remote.host);
    // At most one retry: the stamp moved but the bytes didn't (a `touch`).
    for _ in 0..2 {
        let out = exec(&remote.host, &write_script(&remote.path, &expect), contents)?;
        let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
        match out.code {
            0 => {
                if checked {
                    note_synced(remote, Some(&stamp), contents);
                }
                return Ok(());
            }
            EXIT_CHANGED => {
                let known_hash = baseline().flatten().map(|(_, hash)| hash);
                let same = match known_hash {
                    Some(hash) => read_file(remote)
                        .is_ok_and(|now| content_hash(&now.bytes) == hash && now.stamp == stamp),
                    None => false,
                };
                if !same {
                    return Err(io::Error::other(ChangedOnDisk));
                }
                expect = stamp;
            }
            _ => return Err(script_error(&out, &key)),
        }
    }
    Err(io::Error::other(ChangedOnDisk))
}

/// One directory entry: its full remote path (`/` appended for directories).
type Listing = Vec<String>;

/// Recent listings, so typing a completion pattern doesn't relist the same
/// directory on every keystroke.
fn listings() -> &'static Mutex<HashMap<String, (Instant, Listing)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Listing)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

const LISTING_TTL: Duration = Duration::from_secs(5);

fn invalidate_listings(host: &Host) {
    let prefix_of = |key: &str| parse(key).is_some_and(|r| &r.host == host);
    listings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|key, _| !prefix_of(key));
}

/// The entries of a remote directory as full paths, directories ending in `/`,
/// sorted (the shape of the local `list-directory`).
pub fn list_dir(remote: &RemotePath) -> io::Result<Listing> {
    let key = remote.to_string();
    if let Some((at, listing)) = listings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        && at.elapsed() < LISTING_TTL
    {
        return Ok(listing.clone());
    }
    let out = exec(&remote.host, &list_script(&remote.path), &[])?;
    if out.code != 0 {
        return Err(script_error(&out, &key));
    }
    let base = if key.ends_with('/') {
        key.clone()
    } else {
        format!("{key}/")
    };
    let mut entries: Listing = out
        .stdout
        .split(|&b| b == 0)
        .filter_map(|rec| {
            let rec = std::str::from_utf8(rec).ok()?;
            let (kind, name) = rec.split_once('/')?;
            (!name.is_empty()).then(|| match kind {
                "d" => format!("{base}{name}/"),
                _ => format!("{base}{name}"),
            })
        })
        .collect();
    entries.sort();
    listings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, (Instant::now(), entries.clone()));
    Ok(entries)
}

/// What a remote path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Missing,
    File,
    Dir,
}

/// Classify a remote path, from a recent listing of its directory when there
/// is one (no round trip).
pub fn kind(remote: &RemotePath) -> io::Result<Kind> {
    if let Some(parent) = remote.parent() {
        let key = parent.to_string();
        if let Some((at, listing)) = listings()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            && at.elapsed() < LISTING_TTL
        {
            let full = remote.to_string();
            return Ok(if listing.iter().any(|e| *e == format!("{full}/")) {
                Kind::Dir
            } else if listing.contains(&full) {
                Kind::File
            } else {
                Kind::Missing
            });
        }
    }
    let out = exec(&remote.host, &kind_script(&remote.path), &[])?;
    if out.code != 0 {
        return Err(script_error(&out, &remote.to_string()));
    }
    Ok(match out.stdout.first() {
        Some(b'd') => Kind::Dir,
        Some(b'f') => Kind::File,
        _ => Kind::Missing,
    })
}

/// Run `program args…` on the host the remote paths in `args` name (their
/// prefixes stripped; other arguments passed as-is). The shape of
/// `call-process-full`: `(exit code, stdout, stderr)`. All remote arguments
/// must be on one host.
pub fn run_program(program: &str, args: &[String]) -> io::Result<(i32, String, String)> {
    let mut host: Option<Host> = None;
    let mut plain = Vec::with_capacity(args.len());
    for arg in args {
        match parse(arg) {
            Some(remote) => {
                if host.as_ref().is_some_and(|h| *h != remote.host) {
                    return Err(io::Error::other(
                        "files are on different hosts — copying between hosts isn't supported",
                    ));
                }
                host = Some(remote.host);
                plain.push(remote.path);
            }
            None if arg.starts_with('/') || arg.starts_with('~') => {
                return Err(io::Error::other(format!(
                    "{arg} is a local path, the others are remote — copying between hosts isn't supported"
                )));
            }
            None => plain.push(arg.clone()),
        }
    }
    let host = host.ok_or_else(|| io::Error::other("no remote path in the arguments"))?;
    invalidate_listings(&host);
    let out = exec(&host, &command_script(program, &plain), &[])?;
    Ok((
        out.code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.stderr,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    pub(super) use super::TEST_HOST as LOCAL_HOST;

    #[test]
    fn parses_tramp_paths() {
        let r = parse("/ssh:me@example.com#2222:/etc/hosts").unwrap();
        assert_eq!(r.method, "ssh");
        assert_eq!(r.host.user.as_deref(), Some("me"));
        assert_eq!(r.host.host, "example.com");
        assert_eq!(r.host.port, Some(2222));
        assert_eq!(r.path, "/etc/hosts");
        assert_eq!(r.to_string(), "/ssh:me@example.com#2222:/etc/hosts");

        let r = parse("/scp:box:/srv/a b/c.txt").unwrap();
        assert_eq!(r.method, "scp");
        assert_eq!((r.host.user.clone(), r.host.port), (None, None));
        assert_eq!(r.path, "/srv/a b/c.txt");

        // Empty and relative paths are in the home directory.
        assert_eq!(parse("/ssh:box:").unwrap().path, "~");
        assert_eq!(parse("/ssh:box:notes.org").unwrap().path, "~/notes.org");
        assert_eq!(parse("/ssh:box:~/x//y/./z/../w").unwrap().path, "~/x/y/w");
        assert_eq!(parse("/ssh:box:/a/../../b/").unwrap().path, "/b");
        assert_eq!(parse("/ssh:box:~/../x").unwrap().path, "~/../x");

        // IPv6, in brackets.
        let r = parse("/ssh:root@[fe80::1%en0]#22:/").unwrap();
        assert_eq!(r.host.host, "fe80::1%en0");
        assert_eq!(r.to_string(), "/ssh:root@[fe80::1%en0]#22:/");
    }

    #[test]
    fn rejects_non_remote_and_malformed_paths() {
        for bad in [
            "/etc/hosts",
            "ssh:box:/x",
            "/ftp:box:/x",
            "/ssh:",
            "/ssh::/x",                    // no host
            "/ssh:box",                    // no path separator
            "/ssh:-oProxyCommand=evil:/x", // would be an ssh option
            "/ssh:-l:/x",
            "/ssh:-me@box:/x",
            "/ssh:box#0:/x",
            "/ssh:box#99999:/x",
            "/ssh:box#22x:/x",
            "/ssh:bo x:/x",
            "/ssh:box;rm:/x",
            "/ssh:me$(id)@box:/x",
        ] {
            assert_eq!(parse(bad), None, "{bad} should not parse");
        }
        assert!(!is_remote(Path::new("/home/me/ssh:notes")));
    }

    #[test]
    fn parents_keep_the_host_prefix() {
        let parent = |s: &str| parent_of(Path::new(s)).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(parent("/ssh:box:/a/b").as_deref(), Some("/ssh:box:/a"));
        assert_eq!(parent("/ssh:box:/a").as_deref(), Some("/ssh:box:/"));
        assert_eq!(parent("/ssh:box:/"), None);
        assert_eq!(parent("/ssh:box:~/x").as_deref(), Some("/ssh:box:~"));
        assert_eq!(parent("/ssh:box:~").as_deref(), Some("/ssh:box:~/.."));
        assert_eq!(parent("/local/x").as_deref(), Some("/local"));
    }

    #[test]
    fn quoting_is_one_inert_word() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_path("~"), "\"$HOME\"");
        assert_eq!(sh_path("~/a b"), "\"$HOME\"'/a b'");
        assert_eq!(sh_path("/x/~/y"), "'/x/~/y'");
        // The login-shell command is fixed text with nothing a shell other
        // than /bin/sh would interpret differently.
        let cmd = login_command(1234);
        assert!(!cmd.contains(['\\', '!', '\n', '`']), "{cmd}");
        assert!(cmd.contains("count=1234"));
    }

    #[test]
    fn ssh_command_is_batch_mode_and_ends_options_before_the_host() {
        let host = parse("/ssh:me@box#2200:/x").unwrap().host;
        let cmd = ssh_command(&host, "REMOTE");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(cmd.get_program(), "ssh");
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-p", "2200"]));
        assert!(args.windows(2).any(|w| w == ["-l", "me"]));
        assert_eq!(args[args.len() - 3..], ["--", "box", "REMOTE"]);
        #[cfg(unix)]
        assert!(args.iter().any(|a| a == "ControlMaster=auto"));
    }

    #[test]
    fn ssh_failures_say_how_to_fix_them() {
        let host = parse("/ssh:me@box#2200:/x").unwrap().host;
        let auth = ssh_failure(&host, "me@box: Permission denied (publickey).").to_string();
        assert!(auth.contains("ssh-add"), "{auth}");
        let key = ssh_failure(&host, "Host key verification failed.").to_string();
        assert!(key.contains("`ssh -p 2200 me@box`"), "{key}");
    }

    #[cfg(unix)]
    mod local {
        use super::super::*;
        use super::LOCAL_HOST;

        /// Paths no shell may misread: spaces, quotes, globs, `$()`,
        /// backticks, `;`, `|`, `&`, newlines, a leading dash.
        const NASTY: &[&str] = &[
            "with space.txt",
            "it's.txt",
            "dq\"uote.txt",
            "$(touch PWNED).txt",
            "`touch PWNED`.txt",
            "semi;touch PWNED.txt",
            "pipe|touch PWNED.txt",
            "amp&touch PWNED&.txt",
            "glob*?[x].txt",
            "back\\slash.txt",
            "new\nline.txt",
            "-dash.txt",
            "'; touch PWNED; '.txt",
            "$HOME.txt",
            "%s%n.txt",
        ];

        fn scratch(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "macros-remote-{tag}-{}-{}",
                std::process::id(),
                NEXT_OP.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn remote(path: &Path) -> RemotePath {
            parse(&format!("/ssh:{LOCAL_HOST}:{}", path.display())).unwrap()
        }

        #[test]
        fn read_write_round_trips_nasty_names_without_injection() {
            let dir = scratch("nasty");
            for name in NASTY {
                let file = dir.join(name);
                let r = remote(&file);
                let body: Vec<u8> = format!("body of {name}\n\0binary\x01\r\n").into_bytes();
                write_file(&r, &body, false).unwrap();
                assert_eq!(std::fs::read(&file).unwrap(), body, "{name}");
                let read = read_file(&r).unwrap();
                assert_eq!(read.bytes, body, "{name}");
                assert!(!read.stamp.is_empty());
            }
            let listed = list_dir(&remote(&dir)).unwrap();
            for name in NASTY {
                let full = format!("{}/{name}", remote(&dir));
                assert!(listed.contains(&full), "{name} missing from {listed:?}");
            }
            assert!(!dir.join("PWNED").exists());
            assert!(!Path::new("PWNED").exists());
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn read_reports_missing_and_directories() {
            let dir = scratch("kinds");
            let missing = read_file(&remote(&dir.join("nope"))).err().unwrap();
            assert_eq!(missing.kind(), io::ErrorKind::NotFound);
            let is_dir = read_file(&remote(&dir)).err().unwrap();
            assert_eq!(is_dir.kind(), io::ErrorKind::IsADirectory);
            std::fs::write(dir.join("f"), "x").unwrap();
            std::fs::create_dir(dir.join("sub")).unwrap();
            assert_eq!(kind(&remote(&dir.join("f"))).unwrap(), Kind::File);
            assert_eq!(kind(&remote(&dir.join("sub"))).unwrap(), Kind::Dir);
            assert_eq!(kind(&remote(&dir.join("zz"))).unwrap(), Kind::Missing);
            let listed = list_dir(&remote(&dir)).unwrap();
            assert_eq!(
                listed,
                vec![
                    format!("{}/f", remote(&dir)),
                    format!("{}/sub/", remote(&dir))
                ]
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn writes_keep_mode_follow_symlinks_and_leave_no_temp_files() {
            use std::os::unix::fs::PermissionsExt;
            let dir = scratch("mode");
            let file = dir.join("script.sh");
            std::fs::write(&file, "old").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
            write_file(&remote(&file), b"new", false).unwrap();
            let meta = std::fs::metadata(&file).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o750);
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");

            let link = dir.join("link");
            std::os::unix::fs::symlink("script.sh", &link).unwrap();
            write_file(&remote(&link), b"through", false).unwrap();
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "through");

            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains("macros-save"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn checked_saves_refuse_to_clobber_external_changes() {
            let dir = scratch("check");
            let file = dir.join("f.txt");
            let r = remote(&file);
            std::fs::write(&file, "one\n").unwrap();
            assert_eq!(read_for_buffer(&r).unwrap(), b"one\n");
            // Our own save is the new baseline.
            write_file(&r, b"mine\n", true).unwrap();
            write_file(&r, b"mine again\n", true).unwrap();
            // Someone else rewrites it (a different size, so the stamp moves
            // even within one mtime second).
            std::fs::write(&file, "theirs, longer\n").unwrap();
            let err = write_file(&r, b"clobber\n", true).unwrap_err();
            assert!(is_changed_on_disk(&err), "{err}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "theirs, longer\n");
            // "Save over it anyway" forgets the baseline.
            forget_baseline(Path::new(&r.to_string()));
            write_file(&r, b"forced\n", true).unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "forced\n");
            // Rewritten with the same bytes (new inode): not a change.
            std::fs::remove_file(&file).unwrap();
            std::fs::write(&file, "forced\n").unwrap();
            write_file(&r, b"after touch\n", true).unwrap();
            // A deleted file is simply recreated.
            std::fs::remove_file(&file).unwrap();
            write_file(&r, b"recreated\n", true).unwrap();
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "recreated\n");
            // Visited as new, then created by someone else: a change.
            let new = remote(&dir.join("new.txt"));
            assert!(read_for_buffer(&new).is_err());
            std::fs::write(dir.join("new.txt"), "theirs").unwrap();
            assert!(is_changed_on_disk(
                &write_file(&new, b"x", true).unwrap_err()
            ));
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn programs_run_with_quoted_remote_arguments() {
            let dir = scratch("prog");
            let src = dir.join("a $(touch PWNED) b");
            std::fs::write(&src, "x").unwrap();
            let dest = dir.join("c'd");
            let (code, _, err) =
                run_program("mv", &[remote(&src).to_string(), remote(&dest).to_string()]).unwrap();
            assert_eq!(code, 0, "{err}");
            assert!(dest.exists() && !src.exists());
            assert!(!dir.join("PWNED").exists());
            // Mixing local and remote paths is refused, not guessed at.
            assert!(run_program("cp", &[remote(&dest).to_string(), "/tmp/x".into()]).is_err());
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn rc_file_noise_before_the_sentinel_is_skipped() {
            let host = Host {
                user: None,
                host: LOCAL_HOST.into(),
                port: None,
            };
            let out = exec(
                &host,
                "echo NOISE; printf '\\001macros-ok\\n'; echo real",
                &[],
            )
            .unwrap();
            assert_eq!(out.stdout, b"real\n");
        }
    }
}
