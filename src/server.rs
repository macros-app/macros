//! The edit server, Emacs `server-start` / `emacsclient` style: a running
//! Macros listens on a per-user local socket, and `macros --client FILE…`
//! hands files to it instead of starting a second editor. With `--wait` (the
//! default, as with emacsclient) the client blocks until the user is done with
//! every file it sent — `C-x #` (`server-edit`) saves and releases a buffer —
//! so `EDITOR="macros --client"` works for `git commit`.
//!
//! Transport: on Unix, a socket at `$XDG_RUNTIME_DIR/macros/server` (or
//! `$TMPDIR/macros-<uid>/server`), in a directory only the user can enter and
//! with mode 0600, so the file system is the access check. Windows has no such
//! socket permissions, so there the server listens on an ephemeral 127.0.0.1
//! port and writes `ADDRESS TOKEN` to `%LOCALAPPDATA%\macros\server`; every
//! request must carry that random token (Emacs's `server-use-tcp` scheme).
//!
//! Protocol (version [`PROTOCOL_VERSION`]): newline-delimited JSON. The client
//! sends one [`Request`]; the server answers [`Reply::Opened`] once the files
//! are visited and, for a waiting client, [`Reply::Done`] once all of them
//! have been released. Any problem is a [`Reply::Error`].
//!
//! This module has no editor access: requests reach the UI through the
//! dispatch function `main.rs` installs ([`set_dispatch`]), and the editor
//! reports finished buffers back through [`hold`] / [`release`].

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`Request`] / [`Reply`].
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest request line accepted (thousands of paths fit).
const MAX_REQUEST: u64 = 1 << 20;
/// How long a client has to send its request after connecting.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a reply write may block before the client is given up on.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// One file for the server to visit; `line` and `column` are 1-based.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileArg {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
}

/// What a client asks for. Paths are absolute (the client resolves them
/// against its own working directory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    /// The TCP transport's shared secret; empty over a Unix socket.
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub wait: bool,
    #[serde(default)]
    pub files: Vec<FileArg>,
}

/// A server → client message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "lowercase")]
pub enum Reply {
    /// The files are open in the editor.
    Opened {
        files: usize,
    },
    /// Every file a waiting client sent has been released.
    Done,
    Error {
        message: String,
    },
}

/// Parse one request line.
pub fn parse_request(line: &str) -> Result<Request, String> {
    let request: Request =
        serde_json::from_str(line.trim()).map_err(|e| format!("bad request: {e}"))?;
    if request.version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version {} not supported (this Macros speaks {PROTOCOL_VERSION})",
            request.version
        ));
    }
    Ok(request)
}

/// Encode a message as its wire line (JSON + `\n`).
fn encode<T: Serialize>(message: &T) -> String {
    let mut line = serde_json::to_string(message).expect("plain data serializes");
    line.push('\n');
    line
}

// ── Transport ───────────────────────────────────────────────────────────────

/// How the server is reached. [`Transport::native`] picks the platform's; both
/// compile everywhere so the TCP path is tested on Unix CI too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A Unix socket at the server path.
    #[cfg(unix)]
    Unix,
    /// 127.0.0.1 + a token, both written to the server path. Only Windows
    /// uses it outside tests.
    #[cfg_attr(unix, allow(dead_code))]
    Tcp,
}

impl Transport {
    pub fn native() -> Transport {
        #[cfg(unix)]
        return Transport::Unix;
        #[cfg(not(unix))]
        return Transport::Tcp;
    }
}

/// The per-user server path: the socket (Unix) or the address + token file
/// (Windows).
pub fn default_path() -> PathBuf {
    #[cfg(unix)]
    {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty());
        let dir = match runtime {
            Some(dir) => PathBuf::from(dir).join("macros"),
            // SAFETY: getuid has no preconditions and cannot fail.
            None => std::env::temp_dir().join(format!("macros-{}", unsafe { libc::getuid() })),
        };
        dir.join("server")
    }
    #[cfg(not(unix))]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("macros").join("server")
    }
}

/// A byte stream to or from a client.
trait Stream: Read + Write + Send {
    fn try_clone_box(&self) -> std::io::Result<Box<dyn Stream>>;
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>);
}

