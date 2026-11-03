---
title: Mode-local variables
description: Mode-scoped variables in Steel — one value per major mode, inherited by derived children; set once from config, read while the mode is active.
---

# Mode-local variables

The bundled `mode-local` module gives major modes their own variables — one value per mode, shared across every buffer running that mode (the same semantics as Emacs's `mode-local.el`, which backs `setq-mode-local`). This is the setup-function pattern: set the values once from your `init.scm`, and any function running while that mode is active reads them.

```scheme
;; requires: mode-local

(setq-mode-local "mylang-mode"
  (indent 4)
  (compile-cmd "mylangc --check"))
```

## What you get

| Piece | Behavior |
|---|---|
| `setq-mode-local` | `(setq-mode-local MODE (VAR VALUE) ...)` — set one or more values. MODE and the vars are unevaluated, as in Emacs: `mylang-mode`, `"mylang-mode"` and the bare stack name `"mylang"` all name the same major mode. Values are evaluated. Load-order safe: the table exists whether or not the mode has been defined yet. |
| `mode-local-value` | `(mode-local-value MODE VAR DEFAULT)` — read a value, walking the derived-mode parent chain; `DEFAULT` when nothing on the chain has it. The two-argument form `(mode-local-value VAR DEFAULT)` searches the **current buffer's mode stack top-down** — an active minor mode overrides the major mode, and a child overrides its parent (the ancestors are on the stack, so inheritance is free). |
| `mode-local-set` | Functional core under `setq-mode-local`; `(mode-local-set MODE 'VAR VALUE)` when you have the variable name in a variable. |
| `mode-local-parent` | `(mode-local-parent MODE)` — the parent in the editor's mode registry (`"fundamental"` for a root major mode), or `#f` for `fundamental` and unregistered modes. Same as the `mode-parent` primitive. |

## Inheritance

`define-derived-mode` registers the child→parent edge in the editor's mode registry, so a child inherits every parent value it has not set itself:

```scheme
(define-derived-mode mylang-mode fundamental "My language mode.")
(define-derived-mode mylang-test-mode mylang-mode "Test variant.")

(setq-mode-local "mylang-mode" (indent 4))
(mode-local-value "mylang-test-mode" 'indent 2) ; ⇒ 4 (inherited)

(setq-mode-local "mylang-test-mode" (indent 2)) ; child override wins
(mode-local-value "mylang-test-mode" 'indent 2) ; ⇒ 2
```

## Scope notes

- **Per mode, not per buffer.** Every buffer running the mode sees the same value (Emacs semantics). For a per-buffer override, combine with `buffer-state-get`/`buffer-state-set` (see `scheme/core.scm`).
- **Global modes (like `fundamental`) are shared space.** Every mode derives from something; a value set on `"fundamental"` is visible to every mode that has not set its own. Set values on the specific mode you mean.
- An explicitly-set `#f` is a real value, distinct from "never set" — pass `'unset` as the default to distinguish.
- `'unset` is a sentinel, not a storage slot: `(setq-mode-local "m" (v 'unset))` stores a value that `mode-local-value` cannot distinguish from "never set" (it falls through to the parent chain / default). Don't store it.
