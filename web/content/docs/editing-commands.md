---
title: Editing commands
description: Region and kill-ring editing, sexp and defun motion, whitespace and line utilities, search and replace, occur, sorting, dabbrev, and case commands.
---

# Editing commands

Beyond raw motion, Macros provides the core Emacs editing commands — region-aware where Emacs is, and each command's edits coalesce into a single undo group so they undo as a unit.

## The region and the kill ring

Set the mark with <kbd>C-Space</kbd>, then move point; the text between mark and point is the **region**. Commands that say "region" act on it (or fall back sensibly when there's no active region).

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-Space</kbd> | `set-mark` | Start a selection |
| <kbd>C-w</kbd> | `kill-region` | Cut the region to the kill ring |
| <kbd>M-w</kbd> | `kill-ring-save` | Copy the region |
| <kbd>C-y</kbd> | `yank` | Paste the latest kill |
| <kbd>M-y</kbd> | `yank-pop` | Cycle through earlier kills (after a yank) |
| <kbd>C-x C-x</kbd> | `exchange-point-and-mark` | Swap point and mark |
| <kbd>C-x C-h</kbd> / <kbd>cmd-a</kbd> | `mark-whole-buffer` | Select all |

The kill ring and the system clipboard are kept in sync by the clipboard commands (<kbd>cmd-c</kbd>/<kbd>x</kbd>/<kbd>v</kbd>), so copying in Macros and pasting in another app just works.

The same keys paste into a **minibuffer** prompt: <kbd>cmd-v</kbd> inserts the clipboard, and <kbd>C-y</kbd> inserts the latest kill (taking in anything copied elsewhere first). A prompt is one line, so a trailing newline is dropped and only the first line of a multi-line paste goes in. In a password prompt both read the clipboard directly and never put the secret on the kill ring; see [Passwords & secrets](/docs/secrets#what-you-see).

## Killing & transposing

| Key | Command |
| --- | --- |
| <kbd>C-k</kbd> | `kill-line` |
| <kbd>C-S-Backspace</kbd> | `kill-whole-line` — the whole line and its newline, wherever point is |
| <kbd>M-d</kbd> / <kbd>M-Backspace</kbd> | `kill-word` / `backward-kill-word` |
| <kbd>M-z</kbd> | `zap-to-char` |
| <kbd>C-t</kbd> | `transpose-chars` |
| <kbd>C-x C-t</kbd> | `transpose-lines` |
| <kbd>M-^</kbd> | `join-line` |
| <kbd>C-o</kbd> | `open-line` |

## Sexps and defuns

Emacs's balanced-expression motion works in every buffer, not just Lisp. Lisp buffers read as Lisp (`;` comments, `'` quote prefixes, `#\(` char literals). Every other language reads as bracket-balanced code: a sexp is a `(…)`, `[…]` or `{…}` group, an identifier or number, or a string literal, and operators and other punctuation are skipped. Strings and comments come from the buffer's tree-sitter tree, so a bracket inside `"…"` or `// …` never unbalances anything. Without a tree, `"…"`, `'c'`, `//` and `/* */` are recognised.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-M-f</kbd> / <kbd>C-M-b</kbd> | `forward-sexp` / `backward-sexp` | Move over a sexp |
| <kbd>C-M-n</kbd> / <kbd>C-M-p</kbd> | `forward-list` / `backward-list` | Move over the next / previous bracketed group, skipping atoms |
| <kbd>C-M-u</kbd> | `backward-up-list` | Move out to the start of the enclosing group |
| <kbd>C-M-d</kbd> | `down-list` | Move into the next group |
| <kbd>C-M-k</kbd> | `kill-sexp` | Kill the sexp after point |
| <kbd>C-M-t</kbd> | `transpose-sexps` | Swap the sexps before and after point |
| <kbd>C-M-SPC</kbd> / <kbd>C-M-@</kbd> | `mark-sexp` | Mark the next sexp; repeat to extend the region by another |
| <kbd>C-M-a</kbd> / <kbd>C-M-e</kbd> | `beginning-of-defun` / `end-of-defun` | To the start of this (or the previous) defun / the end of this (or the next) one |
| <kbd>C-M-h</kbd> | `mark-defun` | Mark the defun around point |

`up-list` (out past the closing bracket) is on <kbd>M-x</kbd>.

A **defun** is a top-level form in Lisp. In other languages it is a named function or method node from the tree-sitter tree. Lambdas and closures don't count, so <kbd>C-M-a</kbd> finds the enclosing named function. In a buffer with no tree, a defun is a top-level `{…}` block, starting at the line that opens it. To add node kinds for a language, extend the list in your `init.scm`:

```scheme
(set! *defun-node-kinds* (cons "rule_definition" *defun-node-kinds*))
```

The paredit edits (slurp, barf, wrap, raise, splice) remain Lisp-only. See [Structural editing](/docs/scripting#structural-editing).

## Whitespace, lines and alignment

| Key | Command | Action |
| --- | --- | --- |
| <kbd>M-SPC</kbd> | `just-one-space` | Collapse the spaces around point to one |
| <kbd>M-\\</kbd> | `delete-horizontal-space` | Delete all spaces and tabs around point |
| <kbd>C-x C-o</kbd> | `delete-blank-lines` | Collapse blank lines around point |
| <kbd>C-x TAB</kbd> | `indent-rigidly` | Shift the region's lines (or the current line) left or right |
| <kbd>M-x</kbd> | `delete-trailing-whitespace` | Strip trailing blanks in the region, or in the whole buffer along with blank lines at its end |
| <kbd>M-x</kbd> | `align-regexp` | Line up the first match of a regexp on each line of the region (or paragraph) |
| <kbd>M-x</kbd> | `keep-lines` / `flush-lines` | Keep / delete the lines matching a regexp |
| <kbd>M-x</kbd> | `delete-duplicate-lines` | Delete every repeat of an earlier line, keeping the first |

After <kbd>C-x TAB</kbd>, <kbd>←</kbd> / <kbd>→</kbd> move the lines one column left or right, and <kbd>S-←</kbd> / <kbd>S-→</kbd> move them four. Any other key stops. With a prefix count it shifts by that many columns at once (<kbd>C-u 8 C-x TAB</kbd>). Blank lines are left alone.

`keep-lines` and `flush-lines` act on the region, or on the lines from point to the end of the buffer. As with search, an all-lower-case regexp ignores case. `delete-duplicate-lines` acts on the region, or on the whole buffer. `align-regexp` adds spaces before each match so all the matches start one column past the longest text before them. For example, aligning `=` turns `a = 1` / `bbb = 2` into `a   = 1` / `bbb = 2`.

## Counting and position

| Key | Command | Action |
| --- | --- | --- |
| <kbd>M-=</kbd> | `count-words-region` | Lines, words and characters in the region (the buffer without one) |
| <kbd>M-x</kbd> | `count-words` | The same for the whole buffer |
| <kbd>C-x =</kbd> | `what-cursor-position` | The character after point (decimal, octal, hex, `U+`), point, percentage, and column |

## Dynamic abbreviations

<kbd>M-/</kbd> (`dabbrev-expand`) completes the word before point from words already in your buffers. Candidates come from this buffer first, nearest match before point first and then after it, and then from the other open file buffers (their saved text). Press <kbd>M-/</kbd> again straight away to replace the expansion with the next candidate. After the last one, the text you typed comes back.

## Inserting characters

<kbd>C-x 8 RET</kbd> (`insert-char`) inserts any character. Type a hex code point (`e9`, `U+1F600`, `#x2014`) and press <kbd>RET</kbd>, or search a built-in list of common characters by name: dashes, quotes, arrows, math symbols, Greek letters, currency signs. The build has no full Unicode name table, so names outside that list need their code point.

## Search

Incremental search, Emacs-style:

| Key | Command |
| --- | --- |
| <kbd>C-s</kbd> | `isearch-forward` |
| <kbd>C-r</kbd> | `isearch-backward` |

Type to extend the match; <kbd>C-s</kbd> / <kbd>C-r</kbd> again to jump to the next/previous hit; <kbd>C-g</kbd> to cancel and return to where you started.

## Replace

| Key | Command |
| --- | --- |
| <kbd>M-%</kbd> | `query-replace` — ask at each match of a literal string |
| <kbd>C-M-%</kbd> | `query-replace-regexp` — same, for a regexp |
| <kbd>M-x</kbd> `replace-string` / `replace-regexp` | replace every match without asking |
| <kbd>C-c p r</kbd> / <kbd>C-c p %</kbd> | `projectile-replace` / `projectile-replace-regexp` — across the project |

All of them run from point to the end of the buffer, or inside the active region (an evil Visual selection counts). An empty search string reuses the previous pair. An all-lower-case search matches any case, and a lower-case replacement then follows the match's case (`FOO` → `BAR`, `Foo` → `Bar`). In the regexp versions `\1`…`\9` insert capture groups, `\&` the whole match, `\n` / `\t` a newline / tab.

At each match of a query session:

| Key | Action |
| --- | --- |
| <kbd>y</kbd> / <kbd>SPC</kbd> | replace and go on |
| <kbd>n</kbd> / <kbd>DEL</kbd> | skip |
| <kbd>!</kbd> | replace all remaining |
| <kbd>.</kbd> | replace this one and stop |
| <kbd>,</kbd> | replace, but stay to look at the result |
| <kbd>u</kbd> / <kbd>U</kbd> | undo the last / every replacement |
| <kbd>^</kbd> | back to the previous match |
| <kbd>E</kbd> | edit the replacement string |
| <kbd>q</kbd> / <kbd>RET</kbd> | stop (any other key stops too) |
| <kbd>?</kbd> | help |

The whole session is a single undo step. The keys work the same under evil and helix (<kbd>M-%</kbd> falls through the modal keymaps).

The project versions find the files with ripgrep (gitignore-aware), then run a query session through each file in turn: <kbd>!</kbd> finishes the current file, <kbd>Y</kbd> replaces everything in all remaining files, <kbd>N</kbd> skips to the next file. At the end you're asked whether to save the files that changed. With evil, bind them on your leader, e.g. `(evil-leader-set "n R" 'projectile-replace)`.

## Occur

<kbd>C-c o</kbd> (`occur`) prompts for a regexp (an all-lower-case one ignores case) and opens an `*occur*` window listing every matching line with its line number, matches highlighted. <kbd>n</kbd> / <kbd>p</kbd> move between hits; <kbd>Enter</kbd>, <kbd>o</kbd> or a click jumps to the match in the source; <kbd>q</kbd> closes.

### Editing results (occur-edit / wgrep)

In `*occur*` and in the project grep buffer (<kbd>C-c p G</kbd>), press <kbd>e</kbd> or <kbd>C-c C-p</kbd> (<kbd>i</kbd> under evil/helix) to make the results editable. Only the text after each `file:line:` prefix can change — the prefixes are read-only. Then:

- <kbd>C-c C-c</kbd> applies every changed line. Sources open in a buffer are edited in that buffer and left modified for you to review and save; files that aren't open are patched directly on disk. A line that changed since the search is skipped and reported.
- <kbd>C-c C-k</kbd> throws the edits away.

## Narrowing

Narrowing restricts a buffer to part of its text, so you can work on one function or one Org subtree without the rest in the way.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-x n n</kbd> | `narrow-to-region` | Narrow to the region |
| <kbd>C-x n d</kbd> | `narrow-to-defun` | Narrow to the function, method or type around point (found with tree-sitter; in Lisps, the top-level form) |
| <kbd>C-x n s</kbd> | `org-narrow-to-subtree` | In Org, narrow to the subtree at point |
| <kbd>C-x n w</kbd> | `widen` | Show the whole buffer again |

While a buffer is narrowed, the mode line shows `Narrow` after the mode names, and:

- Only the accessible part is shown. Line numbers count from its first line.
- Motion stops at its edges, including <kbd>M-&lt;</kbd> / <kbd>M-&gt;</kbd> (`beginning-of-buffer` / `end-of-buffer`).
- Isearch, `/` search, query-replace and occur only find matches inside it.
- Edits can't reach outside it. Deleting past either edge is refused with "Beginning of buffer" or "End of buffer". Text typed at the end extends the narrowing, and the edges move with your edits, including undo.
- Saving still writes the whole file. A formatter that rewrites the whole buffer keeps the narrowing around the same text.

From Scheme, `(narrow-to-region start end)` narrows to explicit byte offsets. `(point-min)` and `(point-max)` return the accessible range, `(buffer-narrowed?)` reports whether the buffer is narrowed, and `(buffer-size)` is always the whole buffer's length. As in Emacs, offsets stay absolute while narrowed. To rewrite a whole buffer, replace `0` to `(buffer-size)`, not `(point-max)`.

In a grid buffer, <kbd>C-x n n</kbd> and <kbd>C-x n w</kbd> keep their grid meaning (`grid-narrow-to-selection` / `grid-widen`; see [Grid](grid.md)).

## Comment toggling

<kbd>M-;</kbd> (`comment-dwim`) toggles a line comment on the current line (or region). The comment prefix is a buffer-local string set per mode (`comment-string`), defaulting to `// `; modes like `steel-mode` set their own.

## Sorting & case

These act on the region, or the whole buffer when there's no region:

- `sort-lines` — sort the lines.
- `upcase-region` / `downcase-region` / `capitalize-region` — case conversion.

Run them by name with <kbd>M-x</kbd>, or bind them in your `init.scm`.

## Undo & redo

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-/</kbd> | `undo` | Undo the last change |
| <kbd>C-?</kbd> | `redo` | Redo |

Undo is **branching** (Emacs `undo-tree`): undoing and then making a new edit starts a branch rather than discarding the redone edits, so no work is ever lost. <kbd>M-x</kbd> `vundo` opens a read-only view of the undo tree (<kbd>g</kbd> refreshes, <kbd>q</kbd> closes); navigate the history itself with <kbd>C-/</kbd> and <kbd>C-?</kbd>.

## Expanding the selection

Grow the region outward along tree-sitter syntax boundaries — word, then expression, statement, block (Emacs `expand-region`). It works in any buffer with a tree-sitter grammar, no language server needed.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-=</kbd> | `er/expand` | Widen the selection to the enclosing node |
| <kbd>C--</kbd> | `er/contract` | Step back to the previous, narrower selection |

In evil visual state, <kbd>+</kbd> and <kbd>-</kbd> do the same.

## Multiple cursors

Edit in many places at once (Emacs `mc.el`). Add secondary cursors, then typed text, <kbd>Enter</kbd>, and <kbd>Backspace</kbd> are replicated at every cursor.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-n</kbd> | `mc-add-cursor-below` | Add a cursor on the next line |
| <kbd>C-c C-p</kbd> | `mc-add-cursor-above` | Add a cursor on the previous line |
| <kbd>C-c C-d</kbd> | `mc-mark-next-like-this` | Add a cursor at the next occurrence of the word at point |
| <kbd>C-g</kbd> | `mc-clear` | Collapse back to a single cursor |

Plain motions move only the primary cursor — add the cursors first, then type or delete.

## Jumping to a character

Jump anywhere on screen in a couple of keystrokes (Emacs `avy` / `ace-jump`).

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c j</kbd> | `avy-goto-char` | Type a character; every visible occurrence is tagged with a hint letter — press the letter to jump |

In evil normal state this is <kbd>g s</kbd>. <kbd>C-g</kbd> cancels the jump.

## The buffer list

<kbd>C-x C-b</kbd> (`ibuffer`) opens a buffer manager (Emacs `ibuffer`) listing every open buffer. <kbd>Enter</kbd> switches to the buffer on the line, <kbd>d</kbd> or <kbd>x</kbd> kills it, <kbd>g</kbd> refreshes. (To pick a buffer through Helm instead, use <kbd>C-x b</kbd>.)
