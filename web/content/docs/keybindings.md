---
title: Keybindings
description: The default keymap, the chord notation, and how to rebind any key.
---

# Keybindings

Every key in Macros is bound in Steel and every binding is overridable. Keymaps are named (`"fundamental"` is the global map; major modes add their own), and you set one with `(bind-key "<map>" "<chord>" <command>)`.

## Notation

- <kbd>C-x</kbd> — hold **Control**, press **x**.
- <kbd>M-x</kbd> — hold **Meta** (Alt/Option), press **x**; or press <kbd>Esc</kbd> then **x**.
- <kbd>C-x C-f</kbd> — a chord: <kbd>C-x</kbd> followed by <kbd>C-f</kbd>.
- <kbd>cmd-c</kbd> — the macOS Command key (system clipboard).

In a binding these are spelled `ctrl-`, `alt-`, `cmd-`, `shift-`, e.g. `"ctrl-x ctrl-f"`.

## Motion (fundamental map)

| Key | Command |
| --- | --- |
| <kbd>C-f</kbd> / <kbd>C-b</kbd> | `forward-char` / `backward-char` |
| <kbd>C-n</kbd> / <kbd>C-p</kbd> | `next-line` / `previous-line` |
| <kbd>C-a</kbd> / <kbd>C-e</kbd> | `move-beginning-of-line` / `move-end-of-line` |
| <kbd>M-f</kbd> / <kbd>M-b</kbd> | `forward-word` / `backward-word` |
| <kbd>M-&lt;</kbd> / <kbd>M-&gt;</kbd> | `beginning-of-buffer` / `end-of-buffer` |
| <kbd>C-v</kbd> / <kbd>M-v</kbd> | `scroll-up` / `scroll-down` |
| <kbd>C-l</kbd> | `recenter` |
| <kbd>M-g g</kbd> | `goto-line-cmd` |

## Editing, kill & yank

| Key | Command |
| --- | --- |
| <kbd>C-d</kbd> | `delete-char` |
| <kbd>C-k</kbd> | `kill-line` |
| <kbd>M-d</kbd> / <kbd>M-Backspace</kbd> | `kill-word` / `backward-kill-word` |
| <kbd>C-Space</kbd> | `set-mark` |
| <kbd>C-w</kbd> / <kbd>M-w</kbd> | `kill-region` / `kill-ring-save` |
| <kbd>C-y</kbd> / <kbd>M-y</kbd> | `yank` / `yank-pop` |
| <kbd>C-/</kbd> | `undo` |
| <kbd>C-?</kbd> | `redo` |
| <kbd>C-t</kbd> | `transpose-chars` |
| <kbd>C-o</kbd> | `open-line` |
| <kbd>M-;</kbd> | `comment-dwim` |
| <kbd>C-x n n</kbd> / <kbd>C-x n d</kbd> | `narrow-to-region` / `narrow-to-defun` |
| <kbd>C-x n s</kbd> | `org-narrow-to-subtree` (Org) |
| <kbd>C-x n w</kbd> | `widen` |

The system clipboard always works, including inside modal states: <kbd>cmd-c</kbd> / <kbd>cmd-x</kbd> / <kbd>cmd-v</kbd> (`clipboard-copy` / `-cut` / `-paste`).

## Files, buffers & windows