impl Stream for TcpStream {
    fn try_clone_box(&self) -> std::io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) {
        let _ = self.set_read_timeout(read);
        let _ = self.set_write_timeout(write);
    }
}

#[cfg(unix)]
impl Stream for std::os::unix::net::UnixStream {
    fn try_clone_box(&self) -> std::io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) {
        let _ = self.set_read_timeout(read);
        let _ = self.set_write_timeout(write);
    }
}

enum Listener {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    fn accept(&self) -> std::io::Result<Box<dyn Stream>> {
        match self {
            #[cfg(unix)]
            Listener::Unix(l) => Ok(Box::new(l.accept()?.0)),
            Listener::Tcp(l) => Ok(Box::new(l.accept()?.0)),
        }
    }
}

/// The error a bind returns when a live server already owns the path.
fn in_use(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!("another Macros is serving {}", path.display()),
    )
}

/// Create the server's directory, private to the user (Unix: 0700, and it
/// must be ours — a directory someone else made in a shared /tmp is refused).
fn prepare_dir(path: &Path) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let meta = std::fs::metadata(dir)?;
        // SAFETY: getuid has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::getuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is owned by another user", dir.display()),
            ));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn bind_unix(path: &Path) -> std::io::Result<Listener> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    prepare_dir(path)?;
    let listener = match UnixListener::bind(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(in_use(path));
            }
            // Left behind by a Macros that exited: take it over.
            std::fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        other => other?,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener::Unix(listener))
}

/// The TCP server file's contents: `ADDRESS TOKEN`.
fn read_tcp_file(path: &Path) -> std::io::Result<(String, String)> {
    let text = std::fs::read_to_string(path)?;
    let mut parts = text.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(addr), Some(token)) => Ok((addr.to_string(), token.to_string())),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is malformed", path.display()),
        )),
    }
}

