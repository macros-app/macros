use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use steel::rvals::{IntoSteelVal, SteelVal};
use steel::steel_vm::ThreadStateController;
use steel::steel_vm::engine::Engine;

use super::*;

/// The outcome of a blocking eval: enqueued commands + the last value.
/// Only [`ScriptEngine::eval`] (tests / REPL) builds this; startup uses
/// [`ScriptEngine::eval_collecting`], which keeps commands even on error.
#[allow(dead_code)]
pub struct EvalResult {
    pub commands: Vec<EditorCommand>,
    pub value: Option<String>,
}

/// Something the engine thread sends back to the editor's foreground drain task.
pub enum ScriptOutcome {
    /// A dispatched eval/action: commands to apply + an optional echo message.
    Commands {
        commands: Vec<EditorCommand>,
        message: Option<String>,
    },
    /// Dynamic Helm candidates for `source`, computed for query `query_id`
    /// (stale results — id mismatch — are discarded by the editor). Each entry is
    /// a candidate plus an optional marginalia-style annotation.
    Candidates {
        source: usize,
        query_id: u64,
        candidates: Vec<(String, Option<String>)>,
    },
    /// Completion-at-point results from Scheme CAPFs: the byte range to replace
    /// and the candidate items (label + optional annotation). Empty = no CAPF
    /// produced anything, so the editor falls back to LSP.
    Capf {
        start: usize,
        end: usize,
        items: Vec<(String, Option<String>)>,
    },
}

pub(crate) struct RawResult {
    commands: Vec<EditorCommand>,
    value: Option<String>,
    error: Option<String>,
}

/// Where a job's result goes: back to a blocking caller, or to the async drain.
pub(crate) enum Reply {
    Sync(Sender<RawResult>),
    Async,
}

pub(crate) enum Task {
    Eval(String),
    Call(String, Option<String>),
    /// Like `Call`, but the argument is a secret (a `read-passwd` result): an
    /// error is reported WITHOUT Steel's message, which quotes the offending
    /// value verbatim (`+ expects a number, found: "hunter2"`) and would put
    /// it in the echo area and `*Messages*`.
    CallSecret(String, String),
    /// Compute dynamic Helm candidates: call `function` with the pattern.
    Candidates {
        source: usize,
        query_id: u64,
        function: String,
        pattern: String,
    },
    /// Run completion-at-point functions in order until one yields candidates.
    Capf {
        functions: Vec<String>,
        prefix: String,
        start: usize,
        end: usize,
    },
}

pub(crate) struct Job {
    /// Watchdog id (see [`Watchdog`]), unique per job.
    id: u64,
    task: Task,
    interactive: bool,
    reply: Reply,
}

/// Lets a blocking caller with a deadline interrupt *its own* job and no
/// other, and lets C-g interrupt whatever has been running too long. The
/// engine thread records the running job's id and start time, and resets the
/// VM's interrupt flag under the lock when each job starts, so an interrupt
/// either lands on the job it was aimed at or is cleared before the next one
/// runs.
#[derive(Clone, Default)]
pub(crate) struct Watchdog {
    next_id: Arc<AtomicU64>,
    state: Arc<Mutex<WatchState>>,
}

#[derive(Default)]
struct WatchState {
    /// The running job's id; 0 while the engine waits for work.
    running: u64,
    /// When the running job started.
    started: Option<Instant>,
    /// Jobs whose blocking caller timed out before they started: skipped
    /// instead of run late with nobody to receive their result.
    cancelled: std::collections::HashSet<u64>,
    controller: Option<ThreadStateController>,
}

impl Watchdog {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Engine thread: install the VM's controller once it is built.
    fn install(&self, controller: ThreadStateController) {
        if let Ok(mut s) = self.state.lock() {
            s.controller = Some(controller);
        }
    }

    /// Engine thread: job `id` is starting — clear any stale interrupt.
    /// Returns false (and doesn't start it) when its caller already cancelled
    /// it. The check and the start happen under one lock: checked separately,
    /// a `cancel` landing between them saw the job as not running yet, so the
    /// job then ran late anyway — exactly what `cancel` exists to prevent.
    fn begin(&self, id: u64) -> bool {
        let Ok(mut s) = self.state.lock() else {
            return true;
        };
        if s.cancelled.remove(&id) {
            return false;
        }
        s.running = id;
        s.started = Some(Instant::now());
        if let Some(c) = &s.controller {
            c.resume();
        }
        true
    }

