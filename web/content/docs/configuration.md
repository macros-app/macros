---
title: Configuration
description: Where your config lives, how it loads, and the editor options you can set.
---

# Configuration

Macros has no settings panel. Your configuration is a **Steel program** — `init.scm` — evaluated at startup. It can set options, rebind keys, define commands, load themes, and call any of the editor's primitives.

## Config file location

The built-in defaults load first (they ship inside the binary). Then your personal `init.scm` loads and can override anything. Macros looks for it in the first of:

| Platform | Path |
| --- | --- |
| macOS / Linux | `~/.config/macros/init.scm` |
| with `$XDG_CONFIG_HOME` set | `$XDG_CONFIG_HOME/macros/init.scm` |
| Windows | `%APPDATA%\macros\init.scm` |

If none exists, the editor just uses its defaults. Create the file and start adding to it.

### Splitting your config across files

```scheme
;; Resolved relative to the config directory:
(include "keys.scm")
(include "lsp.scm")

;; Absolute paths work too:
(load "/Users/me/dotfiles/macros/extra.scm")
```

## Editor options

Set options with `(set-option "name" value)`. Values are **typed** — pass a number, a boolean (`#t` / `#f`), or a string, and the value is coerced to the option's type when you set it. (A quoted `"string"` works for any option too, so older configs keep running.) These are the defaults, set in the built-in config:

```scheme
;; Theme — see Themes & faces
(load-theme "macros-dark")

;; Typography
(set-option "font-size"   13)
(set-option "line-height" 20)
(set-option "tab-width"   4)

;; Completion popup (corfu-style) — see Completion
(set-option "completion-style"       "popup")  ; "popup" or "minibuffer"
(set-option "completion-auto"        #t)       ; pop up while typing
(set-option "completion-auto-prefix" 1)        ; min chars before auto-popup
(set-option "completion-max-rows"    10)
(set-option "completion-show-detail" #t)       ; signature/type beside name
(set-option "completion-show-kind"   #t)       ; kind tag (fn, var, kw, …)

;; Cursor
(set-option "cursor-style" "auto")  ; "auto" | "bar" | "block"
(set-option "cursor-width" 2)       ; bar width in px

;; Editing
(set-option "electric-indent" #t)   ; RET keeps/structures indent, TAB reindents
;; show-paren: on by default for code, off for markdown / toml / plain text.
;; Per-major-mode, so re-enable (or disable) it per language:
(set-option "show-paren:markdown" #t)  ; e.g. turn it back on in Markdown

;; Gutter & chrome
(set-option "line-numbers"  #t)
(set-option "scroll-bar"    #t)
(set-option "divider-width" 1)      ; width of the split-window divider

;; Files on disk
(set-option "auto-revert"   #f)     ; re-read a buffer when its file changes on disk

;; Helm selection UI — see Helm
(set-option "helm-fuzzy"  #t)       ; fuzzy (nucleo) vs substring matching
(set-option "helm-height" 0.5)      ; fraction of the window height

;; MCP server for AI-assisted debugging — off by default; see MCP server
(set-option "mcp-server"      #f)
(set-option "mcp-server-port" 7811)

;; Edit server for `macros --client` / $EDITOR — on by default; see Edit server
(set-option "server" #t)
```

### Reading an option

Read an option back from Scheme with `(get-option "name")`. It returns the current value as a native boolean / number / string, or `#f` if the option isn't set:

```scheme
(when (get-option "completion-auto")
  (message "auto-completion is on"))
```

### Secrets as option values

Don't write passwords or tokens into `init.scm`. Set the option to a **reference** into the encrypted [secrets store](/docs/secrets) instead:

```scheme
(set-option "psql-url" (secret-get "psql-url-1" "alice"))
```

