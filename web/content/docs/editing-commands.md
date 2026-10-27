---
title: Editing commands
description: Region and kill-ring editing, rectangles, registers, search and replace, occur, sorting, and case commands.
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
| <kbd>M-d</kbd> / <kbd>M-Backspace</kbd> | `kill-word` / `backward-kill-word` |
| <kbd>M-z</kbd> | `zap-to-char` |
| <kbd>C-t</kbd> | `transpose-chars` |
| <kbd>C-x C-t</kbd> | `transpose-lines` |
| <kbd>M-^</kbd> | `join-line` |
| <kbd>C-o</kbd> | `open-line` |

## Rectangles

A **rectangle** is the block of text between mark and point taken as two corners: every line from the mark's line to point's, and the columns between the mark's column and point's. Set the mark (<kbd>C-Space</kbd>), move to the opposite corner, then run a command. Columns are *display* columns, as in Emacs: a tab counts up to its next tab stop (the `tab-width` option) and a wide CJK or emoji character counts two. When an edge cuts through a tab or wide character, that character is split into spaces.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-x r k</kbd> | `kill-rectangle` | Delete the rectangle, saving it as the last killed rectangle |
| <kbd>C-x r M-w</kbd> | `copy-rectangle-as-kill` | Save the rectangle without deleting it |
| <kbd>C-x r d</kbd> | `delete-rectangle` | Delete the rectangle without saving it |
| <kbd>C-x r y</kbd> | `yank-rectangle` | Insert the last killed rectangle with its top-left corner at point |
| <kbd>C-x r o</kbd> | `open-rectangle` | Insert blank space filling the rectangle, pushing text right |
| <kbd>C-x r c</kbd> | `clear-rectangle` | Replace the rectangle with spaces |
| <kbd>C-x r t</kbd> | `string-rectangle` | Replace each line of the rectangle with a string from the minibuffer |
| <kbd>C-x r N</kbd> | `rectangle-number-lines` | Number the lines from 1 at the rectangle's left edge |
| <kbd>C-x SPC</kbd> | `rectangle-mark-mode` | Show the region as a column block |

A killed rectangle is a list of lines, and short lines are padded with spaces so every line has the rectangle's width. It also goes on the clipboard as newline-joined text. <kbd>C-x r y</kbd> pads short lines out to point's column and adds lines at the end of the buffer if it needs them. A rectangle of width zero still works: <kbd>C-x r t</kbd> then inserts the string on every line, and <kbd>C-x r N</kbd> numbers them. As in Emacs, <kbd>C-x r o</kbd> and <kbd>C-x r c</kbd> leave alone any line that ends before the rectangle's left edge. Each command undoes as one step.

**Rectangle mark mode.** <kbd>C-x SPC</kbd> sets the mark if none is active and highlights the region as a column block (the same highlight as evil's <kbd>C-v</kbd> block selection). While it's on, <kbd>C-w</kbd> kills the rectangle, <kbd>M-w</kbd> copies it and <kbd>C-t</kbd> runs `string-rectangle`. <kbd>C-g</kbd> or a second <kbd>C-x SPC</kbd> turns it off.

## Registers

A **register** is a slot named by one character that holds text, a number, a rectangle, a position or a window layout (Emacs `register.el`). Each command prompts for the character. These are separate from evil's <kbd>"a</kbd> registers. They last for the session and are not saved across restarts.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-x r s</kbd> / <kbd>C-x r x</kbd> | `copy-to-register` | Copy the region into a register (with <kbd>C-u</kbd>, delete it too) |
| <kbd>C-x r i</kbd> / <kbd>C-x r g</kbd> | `insert-register` | Insert a register's text, number or rectangle. Point stays before it (after it with <kbd>C-u</kbd>) |
| <kbd>C-x r r</kbd> | `copy-rectangle-to-register` | Copy the region's rectangle into a register (with <kbd>C-u</kbd>, delete it too) |
| <kbd>C-x r SPC</kbd> | `point-to-register` | Remember point |
| <kbd>C-x r j</kbd> | `jump-to-register` | Jump to a remembered position, or restore a window layout |
| <kbd>C-x r n</kbd> | `number-to-register` | Store a number: the prefix argument, otherwise the number at point (and move past it), otherwise 0 |
| <kbd>C-x r +</kbd> | `increment-register` | Add the prefix argument (default 1) to a number register. For a text register, append the region to it |
| <kbd>C-x r w</kbd> | `window-configuration-to-register` | Remember the window split layout |
| <kbd>M-x</kbd> `view-register` | | Describe one register in the echo area |
| <kbd>M-x</kbd> `list-registers` | | List every register in a `*Register List*` buffer (<kbd>q</kbd> closes it) |

A position register follows its text as you edit, even while another buffer is current, because it holds a marker bound to its buffer. If that buffer has been killed, <kbd>C-x r j</kbd> visits its file again at the stored position. A register used for counting goes like this: <kbd>C-u 1 C-x r n a</kbd> to start at 1, <kbd>C-x r i a</kbd> to insert the number, <kbd>C-x r + a</kbd> to step it.

Scheme can use these directly: `(get-register "a")` and `(set-register "a" (cons "text" "hi"))` read and write registers, `(rectangle-bounds)` gives the region's rectangle as `(top bottom left right)`, `(rectangle-extract top bottom left right)` returns its lines and `(insert-rectangle lines)` inserts lines at point.

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
| <kbd>M-=</kbd> | `er/contract` | Step back to the previous, narrower selection |

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