    /// Caller: give up on job `id`. If it hasn't started, it never will.
    fn cancel(&self, id: u64) {
        if let Ok(mut s) = self.state.lock()
            && s.running != id
        {
            s.cancelled.insert(id);
        }
    }

    /// Engine thread: the last job finished; waiting for the next.
    fn idle(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.running = 0;
            s.started = None;
        }
    }

    /// How long the running job has been going, if one is.
    fn running_for(&self) -> Option<Duration> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.started.map(|t| t.elapsed()))
    }

    /// Interrupt the running job if it has been going for at least `min_age`
    /// (Emacs C-g). The age floor keeps a stray C-g from cutting short some
    /// routine job that happens to be mid-flight. Returns whether it fired.
    fn interrupt_running(&self, min_age: Duration) -> bool {
        if let Ok(s) = self.state.lock()
            && s.started.is_some_and(|t| t.elapsed() >= min_age)
            && let Some(c) = &s.controller
        {
            c.interrupt();
            return true;
        }
        false
    }

    /// Caller: interrupt job `id` if (and only if) it is the one running.
    fn interrupt(&self, id: u64) {
        if let Ok(s) = self.state.lock()
            && s.running == id
            && let Some(c) = &s.controller
        {
            c.interrupt();
        }
    }
}

/// How long [`ScriptEngine::call_action_within`] waits for an interrupted job
/// to unwind. Steel checks the flag every opcode, so this only runs out when
/// the job is stuck inside a native primitive (e.g. a hung `call-process`).
const INTERRUPT_GRACE: Duration = Duration::from_secs(1);

/// A job running at least this long is "stuck" as far as a blocking caller is
/// concerned: [`ActionHandle::run_sync_within`] reports the engine busy at once
/// instead of queueing behind it and freezing the UI for its whole timeout.
const BUSY_FAIL_FAST: Duration = Duration::from_secs(2);

/// The youngest job C-g will interrupt (see [`Watchdog::interrupt_running`]).
const QUIT_MIN_AGE: Duration = Duration::from_millis(200);

/// A handle to the Steel interpreter, which runs on its OWN thread so a slow or
/// runaway script can never freeze the UI (the editor's command loop solves
/// Emacs's "elisp blocks the main loop" problem). The engine is `!Send`, so it
/// is created on and never leaves that thread — only plain data (`String`,
/// `EditorCommand`) crosses the channel boundary. The editor talks to it two
/// ways: **blocking** (startup/config, where command ordering matters, and
/// tests) and **dispatched** (interactive commands, drained on the foreground).
pub struct ScriptEngine {
    handle: ActionHandle,
    command_names: Vec<String>,
    /// Every name registered into the engine, recorded at registration time by
    /// [`Registrar`] on the engine thread (the single source of truth — it can
    /// never drift from what is actually registered). Registration happens on
    /// the spawned engine thread, so the list arrives on a one-shot channel and
    /// is cached here on first use.
    global_names: std::sync::OnceLock<Vec<String>>,
    global_names_rx: Mutex<Option<Receiver<Vec<String>>>>,
    /// Fast path: a nullary command name → its `EditorCommand`, so trivial
    /// keybindings (motion, etc.) apply synchronously without a thread hop.
    builtins: HashMap<String, EditorCommand>,
    shared: EngineShared,
}

impl ScriptEngine {
    /// Spawn the engine thread; returns the handle and the receiver of async
    /// outcomes (the editor drains it on a foreground task).
    pub fn new() -> (Self, UnboundedReceiver<ScriptOutcome>) {
        let nullary = nullary_commands();
        let command_names: Vec<String> = nullary.iter().map(|(name, _)| name.to_string()).collect();
        let builtins: HashMap<String, EditorCommand> = nullary
            .iter()
            .map(|(n, c)| (n.to_string(), c.clone()))
            .collect();

        let shared = EngineShared::default();
        let (jobs, jobs_rx) = channel::<Job>();
        let (outcomes, outcomes_rx) = unbounded::<ScriptOutcome>();
        let (names_tx, names_rx) = channel::<Vec<String>>();

        let thread_shared = shared.clone();
        let watchdog = Watchdog::default();
        let thread_watchdog = watchdog.clone();
        std::thread::spawn(move || {
            engine_thread(jobs_rx, outcomes, names_tx, thread_shared, thread_watchdog)
        });

        (
            Self {
                handle: ActionHandle {
                    jobs,
                    watchdog: watchdog.clone(),
                },
                command_names,
                global_names: std::sync::OnceLock::new(),
                global_names_rx: Mutex::new(Some(names_rx)),
                builtins,
                shared,
            },
            outcomes_rx,
        )
    }