fn bind_tcp(path: &Path) -> std::io::Result<(Listener, String)> {
    prepare_dir(path)?;
    if let Ok((addr, _)) = read_tcp_file(path)
        && TcpStream::connect_timeout(
            &addr.parse().map_err(std::io::Error::other)?,
            Duration::from_millis(300),
        )
        .is_ok()
    {
        return Err(in_use(path));
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = hex::encode(bytes);
    let contents = format!("{} {token}\n", listener.local_addr()?);
    write_private(path, &contents)?;
    Ok((Listener::Tcp(listener), token))
}

/// Write `contents` to `path`, readable by its owner only on Unix (elsewhere
/// the per-user app-data directory's ACLs apply).
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

/// Constant-time token comparison.
fn token_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

// ── Server ──────────────────────────────────────────────────────────────────

/// A connected client, owning the connection's write half.
pub struct Client {
    id: u64,
    out: Box<dyn Stream>,
}

impl Client {
    /// Send `reply`; false when the client has gone away.
    pub fn send(&mut self, reply: &Reply) -> bool {
        self.out.write_all(encode(reply).as_bytes()).is_ok() && self.out.flush().is_ok()
    }
}

/// A request handed to the editor, with the client to answer.
pub struct Incoming {
    pub request: Request,
    pub client: Client,
}

/// Where accepted requests go (the editor's UI thread, in the app).
pub type Dispatch = Arc<dyn Fn(Incoming) + Send + Sync>;

/// A listening server. Dropping it stops listening and removes its file.
pub struct Server {
    pub path: PathBuf,
    transport: Transport,
    stop: Arc<AtomicBool>,
}

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

impl Server {
    /// Listen at `path` on its own thread, handing each valid request to
    /// `dispatch`. Fails with `AddrInUse` when another live server owns `path`.
    pub fn start(path: &Path, transport: Transport, dispatch: Dispatch) -> std::io::Result<Server> {
        let (listener, token) = match transport {
            #[cfg(unix)]
            Transport::Unix => (bind_unix(path)?, String::new()),
            Transport::Tcp => bind_tcp(path)?,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let token = Arc::new(token);
        std::thread::Builder::new()
            .name("edit-server".into())
            .spawn(move || {
                loop {
                    let stream = listener.accept();
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    };
                    let (dispatch, token) = (dispatch.clone(), token.clone());
                    let _ = std::thread::Builder::new()
                        .name("edit-client".into())
                        .spawn(move || serve(stream, &token, &dispatch));
                }
            })?;
        Ok(Server {
            path: path.to_path_buf(),
            transport,
            stop,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocked `accept` so the thread sees the flag and exits
        // (dropping the listener), then remove the file so clients fall back.
        match self.transport {
            #[cfg(unix)]
            Transport::Unix => {
                let _ = std::os::unix::net::UnixStream::connect(&self.path);
            }
            Transport::Tcp => {
                if let Ok((addr, _)) = read_tcp_file(&self.path)
                    && let Ok(addr) = addr.parse()
                {
                    let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(300));
                }
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One connection: read and check the request, dispatch it, then keep the read
/// half open to notice the client leaving (a `git commit` killed with C-c), so
/// its buffers stop counting as waited on.
fn serve(stream: Box<dyn Stream>, token: &str, dispatch: &Dispatch) {
    stream.set_timeouts(Some(REQUEST_TIMEOUT), Some(WRITE_TIMEOUT));
    let Ok(out) = stream.try_clone_box() else {
        return;
    };
    let mut client = Client {
        id: NEXT_CLIENT.fetch_add(1, Ordering::SeqCst),
        out,
    };
    let mut reader = BufReader::new(stream.take(MAX_REQUEST));
    let mut line = String::new();
    let checked = match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) if !line.ends_with('\n') => Err("request too long or not terminated".to_string()),
        Ok(_) => parse_request(&line).and_then(|request| {
            if token.is_empty() || token_matches(&request.token, token) {
                Ok(request)
            } else {
                Err("bad token".to_string())
            }
        }),
        Err(e) => Err(format!("reading request: {e}")),
    };
    let request = match checked {
        Ok(request) => request,
        Err(message) => {
            client.send(&Reply::Error { message });
            return;
        }
    };
    let id = client.id;
    dispatch(Incoming { request, client });
    // The client sends nothing more: this read ends when it disconnects.
    let mut stream = reader.into_inner().into_inner();
    stream.set_timeouts(None, None);
    let mut sink = [0u8; 64];
    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
    forget(id);
}

// ── Waiting clients ─────────────────────────────────────────────────────────

/// A `--wait` client and the files (as editor file identities) it still waits on.
struct Waiting {
    client: Client,
    files: HashSet<PathBuf>,
    /// Files whose buffers the server created for this client; killed when done
    /// (Emacs `server-kill-new-buffers`), so the next `git commit` rereads.
    created: HashSet<PathBuf>,
}

static WAITING: Mutex<Vec<Waiting>> = Mutex::new(Vec::new());

/// Hold `client` until every one of `files` is [`release`]d. With no files it
/// is done at once.
pub fn hold(mut client: Client, files: HashSet<PathBuf>, created: HashSet<PathBuf>) {
    if files.is_empty() {
        client.send(&Reply::Done);
        return;
    }
    WAITING.lock().unwrap().push(Waiting {
        client,
        files,
        created,
    });
}

/// What [`release`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Released {
    /// Some client was waiting on the file.
    pub waited: bool,
    /// The server created the file's buffer for a client.
    pub created: bool,
    /// Clients that are now done (and were told so).
    pub finished: usize,
}

/// The user is done with `file`: drop it from every waiting client, and tell
/// each client with nothing left that it's done.
pub fn release(file: &Path) -> Released {
    let mut out = Released::default();
    let mut waiting = WAITING.lock().unwrap();
    waiting.retain_mut(|w| {
        if !w.files.remove(file) {
            return true;
        }
        out.waited = true;
        out.created |= w.created.remove(file);
        if !w.files.is_empty() {
            return true;
        }
        w.client.send(&Reply::Done);
        out.finished += 1;
        false
    });
    out
}

/// Whether any client is waiting at all (a cheap check before computing a
/// file identity).
pub fn any_waiting() -> bool {
    !WAITING.lock().unwrap().is_empty()
}

/// Whether some client is waiting on `file`.
pub fn is_waited_on(file: &Path) -> bool {
    WAITING
        .lock()
        .unwrap()
        .iter()
        .any(|w| w.files.contains(file))
}

/// Every file some client still waits on, oldest client first.
pub fn waited_files() -> Vec<PathBuf> {
    let waiting = WAITING.lock().unwrap();
    let mut out: Vec<PathBuf> = Vec::new();
    for file in waiting.iter().flat_map(|w| w.files.iter()) {
        if !out.contains(file) {
            out.push(file.clone());
        }
    }
    out
}

/// The client disconnected: stop holding it.
fn forget(id: u64) {
    WAITING.lock().unwrap().retain(|w| w.client.id != id);
}

// ── The process-wide server ────────────────────────────────────────────────

static DISPATCH: OnceLock<Dispatch> = OnceLock::new();
static RUNNING: Mutex<Option<Server>> = Mutex::new(None);

/// Install where requests go; `main` calls this once at startup. Without it
/// (unit tests) [`reconcile`] refuses to start.
pub fn set_dispatch(dispatch: Dispatch) {
    let _ = DISPATCH.set(dispatch);
}

/// What [`reconcile`] did.
#[derive(Debug, PartialEq)]
pub enum Status {
    Started(PathBuf),
    Stopped,
    Unchanged,
    /// Another Macros (a second app instance) is already serving this path.
    OtherInstance(PathBuf),
}

/// Start or stop this process's server to match the `server` option.
pub fn reconcile(enabled: bool) -> Result<Status, String> {
    let mut running = RUNNING.lock().unwrap();
    match (enabled, running.is_some()) {
        (true, true) | (false, false) => Ok(Status::Unchanged),
        (false, true) => {
            *running = None;
            Ok(Status::Stopped)
        }
        (true, false) => {
            let dispatch = DISPATCH
                .get()
                .ok_or("the edit server is unavailable here")?
                .clone();
            let path = default_path();
            match Server::start(&path, Transport::native(), dispatch) {
                Ok(server) => {
                    *running = Some(server);
                    Ok(Status::Started(path))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    Ok(Status::OtherInstance(path))
                }
                Err(e) => Err(format!("{}: {e}", path.display())),
            }
        }
    }
}

/// The running server's path, if this process is serving.
pub fn running_path() -> Option<PathBuf> {
    RUNNING.lock().unwrap().as_ref().map(|s| s.path.clone())
}

// ── The client (`macros --client`) ─────────────────────────────────────────

/// Parsed `macros --client` arguments.
#[derive(Debug, Default, PartialEq)]
pub struct ClientArgs {
    pub wait: bool,
    pub files: Vec<FileArg>,
    /// `--alternate-editor`: run instead when no server answers.
    pub alternate_editor: Option<String>,
}

pub const CLIENT_USAGE: &str = "usage: macros --client [--wait | -n] [--alternate-editor CMD] \
                                [+LINE[:COLUMN]] FILE...";

/// Parse the arguments after `--client`. Relative paths resolve against `cwd`;
/// `+LINE[:COLUMN]` positions the file after it.
pub fn parse_client_args(
    args: impl IntoIterator<Item = String>,
    cwd: &Path,
) -> Result<ClientArgs, String> {
    let mut out = ClientArgs {
        wait: true,
        ..Default::default()
    };
    let mut position: Option<(usize, Option<usize>)> = None;
    let mut args = args.into_iter();
    let mut files_only = false;
    while let Some(arg) = args.next() {
        if !files_only {
            match arg.as_str() {
                "--client" => continue,
                "-w" | "--wait" => {
                    out.wait = true;
                    continue;
                }
                "-n" | "--no-wait" => {
                    out.wait = false;
                    continue;
                }
                "-a" | "--alternate-editor" => {
                    let cmd = args.next().ok_or(format!("{arg} needs a command"))?;
                    out.alternate_editor = Some(cmd);
                    continue;
                }
                "--" => {
                    files_only = true;
                    continue;
                }
                _ => {}
            }
            if let Some(cmd) = arg.strip_prefix("--alternate-editor=") {
                out.alternate_editor = Some(cmd.to_string());
                continue;
            }
            if let Some(spec) = arg.strip_prefix('+')
                && let Some(parsed) = parse_position(spec)
            {
                position = Some(parsed);
                continue;
            }
            if arg.len() > 1 && arg.starts_with('-') {
                return Err(format!("unknown option {arg}"));
            }
        }
        let (line, column) = position.take().map_or((None, None), |(l, c)| (Some(l), c));
        out.files.push(FileArg {
            path: cwd.join(&arg),
            line,
            column,
        });
    }
    Ok(out)
}

/// `LINE` or `LINE:COLUMN` (1-based; 0 is read as 1).
fn parse_position(spec: &str) -> Option<(usize, Option<usize>)> {
    let (line, column) = match spec.split_once(':') {
        Some((l, c)) => (l, Some(c.parse::<usize>().ok()?.max(1))),
        None => (spec, None),
    };
    Some((line.parse::<usize>().ok()?.max(1), column))
}

/// Connect to the server at `path`, returning the stream and the token to send.
fn connect(path: &Path, transport: Transport) -> std::io::Result<(Box<dyn Stream>, String)> {
    match transport {
        #[cfg(unix)]
        Transport::Unix => Ok((
            Box::new(std::os::unix::net::UnixStream::connect(path)?),
            String::new(),
        )),
        Transport::Tcp => {
            let (addr, token) = read_tcp_file(path)?;
            let addr = addr.parse().map_err(std::io::Error::other)?;
            let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
            Ok((Box::new(stream), token))
        }
    }
}

/// How a client run ended.
#[derive(Debug, PartialEq)]
pub enum ClientOutcome {
    /// No server answered at `path` (fall back to another editor).
    NoServer,
    /// The files were opened and, when waiting, released.
    Finished,
    Failed(String),
}

/// Send `args` to the server at `path` and, when waiting, block until it says
/// done. `on_opened` runs once the files are open (the CLI prints a hint).
pub fn send(
    path: &Path,
    transport: Transport,
    args: &ClientArgs,
    on_opened: impl FnOnce(),
) -> ClientOutcome {
    let Ok((mut stream, token)) = connect(path, transport) else {
        return ClientOutcome::NoServer;
    };
    let request = Request {
        version: PROTOCOL_VERSION,
        token,
        wait: args.wait,
        files: args.files.clone(),
    };
    if let Err(e) = stream.write_all(encode(&request).as_bytes()) {
        return ClientOutcome::Failed(format!("sending to the server: {e}"));
    }
    let mut on_opened = Some(on_opened);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                return ClientOutcome::Failed("the server closed the connection".into());
            }
            Ok(_) => {}
        }
        match serde_json::from_str::<Reply>(line.trim()) {
            Ok(Reply::Opened { .. }) => {
                if let Some(f) = on_opened.take() {
                    f();
                }
                if !args.wait {
                    return ClientOutcome::Finished;
                }
            }
            Ok(Reply::Done) => return ClientOutcome::Finished,
            Ok(Reply::Error { message }) => return ClientOutcome::Failed(message),
            Err(e) => return ClientOutcome::Failed(format!("bad reply from the server: {e}")),
        }
    }
}

