---
title: Getting started
description: Install Macros, launch it for the first time, and learn the handful of keys you need to get moving.
---

# Getting started

Macros is an Emacs-style, GPU-rendered code editor that you script in **Steel** — a fast, embedded Scheme. The entire editing experience — commands, keymaps, modes, even the git interface — is written in Steel source that ships with the binary, and every bit of it is yours to read, rebind, and rewrite.

This page gets you installed and oriented. When you're ready to make it yours, head to [Configuration](/docs/configuration) and [Scripting with Steel](/docs/scripting).

## Install

Download a build for your platform from the [downloads page](/download), or build from source:

```sh
git clone https://github.com/macros-app/macros
cd macros
cargo run --release
```

Macros runs on **macOS**, **Windows**, and **Linux**. It's native — built on Zed's GPUI framework — so there's no Electron and no web runtime.

## First launch

On first launch you get a `*scratch*` buffer and the built-in default configuration. **Macros uses standard Emacs keybindings by default.** If you prefer modal editing, add `(evil-mode)` (Vim-style) or `(helix-mode)` (Helix-style, selection-first) to your config (see [Configuration](/docs/configuration)).

The fastest way to learn the editor is from inside it:

- Press <kbd>C-h t</kbd> to open the **interactive tutorial**. It's a normal editable buffer — you read and practice the commands right there.
- Press <kbd>M-x</kbd> (that's <kbd>Alt-x</kbd>) to open the **command palette** and fuzzy-search every command.
- Press <kbd>C-h b</kbd> to list **all active keybindings**.

> Notation: <kbd>C-x</kbd> means hold **Control** and press **x**. <kbd>M-x</kbd> means hold **Meta** (Alt/Option) and press **x**, or press <kbd>Esc</kbd> then **x**. <kbd>C-x C-f</kbd> is a two-key chord: <kbd>C-x</kbd> followed by <kbd>C-f</kbd>.

## Opening files

Inside the editor, <kbd>C-x C-f</kbd> opens a file. From outside it:

- **From a terminal:** `macros notes.md data.csv` opens each file, the last one in front. `macros --client notes.md` opens it in the Macros you already have running instead (see [Edit server](/docs/server)).
- **From your file manager:** Macros is listed under **Open With** for every language it highlights, plus CSV and TSV files, Excel workbooks (`.xlsx`), plain text, Org, and Markdown. That's **Open With** in Finder on macOS, **Open with** in Explorer on Windows, and **Open With Other Application** (or similar) in your Linux file manager. This needs an installed build; a build run with `cargo run` isn't registered with the OS.

Launching on files gives you just those files: your saved buffers and windows aren't restored, and stay saved for next time (see [Session state](/docs/configuration#session-state)). On macOS, a file opened while Macros is running joins the frontmost window. On Windows and Linux it opens in a new Macros window.

Macros never makes itself the default app for a file type. To make it the default:

- **macOS:** select a file in Finder, choose **File › Get Info**, pick Macros under **Open with**, and click **Change All…**
- **Windows:** right-click a file, choose **Open with › Choose another app**, select Macros, and choose to always use it.
- **Linux:** run `xdg-mime default macros.desktop text/csv`, once per MIME type.

## The essential keys

Whether or not you use modal editing, these editor-level commands always work:

| Key | Command | Does |
| --- | --- | --- |
| <kbd>C-x C-f</kbd> | `find-file` | Open a file |
| <kbd>C-x C-s</kbd> | `save-buffer` | Save |
| <kbd>C-x b</kbd> | `switch-buffer` | Switch buffers (Helm) |
| <kbd>C-x k</kbd> | `kill-buffer` | Close the current buffer |
| <kbd>C-x o</kbd> | `other-window` | Move focus to the next pane |
| <kbd>C-x 2</kbd> / <kbd>C-x 3</kbd> | split | Split below / to the right |
| <kbd>C-x 1</kbd> | `delete-other-windows` | Close other panes |
| <kbd>M-x</kbd> | `execute-extended-command` | Run any command by name |
| <kbd>C-g</kbd> | `keyboard-quit` | Cancel / escape the current action |
| <kbd>C-x C-c</kbd> | `quit` | Quit Macros |

If you enable modal editing, you move around in Normal state with <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> and enter Insert state with <kbd>i</kbd>. See [Modal editing (evil)](/docs/modal-editing) or [Modal editing (helix)](/docs/helix).

## Where to go next

- **[Configuration](/docs/configuration)** — your `init.scm`, editor options, and config file locations.
- **[Example config](/docs/example-config)** — a real, annotated `init.scm` (evil + `SPC` leader) to read and crib from.
- **[Keybindings](/docs/keybindings)** — the full default keymap and how to rebind any key.
- **[Getting help](/docs/getting-help)** — discover commands and bindings without leaving the editor.
- **[Scripting with Steel](/docs/scripting)** — define your own commands, keymaps, and modes.
