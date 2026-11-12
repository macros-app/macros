---
title: Org mode
description: Headings, TODOs, agenda, capture, tables, and babel — a full org-mode in Scheme.
---

# Org mode

Macros implements **Org mode** entirely in Scheme on the editor primitives. Opening a `.org` file activates it automatically (via `find-file-hook`), turning on fontification and the `org-mode` keymap.

It's built in five independently useful layers:

1. **Fontification + visibility cycling** — colored headings you fold and unfold.
2. **Structure editing** — TODO/tag/priority cycling, promote/demote, move subtree, insert heading, toggle checkbox.
3. **An Org AST parser** (`org-parse`) that both export and the agenda consume.
4. **Agenda + capture** across a set of `.org` files.
5. **Tables** (alignment, text charts) and **babel** (executing source blocks).

## Visibility & headings

| Key | Command | Action |
| --- | --- | --- |
| <kbd>Tab</kbd> | `org-cycle` | Cycle the heading at point: folded → children → subtree |
| <kbd>S-Tab</kbd> | `org-global-cycle` | Cycle the whole buffer: overview → contents → show-all |
| <kbd>M-Enter</kbd> | `org-insert-heading` | Insert a sibling heading |
| <kbd>M-←</kbd> / <kbd>M-→</kbd> | `org-promote` / `org-demote` | Change the heading's level (only on a heading line) |
| <kbd>M-↑</kbd> / <kbd>M-↓</kbd> | `org-move-subtree-up` / `org-move-subtree-down` | Move the subtree up / down (only on a heading line) |

Leading stars are hidden, so a heading shows only its colored title (all stars collapse from display — the buffer text is untouched). Toggle it with `M-x org-toggle-leading-stars`, or set `(set! *org-hide-leading-stars* #f)` in `init.scm`.

Headings and their body text are **virtually indented** by depth, like Emacs `org-indent-mode`: a level-N heading is indented `(N-1)` levels and its body one level further. The indentation is display-only — no spaces are written to the file, so there is nothing for the cursor to step through (there is no "indent wall"; `C-a`, backspace, etc. behave exactly as on flush text). Toggle with `M-x org-toggle-indent`, set the step (default 4) with `(set! *org-indent-per-level* 4)`, or disable with `(set! *org-indent* #f)` in `init.scm`. When the cursor is on a heading line, its stars are revealed so you can edit the level.

## TODOs, tags, priorities

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-t</kbd> | `org-todo` | Cycle the TODO state |
| <kbd>C-c C-q</kbd> | `org-set-tags` | Set tags |
| <kbd>C-c ,</kbd> | `org-set-priority` | Set priority |
| <kbd>C-c C-c</kbd> | `org-ctrl-c-ctrl-c` | Context action (toggle checkbox, align table, run block…) |

## Heading navigation

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-n</kbd> | `org-next-visible-heading` | Next heading *as displayed* — folded-away headings are skipped |
| <kbd>C-c C-p</kbd> | `org-previous-visible-heading` | Previous visible heading |

## Agenda & capture

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c a</kbd> | `org-agenda` | Open the agenda across your org files |
| <kbd>C-c t</kbd> | `org-todo-list` | List all TODOs |
| <kbd>C-c c</kbd> | `org-capture` | Capture a new note/task |

In the agenda buffer: <kbd>Enter</kbd> visits the entry, <kbd>g</kbd> refreshes, <kbd>n</kbd>/<kbd>p</kbd> move, <kbd>q</kbd> closes.

## Tables & babel

Tables are interactive, like Emacs. Inside a table:

| Key | Action |
| --- | --- |
| <kbd>Tab</kbd> / <kbd>S-Tab</kbd> | Re-align and move to the next / previous cell (a new row is created past the last cell) |
| <kbd>Enter</kbd> | Re-align and move to the first cell of the next row (creating one) |
| <kbd>M-←</kbd> / <kbd>M-→</kbd> | Move the current column left / right |
| <kbd>M-↑</kbd> / <kbd>M-↓</kbd> | Move the current row up / down |
| <kbd>M-S-→</kbd> / <kbd>M-S-←</kbd> | Insert / delete a column |
| <kbd>M-S-↓</kbd> / <kbd>M-S-↑</kbd> | Insert / delete a row |
| <kbd>C-c C-c</kbd> | Re-align the table |
| <kbd>C-c " g</kbd> | Chart the table (`org-plot/text`) |

Outside a table these keys keep their heading meanings (fold, promote/demote, move subtree). `C-c C-c` also executes the source block at point and writes its output to a `#+RESULTS:` block below it. Blocks can be `sh`/`bash`, `python`, `ruby`, `js`, or `scheme`. The Org AST parser drives tables, babel, and export, so they all see the same structure.

### Scheme blocks

A `scheme` (or `steel`) block runs inside the editor, like an `emacs-lisp` block in Emacs. Its defines stay defined, and it can call any editor function. The value of the last form is the result. A string is written as-is, and other values are printed. If the value is a [text chart](/docs/plotting), the chart is written, colors included:

```org
#+begin_src scheme
(plot-function sin -6.3 6.3 'width 40 'height 6 'shade (list 0 3.14))
#+end_src
```

Output a block prints with `display` isn't captured; only the value is.

### Charting a table

<kbd>C-c " g</kbd> (`org-plot/text`) charts the table at point as a [text chart](/docs/plotting) in a `#+RESULTS:` block below it, and running it again redraws the chart. Settings come from a `#+PLOT:` line above the table, in Emacs org-plot's syntax:

```org
#+PLOT: title:"Requests" ind:1 deps:(2 3) type:bars
| day | req | err |
|-----+-----+-----|
| mon |  10 |   1 |
| tue |  25 |   3 |
```

| Option | Meaning |
| --- | --- |
| `ind` | The x column, counting from 1 (default 1). If it holds numbers, they're the x values. Otherwise they label the bars, and line charts use the row number. |
| `deps` | The columns to plot (default: every other column that has a number) |
| `type` | `lines` (the default), `bars`, or `scatter`. Emacs's `with:points` also means scatter, and `with:boxes` / `with:histograms` mean bars. |
| `title`, `width`, `height` | Passed to the chart |

Column names come from the header row above the first `|---|` line. Bars draw one chart per column, and lines and scatter draw one series per column, with a legend.

Org repaints its colors on every edit, so it keeps the colors of charts written this session itself. After the file is reopened a chart shows uncolored until you run its block or `C-c " g` again.

## Export & previews

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-e</kbd> | `org-export-html` | Export the buffer to HTML |
| <kbd>C-c C-x C-v</kbd> | `org-toggle-inline-images` | Toggle inline image display |
| <kbd>C-c C-x C-l</kbd> | `org-toggle-latex-preview` | Toggle LaTeX fragment previews |

## Org-roam (linked notes)

A Zettelkasten layer over your org files — the Macros port of Emacs **org-roam**. Each note is an `.org` file with an `:ID:` property and a `#+title:`, and you link notes to each other by ID. Backlinks and a graph view come for free.

Notes live in `~/org-roam` by default. Point it somewhere else from `init.scm`:

```scheme
(org-roam-set-directory "~/notes")
```

### Nodes

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c n f</kbd> | `org-roam-node-find` | Find a node by title — creates it if the title is new |
| <kbd>C-c n c</kbd> | `org-roam-capture` | Create a new node, then open it |
| <kbd>C-c n i</kbd> | `org-roam-node-insert` | Insert an `[[id:…]]` link to a node (org buffers only) |
| <kbd>C-c n t</kbd> | `org-roam-tag-find` | Pick a tag, then a node carrying it |
| <kbd>C-c n e</kbd> | `org-roam-ref-find` | Pick a `ROAM_REFS` ref (URL / cite key) and visit its node |
| <kbd>C-c n s</kbd> | `org-roam-db-sync` | Re-scan the directory and rebuild the index |

`C-c n f`, `C-c n c`, `C-c n s`, `C-c n g`, `C-c n t`, and `C-c n e` are bound globally as well as in org buffers; `C-c n i` and the backlinks toggle are org-mode only.

A `#+title:` held by more than one node is offered once per node, annotated with the file (`Shared Title — c.org`), so you pick the one you mean. `ROAM_ALIASES` work per node — the file's `:ROAM_ALIASES:` for the file node, a subtree drawer's for that heading — and alias titles complete in find/insert too.

### Links & backlinks

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c C-o</kbd> | `org-open-at-point` | Follow the link at point — `id:` (node), `file:`, or `http(s)://` |
| <kbd>C-c n l</kbd> | `org-roam-buffer-toggle` | Show notes that link **to** the current node in a side buffer |

In the `*org-roam-backlinks*` buffer: <kbd>Enter</kbd> visits the entry, <kbd>g</kbd> refreshes, <kbd>n</kbd>/<kbd>p</kbd> move, <kbd>q</kbd> closes.

Backlinks come from the in-memory link index, so they list the *node* each link lives in (a link inside a subtree names that subtree), and they never grep the vault.

### Graph

<kbd>C-c n g</kbd> (`org-roam-graph`) renders the note network with Graphviz and shows it inline as an image, so it needs the `dot` command (`graphviz`) on your `PATH`. In the graph buffer, <kbd>g</kbd> re-renders and <kbd>q</kbd> closes. Tune the render from `init.scm`:

```scheme
(set! *org-roam-graph-width* 1600)  ; display width in px (default 1200)
(set! *org-roam-graph-dpi* 200)     ; Graphviz DPI (default 150)
```

## Fontification & the overlay layer

Org's heading colors paint the shared overlay-face layer (the same layer used by [rainbow delimiters](/docs/rainbow-delimiters) and LSP semantic tokens). Rainbow deliberately skips `.org` buffers so the two don't fight over that layer.