    /// Shift markers after an edit replacing `[start, old_end)` with `new_len`
    /// bytes: markers inside the deleted span collapse to `start`, markers after
    /// it shift by the length delta.
    pub fn adjust_markers(&self, start: usize, old_end: usize, new_len: usize) {
        let delta = new_len as isize - (old_end - start) as isize;
        let mut store = self.shared.markers.lock().unwrap();
        for pos in store.positions.values_mut() {
            if *pos >= old_end {
                *pos = (*pos as isize + delta).max(0) as usize;
            } else if *pos > start {
                *pos = start;
            }
        }
    }

    /// The active buffer changed from `left` to `entered`: drop the plain
    /// markers (they are positions in `left`) and swap the buffer-bound ones
    /// (see [`MarkerStore::switch`](super::state::MarkerStore::switch)).
    pub fn switch_markers(&self, left: &str, entered: &str) {
        self.shared.markers.lock().unwrap().switch(left, entered);
    }

    /// Create a marker at `pos`, returning its id. The snippet session uses these
    /// to track tab-stop fields, which then ride along on `adjust_markers`.
    pub fn add_marker(&self, pos: usize) -> u64 {
        self.shared.markers.lock().unwrap().alloc(pos)
    }

    /// Current byte position of marker `id`, if it still exists.
    pub fn marker_pos(&self, id: u64) -> Option<usize> {
        self.shared
            .markers
            .lock()
            .unwrap()
            .positions
            .get(&id)
            .copied()
    }

    /// Remove marker `id` (snippet field cleanup).
    pub fn remove_marker(&self, id: u64) {
        self.shared.markers.lock().unwrap().remove(id);
    }

    /// The nullary command names, for `M-x` completion.
    pub fn command_names(&self) -> &[String] {
        &self.command_names
    }

    /// Every registered global name, for the built-in Steel LSP. Recorded on
    /// the engine thread as functions are registered; the first call waits for
    /// registration to finish (fast — it happens before any job is processed).
    pub fn global_names(&self) -> &[String] {
        self.global_names.get_or_init(|| {
            self.global_names_rx
                .lock()
                .unwrap()
                .take()
                .and_then(|rx| rx.recv().ok())
                .unwrap_or_default()
        })
    }

    /// Update the buffer list Scheme sees via `(buffer-names)`.
    pub fn set_buffer_names(&self, names: Vec<String>) {
        *self.shared.buffers.lock().unwrap() = names;
    }

    /// Publish the buffer name → file path map Scheme reads via
    /// `(buffer-file-name-of name)`. Only file-visiting buffers appear.
    pub fn set_buffer_paths(&self, paths: HashMap<String, String>) {
        *self.shared.buffer_paths.lock().unwrap() = paths;
    }

    /// Publish the current editor options so Scheme can read them via
    /// `(get-option "name")`. Called after any option change.
    pub fn set_options(&self, options: HashMap<String, crate::settings::OptionValue>) {
        *self.shared.options.lock().unwrap() = options;
    }

    /// Publish the session-persistence state (rings, buffers, layout) that
    /// `(session-ring …)` / `(window-layout)` / `(session-file-buffers)` read.
    pub fn set_session(&self, state: SessionState) {
        *self.shared.session.lock().unwrap() = state;
    }

    /// Publish the active buffer name and point line for `(current-buffer)` /
    /// `(buffer-line)`. Called just before dispatching so a command reads point
    /// as it was when invoked.
    pub fn set_snapshot(&self, snapshot: EditorSnapshot) {
        self.shared.snapshot.store(Arc::new(snapshot));
    }

