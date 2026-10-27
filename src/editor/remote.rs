//! Remote (`/ssh:host:/path`) visits, reverts and saves for [`Editor`]: the
//! asynchronous halves that run [`crate::remote`]'s blocking ssh operations on
//! their own thread and finish on the UI thread. See `crate::remote` for the
//! transport and the save guarantees.

use super::*;

impl Editor {
    /// The remote branch of [`Self::read_and_open`]: read over ssh, then
    /// install the buffer (or, for a directory, show it in dired).
    pub(super) fn remote_read_and_open(
        &mut self,
        name: String,
        path: PathBuf,
        identity: PathBuf,
        remote: crate::remote::RemotePath,
        as_grid: bool,
        cx: &mut Context<Self>,
    ) {
        let read = crate::remote::spawn(move || crate::remote::read_for_buffer(&remote));
        cx.spawn(async move |this, cx| {
            let result = read
                .await
                .unwrap_or_else(|| Err(std::io::Error::other("remote read thread died")));
            this.update(cx, |editor, cx| {
                editor.pending_opens.remove(&identity);
                match result {
                    Ok(bytes) if as_grid => editor.open_grid(name, path, bytes, cx),
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => editor.open_file(name, path, text, cx),
                        Err(_) => editor.echo(format!("Could not open {name}: not UTF-8 text"), cx),
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound && as_grid => {
                        editor.open_grid(name, path, Vec::new(), cx)
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        editor.open_file(name, path, String::new(), cx)
                    }
                    // `C-x C-f /ssh:host:/etc` lands in dired, as a typed
                    // directory does in Emacs.
                    Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => {
                        let dir = format!("{}/", path.to_string_lossy().trim_end_matches('/'));
                        editor.publish_point();
                        editor.script.dispatch_action("dired-in", Some(&dir));
                    }
                    // As for a local file: never open an empty buffer on a
                    // file that couldn't be read — saving it would truncate it.
                    Err(err) => editor.echo(format!("Could not open {name}: {err}"), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// The remote branch of `revert-buffer`: re-read over ssh, then replace
    /// the text of whichever buffer still visits `path`.
    pub(super) fn remote_revert(
        &mut self,
        path: PathBuf,
        remote: crate::remote::RemotePath,
        cx: &mut Context<Self>,
    ) {
        let read = crate::remote::spawn(move || {
            crate::remote::read_for_buffer(&remote).and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })
        });
        cx.spawn(async move |this, cx| {
            let result = read
                .await
                .unwrap_or_else(|| Err(std::io::Error::other("remote read thread died")));
            this.update(cx, |editor, cx| {
                let text = match result {
                    Ok(text) => text,
                    Err(err) => return editor.echo(format!("Revert failed: {err}"), cx),
                };
                if editor.active.path.as_deref() == Some(path.as_path()) {
                    let point = editor.point();
                    editor.replace_whole_buffer(&text, point, cx);
                    editor.active.modified = false;
                    let name = editor.active.name.clone();
                    editor.echo(format!("Reverted {name}"), cx);
                } else if let Some(idx) = editor
                    .inactive
                    .iter()
                    .position(|d| d.path.as_deref() == Some(path.as_path()))
                {
                    editor.replace_inactive_text(idx, &text, false);
                    editor.inactive[idx].modified = false;
                    let name = editor.inactive[idx].name.clone();
                    editor.echo(format!("Reverted {name}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Save buffer `name` (holding `bytes`) to the remote file `path` off the
    /// UI thread — the remote branch of `write_active_to_disk`, and of
    /// `save-some-buffers` for a buffer that isn't active. The caller has
    /// already marked the buffer unmodified; a failure re-marks it.
    ///
    /// Same ordering as a local save (a [`crate::save`] ticket taken now, with
    /// the text). The host refuses the write if the file changed there since
    /// the buffer read it; that is put to the user exactly like a local
    /// external change.
    pub(super) fn save_remote(
        &mut self,
        name: String,
        path: PathBuf,
        bytes: Vec<u8>,
        uri: String,
        cx: &mut Context<Self>,
    ) {
        let ticket = crate::save::ticket(&path);
        // Until the result is handled, a quit waits for this save.
        let pending = crate::save::begin_pending();
        let write_path = path.clone();
        let write = crate::remote::spawn(move || {
            let outcome = crate::save::write_sequenced(&ticket, &write_path, &bytes);
            (ticket, outcome)
        });
        cx.spawn(async move |this, cx| {
            let Some((ticket, result)) = write.await else {
                pending.failed();
                return;
            };
            if result.is_err() {
                pending.failed();
            }
            this.update(cx, |editor, cx| match result {
                Ok(crate::save::SaveOutcome::Written) => {
                    editor.echo(format!("Wrote {name}"), cx);
                    editor.lsp_did_save(&uri);
                    editor.run_hook("after-save-hook");
                }
                Ok(crate::save::SaveOutcome::Superseded) => {}
                Err(err) => {
                    if crate::save::is_latest(&ticket)
                        && let Some(doc) = editor
                            .docs_mut()
                            .find(|d| d.path.as_deref() == Some(path.as_path()))
                    {
                        doc.modified = true;
                    }
                    if crate::remote::is_changed_on_disk(&err) && editor.active.name == name {
                        let file = path.file_name().map_or_else(
                            || path.display().to_string(),
                            |f| f.to_string_lossy().into_owned(),
                        );
                        editor.open_confirm(
                            &format!(
                                "{file} changed on the remote host since it was read; save over it anyway?"
                            ),
                            PendingAction::SaveOverChanged(name),
                            cx,
                        );
                    } else if crate::remote::is_changed_on_disk(&err) {
                        editor.echo(super::buffers::changed_on_disk_failure(&name), cx);
                    } else {
                        editor.echo(format!("Save failed: {name}: {err}"), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}
