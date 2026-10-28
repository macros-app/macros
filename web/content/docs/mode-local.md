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
| `setq-mode-local` | `(setq-mode-local MODE (VAR VALUE) ...)` — set one or more values. Vars are unevaluated symbols; values are evaluated. Load-order safe: the table exists whether or not the mode has been defined yet. |
| `mode-local-value` | `(mode-local-value MODE VAR DEFAULT)` — read a value, walking the derived-mode parent chain; `DEFAULT` when nothing on the chain has it. |
| `mode-local-set` | Functional core under `setq-mode-local`; `(mode-local-set MODE 'VAR VALUE)` when you have the variable name in a variable. |
| `mode-local-parent` | `(mode-local-parent MODE)` — the registered parent mode, or `#f`. |

## Inheritance

`define-derived-mode` registers the child→parent edge automatically, so a child inherits every parent value it has not set itself:

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
