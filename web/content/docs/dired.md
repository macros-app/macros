---
title: Dired
description: A directory editor — browse and manipulate the filesystem from a buffer.
---

# Dired

Dired is a **directory editor**: a buffer that lists a directory's contents and lets you act on files directly — visit, rename, copy, delete, make directories. It's built on `list-directory`, the special-buffer model, and `call-process` for the filesystem operations.

## Opening Dired

| Key | Command |
| --- | --- |
| <kbd>C-x d</kbd> | `dired` |

You're prompted for a directory; Dired opens a listing where directories keep a trailing `/`.

Dired works on other machines too. Open a `/ssh:host:/dir` path, or press <kbd>C-x d</kbd> in a remote buffer, and the listing, copy, rename, mkdir and delete all run on that host. See [Remote files](/docs/remote-files).

## Keys

| Key | Command | Action |
| --- | --- | --- |
| <kbd>Enter</kbd> | `dired-visit` | Open the file or enter the directory |
| <kbd>^</kbd> | `dired-up` | Go to the parent directory |
| <kbd>g</kbd> | `dired-refresh` | Re-read the directory |
| <kbd>d</kbd> | `dired-mark` | Mark the file (for deletion) |
| <kbd>u</kbd> | `dired-unmark` | Remove the mark |
| <kbd>x</kbd> | `dired-do-delete` | Delete the marked files |
| <kbd>R</kbd> | `dired-rename` | Rename the file at point |
| <kbd>C</kbd> | `dired-copy` | Copy the file at point |
| <kbd>+</kbd> | `dired-mkdir` | Create a directory |
| <kbd>n</kbd> / <kbd>p</kbd> | | Move down / up |
| <kbd>q</kbd> | `kill-buffer` | Close |
| <kbd>C-x C-q</kbd> / <kbd>C-c C-e</kbd> | `wdired-change-to-wdired-mode` | Edit the file names (below) |

## Renaming by editing (wdired)

<kbd>C-x C-q</kbd> turns the listing into editable text — Emacs's **wdired**. Only the file names can change: the header and each line's mark column are read-only, so an edit that touches them is refused ("Text is read-only"). Everything that edits text works on the names, so you can rename many files at once with `query-replace`, `replace-regexp`, multiple cursors, rectangles or a keyboard macro.

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-c</kbd> / <kbd>C-x C-s</kbd> / <kbd>C-x C-q</kbd> | `wdired-finish-edit` | Rename every changed file |
| <kbd>C-c C-k</kbd> / <kbd>C-c ESC</kbd> | `wdired-abort-changes` | Throw the edits away and restore the listing |

Under evil or helix, <kbd>i</kbd> in the listing starts editing (as with `wgrep`); edit in Insert/Normal state as usual, then <kbd>C-c C-c</kbd>.

How the renames are applied:

- A name containing `/` moves the file — `old/name` relative to the listing's directory, or an absolute path. Missing directories are created.
- A directory's trailing `/` is optional.
- Swaps and cycles (`a` → `b` while `b` → `a`) work: one file steps aside to a temporary `.wdired-N-name` first. A case-only rename (`foo` → `Foo`) goes the same way, so it works on case-insensitive file systems.
- The whole plan is checked before anything moves. An empty name, two files renamed to the same name, or a name that already exists (and isn't being renamed away in the same commit) renames **nothing**: the echo area says why, and you stay in the editable listing to fix it or abort.
- A rename never overwrites an existing file. If one fails partway (permissions, a file that appeared meanwhile), the renames already done are undone, newest first, and the error is shown — no file is lost or left at a temporary name. In the unlikely case the undo also fails, the message lists where each file was left.
- Open buffers visiting a renamed file follow it, and `d` marks follow their files.

While you edit, the listing stops auto-refreshing so a change on disk can't overwrite your edits.

## Customizing

Dired is on <kbd>C-x d</kbd> by default. Rebind it, or any key inside the listing (the `"dired-mode"` map), from your `init.scm`:

```scheme
;; Rebind the global entry point:
(define-key "fundamental" "ctrl-x d" 'dired)

;; Rebind keys inside the listing — e.g. lowercase `r` to rename:
(define-key "dired-mode" "r" 'dired-rename)

;; Or edit the names with `e` (wdired), and save them with C-c C-s:
(define-key "dired-mode" "e" 'wdired-change-to-wdired-mode)
(define-key "wdired-mode" "ctrl-c ctrl-s" 'wdired-finish-edit)
```

## Dired vs. the sidebar

Dired is a **buffer per directory** focused on *operating* on files (mark, delete, rename, copy). If you want a persistent, expandable **tree sidebar** for navigation, use the bundled [Dired sidebar](/docs/dired-sidebar) instead — they complement each other.