| Key | Command |
| --- | --- |
| <kbd>C-x C-f</kbd> | `find-file` |
| <kbd>C-x C-s</kbd> | `save-buffer` |
| <kbd>C-x C-w</kbd> | `write-file` (save as a new file and visit it) |
| <kbd>C-x i</kbd> | `insert-file` |
| <kbd>C-x C-q</kbd> | `read-only-mode` (toggle) |
| <kbd>C-x b</kbd> | `switch-buffer` |
| <kbd>C-x C-b</kbd> | `ibuffer` (the buffer manager) |
| <kbd>C-x k</kbd> | `kill-buffer` |
| <kbd>C-x 2</kbd> / <kbd>C-x 3</kbd> | `split-window-below` / `split-window-right` |
| <kbd>C-x o</kbd> | `other-window` |
| <kbd>C-x 0</kbd> / <kbd>C-x 1</kbd> | `delete-window` / `delete-other-windows` |
| <kbd>C-x 4 f</kbd> | `find-file-other-window` |
| <kbd>C-x 4 b</kbd> | `switch-to-buffer-other-window` |
| <kbd>C-x 4 0</kbd> | `kill-buffer-and-window` |
| <kbd>C-x 4 C-o</kbd> | `display-buffer` (show a buffer in another window, stay here) |
| <kbd>C-x ^</kbd> / <kbd>C-x }</kbd> / <kbd>C-x {</kbd> | resize windows |
| <kbd>C-x C-c</kbd> | `quit` |

`M-x rename-buffer` renames the current buffer, and `M-x find-file-at-point` (alias `ffap`) opens the file or URL at point — see [Files and buffers](/docs/editing-commands#files-and-buffers).

## Search, command, eval

| Key | Command |
| --- | --- |
| <kbd>C-s</kbd> / <kbd>C-r</kbd> | `isearch-forward` / `isearch-backward` |
| <kbd>M-%</kbd> / <kbd>C-M-%</kbd> | `query-replace` / `query-replace-regexp` |
| <kbd>C-c o</kbd> | `occur` |
| <kbd>M-x</kbd> | `execute-extended-command` (command palette) |
| <kbd>M-:</kbd> | `eval-expression` |
| <kbd>M-i</kbd> | `completion-at-point` |
| <kbd>C-g</kbd> | `keyboard-quit` |
| <kbd>M-!</kbd> | `shell-command` (<kbd>C-u</kbd>: insert the output at point) |
| <kbd>M-\|</kbd> | `shell-command-on-region` (<kbd>C-u</kbd>: replace the region with the output) |
| <kbd>M-&</kbd> | `async-shell-command` |
| <kbd>M-x</kbd> `grep` / `rgrep` / `lgrep` | search a directory, then walk the matches with <kbd>M-g n</kbd> / <kbd>M-g p</kbd> |

See [Shell & compile](/docs/shell-and-compile) for the details.

## Keyboard macros

| Key | Command |
| --- | --- |
| <kbd>C-x (</kbd> / <kbd>C-x )</kbd> | `start-kbd-macro` / `end-kbd-macro` |
| <kbd>C-x e</kbd> | `call-last-kbd-macro` |

## Bookmarks

Named positions in files that survive a restart (Emacs `bookmark.el`):

| Key | Command |
| --- | --- |
| <kbd>C-x r m</kbd> | `bookmark-set` — bookmark point here, under a name |
| <kbd>C-x r b</kbd> | `bookmark-jump` — pick a bookmark in Helm and jump to it |
| <kbd>C-x r l</kbd> | `helm-bookmarks` — browse all bookmarks |
| <kbd>M-x</kbd> `bookmark-delete` / `bookmark-rename` | upkeep |
| <kbd>M-x</kbd> `bookmark-edit-annotation` | annotate a bookmark (shown in the Helm picker) |

Bookmarks are stored as an s-expression file at `~/.config/macros/bookmarks` (see [configuration](/docs/configuration#what-persists)), rewritten on every change. `org-capture` maintains an `org-capture-last-stored` bookmark on the headline it just inserted.

Like `bookmark.el`, a bookmark can carry a free-text annotation — `bookmark-edit-annotation` prompts for it (the current text is prefilled; empty input clears it), and annotated bookmarks show it dimmed next to their line in <kbd>C-x r b</kbd> / <kbd>C-x r l</kbd>. Programs can use `bookmark-annotation` / `bookmark-set-annotation!` directly. Hooks (`add-hook`): `bookmark-save-hook` fires after every rewrite of the file, `bookmark-load-hook` after the file loads at startup, and `bookmark-after-jump-hook` once a jump's point has landed.

Feature-specific prefixes live in their own pages: <kbd>C-c C-l …</kbd> for [LSP](/docs/lsp), <kbd>C-c p …</kbd> for [projects](/docs/projects), and <kbd>C-h …</kbd> for [help](/docs/getting-help). Modal editing is available too, but off by default — in either a Vim-style ([evil](/docs/modal-editing)) or selection-first ([helix](/docs/helix)) flavor.

## Prefix arguments

A numeric prefix repeats the next command, Emacs-style:

| Key | Effect |
| --- | --- |
| <kbd>C-u</kbd> *N* <kbd>cmd</kbd> | run `cmd` *N* times — e.g. <kbd>C-u 10 C-n</kbd> moves down 10 lines |
| <kbd>M-*N*</kbd> <kbd>cmd</kbd> | shorthand for the same — <kbd>M-10 C-n</kbd> |
| <kbd>C-u</kbd> <kbd>cmd</kbd> | <kbd>C-u</kbd> alone means 4; <kbd>C-u C-u</kbd> means 16, and so on |
| <kbd>C-u 5 a</kbd> | self-insert works too — inserts `aaaaa` |

The argument applies to motions and simple edits (move, kill, yank, delete, newline, undo/redo). Modal or one-shot commands (<kbd>M-x</kbd>, save, quit, toggles) ignore it and run once. A Scheme command reads the argument with `(prefix-count)` (Emacs's `current-prefix-arg`; 0 when none was given) and decides what it means. For example, <kbd>C-u M-!</kbd> inserts the command's output. Negative arguments aren't supported yet.

## Rebinding

Add `bind-key` calls to your `init.scm` — it's the recommended way to bind a key. The map name for the global map is `"fundamental"`; mode maps are named after the mode (`"org-mode"`, `"magit-status"`, `"dired-mode"`, …).

```scheme
;; Rebind in the global map — pass the command name as a 'name symbol
(bind-key "fundamental" "ctrl-x n" 'next-buffer)

;; Rebind only inside a mode
(bind-key "org-mode" "ctrl-c x" 'org-todo)

;; Bind a command of your own, or an inline lambda
(define (hello) (message "hi"))
(bind-key "fundamental" "ctrl-c h" 'hello)
(bind-key "fundamental" "ctrl-c !" (lambda () (insert-string "!")))
```

Prefer the **`'name` symbol** form. It names the command without evaluating it, so the binding works no matter what order your config files load in. Both your own `define`d commands and the built-in commands (`next-buffer`, `find-file`, `magit-status`, …) bind this way.

`bind-key` also accepts the command **directly** as a bare identifier (`next-buffer`, no quote). That form evaluates the function at bind time — a typo becomes an error when your config loads, and editor tooling like go-to-definition follows the binding — but the command must already be defined at that point. A plain **string** name works too, handy when pasting straight from a command list:

```scheme
(bind-key "fundamental" "ctrl-x ctrl-d" next-buffer)    ; bare identity (must be defined)
(bind-key "fundamental" "ctrl-x ctrl-f" "find-file")    ; string name
```

### define-key (the primitive)

`bind-key` is a thin wrapper over `define-key`, the lower-level primitive the keymap is built on. `define-key` takes a command **name** — a `'symbol` (preferred) or a string. It resolves the name at keypress time, so it can't take a function or a `lambda` (use `bind-key` for those):

```scheme
(define-key "fundamental" "ctrl-c d" 'lsp-find-definition)  ; 'symbol (preferred)
(define-key "fundamental" "ctrl-c d" "lsp-find-definition")  ; string still works
```

You'll see `define-key` throughout the bundled config and it's fully supported — reach for it when you specifically want the primitive or the exact Emacs spelling. For everyday rebinding, `bind-key` does everything it does and takes functions too.

To see what a key currently does, press <kbd>C-h k</kbd> (`describe-key`) and then the key. To list everything active, <kbd>C-h b</kbd> (`describe-bindings`). See [Getting help](/docs/getting-help).