    /// Recognise one or more bare `(builtin)` forms — `(forward-char)` or a
    /// sequence like `(set-mark) (move-end-of-line)` — and return their commands
    /// in order, so a motion/edit keybinding applies synchronously instead of
    /// paying a thread hop. All-or-nothing: if any form takes an argument, is a
    /// string, nests, or names a non-builtin, returns `None` and the whole
    /// expression goes to the engine thread (where its semantics are exact).
    pub fn builtins_for(&self, expr: &str) -> Option<Vec<EditorCommand>> {
        let mut out = Vec::new();
        let mut rest = expr.trim();
        while !rest.is_empty() {
            // Each form is `(name)` with no nested parens, so the first `)`
            // closes it. Anything with a string/arg fails the char check below
            // and drops the whole expression to the engine.
            let inner = rest.strip_prefix('(')?;
            let close = inner.find(')')?;
            let name = inner[..close].trim();
            if name.is_empty() || name.contains(|c: char| c.is_whitespace() || c == '(' || c == '"')
            {
                return None;
            }
            out.push(self.builtins.get(name)?.clone());
            rest = inner[close + 1..].trim();
        }
        (!out.is_empty()).then_some(out)
    }

    /// Blocking eval — tests and the REPL. Startup config uses
    /// [`Self::eval_collecting`] (which keeps partial commands on error).
    #[allow(dead_code)]
    pub fn eval(&self, source: &str) -> Result<EvalResult, String> {
        let raw = self.handle.run_sync(Task::Eval(source.to_string()))?;
        match raw.error {
            Some(error) => Err(error),
            None => Ok(EvalResult {
                commands: raw.commands,
                value: raw.value,
            }),
        }
    }

    /// Like [`Self::eval`] but never throws the commands away on error: returns
    /// everything queued *before* the failure together with the error message.
    /// Startup config uses this so one bad form in `init.scm` can't silently drop
    /// the rest of the file — including the `(evil-mode)` that usually precedes
    /// it (Emacs `init.el` semantics: apply up to the error, then report it).
    pub fn eval_collecting(&self, source: &str) -> (Vec<EditorCommand>, Option<String>) {
        match self.handle.run_sync(Task::Eval(source.to_string())) {
            Ok(raw) => (raw.commands, raw.error),
            Err(stopped) => (Vec::new(), Some(stopped)),
        }
    }

    /// Blocking action call — tests only; the editor uses `dispatch_action`,
    /// or [`Self::call_action_within`] when it must wait (it runs on the UI
    /// thread, so it needs the deadline).
    #[allow(dead_code)]
    pub fn call_action(&self, name: &str, candidate: Option<&str>) -> Vec<EditorCommand> {
        self.handle
            .run_sync(Task::Call(name.to_string(), candidate.map(str::to_string)))
            .map(|raw| raw.commands)
            .unwrap_or_default()
    }

    /// Call the nullary function `name` and wait at most `timeout` for it —
    /// for the few calls the UI thread must block on (`before-save-hook`,
    /// Org fontify). Past the deadline the job is interrupted, so a hook stuck
    /// in a loop can't freeze the editor. Returns the commands queued (even
    /// those before an error) and the error, if any.
    pub fn call_action_within(
        &self,
        name: &str,
        timeout: Duration,
    ) -> (Vec<EditorCommand>, Option<String>) {
        match self
            .handle
            .run_sync_within(Task::Call(name.to_string(), None), timeout)
        {
            Ok(raw) => (raw.commands, raw.error),
            Err(error) => (Vec::new(), Some(error)),
        }
    }

    /// Like [`Self::eval_collecting`] with a deadline: past `timeout` the eval
    /// is interrupted (an infinite loop in `init.scm` must not hang startup,
    /// which waits on this on the UI thread).
    pub fn eval_collecting_within(
        &self,
        source: &str,
        timeout: Duration,
    ) -> (Vec<EditorCommand>, Option<String>) {
        match self
            .handle
            .run_sync_within(Task::Eval(source.to_string()), timeout)
        {
            Ok(raw) => (raw.commands, raw.error),
            Err(error) => (Vec::new(), Some(error)),
        }
    }

    /// C-g: interrupt the running evaluation if it has been going a moment
    /// (a runaway `M-:` loop, a stuck command). Returns whether one was
    /// interrupted.
    pub fn interrupt_running(&self) -> bool {
        self.handle.watchdog.interrupt_running(QUIT_MIN_AGE)
    }