Only the reference is stored. `get-option` returns the real value once the store has it (`#f` until then), and the first command that needs it asks for it, masked, and offers to save it. Nothing prompts at startup. See [Secrets in your config](/docs/secrets#secrets-in-your-config).

### Per-mode options

Append `:mode-name` to scope an option to one major mode. For example, the dired sidebar turns its line-number gutter off:

```scheme
(set-option "line-numbers:dired-sidebar-mode" #f)
```

For code buffers the mode name is the **language id** (`ocaml`, `rust`, `tsx`, …, the same id used by `lsp-language` and syntax highlighting), so `:mode` doubles as a per-language override. Several editor defaults use this:

```scheme
;; Indent step per language — one structural level is this many columns.
;; The bundled config ships each language's convention (OCaml/Ruby → 2,
;; Rust/Python → 4, …) so live editing matches its formatter; override freely:
(set-option "tab-width:ocaml" 2)   ; vs the global "tab-width" (default 4)

;; Extra completion trigger characters, added to whatever the language server
;; advertises — "." is the built-in default for every language, so member
;; access auto-opens completion even when the server lists none (ocamllsp
;; doesn't advertise "."). Set "" to rely on server-advertised triggers only.
(set-option "completion-trigger-chars:rust" ".:")
```

The bundled defaults live in `scheme/indent.scm` and `scheme/completion.scm`; because your `init.scm` loads last, any `(set-option …)` there wins. The full per-language indent set — including the trailing-token rules (`indent-after` / `align-after`) — is covered under [Auto-indentation](/docs/languages#auto-indentation-indent-query).

## Watching the filesystem

Macros watches the filesystem so the editor stays in sync with changes made outside it (a `git checkout`, a code formatter, a build step, another editor).

**Auto-revert** is opt-in (Emacs `global-auto-revert-mode`). Turn it on with:

```scheme
(set-option "auto-revert" #t)
```

When a file you're visiting changes on disk and the buffer has **no unsaved edits**, Macros re-reads it automatically, keeping point where it was. A buffer with unsaved changes is **never** clobbered — instead you get a one-line warning, and you can reconcile with `M-x revert-buffer` (`:e!`) when ready. Macros' own saves don't trigger a spurious revert (the on-disk text already matches the buffer).

**Live sidebar/dired refresh** is always on — no option needed. While the file tree or a dired buffer is open, creating, renaming, or deleting files in the directories it shows refreshes the view automatically. The refresh never steals focus: if the sidebar is in another pane while you edit, it updates in place and your cursor stays put.

## Session state

Some state persists across restarts automatically, no configuration required:

- **Cursor position** — when you reopen a file, point jumps back to where you left it (Emacs `save-place-mode`).
- **Recent files** — the recent-files ring survives a restart (Emacs `recentf`).
- **Histories** — minibuffer, search, and <kbd>M-x</kbd> command history, and the kill ring (Emacs `savehist-mode`).
- **Open buffers and windows** — the files you had open, where point was in each, the window splits, and the current perspective (Emacs `desktop-save-mode`). When you launch Macros on specific files (from a terminal, or with **Open With** from your file manager), you get just those files: the saved buffers and windows are neither restored nor overwritten, so they're still there next time you launch without files.
- **Projects and perspectives** — recent projects for the welcome screen, and the [perspective](/docs/projects#names-and-persistence) list.
- **Breakpoints** — debugger breakpoints and their conditions.

| File in `~/.config/macros/` | Holds | Written |
|---|---|---|
| `saveplace`, `recentf` | cursor positions, recent files | when you save a file |
| `bookmarks` | named positions (Emacs `bookmark.el`) | when a bookmark is set or removed |
| `savehist`, `desktop` | histories and kill ring, buffers and windows | on quit, and every 5 minutes |
| `projects`, `perspectives` | recent projects, perspectives | when a project is opened or changed |
| `dap-breakpoints` | breakpoints | when breakpoints change |

These always live in `~/.config/macros`, even when `$XDG_CONFIG_HOME` points your `init.scm` somewhere else. From Scheme, `(persistence-directory)` returns the path. To opt out of the histories or the buffer and window restore, add any of these to `init.scm`:

```scheme
(savehist-mode #f)              ; don't save or restore histories
(savehist-save-kill-ring #f)    ; keep histories, but never write the kill ring to disk
(savehist-set-max 500)          ; entries kept per history (default 300)
(desktop-save-mode #f)          ; don't save or restore buffers and windows
```

## Turning on modal editing

Macros uses standard Emacs keybindings by default, and offers modal editing in two flavors. For Vim-style modal editing, enable `evil-mode`:

```scheme
(evil-mode)
;; ...and (evil-disable) turns it back off.
```

This is vanilla `evil-mode` — plain Vim keys, no leader. See [Modal editing (evil)](/docs/modal-editing) for the full picture.

Prefer Helix's selection-first model instead? Enable `helix-mode`:

```scheme
(helix-mode)
;; ...and (helix-disable) turns it back off.
```

Motions select, then verbs act on the selection, with first-class multiple selections. See [Modal editing (helix)](/docs/helix). Turn on one or the other, not both.

## A starter init.scm

```scheme
;; ~/.config/macros/init.scm
(load-theme "macros-dark")
(set-option "font-size" 14)

;; A personal binding: jump to definition.
(bind-key "fundamental" "ctrl-c d" 'lsp-find-definition)

;; A command of your own, callable with M-x insert-date.
(define (insert-date)
  (insert-string (call-process "date" '("+%Y-%m-%d"))))
```

Reload by restarting, or evaluate expressions live with <kbd>M-:</kbd> (`eval-expression`). For the full API, see [Scripting with Steel](/docs/scripting).

For a complete, lived-in config — `evil-mode`, a Spacemacs-style `SPC` leader, custom commands, and per-mode LSP/Org bindings, annotated piece by piece — see [Example config](/docs/example-config).
