---
title: Derived modes
description: Write Emacs-style derived major modes in Steel — one form defines the mode command, its keymap, and hook ordering; associate it with files via add-auto-mode.
---

# Derived modes

`define-derived-mode` (in the bundled `derived-mode` module) defines a major mode the Emacs way: derive from a parent, add your own bindings and setup.

```scheme
(define-derived-mode mylang-mode fundamental
  "My language mode."
  :keymap (("ctrl-c m c" mylang-compile)
           ("g g" mylang-goto-top))
  (message "mylang-mode enabled"))

(define-derived-mode mylang-test-mode mylang-mode
  "Test-file variant of mylang."
  (message "mylang-test-mode: test helpers on"))
```

Then `M-x mylang-mode` switches the current buffer.

## What you get

| Piece | Behavior |
|---|---|
| `mylang-mode` command | M-x registered. Enters the mode: runs the **parent** mode's hook, switches the mode stack, then runs your body. |
| `mylang-mode-map` | An introspection alist of the mode's `:keymap` bindings. |
| `mylang-mode-hook` | Fires when the editor applies the mode switch (hooks keyed by `"mylang-mode"` and `"mylang-mode-hook"` both run). Add hooks with `(add-hook "mylang-mode-hook" #'my-setup)`. |
| Highlighting | Free: `set-major-mode` attaches a bundled tree-sitter grammar when the mode name matches a language id — name your mode after the grammar (`mylang-mode` for the `mylang` grammar) or install a user grammar with that id (`install-language`). |

## Keywords

- `:keymap ((CHORD COMMAND) ...)` — the mode's own bindings, active while the mode is the buffer's major mode. Steel list shape: chord and command as two elements.

## Deviations from Emacs

- **Keymap inheritance is not automatic.** Keymaps resolve per mode-name over the mode stack; a child's map holds only the child's bindings. To share chords with the parent, repeat them or put them on the parent.
- **Hook order**: parent hook fires first, then the switch applies and the child's hooks fire — Emacs `run-mode-hooks` order. The child's hooks are fired by the editor itself, not by your body. **Do not add `(run-hooks "mylang-mode-hook")` to the body** (the older Emacs idiom) — the editor already fires it, so it would run twice.
- **No `:syntax-table`, `:abbrev-table`, or `:group`** — not part of Macros' model.
- **Mode-local variables**: the expansion registers the parent edge with the `mode-local` module, so the child inherits every mode-local value the parent has set. See [Mode-local variables](/docs/mode-local).

## File association: `add-auto-mode`

Emacs's `auto-mode-alist` is a separate mechanism and stays separate here. The bundled `derived-mode` module provides it:

```scheme
(add-auto-mode "\\.mylang$" "mylang-mode")
(add-auto-mode "\\.mytest$" "mylang-test-mode")
```

- Patterns are Rust-`regex` dialect — use `$` for end-of-string (Emacs's `'` is not supported).
- Calls **prepend**, so later calls win for overlapping patterns (Emacs `add-to-list` priority semantics).
- One `find-file-hook` walks the whole list; buffers with no matching entry pay only a handful of `string-match` calls.

## A complete example

```scheme
;; requires: derived-mode

(define-derived-mode notes-mode fundamental
  "Mode for my notes files."
  :keymap (("ctrl-c n d" notes-today))
  (message "notes-mode ready"))

(add-auto-mode "\\.notes$" "notes-mode")
```