    /// Async eval — runs on the engine thread; the outcome is delivered to the
    /// foreground drain task. A slow script never blocks the UI.
    pub fn dispatch(&self, source: &str, interactive: bool) {
        self.handle
            .send_async(Task::Eval(source.to_string()), interactive);
    }

    /// Async action call (Helm actions, mode hooks).
    pub fn dispatch_action(&self, name: &str, candidate: Option<&str>) {
        self.handle.dispatch_action(name, candidate);
    }

    /// Async call of `name` with a secret argument (see [`Task::CallSecret`]).
    pub fn dispatch_secret_action(&self, name: &str, secret: &str) {
        self.handle.send_async(
            Task::CallSecret(name.to_string(), secret.to_string()),
            false,
        );
    }

    /// A cheap `Send + Clone` handle that can deliver `dispatch_action` calls
    /// from another thread — for work that must leave the UI thread entirely
    /// (the spell-check worker) yet still hand its result to a named Scheme
    /// callback.
    pub fn action_handle(&self) -> ActionHandle {
        self.handle.clone()
    }

    /// Compute a dynamic Helm source's candidates on the engine thread; the
    /// result arrives as `ScriptOutcome::Candidates` for `query_id`.
    pub fn dispatch_candidates(&self, source: usize, query_id: u64, function: &str, pattern: &str) {
        self.handle.send_async(
            Task::Candidates {
                source,
                query_id,
                function: function.to_string(),
                pattern: pattern.to_string(),
            },
            false,
        );
    }

    /// Run the registered completion-at-point functions on the engine thread,
    /// over the prefix `buffer[start..end]`. The result arrives as
    /// `ScriptOutcome::Capf`; empty means the editor should fall back to LSP.
    pub fn dispatch_capf(&self, functions: Vec<String>, prefix: &str, start: usize, end: usize) {
        self.handle.send_async(
            Task::Capf {
                functions,
                prefix: prefix.to_string(),
                start,
                end,
            },
            false,
        );
    }
}

/// A clone of the engine's job sender: delivers jobs to the engine thread from
/// any thread. [`ScriptEngine`] sends through one too; see
/// [`ScriptEngine::action_handle`].
#[derive(Clone)]
pub struct ActionHandle {
    jobs: Sender<Job>,
    watchdog: Watchdog,
}

impl ActionHandle {
    /// Queue `task` with its outcome going to the foreground drain task. A
    /// stopped engine drops it silently.
    fn send_async(&self, task: Task, interactive: bool) {
        let _ = self.jobs.send(Job {
            id: self.watchdog.next_id(),
            task,
            interactive,
            reply: Reply::Async,
        });
    }

    /// Send `task` to the engine thread and block for its raw result. Both
    /// failure points — the engine thread gone at send or at receive —
    /// collapse to the same "script engine stopped" error. The blocking
    /// entry points ([`ScriptEngine::eval`], [`ScriptEngine::eval_collecting`],
    /// [`ScriptEngine::call_action`], [`Self::eval_sync`]) are thin wrappers
    /// over this.
    fn run_sync(&self, task: Task) -> Result<RawResult, String> {
        let (tx, rx) = channel();
        self.jobs
            .send(Job {
                id: self.watchdog.next_id(),
                task,
                interactive: false,
                reply: Reply::Sync(tx),
            })
            .map_err(|_| "script engine stopped".to_string())?;
        rx.recv().map_err(|_| "script engine stopped".to_string())
    }

