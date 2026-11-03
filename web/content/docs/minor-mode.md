---
title: Minor modes
description: Write Emacs-style minor modes in Steel — one form gives you the toggle command, per-buffer state, a mode-line lighter, and a mode hook.
---

# Minor modes

`define-minor-mode` is the Emacs-style way to write a buffer-local minor mode in Steel. One form defines everything a mode needs: the toggle command (M-x registered), per-buffer on/off state, a mode-line lighter, and a mode hook.

```scheme
(define-minor-mode my-mode
  "Do my thing in the current buffer."
  :lighter " My"
  :keymap (("ctrl-c m t" my-toggle-thing))
  ;; Optional body — runs when the mode actually flips on or off.
  (message "my-mode flipped"))
```

Then `M-x my-mode` toggles it in the current buffer.

## How it works

A minor mode is a real entry on the buffer's [mode stack](derived-mode), pushed **above** the major mode when enabled and removed when disabled. The editor owns the per-buffer state; Scheme is a thin layer on top. This gives minor modes the same machinery as major modes:

- **Keys** resolve over the stack top-down, so an enabled minor mode's bindings shadow the major mode's — as in Emacs.
- **The mode line** renders the lighter straight from the stack entry.
- **`C-h m`** (`describe-mode`) reads the stack, so your mode shows up under "Enabled minor modes" when it's on.
- **Minors survive a major-mode switch.** `M-x fundamental-mode` (or any other major mode) keeps your enabled minor modes on, exactly like Emacs keeps buffer-local minor modes.

## What you get

| Piece | Behavior |
|---|---|
| `my-mode` command | M-x registered. No argument toggles; a truthy/positive argument enables; `#f`, `0`, or a negative number disables. |
| Per-buffer state | The mode is buffer-local: enabling pushes it onto *this buffer's* mode stack. `my-mode` in one buffer doesn't affect another. |
| `:lighter` | Rendered in the mode line from the stack entry while the mode is on in that buffer. |
| `:keymap` | Bound into the mode's **own keymap** (active only while the mode is on the stack — so non-modal users get the bindings too) plus `evil-normal:my-mode` and `helix-normal:my-mode` for modal users. Two minor modes can bind the same chord and each keeps its own binding; nothing lands in the shared global Normal maps. Chords are the usual whitespace-separated sequences and may be any length. |
| `my-mode-hook` | Fired by the editor after every enable **and** disable. Add hooks with `(add-hook "my-mode-hook" 'my-setup)` — a `'symbol` or string, not `#'` (Steel's `#'` is `syntax`, and the hook fails with "Application not a procedure"). |
| Body | Runs when the mode **actually flips** — not on redundant re-invocations. |

## Keywords

- `:lighter STRING` — mode-line text while on (default `""`).
- `:init-value BOOL` — the mode is enabled in a buffer's **first activation** (default `#f`). If the user disables it there, it stays off in that buffer; other new buffers still start with it on.
- `:keymap ((CHORD COMMAND) ...)` — chords active while the mode is on. Note this is a Steel list: chord and command as two elements, not Emacs's dotted `(CHORD . COMMAND)`. Commands are `'symbol`s (or plain strings without `%c`); `%c` substitution forms aren't supported inside a minor-mode keymap — wrap them in a named command first.

Unknown keywords raise an error at definition time. Not supported: `:global`, `:variable`, `:interactive`, `:set`/`:initialize`/`:type`, `:after-hook`, and the pre-Emacs-21 positional triplet.

## What the form defines

Like Emacs, `define-minor-mode` defines `my-mode`, `my-mode-map`, and `my-mode-hook`:

- `my-mode` — the toggle command. Internally it queues `enable-minor-mode` / `disable-minor-mode`, and the editor applies the push/remove and fires the hooks. Reads in the same script already see the change: `(my-mode 1)` followed by `(minor-mode-enabled? 'my-mode)` returns `#t`, and `(my-mode) (my-mode)` flips twice (toggles queued after switching to another buffer or window aren't tracked this way, since they apply to the new buffer).
- `my-mode-map` — an introspection alist of your `:keymap` bindings. The framework *also* installs the chords into the live `my-mode` / `evil-normal:my-mode` / `helix-normal:my-mode` keymaps, which resolve only while the mode is on the stack.
- `my-mode-hook` — a parity binding holding `#f`. The **live** hook is keyed by the string `"my-mode-hook"`; add and remove hooks by that name:

```scheme
(add-hook "my-mode-hook" 'my-setup)       ; or the string "my-setup"
(remove-hook "my-mode-hook" 'my-setup)
```

## Deviations from Emacs

- **The body runs only on an actual flip.** Emacs runs the body on every invocation, even when the mode's state doesn't change. If you need work done on every call regardless, put it on `my-mode-hook` instead (the hook runs after every enable *and* disable).
- **`:init-value` applies on a buffer's first activation**, not "in every buffer until toggled". A buffer that existed before the mode was defined (or one where you disabled it) doesn't get it pushed again on later switches.
- **Special buffers clear minors.** Modes set via the editor's special-mode path (magit-style buffers) take over the buffer and start from a fresh stack — the analog of Emacs's `kill-all-local-variables`.
- **No custom `defcustom` wiring** (`:set`, `:type`, groups). Mode variables are plain Scheme state.
