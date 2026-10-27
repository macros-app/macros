---
title: Editing commands
description: Region and kill-ring editing, search and replace, occur, sorting, and case commands.
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

## Files and buffers

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-x C-w</kbd> | `write-file` | Save the buffer under a new path (with file-name completion, like <kbd>C-x C-f</kbd>). The buffer then visits that file and takes its name. Asks before overwriting an existing file. Give a directory to keep the file name. |
| <kbd>C-x i</kbd> | `insert-file` | Insert a file's contents at point. Point stays before the inserted text. |
| <kbd>C-x C-q</kbd> | `read-only-mode` | Toggle whether the buffer can be changed. The mode line shows `%%` while it's read-only (`%*` if it has unsaved changes). Files you can't write open read-only. |
| — | `rename-buffer` | Give the current buffer a new name. |
| — | `find-file-at-point` (`ffap`) | Open the file or URL at point. A relative path is relative to the buffer's directory. `path:LINE` (or `path:LINE:COL`, as compilers print it) lands on that line, and a URL opens in your browser. With nothing recognizable at point it prompts like <kbd>C-x C-f</kbd>. |

The <kbd>C-x 4</kbd> prefix does the same things in another window, splitting the frame if there's only one window:

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-x 4 f</kbd> | `find-file-other-window` | Visit a file in the other window and select it |
| <kbd>C-x 4 b</kbd> | `switch-to-buffer-other-window` | Show a buffer in the other window and select it |
| <kbd>C-x 4 C-o</kbd> | `display-buffer` | Show a buffer in the other window, but stay where you are |
| <kbd>C-x 4 0</kbd> | `kill-buffer-and-window` | Kill the current buffer and close its window |

Saving keeps a backup of each file's original contents, and can hold Emacs-style lock files. See [Backups and lock files](/docs/configuration#backups-and-lock-files).

In Scheme, each of these commands takes its argument directly instead of prompting: `(rename-buffer "notes")`, `(insert-file "~/header.txt")`, `(find-file-other-window path)`, `(display-buffer "*Messages*")`. `(write-file path)` saves the buffer as `path`, and `(write-file path text)` still writes `text` to a file as before. That two-argument form is also available as `write-file-text`.

## The buffer list

<kbd>C-x C-b</kbd> (`ibuffer`) opens a buffer manager (Emacs `ibuffer`) listing every open buffer. <kbd>Enter</kbd> switches to the buffer on the line, <kbd>d</kbd> or <kbd>x</kbd> kills it, <kbd>g</kbd> refreshes. (To pick a buffer through Helm instead, use <kbd>C-x b</kbd>.)