    /// [`Self::run_sync`] with a deadline: past `timeout` the job is
    /// interrupted and, after [`INTERRUPT_GRACE`], abandoned (its result, if
    /// it ever finishes, is dropped).
    fn run_sync_within(&self, task: Task, timeout: Duration) -> Result<RawResult, String> {
        use std::sync::mpsc::RecvTimeoutError;
        // Queueing behind a job that is already stuck would just burn the whole
        // timeout with the UI frozen — and then interrupt the wrong job.
        if let Some(busy) = self.watchdog.running_for().filter(|t| *t >= BUSY_FAIL_FAST) {
            return Err(format!(
                "script engine busy (an evaluation has run {:.0}s; C-g interrupts it)",
                busy.as_secs_f32()
            ));
        }
        let id = self.watchdog.next_id();
        let (tx, rx) = channel();
        self.jobs
            .send(Job {
                id,
                task,
                interactive: false,
                reply: Reply::Sync(tx),
            })
            .map_err(|_| "script engine stopped".to_string())?;
        match rx.recv_timeout(timeout) {
            Ok(raw) => return Ok(raw),
            Err(RecvTimeoutError::Disconnected) => return Err("script engine stopped".into()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Not started yet (queued behind something slow): make sure it never
        // runs late, applying edits after the caller already moved on. Cancel
        // BEFORE interrupting: a job that starts between the two is then
        // either skipped (cancelled first) or running when the interrupt
        // lands — the other order let one slip between them uninterrupted.
        self.watchdog.cancel(id);
        self.watchdog.interrupt(id);
        let secs = timeout.as_secs_f32();
        match rx.recv_timeout(INTERRUPT_GRACE) {
            // The interrupted job unwound: keep what it queued, but report the
            // timeout rather than Steel's generic interrupt error.
            Ok(raw) => Ok(RawResult {
                error: Some(format!("timed out after {secs}s; interrupted")),
                ..raw
            }),
            Err(_) => Err(format!(
                "timed out after {secs}s (still running in a native call; abandoned)"
            )),
        }
    }

    /// Blocking eval from any thread (the MCP server's): returns the editor
    /// commands queued — including those queued before an error — plus the
    /// printed value and the error, if any. The caller applies the commands
    /// on the editor's foreground; nothing here touches editor state.
    pub fn eval_sync(&self, source: &str) -> (Vec<EditorCommand>, Option<String>, Option<String>) {
        match self.run_sync(Task::Eval(source.to_string())) {
            Ok(raw) => (raw.commands, raw.value, raw.error),
            Err(stopped) => (Vec::new(), None, Some(stopped)),
        }
    }

    /// Async action call (Helm actions, mode hooks, worker-thread callbacks).
    pub fn dispatch_action(&self, name: &str, candidate: Option<&str>) {
        self.send_async(
            Task::Call(name.to_string(), candidate.map(str::to_string)),
            false,
        );
    }
}

/// The text of a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Call a query function (a dynamic Helm source or a CAPF) by name. Queries
/// aren't commands, so anything they queued is discarded rather than leaking
/// into — and being attributed to — the next eval's command batch.
fn call_query(
    engine: &mut Engine,
    queue: &Queue,
    function: &str,
    arg: String,
) -> Result<SteelVal, String> {
    let arg = arg.into_steelval().unwrap_or(SteelVal::Void);
    let result = engine
        .call_function_by_name_with_args(function, vec![arg])
        .map_err(|e| format!("error in {function}: {}", steel_error_string(&e)));
    drop(queue.drain());
    result
}

/// Report a failed query in the echo area (an empty candidate list alone
/// would hide a typo in the user's function).
fn report_query_error(outcomes: &UnboundedSender<ScriptOutcome>, error: String) {
    let _ = outcomes.unbounded_send(ScriptOutcome::Commands {
        commands: Vec::new(),
        message: Some(error),
    });
}

/// Run one task on the engine. Queries (Helm candidates, CAPFs) answer on the
/// async channel themselves and return `None`; everything else returns its
/// result for the caller to deliver.
fn run_task(
    engine: &mut Engine,
    queue: &Queue,
    outcomes: &UnboundedSender<ScriptOutcome>,
    task: Task,
) -> Option<RawResult> {
    let raw = match task {
        // Dynamic Helm candidates: call the fn, parse a string list, reply
        // directly on the async channel (no command/value semantics).
        Task::Candidates {
            source,
            query_id,
            function,
            pattern,
        } => {
            let candidates = match call_query(engine, queue, &function, pattern) {
                Ok(value) => steel_candidate_list(&value),
                Err(error) => {
                    report_query_error(outcomes, error);
                    Vec::new()
                }
            };
            let _ = outcomes.unbounded_send(ScriptOutcome::Candidates {
                source,
                query_id,
                candidates,
            });
            return None;
        }
        // Completion-at-point: call each registered CAPF with the prefix; the
        // first one returning a non-empty candidate list wins. A CAPF returns
        // either a flat candidate list, or `(start end (cand …))` to override
        // the replaced byte range. Reply on the async channel.
        Task::Capf {
            functions,
            prefix,
            start,
            end,
        } => {
            let mut out_start = start;
            let mut out_end = end;
            let mut items: Vec<(String, Option<String>)> = Vec::new();
            for function in &functions {
                let value = match call_query(engine, queue, function, prefix.clone()) {
                    Ok(value) => value,
                    Err(error) => {
                        report_query_error(outcomes, error);
                        continue;
                    }
                };
                let (s, e, parsed) = parse_capf_result(&value, start, end);
                if !parsed.is_empty() {
                    out_start = s;
                    out_end = e;
                    items = parsed;
                    break;
                }
            }
            let _ = outcomes.unbounded_send(ScriptOutcome::Capf {
                start: out_start,
                end: out_end,
                items,
            });
            return None;
        }
        Task::Eval(source) => match engine.run(source) {
            Ok(values) => RawResult {
                commands: queue.drain(),
                value: values.last().and_then(format_value),
                error: None,
            },
            Err(error) => RawResult {
                commands: queue.drain(),
                value: None,
                error: Some(steel_error_string(&error)),
            },
        },
        Task::Call(name, arg) => {
            let args: Vec<SteelVal> = arg
                .and_then(|c| c.into_steelval().ok())
                .into_iter()
                .collect();
            let error = engine
                .call_function_by_name_with_args(&name, args)
                .err()
                .map(|e| steel_error_string(&e));
            RawResult {
                commands: queue.drain(),
                value: None,
                error,
            }
        }
        Task::CallSecret(name, secret) => {
            let args: Vec<SteelVal> = secret.into_steelval().into_iter().collect();
            let error = engine
                .call_function_by_name_with_args(&name, args)
                .err()
                .map(|_| format!("{name} failed (details hidden: its argument is secret)"));
            RawResult {
                commands: queue.drain(),
                value: None,
                error,
            }
        }
    };
    Some(raw)
}

/// The engine thread: owns the `!Send` `Engine` for its whole life, processing
/// jobs FIFO so results stay ordered.
pub(crate) fn engine_thread(
    jobs: Receiver<Job>,
    outcomes: UnboundedSender<ScriptOutcome>,
    names_tx: Sender<Vec<String>>,
    shared: EngineShared,
    watchdog: Watchdog,
) {
    let queue = shared.queue.clone();
    let (mut engine, names) = build_engine(&shared);
    watchdog.install(engine.get_thread_state_controller());
    let _ = names_tx.send(names);

    loop {
        watchdog.idle();
        let Ok(Job {
            id,
            task,
            interactive,
            reply,
        }) = jobs.recv()
        else {
            break;
        };
        if !watchdog.begin(id) {
            continue; // its caller timed out and gave up before it started
        }
        // `with-process-env` extents never span jobs (one an error escaped
        // without popping would otherwise leak into every later spawn).
        crate::process::reset_dynamic();
        // A panicking primitive must not kill the engine thread: without this
        // every later eval would fail with "script engine stopped" until the
        // editor restarts. The job reports the panic as its error instead.
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_task(&mut engine, &queue, &outcomes, task)
        }));
        let raw = match ran {
            Ok(Some(raw)) => raw,
            Ok(None) => continue, // answered on the async channel itself
            Err(payload) => RawResult {
                commands: queue.drain(),
                value: None,
                error: Some(format!(
                    "internal error in a primitive: {}",
                    panic_message(payload.as_ref())
                )),
            },
        };
        match reply {
            Reply::Sync(tx) => {
                let _ = tx.send(raw);
            }
            Reply::Async => {
                let RawResult {
                    commands,
                    value,
                    error,
                } = raw;
                let message = if let Some(error) = error {
                    Some(format!("error: {error}"))
                } else if interactive {
                    match value {
                        Some(v) => Some(format!("=> {v}")),
                        // A `(message ...)` side-effect already echoed something;
                        // don't clobber it with a generic "Done".
                        None if commands
                            .iter()
                            .any(|c| matches!(c, EditorCommand::Message(_))) =>
                        {
                            None
                        }
                        None => Some("Done".to_string()),
                    }
                } else {
                    None
                };
                let _ = outcomes.unbounded_send(ScriptOutcome::Commands { commands, message });
            }
        }
    }
}
