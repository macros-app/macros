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

## Customizing

Dired is on <kbd>C-x d</kbd> by default. Rebind it, or any key inside the listing (the `"dired-mode"` map), from your `init.scm`:

```scheme
;; Rebind the global entry point:
(define-key "fundamental" "ctrl-x d" 'dired)

;; Rebind keys inside the listing — e.g. lowercase `r` to rename:
(define-key "dired-mode" "r" 'dired-rename)
```

## Dired vs. the sidebar

Dired is a **buffer per directory** focused on *operating* on files (mark, delete, rename, copy). If you want a persistent, expandable **tree sidebar** for navigation, use the bundled [Dired sidebar](/docs/dired-sidebar) instead — they complement each other.
