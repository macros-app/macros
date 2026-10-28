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

## What you get

| Piece | Behavior |
|---|---|
| `my-mode` command | M-x registered. No argument toggles; a truthy/positive argument enables; `#f`, `0`, or a negative number disables. |
| Per-buffer state | The mode is buffer-local. `my-mode` in one buffer doesn't affect another. The mode-line always reflects the focused buffer. |
| `:lighter` | A mode-line slot showing `" My"` while the mode is on in the focused buffer. |
| `:keymap` | Chords bound into both `evil-normal` and `helix-normal`, dispatched only while the mode is on. Chords are the usual whitespace-separated sequences and may be any length. |
| `my-mode-hook` | Run (by name string) after every enable/disable. Add hooks with `(add-hook "my-mode-hook" #'my-setup)`. |
| Body | Runs when the mode **actually flips** — not on redundant re-invocations. |

## Keywords

- `:lighter STRING` — mode-line text while on (default `""`).
- `:init-value BOOL` — the mode's state in buffers that haven't toggled it (default `#f`).
- `:keymap ((CHORD COMMAND) ...)` — chords active while the mode is on. Note this is a Steel list: chord and command as two elements, not Emacs's dotted `(CHORD . COMMAND)`. Commands are `'symbol`s (or plain strings without `%c`); `%c` substitution forms aren't supported inside a minor-mode keymap — wrap them in a named command first.

Unknown keywords raise an error at definition time. Not supported: `:global`, `:variable`, `:interactive`, `:set`/`:initialize`/`:type`, `:after-hook`, and the pre-Emacs-21 positional triplet.

## What the form defines

Like Emacs, `define-minor-mode` defines `my-mode`, `my-mode-map`, and `my-mode-hook`:

- `my-mode` — the toggle command.
- `my-mode-map` — an introspection alist of your `:keymap` bindings. It is *not* a live editor keymap: the framework installs the chords into the modal Normal maps and gates them on mode state.
- `my-mode-hook` — a parity binding holding `#f`. The **live** hook is keyed by the string `"my-mode-hook"`; add and remove hooks by that name:

```scheme
(add-hook "my-mode-hook" #'my-setup)     ; or 'my-setup
(remove-hook "my-mode-hook" #'my-setup)
```

## Deviations from Emacs

- **The body runs only on an actual flip.** Emacs runs the body on every invocation, even when the mode's state doesn't change. If you need work done on every call regardless, put it on `my-mode-hook` instead (the hook runs after every enable *and* disable).
- **Lighters are focus-scoped.** The mode line is a global register, so `:lighter` shows the focused buffer's mode states only — there are no per-buffer lighter regions yet.
- **`:keymap` chords install into `evil-normal` + `helix-normal`** and are gated on the mode's state, so they are active in all editing contexts those maps reach.
- **No custom `defcustom` wiring** (`:set`, `:type`, groups). Mode variables are plain Scheme state.