/// `macros --client …`: the whole client process. Returns the exit code.
pub fn run_client(argv: Vec<String>) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let args = match parse_client_args(argv, &cwd) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("macros: {e}\n{CLIENT_USAGE}");
            return 2;
        }
    };
    let wait = args.wait;
    let outcome = send(&default_path(), Transport::native(), &args, || {
        if wait {
            eprintln!("Waiting for Macros... (C-x # when done)");
        }
    });
    match outcome {
        ClientOutcome::Finished => 0,
        ClientOutcome::Failed(message) => {
            eprintln!("macros: {message}");
            1
        }
        ClientOutcome::NoServer => fallback(&args),
    }
}

/// No server: run `--alternate-editor` (or `$ALTERNATE_EDITOR`) on the files,
/// else start Macros itself on them — waiting for it to exit when the client
/// was asked to wait.
fn fallback(args: &ClientArgs) -> i32 {
    let files: Vec<&Path> = args.files.iter().map(|f| f.path.as_path()).collect();
    let alternate = args
        .alternate_editor
        .clone()
        .or_else(|| std::env::var("ALTERNATE_EDITOR").ok())
        .filter(|cmd| !cmd.trim().is_empty());
    let mut command = match &alternate {
        Some(cmd) => {
            let mut words = cmd.split_whitespace();
            let mut command = std::process::Command::new(words.next().unwrap_or_default());
            command.args(words);
            command
        }
        None => match std::env::current_exe() {
            Ok(exe) => std::process::Command::new(exe),
            Err(e) => {
                eprintln!("macros: no server running, and can't start Macros: {e}");
                return 1;
            }
        },
    };
    command.args(&files);
    if args.wait || alternate.is_some() {
        match command.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(e) => {
                eprintln!("macros: no server running, and the fallback failed: {e}");
                1
            }
        }
    } else {
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match command.spawn() {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("macros: no server running, and can't start Macros: {e}");
                1
            }
        }
    }
}

