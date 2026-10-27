---
title: Scripting with Steel
description: Define commands, keymaps, and modes in Steel — the same language the editor is written in.
---

# Scripting with Steel

Macros is scripted in **Steel**, a fast embedded Scheme. This isn't a plugin API bolted onto a closed core — the editor's own features (evil-mode, Magit, LSP commands, dired, org-mode) are written in the *same* Steel that you write your config in, using the *same* primitives. If the editor can do it, your scripts can too.

Your code lives in `~/.config/macros/init.scm` (see [Configuration](/docs/configuration)). Evaluate snippets live with <kbd>M-:</kbd> (`eval-expression`).

## Defining a command

Any zero-argument function is bindable to a key and runnable by name. To also list it in the <kbd>M-x</kbd> palette, register it — `define-command` defines and registers in one form, so the name lives in exactly one place:

```scheme
;; define-command = define + register for M-x, in one form.
(define-command (insert-date)
  (insert-string (call-process "date" '("+%Y-%m-%d"))))

;; Now: M-x insert-date, or bind it.
(bind-key "fundamental" "ctrl-c d" 'insert-date)
```

The longhand is a plain `define` plus `(register-command 'insert-date)`; either way, pass the command name as a `'symbol` (a string also works).

## Keymaps

`bind-key` is the recommended way to bind a key — pass the command name as a `'name` symbol:

```scheme
;; (bind-key "<map>" "<chord>" <command>)
(bind-key "fundamental" "ctrl-c g" 'magit-status)  ; global map
(bind-key "org-mode"    "ctrl-c x" 'org-todo)      ; mode-local map
```

The `'name` symbol names the command without evaluating it, so the binding is independent of file load order. Built-in commands bind the same way. `bind-key` also accepts the bare identity (`magit-status`, no quote) — that turns a typo into a load-time error and lets go-to-definition follow the binding, but the command must already be defined — and a plain string name. `bind-key` is a thin wrapper over the lower-level `define-key` primitive, which takes a command **name** as a `'symbol` (preferred) or string — the canonical form across the API. Name-reference arguments throughout Macros work this way: prefer a `'symbol`, with strings still accepted.

The global map is `"fundamental"`; major modes have maps named after them. See [Keybindings](/docs/keybindings).

## Reading & editing buffers

Macros exposes Emacs-style primitives. Point and the buffer are addressed by **byte** offsets:

```scheme
(point)               ; current cursor position
(goto-char n)         ; move point (queued, so save-excursion restores it)
(buffer-string)       ; whole buffer text
(insert-string "x")   ; insert at point
(message "hello")     ; echo-area message
```

`save-excursion` runs a body and restores point afterward, just like Emacs:

```scheme
(save-excursion
  (beginning-of-buffer)
  (insert-string ";; generated\n"))
```

Other building blocks you'll reach for: `range-edit` (replace a byte range), regexp search, undo grouping (a command's edits coalesce into one undo step), buffer-local variables, marker tracking, and minibuffer reads for prompting the user (`read-string`, `completing-read`, `y-or-n-p`, and `read-passwd` for a masked, history-free secret). For passwords and API keys, don't prompt yourself: ask the [secrets store](/docs/secrets) with `secret-get`.

## Running processes

```scheme
;; Synchronous — returns stdout as a string:
(call-process "git" '("rev-parse" "--abbrev-ref" "HEAD"))

;; Synchronous with stdin, exit code and stderr → (code stdout stderr):
(call-process-full "git" '("apply" "-") patch-text)

;; Asynchronous — make-process / run-with-timer for long-running work
;; without blocking the UI.
```

`call-process` is the foundation of Magit, compile, grep, and dired.

To give a child extra environment, without changing the editor's own, use `call-process-env` / `make-process-env`, which take a trailing list of `"KEY=VALUE"` strings (a bare `"KEY"` removes the variable). To cover everything spawned inside a block, use `with-process-env`:

```scheme
(call-process-env "psql" args "" (list (string-append "PGPASSWORD=" pw)))

(with-process-env (list "RUST_LOG=debug")
  (lambda () (make-process "cargo" '("run") 'on-output 'on-exit)))
```

`with-process-env` applies to `call-process*`, `make-process*`, `make-terminal`, and `dap-start`; blocks nest, and the inner one wins. This is how to hand a subprocess a **secret**: argv is visible to every local user through `ps`, but the environment isn't. See [Passwords & secrets](/docs/secrets#handing-a-secret-to-a-subprocess).

## Special (read-only) buffers

The faced, read-only buffers used by Magit, LSP results, dired, and the diagnostics list are built from a small set of primitives you can use too:

- `render-buffer` — draw a read-only buffer with per-line faces.
- `set-line-icon` — put an icon beside a line (the dired sidebar uses this).
- `buffer-line` / `goto-line` — find and restore the item at point.
- overlay faces — color sub-line spans (diff accents, semantic tokens, rainbow).

This is exactly how a "list buffer" UI (a results list with <kbd>n</kbd>/<kbd>p</kbd>/<kbd>Enter</kbd>/<kbd>q</kbd>) is built — pick a Scheme module like `dired.scm` or `lsp.scm` in the source as a template.

## Display overlays & tree-sitter queries

Display-only overlays change how text *renders* without touching the buffer. They're the building blocks behind markdown/org heading concealment, the Org indent look, inlay hints, and rainbow brackets — and they work in any buffer, whatever its major mode. All ranges are **byte** offsets.

| Primitive | Effect |
| --- | --- |
| `(overlay-set-face start end "face")` | color a byte range |
| `(overlay-hide start end)` | conceal a byte range (revealed while point is on the line) |
| `(set-line-scale line n)` | render a line at `n`× the font size (e.g. headings) |
| `(set-line-indent line cols)` | shift a line right by `cols` columns (display-only) |
| `(put-text-property start end "key" "val")` | attach a property to a byte range |
| `(clear-overlay-faces)` / `(clear-overlay-hides)` | reset faces / hides (each owner clears only its own) |
| `(clear-line-scales)` / `(clear-line-indents)` | reset scaling / indentation |

