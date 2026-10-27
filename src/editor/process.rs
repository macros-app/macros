//! `make-process` subprocesses for [`Editor`]: spawning, stdin, signalling
//! and reaping (extracted from the editor module for size). These methods
//! extend the same `Editor` type; cross-module entry points are `pub(super)`.

use std::io::{BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::Context;

use crate::process::{ProcessGroup, Signal, lossy_lines};
use crate::util::HideConsole;

use super::*;

/// A message from a [`make_process`](Editor::make_process) worker: one stdout
/// line, or the exit code once the worker has reaped the child.
enum ChildMsg {
    Line(String),
    Exited(i32),
}

/// How long a terminated process group gets to exit on SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(500);
/// How long quitting waits, in total, for language servers to exit after
/// `shutdown` + `exit` before SIGKILLing their groups.
const LSP_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// A live `make-process` child kept so Scheme can write to its stdin or kill
/// it. The child's [`ProcessGroup`] is shared with the worker thread that reaps it (so the UI
/// never blocks in `wait`); stdin writes go through a dedicated writer thread
/// (so a full pipe never blocks the UI). Dropping the handle — the owning
/// buffer is killed, the editor goes away — terminates the process group.
pub(super) struct ProcessHandle {
    child: Arc<Mutex<ProcessGroup>>,
    stdin: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// Buffer whose kill also kills this process (`set-process-buffer`).
    buffer: Option<String>,
}

/// Lock a shared child, ignoring poison (a panicked holder can't corrupt it).
fn lock_child(child: &Mutex<ProcessGroup>) -> std::sync::MutexGuard<'_, ProcessGroup> {
    child.lock().unwrap_or_else(|e| e.into_inner())
}

/// Signal `child`'s whole process group (it leads its own group, see
/// [`Editor::make_process`]) if it hasn't been reaped; true if it was still
/// running. Holding the lock across the check and the kill means the reaper
/// can't free the pid/pgid in between, so we never signal a recycled group.
fn signal_group(child: &Mutex<ProcessGroup>, sig: Signal) -> bool {
    lock_child(child).signal(sig)
}

impl ProcessHandle {
    /// SIGKILL the group now (Emacs `kill-process`).
    fn kill_now(&self) {
        signal_group(&self.child, Signal::Kill);
    }

    /// Has the child exited (polling, never blocking)?
    fn exited(&self) -> bool {
        !matches!(lock_child(&self.child).try_wait(), Ok(None))
    }
}

impl Drop for ProcessHandle {
    /// SIGTERM the group, then SIGKILL it after [`TERM_GRACE`] if it lingers —
    /// on a background thread, so dropping never blocks the UI. No-op for a
    /// process that already exited (the normal exit path drops it too).
    fn drop(&mut self) {
        terminate_gracefully(&self.child);
    }
}

/// SIGTERM `child`'s group, then SIGKILL it after [`TERM_GRACE`] if it
/// lingers — on a background thread, so the caller never blocks. No-op for a
/// process that already exited.
fn terminate_gracefully(child: &Arc<Mutex<ProcessGroup>>) {
    if signal_group(child, Signal::Term) {
        let child = child.clone();
        std::thread::spawn(move || {
            std::thread::sleep(TERM_GRACE);
            signal_group(&child, Signal::Kill);
        });
    }
}

impl Editor {
    /// Bind process `id` to buffer `name`: killing that buffer terminates the
    /// process (Emacs `set-process-buffer`). Unbound processes live until they
    /// exit, are killed, or the app quits.
    pub(super) fn set_process_buffer(&mut self, id: u64, name: String) {
        if let Some(h) = self.processes.get_mut(&id) {
            h.buffer = Some(name);
        }
    }

    /// Terminate every process bound to buffer `name` (it's being killed). The
    /// exit callbacks still fire as each one is reaped.
    pub(super) fn kill_processes_for_buffer(&mut self, name: &str) {
        for h in self.processes.values() {
            if h.buffer.as_deref() == Some(name) {
                terminate_gracefully(&h.child);
            }
        }
    }

    /// Buffer `old` was renamed to `new`: processes writing to it follow.
    pub(super) fn rename_process_buffers(&mut self, old: &str, new: &str) {
        for h in self.processes.values_mut() {
            if h.buffer.as_deref() == Some(old) {
                h.buffer = Some(new.to_string());
            }
        }
    }

