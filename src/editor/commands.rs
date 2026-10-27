//! Editor command dispatch for [`Editor`], extracted from the editor module for size.
//! These methods extend the same `Editor` type via an additional
//! `impl` block; cross-module entry points are `pub(super)`.

use std::path::PathBuf;

use gpui::{Context, SharedString, px};

use crate::highlight::Highlighter;
use crate::mode::{Mode, ModeStack};
use crate::script::EditorCommand;
use crate::theme::Theme;

use super::*;

/// A category dispatcher: handles the commands it owns, handing any other back.
type CommandHandler =
    fn(&mut Editor, EditorCommand, &mut Context<Editor>) -> Result<(), EditorCommand>;

impl Editor {
    /// Apply a command, honoring a pending universal-argument (Emacs `C-u N`):
    /// a repeatable command runs `N` times (as one undo group for edits); others
    /// run once and the argument is discarded. Every command — from a key, a
    /// script, or an OS-special action — flows through here.
    pub(super) fn apply_command(&mut self, command: EditorCommand, cx: &mut Context<Self>) {
        match self.uarg.pending.take() {
            Some(n) if n > 1 && command.is_repeatable() => {
                self.status = SharedString::default();
                self.begin_undo_group();
                for _ in 0..n {
                    self.apply_command_once(command.clone(), cx);
                }
                self.end_undo_group();
            }
            _ => self.apply_command_once(command, cx),
        }
        self.reconcile_locks(cx);
    }

    /// Apply a single command exactly once.
    pub(super) fn apply_command_once(&mut self, command: EditorCommand, cx: &mut Context<Self>) {
        let yank_like = matches!(command, EditorCommand::Yank | EditorCommand::YankPop);
        self.dispatch_command(command, cx);
        self.last_was_yank = yank_like;
    }

    /// The category dispatchers, in routing order. Each `cmd_*` handler
    /// returns `Err(command)` for variants it does not own, so the command
    /// falls through to the next; the partition is exhaustive. Grid commands,
    /// and generic ones (undo, save, clipboard) that mean something else in a
    /// grid buffer, go first.
    const COMMAND_HANDLERS: &[CommandHandler] = &[
        Self::cmd_grid,
        Self::cmd_motion,
        Self::cmd_narrow,
        Self::cmd_edit,
        Self::cmd_view,
        Self::cmd_buffer,
        Self::cmd_window,
        Self::cmd_lsp,
        Self::cmd_completion,
        Self::cmd_helm,
        Self::cmd_evil,
        Self::cmd_overlay,
        Self::cmd_proc,
        Self::cmd_extras,
        Self::cmd_misc,
        Self::cmd_session,
    ];

    /// Route a command to the first of [`Self::COMMAND_HANDLERS`] that owns it.
    fn dispatch_command(&mut self, mut command: EditorCommand, cx: &mut Context<Self>) {
        for handler in Self::COMMAND_HANDLERS {
            match handler(self, command, cx) {
                Ok(()) => return,
                Err(c) => command = c,
            }
        }
        debug_assert!(false, "unhandled editor command: {command:?}");
    }

