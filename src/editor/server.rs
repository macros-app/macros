//! Editor side of the edit server (src/server.rs): visiting the files a
//! `macros --client` sends, `C-x #` (`server-edit`) to finish with one, and
//! starting or stopping the server from the `server` option.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gpui::Context;

use super::Editor;
use super::buffers::file_identity;
use crate::server::{Incoming, Reply, Status};

/// A client's 1-based `+LINE[:COLUMN]`.
type Goto = (usize, Option<usize>);

/// `+LINE[:COLUMN]` for files whose read is still in flight, by file identity;
/// applied when [`Editor::open_file`] installs the buffer.
static PENDING_GOTO: Mutex<Option<HashMap<PathBuf, Goto>>> = Mutex::new(None);

/// Take the position a client asked for on `path`, if any.
pub(super) fn take_server_goto(path: &Path) -> Option<Goto> {
    let mut pending = PENDING_GOTO.lock().unwrap();
    let map = pending.as_mut().filter(|m| !m.is_empty())?;
    map.remove(&file_identity(path))
}

/// A buffer visiting `path` was killed: it no longer holds up any client.
pub(super) fn server_buffer_killed(path: Option<&Path>) {
    if let Some(path) = path
        && crate::server::any_waiting()
    {
        crate::server::release(&file_identity(path));
    }
}

impl Editor {
    /// Start or stop the edit server to match the `server` option. `announce`
    /// echoes the outcome (an explicit change); at startup only failures are.
    pub(crate) fn reconcile_server(&mut self, announce: bool, cx: &mut Context<Self>) {
        let enabled = cx.global::<crate::settings::Settings>().server();
        match crate::server::reconcile(enabled) {
            Ok(Status::Started(path)) if announce => {
                self.echo(format!("Edit server listening on {}", path.display()), cx)
            }
            Ok(Status::Stopped) if announce => self.echo("Edit server stopped", cx),
            Ok(Status::Unchanged) if announce && enabled => self.echo(
                format!(
                    "Edit server already running on {}",
                    crate::server::running_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                cx,
            ),
            Ok(Status::OtherInstance(path)) if announce => self.echo(
                format!(
                    "Edit server not started: another Macros is serving {}",
                    path.display()
                ),
                cx,
            ),
            Ok(_) => {}
            Err(e) => self.echo(format!("Edit server not started: {e}"), cx),
        }
    }

    /// Visit the files a client sent (each at its `+LINE:COLUMN`), answer it,
    /// and — when it waits — hold it until `C-x #` releases every file.
    pub fn server_visit(&mut self, incoming: Incoming, cx: &mut Context<Self>) {
        let Incoming {
            request,
            mut client,
        } = incoming;
        let mut files = HashSet::new();
        let mut created = HashSet::new();
        for file in &request.files {
            let identity = file_identity(&file.path);
            if !self.is_visiting(&identity) && !self.pending_opens.contains(&identity) {
                created.insert(identity.clone());
            }
            if let Some(line) = file.line {
                PENDING_GOTO
                    .lock()
                    .unwrap()
                    .get_or_insert_default()
                    .insert(identity.clone(), (line, file.column));
            }
            self.find_file(&file.path.to_string_lossy(), cx);
            // Already open: find_file switched to it synchronously.
            if self
                .active
                .path
                .as_deref()
                .is_some_and(|p| file_identity(p) == identity)
                && let Some(goto) = take_server_goto(&file.path)
            {
                self.goto_line_column(goto, cx);
            }
            files.insert(identity);
        }
        client.send(&Reply::Opened {
            files: request.files.len(),
        });
        if request.wait {
            if !files.is_empty() {
                self.echo("When done with a buffer, type C-x #", cx);
            }
            crate::server::hold(client, files, created);
        }
    }

    /// Move point to 1-based `line` and `column` (column 1 when absent).
    pub(super) fn goto_line_column(&mut self, (line, column): Goto, cx: &mut Context<Self>) {
        if self.active.grid.is_some() {
            return;
        }
        let offset = self.active.buffer.byte_at_char_column(
            line.saturating_sub(1),
            column.unwrap_or(1).saturating_sub(1),
        );
        self.move_to(offset, cx);
    }

    /// `C-x #` (Emacs `server-edit`): save the current buffer, tell any client
    /// waiting on it that it is done, and move on to the next client buffer. A
    /// buffer the server opened just for the client is killed.
    pub(super) fn server_edit(&mut self, cx: &mut Context<Self>) {
        let identity = self.active.path.as_deref().map(file_identity);
        let Some(identity) = identity.filter(|id| crate::server::is_waited_on(id)) else {
            self.echo("No server buffers remain to edit", cx);
            return;
        };
        if self.active.modified && !self.save_active_sync(true, cx) {
            return; // the save failure is already in the echo area
        }
        let released = crate::server::release(&identity);
        if released.created {
            self.kill_active_buffer(cx);
        }
        // Emacs moves on to the next buffer a client is still waiting on.
        let next = crate::server::waited_files().into_iter().find_map(|file| {
            self.docs()
                .find(|d| d.path.as_deref().is_some_and(|p| file_identity(p) == file))
                .map(|d| d.name.clone())
        });
        if let Some(name) = next {
            self.switch_to_buffer(&name, cx);
        }
        let message = match released.finished {
            0 => "Done with this buffer; the client waits on others".to_string(),
            1 => "Client finished".to_string(),
            n => format!("{n} clients finished"),
        };
        self.echo(message, cx);
    }
}

#[cfg(test)]
mod tests {