    /// App quit: SIGTERM every `make-process` group, give them a brief grace
    /// to exit, SIGKILL the stragglers; and drop embedded terminals (their PTY
    /// teardown SIGHUPs the shell). Synchronous — the app is exiting, so a
    /// background "kill later" thread would never run.
    pub(super) fn shutdown_processes(&mut self) {
        let handles: Vec<ProcessHandle> = self.processes.drain().map(|(_, h)| h).collect();
        let live: Vec<&ProcessHandle> = handles
            .iter()
            .filter(|h| signal_group(&h.child, Signal::Term))
            .collect();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline && live.iter().any(|h| !h.exited()) {
            std::thread::sleep(Duration::from_millis(10));
        }
        for h in &live {
            h.kill_now();
        }
        drop(handles);
        self.terminals.clear();
        self.terminal_geom.clear();
        self.shutdown_lsp_servers();
    }

    /// App quit: send every language server `shutdown` + `exit` together,
    /// then share one grace period across them before SIGKILLing stragglers'
    /// groups. Without this they'd only get `LspClient`'s drop-time SIGKILL,
    /// and only if the editor is dropped before the process exits.
    fn shutdown_lsp_servers(&mut self) {
        let groups: Vec<_> = self
            .servers
            .values_mut()
            .filter_map(|server| server.client.begin_shutdown())
            .collect();
        let deadline = Instant::now() + LSP_SHUTDOWN_GRACE;
        for mut group in groups {
            group.reap_with_grace(deadline.saturating_duration_since(Instant::now()));
        }
    }