/// A client connected over loopback, and the far end it replies to — for
/// editor tests that drive [`Incoming`] requests without a server.
#[cfg(test)]
pub fn test_client() -> (Client, BufReader<TcpStream>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let far = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    far.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (near, _) = listener.accept().unwrap();
    let client = Client {
        id: NEXT_CLIENT.fetch_add(1, Ordering::SeqCst),
        out: Box::new(near),
    };
    (client, BufReader::new(far))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn client_args_parse_flags_positions_and_paths() {
        let cwd = Path::new("/work");
        let parsed = parse_client_args(
            args(&["--client", "-n", "+12:3", "a.rs", "/abs/b.md", "+7", "c"]),
            cwd,
        )
        .unwrap();
        assert!(!parsed.wait);
        assert_eq!(
            parsed.files,
            vec![
                FileArg {
                    path: "/work/a.rs".into(),
                    line: Some(12),
                    column: Some(3)
                },
                FileArg {
                    path: "/abs/b.md".into(),
                    line: None,
                    column: None
                },
                FileArg {
                    path: "/work/c".into(),
                    line: Some(7),
                    column: None
                },
            ]
        );
        // Waiting is the default, as with emacsclient.
        let parsed = parse_client_args(args(&["x"]), cwd).unwrap();
        assert!(parsed.wait);
        let parsed = parse_client_args(
            args(&["--alternate-editor=vi", "--", "-dash", "+notnum"]),
            cwd,
        )
        .unwrap();
        assert_eq!(parsed.alternate_editor.as_deref(), Some("vi"));
        assert_eq!(parsed.files[0].path, Path::new("/work/-dash"));
        assert_eq!(parsed.files[1].path, Path::new("/work/+notnum"));
        assert!(parse_client_args(args(&["--bogus"]), cwd).is_err());
        assert!(parse_client_args(args(&["-a"]), cwd).is_err());
    }

    #[test]
    fn requests_and_replies_round_trip() {
        let request = Request {
            version: PROTOCOL_VERSION,
            token: String::new(),
            wait: true,
            files: vec![FileArg {
                path: "/tmp/COMMIT_EDITMSG".into(),
                line: Some(1),
                column: None,
            }],
        };
        assert_eq!(parse_request(&encode(&request)).unwrap(), request);
        assert!(parse_request("{\"version\":99}").is_err());
        assert!(parse_request("not json").is_err());
        // Missing optional fields default.
        let minimal = parse_request("{\"version\":1}").unwrap();
        assert!(!minimal.wait && minimal.files.is_empty());
        for reply in [
            Reply::Opened { files: 2 },
            Reply::Done,
            Reply::Error {
                message: "x".into(),
            },
        ] {
            let back: Reply = serde_json::from_str(encode(&reply).trim()).unwrap();
            assert_eq!(back, reply);
        }
        assert_eq!(encode(&Reply::Done), "{\"reply\":\"done\"}\n");
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abd", "abc"));
        assert!(!token_matches("ab", "abc"));
    }

    fn temp_path(tag: &str) -> PathBuf {
        // Short: macOS caps socket paths near 104 bytes.
        std::env::temp_dir()
            .join(format!("mcs-{tag}-{}", std::process::id()))
            .join("s")
    }

    /// A dispatch that opens nothing: it answers `Opened` and, for a waiting
    /// client, holds it on its files until the test releases them.
    fn fake_editor() -> (Dispatch, mpsc::Receiver<Vec<PathBuf>>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let dispatch: Dispatch = Arc::new(move |mut incoming: Incoming| {
            let files: Vec<PathBuf> = incoming
                .request
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect();
            incoming.client.send(&Reply::Opened { files: files.len() });
            if incoming.request.wait {
                hold(
                    incoming.client,
                    files.iter().cloned().collect(),
                    HashSet::new(),
                );
            }
            tx.lock().unwrap().send(files).unwrap();
        });
        (dispatch, rx)
    }

    fn round_trip(transport: Transport, tag: &str) {
        let path = temp_path(tag);
        let (dispatch, opened) = fake_editor();
        let server = Server::start(&path, transport, dispatch.clone()).unwrap();
        // A second server on the same path sees the live one.
        let err = Server::start(&path, transport, dispatch).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        // No-wait: returns once the files are open.
        let file = PathBuf::from(format!("/nowhere/{tag}-a.txt"));
        let nowait = ClientArgs {
            wait: false,
            files: vec![FileArg {
                path: file.clone(),
                line: Some(3),
                column: None,
            }],
            alternate_editor: None,
        };
        assert_eq!(
            send(&path, transport, &nowait, || {}),
            ClientOutcome::Finished
        );
        assert_eq!(opened.recv().unwrap(), vec![file.clone()]);

        // Wait: blocks until the file is released.
        let waiting = ClientArgs {
            wait: true,
            ..nowait
        };
        let client = {
            let path = path.clone();
            std::thread::spawn(move || send(&path, transport, &waiting, || {}))
        };
        assert_eq!(opened.recv().unwrap(), vec![file.clone()]);
        assert!(is_waited_on(&file));
        let released = release(&file);
        assert!(released.waited);
        assert_eq!(released.finished, 1);
        assert_eq!(client.join().unwrap(), ClientOutcome::Finished);
        assert!(!is_waited_on(&file));

        // Stopping removes the file; clients then find no server.
        drop(server);
        assert!(!path.exists());
        assert_eq!(
            send(&path, transport, &ClientArgs::default(), || {}),
            ClientOutcome::NoServer
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_round_trip_and_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = temp_path("perm");
        let (dispatch, _opened) = fake_editor();
        // A stale socket (its server exited without cleanup) is taken over.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let server = Server::start(&path, Transport::Unix, dispatch).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        drop(server);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        round_trip(Transport::Unix, "unix");
    }

    #[test]
    fn tcp_round_trip_and_token() {
        round_trip(Transport::Tcp, "tcp");

        // A request with the wrong token is refused.
        let path = temp_path("tok");
        let (dispatch, _opened) = fake_editor();
        let _server = Server::start(&path, Transport::Tcp, dispatch).unwrap();
        let (addr, _) = read_tcp_file(&path).unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        let request = Request {
            version: PROTOCOL_VERSION,
            token: "wrong".into(),
            wait: false,
            files: Vec::new(),
        };
        stream.write_all(encode(&request).as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Reply>(line.trim()).unwrap(),
            Reply::Error {
                message: "bad token".into()
            }
        );
    }

    #[test]
    fn disconnected_clients_are_forgotten() {
        let path = temp_path("gone");
        let (dispatch, opened) = fake_editor();
        let _server = Server::start(&path, Transport::native(), dispatch).unwrap();
        let file = PathBuf::from("/nowhere/gone.txt");
        let (mut stream, token) = connect(&path, Transport::native()).unwrap();
        let request = Request {
            version: PROTOCOL_VERSION,
            token,
            wait: true,
            files: vec![FileArg {
                path: file.clone(),
                line: None,
                column: None,
            }],
        };
        stream.write_all(encode(&request).as_bytes()).unwrap();
        opened.recv().unwrap();
        assert!(is_waited_on(&file));
        drop(stream);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while is_waited_on(&file) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!is_waited_on(&file));
    }
}
