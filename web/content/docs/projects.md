---
title: Projects & search
description: Projectile-style project navigation, ripgrep-backed find-file and grep, and per-project perspectives switched from the title bar.
---

# Projects & search

Macros follows the **projectile.el** pattern: a project is the directory tree above the file you're editing, found by walking up to a marker (like `.git`). On top of that, ripgrep's gitignore-aware walker powers fast file finding and project-wide grep — and it works in non-git directories too.

## Commands

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c p f</kbd> | `projectile-find-file` | Find a file in the current project (Helm) |
| <kbd>C-c p p</kbd> | `projectile-switch-project` | Switch to another known project |
| <kbd>C-c p g</kbd> | `projectile-grep` | Live grep across the project (Helm) |
| <kbd>C-c p G</kbd> | `projectile-grep-buffer` | Grep into a `grep-mode` results buffer |
| <kbd>C-c p r</kbd> | `projectile-replace` | Query-replace across the project |
| <kbd>C-c p s</kbd> | `perspective-switch` | Switch perspective — reopen a project at its last file (Helm) |

The project root is shown in the mode line.

### Find file

`projectile-find-file` lists every file under the project root using ripgrep's walker (so `.gitignore` is honored). It's a static [Helm](/docs/helm) source — the file list is walked once when the picker opens, then filtered in memory as you type.

### Switch project

`projectile-switch-project` lets you pick a known project's root, makes it current, and immediately drops you into its find-file picker.

### Grep

`projectile-grep` is a **live** [Helm](/docs/helm) picker — the `helm-ag` / `counsel-rg` experience. Type a regexp and matches narrow in the panel as you type; <kbd>Enter</kbd> jumps to the file at that line, and <kbd>C-SPC</kbd> multi-marks like any Helm source. The search re-runs on every keystroke (above two characters), backed by ripgrep's gitignore-aware content searcher.

When you'd rather keep a persistent, walkable result set, `projectile-grep-buffer` (<kbd>C-c p G</kbd>) prompts for a pattern and dumps the matches into a `grep-mode` buffer instead:

| Key | Action |
| --- | --- |
| <kbd>Enter</kbd> / <kbd>o</kbd> / click | jump to the match |
| <kbd>n</kbd> / <kbd>p</kbd> | next / previous match |
| <kbd>e</kbd> / <kbd>C-c C-p</kbd> (<kbd>i</kbd> under evil/helix) | edit the results in place (wgrep) — <kbd>C-c C-c</kbd> writes them back, <kbd>C-c C-k</kbd> aborts; see [Editing results](/docs/editing-commands) |
| <kbd>g</kbd> | re-run the grep |
| <kbd>q</kbd> | close |

### Replace across the project

`projectile-replace` (<kbd>C-c p r</kbd>, alias `project-query-replace`) and `projectile-replace-regexp` (<kbd>C-c p %</kbd>) find every project file containing a match, then run a [query-replace](/docs/editing-commands) session through each in turn — <kbd>!</kbd> does the rest of a file, <kbd>Y</kbd> the rest of all files, <kbd>N</kbd> skips a file — and finally offer to save the changed files.

## Perspectives

A **perspective** is a per-project context: each project you touch remembers the file you were last visiting, and you can jump between projects — landing exactly where you left off — from a dropdown in the title bar.

Perspectives are created automatically. The first time you visit a file inside a project, that project appears in the title-bar dropdown; every subsequent visit updates its *resume point* (the last file you had open there). There is nothing to configure or declare.

### Switching

Click **projects** in the title bar (just right of the traffic lights) to unfold the dropdown, then pick a project. Macros reopens that project's last visited file, and a checkmark marks it as the current perspective in the list. Type to filter the list; <kbd>↑</kbd>/<kbd>↓</kbd> (or <kbd>C-p</kbd>/<kbd>C-n</kbd>) and <kbd>Enter</kbd> pick from the keyboard. Clicking anywhere outside the menu folds it without switching.

The same list is available from the keyboard as `perspective-switch` (<kbd>C-c p s</kbd>), a [Helm](/docs/helm) source.

If a project's resume file has been deleted since your last visit, switching to it falls back to the project's find-file picker instead of opening a dead path.

### Hiding the dropdown

The dropdown is a regular [title-bar menu](/docs/titlebar-menus) named `projects`, and it's on by default. To remove it from the title bar, add this to your `init.scm`:

```scheme
(titlebar-menu-hide "projects")
```

Perspectives keep tracking while it's hidden, and <kbd>C-c p s</kbd> still switches between them. `(titlebar-menu-show "projects")` brings it back.

### Names and persistence

The dropdown shows each project's directory name. If two projects share a name (say, two checkouts of `proj`), both are qualified with their parent directory — `work/proj` and `scratch/proj`.

Perspectives survive restarts: the list is written through to `~/.config/macros/perspectives` as soon as a project is touched (no clean quit required) and restored at startup, so the dropdown is populated before you open anything. Projects whose roots no longer exist are pruned on load.

### Relationship to projectile

Perspectives sit on top of the same root detection as everything else on this page (`project-root`, walking up to a marker like `.git`). `projectile-switch-project` drops you into a project's find-file picker; the perspective dropdown instead resumes the project where you left it. Use whichever fits — visiting any file through either path keeps both in sync.

## Tuning the search

The ripgrep walker is configured through `search-*` primitives in your `init.scm`. Because the walker is gitignore-aware by default, you usually don't need to configure anything — but you can prune, whitelist, or loosen it:

```scheme
(search-exclude "target")                       ; skip a directory or glob
(search-exclude "*.min.js")
(search-include "*.rs")                          ; whitelist: only walk matching files
(set-search-option "hidden" #t)                 ; include dotfiles
(set-search-option "respect-gitignore" #f)      ; walk ignored files too
(set-search-option "max-depth" 8)               ; cap recursion depth
```

The root resolver (`project-root`) is itself a Scheme function — `walk-up-to-marker` — so you can teach Macros about project markers beyond `.git` if your repos use something else.