    use gpui::{Entity, TestAppContext};

    use super::super::*;
    use super::{Incoming, Reply, file_identity};
    use crate::server::{FileArg, Request};

    fn setup(cx: &mut TestAppContext) -> (Entity<Editor>, PathBuf) {
        test_context(cx);
        let dir = std::env::temp_dir().join(format!("macros-server-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let editor = cx.new(Editor::new);
        editor.update(cx, |e, cx| {
            for src in crate::runtime::startup_scripts() {
                e.run_script_blocking(src, cx);
            }
            e.mark_config_loaded(cx);
        });
        settle(&editor, cx);
        (editor, dir)
    }

    fn settle(editor: &Entity<Editor>, cx: &mut TestAppContext) {
        for _ in 0..6 {
            cx.run_until_parked();
            editor.read_with(cx, |e, _| {
                let _ = e.script.eval("0");
            });
            cx.run_until_parked();
        }
    }

    fn reply(far: &mut impl std::io::BufRead) -> Reply {
        let mut line = String::new();
        far.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[gpui::test]
    fn client_file_opens_at_position_and_c_x_hash_saves_and_releases(cx: &mut TestAppContext) {
        let (ed, dir) = setup(cx);
        let path = dir.join("COMMIT_EDITMSG");
        std::fs::write(&path, "one\ntwo words\nthree\n").unwrap();
        let (client, mut far) = crate::server::test_client();
        let request = Request {
            version: crate::server::PROTOCOL_VERSION,
            token: String::new(),
            wait: true,
            files: vec![FileArg {
                path: path.clone(),
                line: Some(2),
                column: Some(5),
            }],
        };
        ed.update(cx, |e, cx| e.server_visit(Incoming { request, client }, cx));
        settle(&ed, cx);
        assert_eq!(reply(&mut far), Reply::Opened { files: 1 });
        ed.read_with(cx, |e, _| {
            assert_eq!(e.active.path.as_deref(), Some(path.as_path()));
            // Line 2, column 5: just before "words".
            assert_eq!(e.active.selected_range, 8..8);
        });
        assert!(crate::server::is_waited_on(&file_identity(&path)));

        ed.update(cx, |e, cx| {
            e.edit(8..8, "more ");
            e.apply_command(EditorCommand::ServerEdit, cx);
        });
        settle(&ed, cx);
        assert_eq!(reply(&mut far), Reply::Done);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "one\ntwo more words\nthree\n"
        );
        // The server opened the buffer for the client, so it is gone again.
        ed.read_with(cx, |e, _| {
            assert!(!e.is_visiting(&file_identity(&path)));
            assert_eq!(e.status.to_string(), "Client finished");
        });

        // Not a client buffer: nothing to finish.
        ed.update(cx, |e, cx| e.apply_command(EditorCommand::ServerEdit, cx));
        ed.read_with(cx, |e, _| {
            assert_eq!(e.status.to_string(), "No server buffers remain to edit")
        });
    }

    #[gpui::test]
    fn killing_a_client_buffer_releases_its_client(cx: &mut TestAppContext) {
        let (ed, dir) = setup(cx);
        let path = dir.join("killed.txt");
        std::fs::write(&path, "x\n").unwrap();
        let (client, mut far) = crate::server::test_client();
        let request = Request {
            version: crate::server::PROTOCOL_VERSION,
            token: String::new(),
            wait: true,
            files: vec![FileArg {
                path: path.clone(),
                line: None,
                column: None,
            }],
        };
        ed.update(cx, |e, cx| e.server_visit(Incoming { request, client }, cx));
        settle(&ed, cx);
        assert_eq!(reply(&mut far), Reply::Opened { files: 1 });
        ed.update(cx, |e, cx| e.apply_command(EditorCommand::KillBuffer, cx));
        settle(&ed, cx);
        assert_eq!(reply(&mut far), Reply::Done);
    }
}