    /// Cursor motion, selection, marks, and goto.
    fn cmd_motion(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            ForwardChar => {
                let o = self.active.buffer.next_grapheme(self.point());
                self.goto(o, cx);
            }
            BackwardChar => {
                let o = self.active.buffer.prev_grapheme(self.point());
                self.goto(o, cx);
            }
            NextLine => {
                let o = self.line_offset(1);
                self.goto(o, cx);
            }
            PreviousLine => {
                let o = self.line_offset(-1);
                self.goto(o, cx);
            }
            ForwardWord => {
                let o = self.active.buffer.forward_word(self.point());
                self.goto(o, cx);
            }
            ForwardWordStart => {
                let o = self.active.buffer.forward_word_start(self.point());
                self.goto(o, cx);
            }
            BackwardWord => {
                let o = self.active.buffer.backward_word(self.point());
                self.goto(o, cx);
            }
            BackwardWordStart => {
                let o = self.active.buffer.backward_word_start(self.point());
                self.goto(o, cx);
            }
            ForwardWordEnd => {
                let o = self.active.buffer.forward_word_end(self.point());
                self.goto(o, cx);
            }
            ForwardBigwordStart => {
                let o = self.active.buffer.forward_bigword_start(self.point());
                self.goto(o, cx);
            }
            BackwardBigwordStart => {
                let o = self.active.buffer.backward_bigword_start(self.point());
                self.goto(o, cx);
            }
            ForwardBigwordEnd => {
                let o = self.active.buffer.forward_bigword_end(self.point());
                self.goto(o, cx);
            }
            IndentRegion => self.shift_lines(1, cx),
            OutdentRegion => self.shift_lines(-1, cx),
            ReindentRegion => self.reindent_region(cx),
            JsonPrettyPrintBuffer => self.json_pretty_print_buffer(cx),
            JsonPrettyPrintRegion => self.json_pretty_print_region(cx),
            BeginningOfLine => {
                let o = self.active.buffer.line_range_at(self.point()).start;
                self.goto(o, cx);
            }
            EndOfLine => {
                let o = self.active.buffer.line_range_at(self.point()).end;
                self.goto(o, cx);
            }
            BeginningOfBuffer => self.goto(self.accessible().start, cx),
            EndOfBuffer => self.goto(self.accessible().end, cx),
            ExtendBackwardChar => {
                let o = self.active.buffer.prev_grapheme(self.point());
                self.select_to(o, cx);
            }
            ExtendForwardChar => {
                let o = self.active.buffer.next_grapheme(self.point());
                self.select_to(o, cx);
            }
            SetMark => self.set_mark(cx),
            ExchangePointAndMark => self.exchange_point_and_mark(cx),
            MarkWholeBuffer => self.mark_whole_buffer(cx),
            GotoLine(n) => {
                let last = self.active.buffer.line_count().saturating_sub(1);
                let offset = self.active.buffer.line_range(n.min(last)).start;
                self.move_to(offset, cx);
            }
            ScrollCursorToRow(row) => self.scroll_cursor_to_row(row, cx),
            // Scheme-supplied offsets: clamp AND floor to a char boundary, so a
            // byte-arithmetic position inside a multibyte char can't become a
            // point the rope later refuses to slice at.
            GotoChar(n) => {
                let offset = self.active.buffer.snap(n);
                self.move_to(offset, cx);
            }
            SelectTo(n) => {
                let offset = self.active.buffer.snap(n);
                self.select_to(offset, cx);
            }
            SetRegion { start, end } => {
                // Inverted ends are a backwards region: point at `end`.
                let (start, end) = (self.clamp_accessible(start), self.clamp_accessible(end));
                let range = self.active.buffer.clamp_range(start..end);
                self.active.mark_active = true;
                self.active.selected_range = range;
                self.active.selection_reversed = end < start;
                self.ensure_cursor_visible();
                cx.notify();
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// Text insertion, deletion, kill ring, and undo.
    fn cmd_edit(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        // A special buffer (magit, dired, *Help*, …) is read-only: its text is
        // program-generated and its line/byte offsets back the mode's own
        // bookkeeping. Plain keys can't reach the buffer (the special-buffer key
        // layer swallows them) and neither can the IME, but the Emacs editing
        // CHORDS fall through to the global `fundamental` map — so without this
        // guard `C-k`, `C-y`, `C-w`, `M-d`, `C-t`, `C-o`, `cmd-v`, … would
        // happily rewrite a magit status buffer. Emacs answers with
        // "Buffer is read-only"; so do we. Programmatic primitives
        // (`replace-region`, `insert-at`, `delete-region`, `insert`) stay
        // allowed — that's how Scheme builds these buffers in the first place.
        // A *running* terminal buffer is special too, but there the clipboard
        // commands delegate to the PTY (cmd-v types into the shell) before ever
        // touching the rope, so they must not be refused here.
        if self.active.special.is_some()
            && self.active_running_terminal_id().is_none()
            && command.is_interactive_edit()
        {
            self.echo("Buffer is read-only", cx);
            return Ok(());
        }
        match command {
            Insert(text) => self.insert_text(&text, cx),
            Newline => self.newline(cx),
            OpenLine => self.open_line(cx),
            IndentForTab => self.indent_for_tab(cx),
            DeleteChar => self.delete_char_forward(cx),
            DeleteBackwardChar => self.delete_char_backward(cx),
            TransposeChars => self.transpose_chars(cx),
            KillLine => self.kill_line(cx),
            KillWord => self.kill_word(cx),
            BackwardKillWord => self.backward_kill_word(cx),
            KillRegion => self.kill_region(cx),
            KillRingSave => self.kill_ring_save(cx),
            ClipboardCopy => self.clipboard_copy(cx),
            ClipboardCut => self.clipboard_cut(cx),
            ClipboardPaste => self.clipboard_paste(cx),
            ClipboardSet(text) => self.set_clipboard(&text, cx),
            Yank => self.yank(cx),
            YankPop => self.yank_pop(cx),
            Undo => self.undo(cx),
            Redo => self.redo(cx),
            UndoTreeDump(callback) => {
                let dump = self.undo_tree_dump();
                self.script.dispatch_action(&callback, Some(&dump));
            }
            DeleteRegion { start, end } => self.delete_region(start, end, cx),
            KillRegionBetween { start, end } => {
                let range = self.active.buffer.clamp_range(start..end);
                self.kill_range(range, cx)
            }
            ReplaceRegion { start, end, text } => self.replace_region(start, end, &text, cx),
            InsertAt { pos, text } => self.insert_at(pos, &text, cx),
            UndoBoundary => self.undo_boundary(),
            UndoAmalgamateBegin => self.undo_amalgamate = Some(self.undo_base()),
            UndoAmalgamateEnd => {
                if let Some(base) = self.undo_amalgamate.take() {
                    self.squash_undo_since(&base);
                    if let (Some(depth), Some(g)) = (base.grid, self.active.grid.as_deref_mut())
                        && self.active.name == base.buffer
                    {
                        g.squash_undo_since(depth);
                    }
                }
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// Scrolling, toggles, and per-line decorations.
    fn cmd_view(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            ScrollUp => self.scroll_page(1, cx),
            ScrollDown => self.scroll_page(-1, cx),
            Recenter => self.recenter(cx),
            SetLineIcon(line, col, name) => {
                self.redirect_target()
                    .line_icons
                    .insert(line, vec![(col, SharedString::from(name), None)]);
                cx.notify();
            }
            AddLineIcon(line, col, name, face) => {
                let face = (!face.is_empty()).then(|| SharedString::from(face));
                self.redirect_target()
                    .line_icons
                    .entry(line)
                    .or_default()
                    .push((col, SharedString::from(name), face));
                cx.notify();
            }
            ClearLineIcons => {
                self.redirect_target().line_icons.clear();
                cx.notify();
            }
            SetGutterDot(line, face) => {
                self.redirect_target()
                    .gutter_dots
                    .insert(line, SharedString::from(face));
                cx.notify();
            }
            ClearGutterDots => {
                self.redirect_target().gutter_dots.clear();
                cx.notify();
            }
            SetLineImage {
                line,
                path,
                max_width,
            } => self.set_line_image(line, path, max_width, cx),
            ClearLineImages => {
                let redirected = self.redirect_index().is_some();
                let images = &mut self.redirect_target().line_images;
                if !images.is_empty() {
                    images.clear();
                    // Only the active buffer's rows moved under the cursor.
                    if !redirected {
                        self.ensure_cursor_visible();
                    }
                    cx.notify();
                }
            }
            GuardBufferUnchanged {
                name,
                tick,
                on_stale,
            } => {
                // Compare against the buffer this batch's decorations will land
                // on. A mismatch means the text (or the buffer under the batch)
                // changed after the script read its snapshot: the remaining
                // commands carry stale byte offsets, so the batch loop drops
                // them (`apply_script_outcome`) and the caller's `on_stale`
                // reschedules the repaint.
                let target = match self.redirect_index() {
                    Some(i) => &self.inactive[i],
                    None => &self.active,
                };
                let (cur_name, cur_tick) = (target.name.as_str(), target.buffer.tick());
                if cur_name != name || cur_tick != tick {
                    self.script_batch_stale = true;
                    if !on_stale.is_empty() {
                        self.publish_point();
                        self.script.dispatch_action(&on_stale, None);
                    }
                }
            }
            SetLineIndent { line, cols } => {
                let indents = &mut self.redirect_target().overlays.indents;
                if cols == 0 {
                    indents.remove(&line);
                } else {
                    indents.insert(line, cols);
                }
                cx.notify();
            }
            ClearLineIndents => {
                let indents = &mut self.redirect_target().overlays.indents;
                if !indents.is_empty() {
                    indents.clear();
                    cx.notify();
                }
            }
            SetLineScale { line, scale } => {
                let scales = &mut self.redirect_target().overlays.scales;
                if scale <= 1.0 {
                    scales.remove(&line);
                } else {
                    scales.insert(line, scale);
                }
                cx.notify();
            }
            ClearLineScales => {
                let scales = &mut self.redirect_target().overlays.scales;
                if !scales.is_empty() {
                    scales.clear();
                    cx.notify();
                }
            }
            GhostShow { text } => {
                // Anchored to point as of application (commands apply on the
                // foreground in order, so an edit queued after the show clears
                // it again — never a stale ghost). An empty string is a clear.
                self.active.ghost = (!text.is_empty()).then(|| GhostText {
                    at: self.point(),
                    text,
                });
                // Scroll a multi-line suggestion's reserved rows into view.
                self.ensure_cursor_visible();
                cx.notify();
            }
            GhostClear => {
                if self.active.ghost.take().is_some() {
                    cx.notify();
                }
            }
            GhostAccept => {
                // Only a *visible* ghost accepts (anchor == point); the text is
                // inserted as one ordinary edit, so undo treats the acceptance
                // as a single unit and `edit` clears the ghost itself.
                if let Some((at, text)) = self.ghost_at().map(|(at, t)| (at, t.to_string())) {
                    let cursor = self.edit(at..at, &text);
                    self.collapse_to(cursor);
                    self.ensure_cursor_visible();
                    cx.notify();
                }
            }
            ToggleLineNumbers => {
                // Toggle for the current buffer's major mode, so it mirrors
                // Emacs's per-mode `display-line-numbers-mode`.
                let major = self.active.modes.names().last().map(|s| s.to_string());
                let key = match &major {
                    Some(m) => format!("line-numbers:{m}"),
                    None => "line-numbers".to_string(),
                };
                let on = cx
                    .global::<crate::settings::Settings>()
                    .line_numbers_for(major.as_deref());
                self.toggle_setting(key, on, "Line numbers", cx);
            }
            ToggleScrollBar => {
                let on = cx.global::<crate::settings::Settings>().scroll_bar();
                self.toggle_setting("scroll-bar", on, "Scroll bar", cx);
            }
            ToggleWordWrap => {
                // Toggle for the current buffer's major mode, mirroring Emacs's
                // per-mode `visual-line-mode`.
                let major = self.active.modes.names().last().map(|s| s.to_string());
                let key = match &major {
                    Some(m) => format!("word-wrap:{m}"),
                    None => "word-wrap".to_string(),
                };
                let on = cx
                    .global::<crate::settings::Settings>()
                    .word_wrap_for(major.as_deref());
                // Leaving word-wrap resets any horizontal scroll to a clean state.
                self.h_scroll_offset = px(0.);
                self.toggle_setting(key, on, "Word wrap", cx);
            }
            ToggleGitGutter => {
                let on = cx.global::<crate::settings::Settings>().magit_gutter();
                self.toggle_setting("magit-gutter", on, "Git gutter", cx);
                self.refresh_git_gutter(cx);
            }
            GitGutterRefresh => self.refresh_git_gutter(cx),
            ToggleElectricPair => {
                self.electric_pair = !self.electric_pair;
                self.echo(
                    format!(
                        "electric-pair-mode {}",
                        if self.electric_pair {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    ),
                    cx,
                );
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// Buffer list, files, saving, and directory watching.
    fn cmd_buffer(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            FindFile => self.find_file_prompt(cx),
            OpenFile(path) => self.find_file(&path, cx),
            SwitchBuffer => {
                self.open_minibuffer(
                    "Switch to buffer: ",
                    MinibufferPurpose::SwitchBuffer,
                    "",
                    cx,
                );
            }
            SwitchToBufferNamed(name) => self.switch_to_buffer(&name, cx),
            KillBufferNamed(name) => self.kill_buffer_named_confirming(&name, cx),
            RetargetFileBuffers(old, new) => self.retarget_file_buffers(&old, &new, cx),
            SetMajorMode(mode) => {
                // A mode whose name matches a bundled tree-sitter language carries
                // that language, so highlighting AND the language server follow the
                // mode — `M-x rust-mode` on any buffer colors it and (via the
                // `fundamental` `enable-lsp` hook below) starts rust-analyzer.
                // Modes with no grammar (shell-mode, git-commit-mode, …) install
                // only the keymap and leave any existing highlighter alone.
                let language_highlighter = Highlighter::for_language(&mode);
                let mut new_mode = Mode::new(&mode);
                if language_highlighter.is_some() {
                    new_mode = new_mode.with_language(&mode);
                }
                self.active.modes = ModeStack::with_major(new_mode);
                if let Some(h) = language_highlighter {
                    self.active.highlighter = Some(h);
                    self.parse_full();
                } else if mode == "anvil-repl-mode" {
                    // The ANVIL REPL displays Scheme, so highlight it in place — no
                    // need to swap to `macros-steel`, which would discard the
                    // anvil-repl keymap (RET → anvil-send-input) and kill the REPL.
                    //
                    // Drive the highlighter DIRECTLY rather than tagging the mode
                    // with a `language`: a mode language also feeds
                    // `active_language`, which `enable-lsp` keys off — so it would
                    // attach the builtin Steel LSP and lint the transcript (the
                    // `;; =>` scrollback isn't a program). The REPL introspects its
                    // live image instead (completion, doc, apropos — see
                    // steel-anvil).
                    self.active.highlighter = Highlighter::scheme();
                    self.parse_full();
                }
                // Entering a mode runs its hooks (Emacs `run-mode-hooks`): a user's
                // `(add-hook "rust" …)` fires whether the buffer reached rust-mode
                // by file detection or `M-x rust-mode`, and `enable-lsp` brings up
                // the server for the now-active language. Idempotent — re-running
                // `fundamental`'s hooks is a no-op when nothing changed.
                self.active.hooks_ran = false;
                self.run_mode_hooks_once(cx);
                cx.notify();
            }
            SetSpecialMode(mode) => {
                if mode.is_empty() {
                    self.active.special = None;
                } else {
                    self.active.modes = ModeStack::with_major(Mode::new(&mode));
                    self.active.special = Some(mode);
                    self.active.modified = false;
                }
                cx.notify();
            }
            NextBuffer => self.next_buffer(cx),
            PreviousBuffer => self.previous_buffer(cx),
            SaveBuffer => self.save_buffer(cx),
            SaveSomeBuffers => self.save_some_buffers(cx),
            RevertBuffer => self.revert_buffer(cx),
            OpenFileOtherWindow(path) => self.open_file_other_window(path, cx),
            WriteFile(path) => self.write_file(&path, false, cx),
            RenameBuffer(name) => self.rename_buffer(&name, cx),
            InsertFile(path) => self.insert_file(&path, cx),
            FindFileOtherWindow(path) => self.find_file_other_window(&path, cx),
            SwitchToBufferOtherWindow(name) => self.switch_to_buffer_other_window(&name, cx),
            DisplayBuffer(name) => self.display_buffer(&name, cx),
            ToggleReadOnly => self.toggle_read_only(cx),
            KillBuffer => self.kill_buffer(cx),
            McpServerInfo => self.show_mcp_server_info(cx),
            ServerEdit => self.server_edit(cx),
            KillBufferAndWindow => self.kill_buffer_and_window(cx),
            Quit => self.quit(cx),
            SetExtension(ext, language) => {
                self.extensions.insert(ext, language);
            }
            WatchDirectories(owner, dirs) => {
                let dirs: Vec<PathBuf> = dirs.into_iter().map(PathBuf::from).collect();
                if dirs.is_empty() {
                    self.fs.owners.remove(&owner);
                } else {
                    self.fs.owners.insert(owner, dirs);
                }
                // Rebuild the flattened membership set, then sync the watcher.
                self.fs.dirs = self.fs.owners.values().flatten().cloned().collect();
                self.reconcile_watches(cx);
            }
            WatchTree(owner, dirs) => {
                let dirs: Vec<PathBuf> = dirs.into_iter().map(PathBuf::from).collect();
                if dirs.is_empty() {
                    self.fs.tree_owners.remove(&owner);
                } else {
                    self.fs.tree_owners.insert(owner, dirs);
                }
                self.fs.trees = self.fs.tree_owners.values().flatten().cloned().collect();
                self.reconcile_watches(cx);
            }
            RenderBuffer {
                name,
                mode,
                text,
                line_faces,
                quiet: false,
            } => self.render_buffer(name, mode, text, line_faces, cx),
            RenderBuffer {
                name,
                mode,
                text,
                line_faces,
                quiet: true,
            } => self.render_buffer_quiet(name, mode, text, line_faces, cx),
            AppendToBuffer { name, text } => self.append_to_buffer(name, text, cx),
            EditFileText { path, callback } => self.edit_file_text(path, callback, 0, cx),
            SetFileText { path, text } => self.set_file_text(path, text, cx),
            FontifyOrgBuffer(name) => self.fontify_org_buffer(name, cx),
            DiffBuffers { a, b, callback } => self.diff_buffers(&a, &b, &callback),
            BufferCopyLines {
                src,
                src_start,
                src_end,
                dst,
                dst_start,
                dst_end,
            } => self.buffer_copy_lines(&src, src_start, src_end, &dst, dst_start, dst_end, cx),
            BufferHighlightLines {
                name,
                start,
                end,
                face,
            } => self.buffer_highlight_lines(&name, start, end, face, cx),
            BufferClearHighlights { name } => self.buffer_clear_highlights(&name, cx),
            BufferLocalSet { key, value } => {
                self.active.locals.insert(key, value);
            }
            ReadFileName { prompt, callback } => {
                let initial = self.default_directory_input();
                self.open_minibuffer(
                    &prompt,
                    MinibufferPurpose::SchemeReadFile(callback),
                    &initial,
                    cx,
                );
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// Window splits and the transient menu.
    fn cmd_window(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            SelectWindow(id) => self.select_pane(id, cx),
            Transient {
                title,
                columns,
                sticky,
            } => self.open_transient(title, columns, sticky, cx),
            TransientQuit => self.transient_quit(cx),
            SplitWindowBelow => self.split_window(SplitAxis::Vertical, cx),
            SplitWindowRight => self.split_window(SplitAxis::Horizontal, cx),
            SplitWindowRightRatio(ratio) => {
                self.split_window_ratio(SplitAxis::Horizontal, ratio as f32, cx)
            }
            OtherWindow => self.other_window(cx),
            DeleteWindow => self.delete_window(cx),
            DeleteOtherWindows => self.delete_other_windows(cx),
            EnlargeWindow => self.resize_window(SplitAxis::Vertical, 0.05, cx),
            ShrinkWindow => self.resize_window(SplitAxis::Vertical, -0.05, cx),
            EnlargeWindowHorizontally => self.resize_window(SplitAxis::Horizontal, 0.05, cx),
            ShrinkWindowHorizontally => self.resize_window(SplitAxis::Horizontal, -0.05, cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// Language server, xref, and tree-sitter requests.
    fn cmd_lsp(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            SetLsp(language, command, env) => {
                self.lsp_commands
                    .insert(language, super::lsp_methods::LspCommand { command, env });
            }
            SetLanguageId(ext, language_id) => {
                self.language_ids.insert(ext, language_id);
            }
            LspDefinition => self.lsp_goto("textDocument/definition", "definition", cx),
            LspDeclaration => self.lsp_goto("textDocument/declaration", "declaration", cx),
            LspTypeDefinition => {
                self.lsp_goto("textDocument/typeDefinition", "type definition", cx)
            }
            LspImplementation => self.lsp_goto("textDocument/implementation", "implementation", cx),
            LspReferences => self.lsp_references(cx),
            LspHover => self.lsp_hover(cx),
            LspSignatureHelp => self.lsp_signature_help(cx),
            LspDocumentSymbols => self.lsp_document_symbols(cx),
            LspCodeAction => self.lsp_code_action(cx),
            LspFormatBuffer => self.lsp_format(cx),
            LspRestart => self.lsp_restart(cx),
            LspCallHierarchyIncoming => {
                self.lsp_call_hierarchy(crate::lsp::CallDirection::Incoming, cx)
            }
            LspCallHierarchyOutgoing => {
                self.lsp_call_hierarchy(crate::lsp::CallDirection::Outgoing, cx)
            }
            LspPrepareRename => self.lsp_prepare_rename(cx),
            LspCodeLens => self.lsp_code_lens(cx),
            LspXrefVisit => self.lsp_xref_visit(cx),
            LspApplyCodeAction => self.lsp_apply_code_action(cx),
            LspRename(name) => self.lsp_rename(name, cx),
            LspWorkspaceSymbol(query) => self.lsp_workspace_symbol(query, cx),
            LspRequest {
                method,
                kind,
                params,
                callback,
            } => self.lsp_script_request(method, kind, params, callback, cx),
            LspGoto {
                uri,
                line,
                character,
            } => self.goto_file_position(uri, line, character, cx),
            LspApplyWorkspaceEdit(json) => self.lsp_apply_workspace_edit_json(json, cx),
            LspApplyTextEdits(json) => self.lsp_apply_text_edits_json(json, cx),
            XrefPushMarker => self.xref_push_marker(),
            XrefPopMarker => self.xref_pop_marker(cx),
            SetLspFormatOnSave(lang, on) => {
                if on {
                    self.lsp_format_on_save.insert(lang);
                } else {
                    self.lsp_format_on_save.remove(&lang);
                }
            }
            LspSemanticTokens(callback) => self.lsp_semantic_tokens(callback, cx),
            LspSemanticTokensRange(callback) => self.lsp_semantic_tokens_range(callback, cx),
            LspSemanticTokensDelta(callback) => self.lsp_semantic_tokens_delta(callback, cx),
            TreeSitterQuery { query, callback } => self.tree_sitter_query(query, callback),
            TreeSitterNodeAt { byte, callback } => self.tree_sitter_node_at(byte, callback),
            EnableLsp => self.maybe_start_lsp(cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// In-buffer completion.
    fn cmd_completion(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            CompletionAtPoint => self.completion_at_point(cx),
            CompletionNext => self.completion_next(cx),
            CompletionPrevious => self.completion_previous(cx),
            CompletionAccept => self.completion_accept(cx),
            CompletionAbort => self.completion_abort(cx),
            AddCapf(function) => {
                if !self.capf_functions.contains(&function) {
                    self.capf_functions.push(function);
                }
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// Helm narrowing UI.
    fn cmd_helm(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            Helm(specs) => {
                let sources = helm_sources_from_specs(specs);
                self.helm_open("Helm: ".to_string(), sources, String::new(), cx);
            }
            HelmFrom(initial, specs) => {
                let sources = helm_sources_from_specs(specs);
                self.helm_open("Helm: ".to_string(), sources, initial, cx);
            }
            HelmMx => self.helm_mx(cx),
            HelmNext => self.helm_next(cx),
            HelmPrevious => self.helm_previous(cx),
            HelmAccept => self.helm_accept(cx),
            HelmAbort => self.helm_abort(cx),
            HelmActions => self.helm_action_menu(cx),
            HelmToggleMark => self.helm_toggle_mark(cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// Evil modal states, motions, and incremental search.
    fn cmd_evil(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            HelixMode => self.helix_enable(cx),
            HelixDisable => self.helix_disable(cx),
            HelixNormalState => self.helix_normal_state(cx),
            EvilMode => self.evil_enable(cx),
            EvilNormalState => self.evil_normal_state(cx),
            EvilInsertState => self.evil_insert_state(cx),
            EvilVisualState => self.evil_visual_state(cx),
            EvilVisualLineState => self.evil_visual_line_state(cx),
            EvilVisualInsert => self.evil_visual_insert(false, cx),
            EvilVisualAppend => self.evil_visual_insert(true, cx),
            EvilVisualBlockState => self.evil_visual_block_state(cx),
            EvilBlockInsert => self.evil_block_insert(false, cx),
            EvilBlockAppend => self.evil_block_insert(true, cx),
            EvilBlockDelete => self.evil_block_delete(cx),
            EvilBlockChange => self.evil_block_change(cx),
            EvilBlockYank => self.evil_block_yank(cx),
            EvilDisable => self.evil_disable(cx),
            EvilScrollDown => self.scroll_half(1, cx),
            EvilScrollUp => self.scroll_half(-1, cx),
            EvilSearchForward => self.evil_search_forward(cx),
            EvilSearchBackward => self.evil_search_backward(cx),
            EvilSearchNext => self.evil_search_next(cx),
            EvilSearchPrevious => self.evil_search_previous(cx),
            EvilPasteAfter => self.evil_paste(true, cx),
            EvilPasteBefore => self.evil_paste(false, cx),
            EvilReplaceChar => {
                // Arm replace-pending; the dispatcher captures the next key.
                // The arming command isn't what a macro / `.` should replay
                // (it would wait for a key) — key_evil_replace records the
                // resolved `(evil-replace-with "x")` in its place.
                self.unrecord_last_event();
                self.evil_replace_pending = true;
                self.evil_replace_count = self.prefix_count.max(1);
                self.status = "r-".into();
                cx.notify();
            }
            EvilReplaceWith(ch) => {
                self.evil_replace_count = self.prefix_count.max(1);
                self.evil_replace_char(&ch, cx);
            }
            ReadChar { prompt, callback } => {
                // Arm the capture; the key dispatcher feeds the next key to it.
                // If a keyboard macro is recording, drop the just-recorded
                // arming expr ("(evil-find-char-forward)"): replaying it would
                // re-arm the capture and hang waiting for a keystroke macro
                // playback can never provide. key_read_char records the
                // RESOLVED callback call in its place, so `f o` replays as one
                // self-contained command.
                self.unrecord_last_event();
                self.read_char_pending = Some(callback);
                self.status = prompt.into();
                cx.notify();
            }
            ReadKeyStart { prompt, callback } => {
                self.read_key_pending = Some(callback);
                self.status = prompt.into();
                cx.notify();
            }
            ReadKeyStop => {
                self.read_key_pending = None;
                cx.notify();
            }
            SetSearchQuery { query, forward } => {
                self.search_query = Some(query);
                self.evil_search_forward = forward;
            }
            SetCountAware(name) => {
                self.count_aware.insert(name);
            }
            CountRepeatStep => self.count_repeat_step(cx),
            ReplayStep => self.replay_step(cx),
            EvilRepeat => self.evil_repeat(cx),
            EvilBlockReplace(ch) => self.evil_block_replace(&ch, cx),
            HelixPasteAfter => self.helix_paste(true, cx),
            HelixPasteBefore => self.helix_paste(false, cx),
            EvilSetLinewise => {
                self.evil_yank_linewise = true;
                self.vim.registers.set_linewise_last(true);
            }
            EvilSetCharwise => {
                self.evil_yank_linewise = false;
                self.vim.registers.set_linewise_last(false);
            }
            EvilUseRegister(r) => self.evil_use_register(&r, cx),
            EvilInsertRegister(r) => self.evil_insert_register(&r, cx),
            EvilMacroRecord(r) => self.evil_macro_record(&r, cx),
            EvilMacroPlay(r) => self.evil_macro_play(&r, cx),
            EvilSetMark(r) => self.evil_set_mark(&r, cx),
            EvilGotoMark(r, exact) => self.evil_goto_mark(&r, exact, cx),
            SetJumpCommand(name) => {
                self.vim.jump_commands.insert(name);
            }
            EvilReselectVisual => self.evil_reselect_visual(cx),
            EvilEx(line) => self.run_ex_command(&line, cx),
            ReindentLines(first, last) => self.reindent_lines(first, last, cx),
            EvilExGotoLine(n) => self.evil_ex_goto_line(&n, cx),
            EvilJumpBackward => self.evil_jump(false, cx),
            EvilJumpForward => self.evil_jump(true, cx),
            WindmoveLeft => self.window_move(panes::Dir::Left, cx),
            WindmoveDown => self.window_move(panes::Dir::Down, cx),
            WindmoveUp => self.window_move(panes::Dir::Up, cx),
            WindmoveRight => self.window_move(panes::Dir::Right, cx),
            PreviousWindow => self.previous_window(cx),
            LastWindow => self.last_window(cx),
            TopLeftWindow => self.window_corner(false, cx),
            BottomRightWindow => self.window_corner(true, cx),
            BalanceWindows => self.balance_windows(cx),
            WinnerUndo => self.winner_undo(cx),
            WinnerRedo => self.winner_redo(cx),
            MaximizeWindowHeight => self.maximize_window(SplitAxis::Vertical, cx),
            MaximizeWindowWidth => self.maximize_window(SplitAxis::Horizontal, cx),
            MoveWindowFarLeft => self.move_window_far(panes::Dir::Left, cx),
            MoveWindowFarDown => self.move_window_far(panes::Dir::Down, cx),
            MoveWindowFarUp => self.move_window_far(panes::Dir::Up, cx),
            MoveWindowFarRight => self.move_window_far(panes::Dir::Right, cx),
            ExchangeWindow => self.exchange_window(cx),
            RotateWindowsDown => self.rotate_windows(true, cx),
            RotateWindowsUp => self.rotate_windows(false, cx),
            NewWindowBelow => self.new_window_buffer(SplitAxis::Vertical, cx),
            NewWindowRight => self.new_window_buffer(SplitAxis::Horizontal, cx),
            EvilExCommand => {
                // From Visual, the command line starts on the selected lines
                // (`:'<,'>`), and Visual ends — as in Vim.
                let visual = matches!(
                    self.evil,
                    EvilState::Visual | EvilState::VisualLine | EvilState::VisualBlock
                ) && !self.modal_helix;
                if visual {
                    self.note_visual_exit();
                    self.evil_normal_state(cx);
                }
                let initial = if visual { "'<,'>" } else { "" };
                self.open_minibuffer(":", MinibufferPurpose::ExCommand, initial, cx);
            }
            IsearchForward => self.isearch_start(Direction::Forward, cx),
            IsearchBackward => self.isearch_start(Direction::Backward, cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// Overlays, folds, faces, and text properties.
    fn cmd_overlay(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            OverlayFace { start, end, face } => {
                let t = self.redirect_target();
                let len = t.buffer.len();
                let (start, end) = (start.min(len), end.min(len));
                if start < end {
                    t.overlays
                        .faces
                        .push((start..end, SharedString::from(face)));
                    cx.notify();
                }
            }
            OverlayHide { start, end } => {
                let t = self.redirect_target();
                let len = t.buffer.len();
                let (start, end) = (start.min(len), end.min(len));
                if start < end {
                    t.overlays.hidden.push(start..end);
                    cx.notify();
                }
            }
            FoldLines {
                start_line,
                end_line,
            } => self.overlay_fold_lines(start_line, end_line, cx),
            UnfoldAll => self.overlay_unfold_all(cx),
            SetFoldMarker { line, open } => self.overlay_set_fold_marker(line, open, cx),
            ClearFoldMarkers => self.overlay_clear_fold_markers(cx),
            ClearOverlays => {
                let redirected = self.redirect_index().is_some();
                self.redirect_target().overlays = Overlays::default();
                // Only the active buffer's folds/rows moved under the cursor.
                if !redirected {
                    self.ensure_cursor_visible();
                }
                cx.notify();
            }
            ClearOverlayFaces => {
                // Faces only — NOT hides. Several features (rainbow-delimiters,
                // LSP highlight) repaint their faces on every edit via this; if
                // it also cleared hides it would wipe another feature's
                // concealment (e.g. markdown/org heading markers). Clear hides
                // with `clear-overlay-hides`.
                self.redirect_target().overlays.faces.clear();
                cx.notify();
            }
            ClearOverlayFacesMatching(face) => {
                self.redirect_target()
                    .overlays
                    .faces
                    .retain(|(_, f)| f.as_ref() != face);
                cx.notify();
            }
            ClearOverlayHides => {
                self.redirect_target().overlays.hidden.clear();
                cx.notify();
            }
            ClearLineSuffixes => {
                self.redirect_target().overlays.suffixes.clear();
                cx.notify();
            }
            PutTextProperty {
                start,
                end,
                key,
                value,
            } => {
                self.put_text_property(start, end, key, value);
            }
            SetLineSuffix { line, text, face } => {
                self.redirect_target()
                    .overlays
                    .suffixes
                    .insert(line, (text.into(), face.into()));
                cx.notify();
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// Subprocesses, terminals, Anvil, and timers.
    fn cmd_proc(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            MakeProcess(args) => self.make_process(*args, cx),
            SendProcess { id, text } => self.send_to_process(id, &text, cx),
            KillProcess { id } => self.kill_process(id),
            SetProcessBuffer { id, name } => self.set_process_buffer(id, name),
            DapStart(args) => self.dap_start(*args, cx),
            DapConnect {
                id,
                host,
                port,
                on_message,
                on_exit,
            } => self.dap_connect(id, host, port, on_message, on_exit, cx),
            DapSend { id, message } => self.dap_send(id, &message),
            DapKill { id } => self.dap_kill(id),
            HttpStream(args) => self.http_stream(*args, cx),
            HttpStreamCancel { id } => self.http_stream_cancel(id),
            HttpRequest(args) => self.http_request(*args, cx),
            HttpServe(args) => self.http_serve(*args, cx),
            HttpServerStop { port } => self.http_server_stop(port),
            SocketConnectAsync {
                id,
                host,
                port,
                tls,
                on_data,
                on_close,
            } => self.socket_connect_async(id, host, port, tls, on_data, on_close, cx),
            SocketAsyncSend { id, hex } => self.socket_async_send(id, &hex),
            SocketAsyncClose { id } => self.socket_async_close(id),
            WsConnect {
                id,
                url,
                on_message,
                on_close,
            } => self.ws_connect(id, url, on_message, on_close, cx),
            WsSend { id, text } => self.ws_send(id, &text),
            WsClose { id } => self.ws_close(id),
            AnvilEval { source, on_result } => self.anvil.eval(&source, &on_result),
            AnvilInterrupt => self.anvil.interrupt(),
            AnvilReplOpen => self.anvil_repl_open(cx),
            AnvilSendInput => self.anvil_send_input(cx),
            AnvilHistory(older) => self.anvil_history(older, cx),
            AnvilReset => self.anvil.reset(),
            AnvilEvalDefun => {
                let text = self.active.buffer.full_text();
                match top_level_form_at(&text, self.point()) {
                    Some(r) => self.anvil.eval(text[r].trim(), "anvil-on-result-echo"),
                    None => self.echo("anvil: no top-level form at point", cx),
                }
            }
            AnvilTestFile => match self.active.path.clone() {
                Some(path) => {
                    // A clean slate, so the run can't pass on leftover state.
                    self.anvil.reset();
                    self.anvil_repl_open(cx);
                    self.anvil_repl_render(
                        &format!(";; --- {} in a fresh image ---\n", path.display()),
                        cx,
                    );
                    let form = format!(
                        "(load {})",
                        crate::script::steel_quote(&path.to_string_lossy())
                    );
                    self.anvil.eval(&form, anvil_repl::ANVIL_REPL_CALLBACK);
                }
                None => self.echo("anvil: buffer is not visiting a file", cx),
            },
            AnvilLoadFile => match &self.active.path {
                Some(path) => {
                    // `{:?}` renders a quoted, escaped Scheme string literal.
                    let form = format!(
                        "(load {})",
                        crate::script::steel_quote(&path.to_string_lossy())
                    );
                    self.anvil.eval(&form, "anvil-on-result-echo");
                }
                None => self.echo("anvil: buffer is not visiting a file", cx),
            },
            AnvilDoc => {
                let text = self.active.buffer.full_text();
                match symbol_at(&text, self.point()) {
                    Some(sym) => self
                        .anvil
                        .query(crate::anvil::AnvilQuery::Doc(sym), "anvil-on-describe"),
                    None => self.echo("anvil: no symbol at point", cx),
                }
            }
            AnvilMacroexpand => {
                let text = self.active.buffer.full_text();
                let form = match self.region() {
                    Some(r) => text.get(r).map(str::to_string),
                    None => top_level_form_at(&text, self.point()).map(|r| text[r].to_string()),
                };
                match form.filter(|f| !f.trim().is_empty()) {
                    Some(form) => self.anvil.query(
                        crate::anvil::AnvilQuery::Macroexpand(form),
                        "anvil-on-describe",
                    ),
                    None => self.echo("anvil: no form to expand", cx),
                }
            }
            AnvilApropos(pattern) => self.anvil.query(
                crate::anvil::AnvilQuery::Apropos(pattern),
                "anvil-on-describe",
            ),
            AnvilSignature => {
                let text = self.active.buffer.full_text();
                match symbol_at(&text, self.point()) {
                    Some(sym) => self
                        .anvil
                        .query(crate::anvil::AnvilQuery::Signature(sym), "anvil-on-eldoc"),
                    // No symbol under point: clear any stale eldoc. The auto
                    // refresh (post-command-hook) calls this on every cursor-line
                    // move, so silence — not "no symbol" — is the right response.
                    None => self.echo("", cx),
                }
            }
            MakeTerminal(args) => self.make_terminal(*args, cx),
            RunWithTimer { ms, callback } => self.run_with_timer(ms, callback, cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// Snippets, multiple cursors, avy, modeline, and macros.
    fn cmd_extras(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            SnippetExpand(template) => self.snippet_expand(template, cx),
            SnippetNextField => self.snippet_step_field(1, cx),
            SnippetPrevField => self.snippet_step_field(-1, cx),
            AvyShowHints(records) => self.avy_show_hints(&records, cx),
            AvyClearHints => self.avy_clear_hints(cx),
            McAddBelow => self.mc_add_line(1, cx),
            McAddAbove => self.mc_add_line(-1, cx),
            McMarkNext => self.mc_mark_next(cx),
            McClear => self.mc_clear(cx),
            SetSelections(sels) => self.set_selections(sels, cx),
            // Back-compat sugar over the generic slot mechanism.
            SetModelineProject(name) => {
                self.set_mode_line_slot("project".into(), name);
                cx.notify();
            }
            SetModelineGit(branch) => {
                self.set_mode_line_slot("git".into(), branch);
                cx.notify();
            }
            SetModeLineSlot(key, value) => {
                self.set_mode_line_slot(key, value);
                cx.notify();
            }
            SetModeLineFormat(tokens) => {
                self.mode_line_format = parse_mode_line_format(tokens);
                cx.notify();
            }
            SetTitlebarMenu(menu) => {
                let crate::script::TitlebarMenuArgs {
                    title,
                    items,
                    commands,
                    active,
                    section,
                    callback,
                } = *menu;
                self.titlebar_dropdown_publish(TitlebarDropdownState {
                    placeholder: format!("Search {}…", title.to_lowercase()).into(),
                    section: if section.is_empty() {
                        title.clone()
                    } else {
                        section
                    }
                    .into(),
                    trigger: title.clone().into(),
                    key: title,
                    items: items.into_iter().map(SharedString::from).collect(),
                    commands,
                    active: active.into(),
                    callback,
                    open: false,
                    filter: String::new(),
                    cursor: 0,
                    selected: None,
                });
                cx.notify();
            }
            StartKbdMacro => self.start_kbd_macro(cx),
            EndKbdMacro => self.end_kbd_macro(cx),
            CallLastKbdMacro => self.call_last_kbd_macro(cx),
            other => return Err(other),
        }
        Ok(())
    }

    /// Scripting, settings, hooks, and prompts.
    fn cmd_misc(
        &mut self,
        command: EditorCommand,
        cx: &mut Context<Self>,
    ) -> Result<(), EditorCommand> {
        use EditorCommand::*;
        match command {
            KeyboardQuit => self.keyboard_quit(cx),
            ExecuteExtendedCommand => {
                self.open_minibuffer("M-x ", MinibufferPurpose::ExecuteCommand, "", cx);
            }
            ListMessages => self.open_log_buffer("*Messages*", |e| &e.messages, cx),
            ListLspLog => self.open_log_buffer("*lsp-log*", |e| &e.lsp_log, cx),
            CheckForUpdates => self.check_for_updates(cx),
            InstallGrammar(args) => {
                // Empty strings are the Scheme wrapper's "use the default" sentinel.
                let non_empty = |s: String| (!s.is_empty()).then_some(s);
                self.install_grammar(
                    crate::grammar::InstallSpec {
                        id: args.id,
                        repo: args.repo,
                        rev: args.rev,
                        extensions: args
                            .extensions
                            .split_whitespace()
                            .map(str::to_string)
                            .collect(),
                        symbol: non_empty(args.symbol),
                        subdir: non_empty(args.subdir),
                        highlights_path: None,
                    },
                    cx,
                );
            }
            EnterMacrosSteel => {
                // Drop diagnostics the previous server (e.g. plain `scheme`, which
                // doesn't know the editor API) already published for this buffer, so
                // its "unknown function" warnings clear immediately; the now-active
                // `macros-scheme` server republishes via the mode hook below.
                if let Some(uri) = self.active_uri() {
                    self.diagnostics.remove(&uri);
                }
                self.active.modes = scheme_mode_stack(true);
                // `macros-scheme` has no grammar of its own; it shares Scheme's,
                // exactly as `make_document` wires it for auto-detected files.
                self.active.highlighter = Highlighter::scheme();
                self.parse_full();
                // Re-run mode hooks so the `fundamental` `enable-lsp` hook brings
                // up the `macros-scheme` server for the now-active language
                // (idempotent: `maybe_start_lsp` skips an already-running server).
                self.active.hooks_ran = false;
                self.run_mode_hooks_once(cx);
                self.echo("Entered macros-steel mode".to_string(), cx);
                cx.notify();
            }
            SetOption(key, value) => {
                // Toggling auto-revert changes which buffer files we watch.
                let touched_auto_revert = key == "auto-revert";
                // The mcp-server* options start/stop/restart the MCP server.
                let touched_mcp = key.starts_with("mcp-server");
                // `server` starts/stops the edit server (src/server.rs).
                let touched_server = key == "server";
                cx.update_global::<crate::settings::Settings, _>(|settings, _| {
                    settings.set(key, value);
                });
                self.script
                    .set_options(cx.global::<crate::settings::Settings>().snapshot());
                if touched_auto_revert {
                    self.reconcile_watches(cx);
                }
                if touched_mcp {
                    self.reconcile_mcp_server(cx);
                }
                self.sync_file_options(cx);
                if touched_server {
                    self.reconcile_server(true, cx);
                }
                cx.notify();
            }
            AddHook(mode, function) => {
                let list = self.hooks.entry(mode).or_default();
                if !list.contains(&function) {
                    list.push(function);
                }
            }
            RemoveHook(mode, function) => {
                if let Some(list) = self.hooks.get_mut(&mode) {
                    list.retain(|f| f != &function);
                }
            }
            RunHook(hook) => {
                self.run_hook(&hook);
            }
            EvalExpression => {
                self.open_minibuffer("Eval: ", MinibufferPurpose::EvalExpression, "", cx);
            }
            EvalBuffer => {
                let source = self.active.buffer.full_text();
                self.eval_interactive(&source, cx);
            }
            EvalLastSexp => {
                // With an active selection, evaluate the whole region (all forms,
                // top-to-bottom) like Emacs `eval-region`; otherwise fall back to
                // the single sexp before point (`eval-last-sexp`).
                match self.region_or_last_sexp() {
                    Some(source) => self.eval_interactive(&source, cx),
                    None => self.echo("Nothing to evaluate", cx),
                }
            }
            SetLispIndent { name, spec } => match LispIndent::parse(&spec) {
                Some(rule) => {
                    self.lisp_indent_rules.insert(name, rule);
                }
                None => self.echo(
                    format!(
                        "lisp-indent-rule: bad spec {spec:?} (body, function, clauses, or a number)"
                    ),
                    cx,
                ),
            },
            Structural(op) => self.structural(op, cx),
            EvalDefun => {
                let text = self.active.buffer.full_text();
                match top_level_form_at(&text, self.point()) {
                    Some(r) => self.eval_interactive(&text[r], cx),
                    None => self.echo("No top-level form at point", cx),
                }
            }
            AnvilEvalLastSexp => match self.region_or_last_sexp() {
                Some(source) => self.anvil.eval(&source, "anvil-on-result-echo"),
                None => self.echo("anvil: nothing to evaluate", cx),
            },
            Message(text) => self.echo(text, cx),
            DefineKey(mode, sequence, command) => {
                self.keymap.define(mode, sequence, format!("({command})"));
            }
            SetFace(face, color) => match crate::theme::parse_color(&color) {
                Some(c) => {
                    cx.update_global::<Theme, ()>(|theme, _| theme.set(face, c));
                    cx.notify();
                }
                None => self.echo(format!("Invalid color: {color}"), cx),
            },
            SetFaceBackground(face, color) => match crate::theme::parse_color(&color) {
                Some(c) => {
                    cx.update_global::<Theme, ()>(|theme, _| theme.set_background(face, c));
                    cx.notify();
                }
                None => self.echo(format!("Invalid color: {color}"), cx),
            },
            SetFaceFont(face, family) => {
                cx.update_global::<Theme, ()>(|theme, _| theme.set_font(face, family));
                cx.notify();
            }
            SetFaceUnderline(face, on) => {
                cx.update_global::<Theme, ()>(|theme, _| theme.set_underline(face, on));
                cx.notify();
            }
            SetFaceBold(face, on) => {
                cx.update_global::<Theme, ()>(|theme, _| theme.set_bold(face, on));
                cx.notify();
            }
            SetFaceWavy(face, on) => {
                cx.update_global::<Theme, ()>(|theme, _| theme.set_wavy(face, on));
                cx.notify();
            }
            // Checking and suggestions use NSSpellChecker's async request
            // APIs (the sync ones wedge inside a running NSApplication — see
            // src/spell.rs); learn/ignore are reply-less sends, run on worker
            // threads so a misbehaving XPC can never stall the UI.
            SpellCheck { text, callback } => {
                let handle = self.script.action_handle();
                crate::spell::request_check(text, move |errors| {
                    let rows: Vec<String> = errors
                        .into_iter()
                        .map(|(s, e, w)| format!("{s}\t{e}\t{w}"))
                        .collect();
                    handle.dispatch_action(&callback, Some(&rows.join("\n")));
                });
            }
            SpellSuggest { word, callback } => {
                let handle = self.script.action_handle();
                crate::spell::request_suggestions(word, move |suggestions| {
                    handle.dispatch_action(&callback, Some(&suggestions.join("\n")));
                });
            }
            SpellLearnWord(word) => crate::spell::learn_async(word),
            LoadTheme(name) => match crate::runtime::theme_script(&name) {
                Some(source) => {
                    // Clear the theme-owned faces, then apply the theme's set-face
                    // calls, so switching themes never leaves stale faces —
                    // while faces only a module or the config sets are kept.
                    let owned = crate::runtime::theme_face_names();
                    cx.update_global::<Theme, ()>(|theme, _| {
                        theme.clear_faces(|name| owned.contains(name))
                    });
                    self.run_script(source, cx);
                }
                None => self.echo(format!("No theme named {name}"), cx),
            },
            ReadString {
                prompt,
                initial,
                callback,
            } => {
                self.open_minibuffer(
                    &prompt,
                    MinibufferPurpose::SchemeCallback(callback),
                    &initial,
                    cx,
                );
            }
            ReadPasswd {
                prompt,
                callback,
                on_cancel,
            } => {
                self.open_minibuffer(&prompt, MinibufferPurpose::SchemeCallback(callback), "", cx);
                if let Some(mb) = self.minibuffer.as_mut() {
                    mb.secret = Some(crate::secure_input::SecureInputGuard::new());
                    mb.on_cancel = on_cancel;
                }
            }
            CompletingRead {
                prompt,
                candidates,
                dynamic,
                initial,
                callback,
            } => {
                self.completing_candidates = candidates;
                match dynamic {
                    Some(function) => {
                        self.open_minibuffer(
                            &prompt,
                            MinibufferPurpose::SchemeCompletingDynamic(callback),
                            &initial,
                            cx,
                        );
                        // Seed the candidate list for the initial input.
                        self.helm_query_counter += 1;
                        let id = self.helm_query_counter;
                        self.mb_dynamic = Some((function.clone(), id));
                        self.script
                            .dispatch_candidates(usize::MAX, id, &function, &initial);
                    }
                    None => self.open_minibuffer(
                        &prompt,
                        MinibufferPurpose::SchemeCompleting(callback),
                        &initial,
                        cx,
                    ),
                }
            }
            YesOrNo {
                prompt,
                callback,
                sticky,
            } => {
                let prompt = format!("{prompt} (yes or no) ");
                self.open_minibuffer(&prompt, MinibufferPurpose::SchemeYesNo(callback), "", cx);
                if sticky && let Some(mb) = self.minibuffer.as_mut() {
                    mb.sticky = true;
                }
            }
            other => return Err(other),
        }
        Ok(())
    }
}

impl EditorCommand {
    /// Whether this command repeats under a universal-argument (Emacs `C-u N`).
    /// Limited to motions and simple edits, where "do it N times" is the meaning;
    /// modal/meta commands (`M-x`, eval, quit, toggles, …) run once regardless.
    pub(super) fn is_repeatable(&self) -> bool {
        use EditorCommand::*;
        matches!(
            self,
            ForwardChar
                | BackwardChar
                | NextLine
                | PreviousLine
                | ForwardWord
                | BackwardWord
                | ScrollUp
                | ScrollDown
                | EvilScrollUp
                | EvilScrollDown
                | DeleteChar
                | DeleteBackwardChar
                | KillWord
                | BackwardKillWord
                | KillLine
                | Newline
                | OpenLine
                | TransposeChars
                | Yank
                | Undo
                | Redo
        )
    }

    /// Whether this is an *interactive* text mutation — the kind a keybinding
    /// runs on "the text at point". These are refused in read-only special buffers
    /// (see `Editor::cmd_edit`). The explicitly-addressed primitives
    /// (`ReplaceRegion`, `DeleteRegion`, `InsertAt`, `KillRegionBetween`, `Insert`)
    /// are NOT listed: Scheme uses them to populate and rewrite these buffers, and
    /// no default keybinding reaches them.
    pub(super) fn is_interactive_edit(&self) -> bool {
        use EditorCommand::*;
        matches!(
            self,
            Newline
                | OpenLine
                | IndentForTab
                | ReindentRegion
                | JsonPrettyPrintBuffer
                | JsonPrettyPrintRegion
                | DeleteChar
                | DeleteBackwardChar
                | TransposeChars
                | KillLine
                | KillWord
                | BackwardKillWord
                | KillRegion
                | ClipboardCut
                | ClipboardPaste
                | Yank
                | YankPop
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::script::EditorCommand::*;

    #[test]
    fn read_only_guard_covers_keybound_edits_only() {
        // Every editing chord `fundamental` binds is refused in a read-only
        // buffer: C-d, C-k, M-d, M-backspace, C-w, C-y, M-y, C-t, C-o, cmd-x,
        // cmd-v, plus RET/TAB.
        for c in [
            DeleteChar,
            KillLine,
            KillWord,
            BackwardKillWord,
            KillRegion,
            Yank,
            YankPop,
            TransposeChars,
            OpenLine,
            ClipboardCut,
            ClipboardPaste,
            Newline,
            IndentForTab,
            DeleteBackwardChar,
        ] {
            assert!(c.is_interactive_edit(), "{c:?} must be refused");
        }
        // Programmatic primitives still run — that is how Scheme builds and
        // rewrites these buffers.
        for c in [
            Insert("x".into()),
            InsertAt {
                pos: 0,
                text: "x".into(),
            },
            DeleteRegion { start: 0, end: 1 },
            ReplaceRegion {
                start: 0,
                end: 1,
                text: "x".into(),
            },
            KillRegionBetween { start: 0, end: 1 },
            KillRingSave,
            ClipboardCopy,
            Undo,
            Redo,
        ] {
            assert!(!c.is_interactive_edit(), "{c:?} must stay allowed");
        }
    }
}
