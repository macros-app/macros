---
title: Abbreviations
description: Emacs-style abbrevs — type a short word, expand it into something longer on demand.
---

# Abbreviations

Emacs-style abbreviations (`abbrev.el`). An abbrev is a short word that expands to something longer — `teh` → `the`. With `abbrev-mode` on, expansion happens automatically when you finish typing the word; you can also expand on demand with `expand-abbrev`.

## Expanding

`abbrev-mode` is a buffer-local minor mode with a global default, like in Emacs (there is no `global-abbrev-mode` — Emacs doesn't have one either). `M-x abbrev-mode` toggles the current buffer: the moment you type a word terminator (space, punctuation, …) after an abbrev, the word is replaced by its expansion — `teh ` becomes `the `. The expansion is silent, and `:enable-function` / `:ts-context` predicates can decline it like any other. Leave the mode off in a buffer and nothing expands behind your back; an `abbrev` indicator appears in the mode line while it's on.

To enable it everywhere by default — Emacs's "enable Abbrev mode by default in all buffers" — put this in your `init.scm`:

```scheme
(set-abbrev-mode-default #t)
```

Buffers where you've toggled `abbrev-mode` explicitly keep their own value.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>M-x</kbd> | `abbrev-mode` | Toggle typing-time abbrev expansion + the abbrev completion source in the current buffer |
| <kbd>C-x '</kbd> | `expand-abbrev` | Replace the word before point with its expansion |
| <kbd>C-x a g</kbd> | `add-global-abbrev` | Define a global abbrev: the word (or region) before point is the expansion, the name is read from the minibuffer |
| <kbd>C-x a m</kbd> | `add-mode-abbrev` | Same, into the current mode's abbrev table |
| <kbd>C-x a i g</kbd> | `inverse-add-global-abbrev` | The word before point becomes the abbrev; read its expansion, which then replaces the word |
| <kbd>C-x a i m</kbd> | `inverse-add-mode-abbrev` | Same, into the current mode's abbrev table |
| <kbd>M-x</kbd> | `unexpand-abbrev` | Undo the last expansion, without undoing anything edited since (unbound by default, like Emacs) |
| <kbd>C-x a -</kbd> | `abbrev-prefix-mark` | Prefix trick: type a prefix, mark it, type the abbrev, expand — the hyphen disappears |

Case works like Emacs: type `TEH` and you get `THE`, type `Teh` and you get `The`, otherwise the expansion is inserted exactly as defined (lookup is case-insensitive unless the abbrev is `:case-fixed`). A prefix predicate or syntax-context rule can silently decline to expand (see below).

When leaving insert mode in [evil](/docs/modal-editing) or [helix](/docs/helix) you can also expand automatically:

```scheme
(set-abbrev-expand-on-insert-exit #t)
```

The expansion then happens silently on <kbd>esc</kbd> — see [Clashing with snippets](#clashing-with-snippets) before turning this on.

## Defining abbrevs

From your `init.scm`:

```scheme
(define-abbrev 'global-abbrev-table "teh" "the")        ; every mode
(define-abbrev 'rust-mode-abbrev-table "fnr" "fn rec")  ; rust-mode only
(define-global-abbrev "btw" "by the way")               ; shorthand for global
```

Every mode has an abbrev table (`<mode>-abbrev-table`, created on demand) and `global-abbrev-table` is shared by all modes. Lookup order is the local table first, then the global one.

To undefine an abbrev, define it with `#f` as the expansion:

```scheme
(define-abbrev 'global-abbrev-table "teh" #f)
```

Interactive definitions go through `add-global-abbrev` / `add-mode-abbrev` (and their `inverse-add-*` counterparts), so you can build up abbrevs without editing your config — they are user abbrevs and land in the abbrev file on exit.

A hook runs after a successful expansion — a procedure or a quoted function name, called with the expansion:

```scheme
(define (my-hook expansion) (message "..."))
(define-abbrev 'global-abbrev-table "brb" "be right back" 'my-hook)
```

You can also declare a whole table at once (handy for mode groups):

```scheme
(define-abbrev-table 'rust-mode-abbrev-table
  '(("fnr" "fn rec")
    ("pdbg" "println!(\"{:?}\", $0)"))
  "Rust abbrevs")
```

Tables can inherit from other tables with `:parents`; listed tables are tried (in order) after the local one and before the global one:

```scheme
(define-abbrev-table 'rust-mode-abbrev-table '()
  nil '(:parents '(prog-mode-abbrev-table)))
```

## Properties

Pass a property list as the last argument:

```scheme
(define-abbrev 'global-abbrev-table "ie" "i.e." #f
               '(:case-fixed #t))
```

- `:case-fixed` — insert the expansion exactly as defined; no case folding or capitalization.
- `:enable-function` — a predicate (procedure or `'symbol`, called with the abbrev) consulted before expanding; returning `#f` declines silently.
- `:ts-context` — expand only inside a matching tree-sitter node kind, e.g. `"@comment"` or `"@comment @string"` (the `@` is optional; `"comment"` matches `line_comment`, `block_comment`, …). The enclosing node chain is resolved synchronously at expansion time; in a buffer without a tree-sitter grammar the abbrev never expands.
- `:system` — see the next section.

## System abbreviations

An abbrev defined with `:system #t` is a *system* abbreviation, like in Emacs:

```scheme
(define-abbrev 'global-abbrev-table "sl" "sort lines" #f '(:system #t))
```

- It is **never saved** to your abbrev file — define system abbrevs in `init.scm`, which runs at every startup anyway.
- It **will not overwrite** a user abbrev of the same name. If you have defined `sl` yourself, the system definition is skipped with a message in the echo area. Pass `:system 'force` to override your abbrev anyway.

Abbrevs declared through `define-abbrev-table` are system abbrevs by default — that is why they live in your config and never pollute the abbrev file. A definition that names its own `:system` keeps that value, so `(:system #f)` opts a single entry out to a normal savable abbrev. The same rule applies to everything defined while startup runs: plain `define-abbrev` calls in `init.scm` are system abbrevs too (pass `:system #f` if you really want one savable), so the abbrev file never duplicates what your config already defines.

## Saving

Your abbrevs auto-save on exit to `abbrev-file-name` (`~/.config/macros/abbrevs.scm` by default), silently — no messages, and no file at all until you have at least one savable abbrev. The file is loaded back automatically at the end of startup, Emacs's `abbrev_defs` style: the file holds only the abbrevs you added interactively, and re-loading replaces entries rather than duplicating them. The file is Scheme data: exactly the `(define-abbrev …)` forms that rebuild the tables, so you can read, edit, or version it like any config.

```scheme
(write-abbrev-file)            ; save now
(read-abbrev-file)             ; reload
(set-save-abbrevs #f)          ; disable auto-save
(set-abbrev-file-name "~/sync/abbrevs.scm")
```

Abbrevs whose hook or predicate is an anonymous procedure can't be serialized; they are skipped with a comment naming them. Use quoted function names for anything that must persist.

## Completion

While `abbrev-mode` is on in a buffer, your abbrevs also appear as a completion source in the [completion popup](/docs/completion): type a prefix and the expansions show up as candidates. Lookup walks the local table, its `:parents`, then the global one — the same resolution `abbrev-symbol` uses — and `set-abbrev-capf-min-prefix` sets the shortest prefix that completion answers (default 2).

## Clashing with snippets

Abbrevs and [snippets](/docs/snippets) occupy the same territory: both replace the word before point with something longer, and an abbrev expansion can even contain snippet syntax like `$0`. Which system owns which word is up to you — Macros doesn't arbitrate — so keep a few things in mind:

- Keep short trigger words in **one** system only; a word defined as both expands by whichever runs first.
- `set-abbrev-expand-on-insert-exit` rebinds <kbd>esc</kbd> in the evil/helix insert keymaps while enabled — your previous <kbd>esc</kbd> binding is saved and put back when you disable it. If you navigate snippet fields with <kbd>esc</kbd> or use `C-c C-f`/`C-c C-b` heavily, leave it off and expand with <kbd>C-x '</kbd> instead.
- Abbrev completion and snippet completion are both completion-at-point functions; the order you enable them in `init.scm` decides their priority in the popup.