To target *syntax* rather than regex matches, query the buffer's tree-sitter grammar. `(tree-sitter-query "<query>" 'callback)` runs the query against whatever grammar the current buffer uses and calls `callback` with the captures — one `name⇥start⇥end⇥kind` record per line (tab-separated, byte offsets). `(tree-sitter-node-at byte 'callback)` reports the node chain at a position ("what am I inside?"). The callback name is a `'symbol` (a string also works).

Combined, they let you conceal — or recolor, or enlarge — a node type in *any* language. This is the markdown `## `-hiding machinery, generalized:

```scheme
;; Hide every node captured by `query` in the current buffer (any grammar).
(define (hide-nodes query)
  (clear-overlay-hides)
  (tree-sitter-query query 'hide-nodes--apply))

(define (hide-nodes--apply records)        ; "name\tstart\tend\tkind" per line
  (for-each
    (lambda (rec)
      (let ((f (split-many rec "\t")))
        (when (>= (length f) 3)
          (overlay-hide (string->number (nth f 1))
                        (string->number (nth f 2))))))
    (split-many records "\n")))

;; Hide TypeScript type annotations, or Rust visibility modifiers:
(hide-nodes "(type_annotation) @h")
(hide-nodes "(visibility_modifier) @h")
```

For a live effect, re-run on edit by scheduling from `after-change-functions` (debounced) — see how `org.scm` and `markdown.scm` drive their fontify passes, and gate the repaint on a structural signature so it doesn't run on every keystroke. Tree-sitter queries return matches only for buffers that have a bundled grammar.

## Modes, faces, and themes

Define a major mode by giving it a keymap and activating it from `find-file-hook` based on the file. Set syntax/UI colors with faces and load themes — see [Themes & faces](/docs/themes-and-faces). For the full recipe — tree-sitter syntax highlighting, bundling a new grammar, and a worked major-mode example — see [Languages & modes](/docs/languages).

## Editing Steel & evaluating it live

Plain `.scm` files open in `steel-mode`. Editor-aware config and extension code opens in `macros-steel-mode`, the same mode with the editor API seeded for completion and docs. That means anything under `~/.config/macros` or a file named `*.macros.scm`; run <kbd>M-x</kbd> `macros-steel-mode` for any other buffer.

In `macros-steel-mode`, the eval keys push code into the running editor without restarting:

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-e</kbd> / <kbd>C-x C-e</kbd> | `eval-last-sexp` | Evaluate the expression before point (or the active region) |
| <kbd>C-M-x</kbd> | `eval-defun` | Evaluate the top-level form at point |
| <kbd>C-c C-b</kbd> | `eval-buffer` | Evaluate the whole buffer |

In plain `steel-mode`, the same keys evaluate in the isolated [Anvil](/docs/anvil) image instead (`anvil-eval-last-sexp`, `anvil-eval-defun`, `anvil-eval-buffer`). Ordinary Steel code that defines a function named `insert` or `message` can't silently replace the editor's own. <kbd>M-x</kbd> `eval-last-sexp` and `eval-buffer` still evaluate live from any buffer.

A live evaluation that runs away (an infinite loop from <kbd>M-:</kbd> or <kbd>C-x C-e</kbd>) can be stopped with <kbd>C-g</kbd>. Steel checks for the interrupt on every instruction, so the loop unwinds with an error and the editor keeps going. Saving while such a loop runs no longer freezes the editor waiting on `before-save-hook`; the hook is skipped with a message. A loop in your `init.scm` is interrupted after two minutes instead of hanging startup.

In `macros-steel-mode` you also get one-key documentation for the API symbol at point:

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-d</kbd> | `macros-eldoc` | Echo a one-line summary of the symbol at point |
| <kbd>C-c C-f</kbd> | `macros-describe-point` | Open the symbol's full docs in `*help*` |

The built-in `macros-scheme` language server also powers completion, "unknown function" diagnostics, and navigation over the editor API. Because command references are `'symbols` (not strings), the server treats them as real identifiers — so <kbd>M-.</kbd> (`lsp-find-definition`) jumps from a `'magit-status` in a keybinding straight to its `(define …)`, and <kbd>M-?</kbd> (`lsp-find-references`) lists its uses (quoted references included). The leading quote is a word boundary, so completion and navigation read the bare name. A string `"magit-status"` is opaque by comparison — this is the practical reason to prefer the `'symbol` form.

So the workflow is: edit a function in your `init.scm`, press <kbd>C-c C-e</kbd> on it, and keep going — the redefinition is live. <kbd>M-:</kbd> (`eval-expression`) evaluates a one-off expression in the minibuffer.

### Structural editing

Lisp buffers (`steel`, `macros-steel`, and the other Lisp modes) get the core paredit edits on top of Emacs's sexp motions. Both work on reader-aware boundaries, so a paren inside a string, a `;` comment or a `#\(` char literal is never treated as a delimiter. The motions are global and work in other languages too; see [Sexps and defuns](/docs/editing-commands#sexps-and-defuns).

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-M-f</kbd> / <kbd>C-M-b</kbd> | `forward-sexp` / `backward-sexp` | Move over a sexp |
| <kbd>C-M-u</kbd> | `backward-up-list` | Move out of the enclosing list |
| <kbd>C-M-n</kbd> / <kbd>C-M-p</kbd> | `forward-list` / `backward-list` | Move over the next / previous list |
| <kbd>C-M-d</kbd> | `down-list` | Move into the next list |
| <kbd>C-M-k</kbd> | `kill-sexp` | Kill the sexp after point |
| <kbd>C-M-a</kbd> / <kbd>C-M-e</kbd> | `beginning-of-defun` / `end-of-defun` | Move to the start / end of the top-level form |
| <kbd>C-)</kbd> / <kbd>C-}</kbd> | `paredit-forward-slurp-sexp` / `-barf-sexp` | Pull the next sexp in / push the last one out |
| <kbd>M-(</kbd> | `paredit-wrap-round` | Wrap the sexp at point in parens |
| <kbd>M-r</kbd> | `paredit-raise-sexp` | Replace the enclosing list with the sexp at point |
| <kbd>M-s s</kbd> | `paredit-splice-sexp` | Remove the enclosing list's parens |

### Lisp indentation

Lisp buffers indent the way Emacs does:
- A definer (`define…`, `def…`, `with-…`, `begin`) indents its body one `tab-width` step. The default step is 2 for every Lisp.
- Forms like `let`, `lambda`, `when`, `case` and `do` treat their first arguments as special: a special argument written on its own line gets two steps, and the body after it gets one.
- `cond` clauses align under the first clause.
- Quoted lists and vectors align under their first element.
- Ordinary calls align under their first argument.

Teach it your own macros with `lisp-indent-rule`, the equivalent of Emacs's `lisp-indent-function`:

```scheme
(lisp-indent-rule "with-db" "body")     ; body indented one step
(lisp-indent-rule "my-let" "1")         ; one special argument, then the body
(lisp-indent-rule "for" "function")     ; align like an ordinary call
(lisp-indent-rule "my-cond" "clauses")  ; cond-style clause alignment
```

## Discoverability is part of the API

Function docs are scraped from the `;;` comments above each definition, so anything you document is searchable from inside the editor:

```scheme
;; Insert today's date in ISO format.
(define (insert-date) ...)

(function-list)            ; all documented commands
(function-doc "insert-date")
```

Then <kbd>C-h f</kbd>, <kbd>C-h a</kbd> (`apropos`), and the live <kbd>C-h C-a</kbd> (`helm-apropos`) find it — the last jumps straight back to this definition on <kbd>Enter</kbd>. See [Getting help](/docs/getting-help).

## Learn by reading the source

The best reference is the runtime itself: the `scheme/` directory ships as readable source. `magit.scm` shows transients and faced buffers, `lsp.scm` shows request/response wiring and list buffers, `helm.scm` shows custom pickers, `org.scm` shows a parser-backed mode. Copy the patterns, then make them yours.
