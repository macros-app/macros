---
title: Derived modes
description: Write Emacs-style derived major modes in Steel — one form defines the mode command, its keymap, and its registry entry; keymaps, hooks and highlighting inherit down the chain.
---

# Derived modes

`define-derived-mode` (in the bundled `derived-mode` module) defines a major mode the Emacs way: derive from a parent, add your own bindings and setup. The parent edge lives in the editor's mode registry, so inheritance is real — keymaps, hooks and highlighting all flow down the chain.

```scheme
(define-derived-mode mylang-mode fundamental
  "My language mode."
  :keymap (("ctrl-c m c" mylang-compile)
           ("g g" mylang-goto-top))
  :language mylang
  (message "mylang-mode enabled"))

(define-derived-mode mylang-test-mode mylang-mode
  "Test-file variant of mylang."
  (message "mylang-test: test helpers on"))
```

Then `M-x mylang-mode` switches the current buffer.

## Naming: one convention

The mode's **internal** name is the bare name — `mylang` — and the command is `mylang-mode`, the same convention as the bundled `rust-mode` → `"rust"`. The macro accepts either spelling for CHILD and PARENT (`mylang` and `mylang-mode` both name `mylang`); a trailing `-mode` is stripped. The bare name is what grammar ids, abbrev tables and `"evil-normal:mylang"` keymaps key on.

Deviation from Emacs: Emacs's `major-mode` would hold `mylang-mode`. Macros' `(major-mode)` holds the bare `mylang`, matching every bundled mode.

## What you get

| Piece | Behavior |
|---|---|
| `mylang-mode` command | M-x registered. Refuses to define a mode whose command already exists (e.g. `rust-mode`) instead of silently replacing it; re-defining your own mode (a config reload) is fine. |
| `mylang-mode-map` | An introspection alist of the mode's `:keymap` bindings. |
| `mylang-mode-hook` | Fires when the editor applies the mode switch — hooks keyed by `"mylang"`, `"mylang-hook"` AND `"mylang-mode-hook"` all run. Add hooks with `(add-hook "mylang-mode-hook" 'my-setup)` — a `'symbol` (or a string), NOT `#'`: Steel's `#'` is `syntax`, and the hook fails with "Application not a procedure". |
| Keymap inheritance | Automatic: the buffer's mode stack is built from the ancestry (`fundamental → mylang → mylang-test`), and keymap resolution walks it top-down — the child's binding wins, unbound keys fall through to the parent. |
| Hook inheritance | Automatic: every ancestor's hook fires, furthest ancestor first, each exactly once (`fundamental` once). |
| Highlighting | The grammar of the nearest mode in the chain that declares one — `:language mylang` on the parent covers the children. Without `:language`, the default is the mode's bare name when a grammar with that exact id exists. |

## Keywords

- `:keymap ((CHORD COMMAND) ...)` — the mode's own bindings, active while the mode is in the buffer's major-mode chain. Steel list shape: chord and command as two elements.
- `:language SYM-OR-STRING` — the tree-sitter language id to highlight with (a bundled grammar id, or a user-installed one). Inherited by children that don't set their own.

## Order of operations

The command runs the mode's setup body — ancestors' bodies first — then switches the mode; the editor fires the hooks when the switch applies. Every body therefore runs before every hook (Emacs's `delay-mode-hooks` / `run-mode-hooks` order). **Do not add `(run-hooks "mylang-mode-hook")` to the body** — the editor already fires it, so it would run twice.

## Deviations from Emacs

- **Naming**: `(major-mode)` holds the bare name (`mylang`), not `mylang-mode` (see above).
- **No `:syntax-table`, `:abbrev-table`, or `:group`** — not part of Macros' model.
- **Mode-local variables**: the `mode-local` module reads the parent edge from the mode registry, so the child inherits every mode-local value the parent has set. See [Mode-local variables](/docs/mode-local).

## File association: `add-auto-mode`

Emacs's `auto-mode-alist` is a separate mechanism and stays separate here. The bundled `derived-mode` module provides it:

```scheme
(add-auto-mode "\\.mylang$" "mylang-mode")
(add-auto-mode "\\.mytest$" "mylang-test-mode")
```

- The second argument is the **command** name (`<bare>-mode` for derived modes). Entries land in a Rust-side table consulted when a file opens — *before* the built-in extension→language detection — so a matching file opens straight into its final mode with its hooks fired exactly once, and a config reload replaces same-pattern entries instead of duplicating them.
- Patterns are Rust-`regex` dialect — use `$` for end-of-string (Emacs's `'` is not supported).
- Later calls win for overlapping patterns (Emacs `add-to-list` priority semantics).

## A complete example

```scheme
;; requires: derived-mode

(define-derived-mode notes-mode fundamental
  "Mode for my notes files."
  :keymap (("ctrl-c n d" notes-today))
  (message "notes-mode ready"))

(add-auto-mode "\\.notes$" "notes-mode")
```