    /// Spawn `program` asynchronously (Emacs `make-process`): a worker thread
    /// reads its stdout line by line and forwards each line to the foreground,
    /// which calls the Steel `on_output` fn with it; `on_exit` runs when the
    /// process finishes. The UI never blocks, and the callbacks' commands flow
    /// through the normal outcome drain — so `(append-to-buffer …)` etc. work.
    pub(super) fn make_process(
        &mut self,
        spec: crate::script::MakeProcessArgs,
        cx: &mut Context<Self>,
    ) {
        let crate::script::MakeProcessArgs {
            id,
            program,
            args,
            env,
            on_output,
            on_exit,
        } = spec;
        // Spawn here (not on the worker) so we can keep the child for stdin/kill.
        // Expand a leading `~/` — `Command::new` does no shell expansion.
        let program = crate::util::expand_tilde(&program);
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .hide_console();
        env.apply(&mut cmd);
        // Its own process group, so kill_process can take down the whole tree
        // — a shell-wrapped command (`sh -c "make && …"`) dies with its shell
        // instead of being orphaned by it.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.echo(format!("make-process: {e}"), cx);
                return;
            }
        };
        let stdout = child.stdout.take();
        // stdin writes go through their own thread: a child that stops reading
        // would otherwise block `write_all` — and the UI — once the pipe fills.
        let stdin = child.stdin.take().map(|mut pipe| {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            std::thread::spawn(move || {
                for chunk in rx {
                    if pipe.write_all(&chunk).and_then(|_| pipe.flush()).is_err() {
                        break;
                    }
                }
                // Sender dropped (handle gone) or broken pipe → stdin closes.
            });
            tx
        });
        let child = Arc::new(Mutex::new(ProcessGroup::new(child)));
        let reaper = child.clone();
        self.processes.insert(
            id,
            ProcessHandle {
                child,
                stdin,
                buffer: None,
            },
        );

        let (tx, mut rx) = futures::channel::mpsc::unbounded::<ChildMsg>();
        std::thread::spawn(move || {
            if let Some(out) = stdout {
                for l in lossy_lines(BufReader::new(out)) {
                    if tx.unbounded_send(ChildMsg::Line(l)).is_err() {
                        break;
                    }
                }
            }
            // stdout closed; reap here, never on the UI thread. Poll with
            // try_wait (not a blocking wait) so the lock is free between polls
            // for kill-process / buffer-kill signalling.
            let code = ProcessGroup::wait_shared(&reaper)
                .and_then(|status| status.code())
                .unwrap_or(-1);
            let _ = tx.unbounded_send(ChildMsg::Exited(code));
        });
        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let alive = this
                    .update(cx, |editor, _| match &msg {
                        ChildMsg::Line(line) => {
                            // Publish the active buffer first, so a callback
                            // reading `(current-buffer)` sees the buffer the user
                            // is actually in — not a stale snapshot. Without this,
                            // async output routed by current-buffer (e.g.
                            // agent-shell streaming) can steal focus to the wrong
                            // buffer. Mirrors `run_script`/`run_hook`.
                            editor.publish_point();
                            editor.script.dispatch_action(&on_output, Some(line))
                        }
                        ChildMsg::Exited(code) => {
                            // Already reaped by the worker: dropping the handle
                            // is a no-op signal-wise.
                            editor.processes.remove(&id);
                            if let Some(cb) = &on_exit {
                                // Same as the Line branch: a process whose
                                // output goes elsewhere (a file) fires only
                                // this callback, which must not see a stale
                                // `(current-buffer)`.
                                editor.publish_point();
                                editor.script.dispatch_action(cb, Some(&code.to_string()));
                            }
                        }
                    })
                    .is_ok();
                if !alive || matches!(msg, ChildMsg::Exited(_)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Write `text` to a live process's stdin (Emacs `process-send-string`).
    pub(super) fn send_to_process(&mut self, id: u64, text: &str, cx: &mut Context<Self>) {
        // Queued to the writer thread — never blocks on a full pipe.
        if let Some(handle) = self.processes.get(&id)
            && let Some(stdin) = handle.stdin.as_ref()
            && stdin.send(text.as_bytes().to_vec()).is_err()
        {
            self.echo(
                "process-send-string: process stdin is closed".to_string(),
                cx,
            );
        }
    }

    /// Kill a live process; its reader thread then reports exit (Emacs `kill-process`).
    pub(super) fn kill_process(&mut self, id: u64) {
        if let Some(handle) = self.processes.get(&id) {
            // The child got its own process group at spawn — signal the group,
            // so a wrapper shell's children die too (not just the shell).
            handle.kill_now();
        }
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;

    /// Is any process left in group `pgid`?
    #[cfg(not(target_os = "linux"))]
    fn group_alive(pgid: i32) -> bool {
        // SAFETY: `kill` with signal 0 takes no pointers and only probes for
        // existence; it sends nothing.
        unsafe { libc::kill(-pgid, 0) == 0 }
    }

    /// Is any *live* process left in group `pgid`? Zombies don't count: a
    /// killed, orphaned child is reparented to PID 1, and in a CI container
    /// PID 1 often never reaps, so the zombie keeps `kill(-pgid, 0)` succeeding.
    #[cfg(target_os = "linux")]
    fn group_alive(pgid: i32) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            // SAFETY: `kill` with signal 0 takes no pointers and only probes
            // for existence; it sends nothing.
            return unsafe { libc::kill(-pgid, 0) == 0 };
        };
        entries.flatten().any(|e| {
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                return false;
            };
            // `pid (comm) state ppid pgrp …` — comm may contain spaces/parens.
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                return false;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next();
            let pgrp = fields.nth(1).and_then(|f| f.parse::<i32>().ok());
            pgrp == Some(pgid) && state != Some("Z")
        })
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Dropping a handle (buffer killed / editor gone) takes the whole process
    /// group down — the wrapper shell *and* its background child.
    #[test]
    fn dropping_handle_kills_process_group() {
        use std::os::unix::process::CommandExt;
        let child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let child = Arc::new(Mutex::new(ProcessGroup::new(child)));
        let handle = ProcessHandle {
            child: child.clone(),
            stdin: None,
            buffer: Some("*shell*".into()),
        };
        assert!(!handle.exited());
        drop(handle);
        // The leader exits (we reap it, as the worker thread would)…
        assert!(wait_until(|| matches!(
            lock_child(&child).try_wait(),
            Ok(Some(_))
        )));
        // …and so does the backgrounded `sleep` in its group.
        assert!(wait_until(|| !group_alive(pgid)));
        // Signalling an already-reaped child is a no-op, not a stray kill.
        assert!(!signal_group(&child, Signal::Kill));
    }
}
