---
title: Getting help
description: Discover commands, keybindings, and documentation from inside the editor.
---

# Getting help

Macros is self-documenting. Function docs are scraped from the `;;` comments above each definition in the Steel source, so the same explanations you'd read in the code are available live through `describe-function`, `apropos`, and `helm-apropos`. This covers not just the built-in library but every [package](/docs/packages) you install — a package's documented definitions are folded into the same index as it loads. The help commands live under the <kbd>C-h</kbd> prefix, mirroring Emacs.

## The command palette — `M-x`

Press <kbd>M-x</kbd> to open a fuzzy-searchable list of **every** invocable command — both Rust built-ins and Scheme-defined commands. Type to narrow, <kbd>Enter</kbd> to run. This is the single best way to explore what the editor can do.

Under the hood `M-x` is `helm-execute-command`, routed through the [Helm](/docs/helm) selection framework.

## Help commands

| Key | Command | Shows |
| --- | --- | --- |
| <kbd>C-h t</kbd> | `help-with-tutorial` | The interactive [tutorial](/docs/tutorial) |
| <kbd>C-h k</kbd> | `describe-key` | What a key is bound to — press the key after |
| <kbd>C-h f</kbd> | `describe-function` | A command's documentation |
| <kbd>C-h v</kbd> | `describe-variable` | A variable's value and documentation |
| <kbd>C-h m</kbd> | `describe-mode` | The buffer's major mode, enabled minor modes, and their bindings |
| <kbd>C-h B</kbd> | `describe-bindings` | Every binding in effect, keymap by keymap |
| <kbd>C-h a</kbd> | `apropos` | Commands whose name or docs match a query (into a buffer) |
| <kbd>C-h C-a</kbd> | `helm-apropos` | Live search over docs; jump to the implementation |
| <kbd>C-h b</kbd> | `which-key-top` | All active top-level keybindings |
| <kbd>C-h e</kbd> | `list-messages` | The echo-area / message scrollback |
| <kbd>C-h l</kbd> | `list-lsp-log` | Language-server stderr |

### describe-key

Press <kbd>C-h k</kbd>, then the chord you're curious about. Macros tells you which command it runs in the current context.

### describe-function & apropos

<kbd>C-h f</kbd> prompts for a command name and shows its documentation — the first lines of the source comment above its definition. <kbd>C-h a</kbd> searches **across** those docs, so you can find a command even when you don't remember its exact name. Try `C-h a` and type `region`, or `buffer`, or `git`. It dumps the matches into a read-only `*apropos*` buffer.

### describe-variable

<kbd>C-h v</kbd> completes over the Scheme globals written `*like-this*`, which is the convention every bundled file follows, plus the editor [options](/docs/configuration). You can also type any other global by name. It shows the variable's current value, its documentation, and how to set it. For an option it shows the current value and the `set-option` call that changes it.

### describe-mode

<kbd>C-h m</kbd> opens a `*help*` buffer showing:

- the buffer's major mode and highlighting language
- the bindings the major mode's own keymap adds
- the minor modes that are on: `abbrev-mode`, `spell-mode`, and `evil-mode` or `helix-mode`
- the modal keymap's bindings, when evil or helix is on

Packages and your config can add their own minor modes to the list with a predicate:

```scheme
(register-minor-mode "my-mode" (lambda () *my-mode-on*))
```

### helm-apropos — search and jump

<kbd>C-h C-a</kbd> (`helm-apropos`) is the interactive version: a live [Helm](/docs/helm) picker that narrows as you type, matching on both names **and** doc-comments across every loaded definition — built-ins, installed packages, and editor [options](/docs/configuration) alike. Each match is annotated with its one-line summary (options show their current value).

Matching is by **word**: type several words in any order and every one must appear (in the name or the docs), the way Emacs `apropos` works. So `font size` finds the `font-size` option, and so does `size font`. Don't know the name of something? Describe what it does — `region`, `kill buffer`, `git stage`. Then:

- <kbd>Enter</kbd> **jumps to the implementation** — it opens the defining source file at the `define` so you can read exactly how it works.
- <kbd>M-a</kbd> → **Describe** shows the documentation in a `*Help*` buffer instead.

When the source file isn't on disk (an installed binary ships its built-in library compiled in, not as files), the jump gracefully falls back to showing the documentation — the same way Emacs handles a primitive defined in C.

## Discovering keys under a prefix — which-key

Forgot what comes after a prefix? Ask for the list (Emacs `which-key`):

| Key | Command | Shows |
| --- | --- | --- |
| <kbd>C-x ?</kbd> | `which-key-cx` | Every binding under the <kbd>C-x</kbd> prefix |
| <kbd>C-c ?</kbd> | `which-key-cc` | Every binding under the <kbd>C-c</kbd> prefix |
| <kbd>C-h b</kbd> | `which-key-top` | All top-level bindings |

A read-only buffer lists the matching chords and their commands; <kbd>q</kbd> closes it. `M-x which-key` prompts for any prefix you want to inspect.

## Querying the API from Scheme

The discoverability layer is also callable as functions, useful while scripting:

```scheme
(function-list)           ; every documented function/variable name
(function-doc "yank")     ; the doc string for a name
(function-source "yank")  ; (file line) of its definition, or '()
```

These read from the same index `helm-apropos` searches, so anything you load — including packages — is queryable here too.

In help/list buffers (`*Help*`, the diagnostics list, xref results, …) the standard read-only keys apply: <kbd>n</kbd> / <kbd>p</kbd> to move, <kbd>Enter</kbd> to act on a row, <kbd>q</kbd> to close.

## License & credits

<kbd>M-x</kbd> `licenses` opens a read-only `*licenses*` buffer with Macros' license (MIT) and its third-party acknowledgments — the Lilex font, and a note that Magit, Org mode, and Helm are original reimplementations, not the Emacs packages themselves. The full text of every bundled Rust crate license is in `THIRD-PARTY-LICENSES.md`.

## Still stuck?

- Open the [tutorial](/docs/tutorial) with <kbd>C-h t</kbd> and practice hands-on.
- Browse this documentation — start at the [overview](/docs).
- File an issue on [GitHub](https://github.com/macros-app/macros/issues).
