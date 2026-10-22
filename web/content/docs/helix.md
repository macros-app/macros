---
title: Modal editing (helix)
description: Selection-first modal editing — motions select, verbs act, and every edit can run across many selections at once, the way Helix works.
---

# Modal editing (helix)

Macros ships a **Helix-style** modal layer alongside [evil](/docs/modal-editing). Where Vim is *verb-then-motion* (`d w` — delete a word), Helix is **selection-first**: a motion *selects* (the noun), then a verb acts on whatever is selected. `w` selects the word, `d` deletes it. And there can be **many selections at once** — every motion and verb applies to all of them.

Like evil, it's built entirely on the editor's keymaps — the Rust side only decides *when* plain keys are commands (Normal state) versus text (Insert state); every binding is Scheme and fully overridable. It is **not on by default**; Macros uses standard Emacs bindings unless you opt in.

Enable it from your `init.scm` (see [Configuration](/docs/configuration)):

```scheme
(helix-mode)     ; opt in to Helix-style modal editing
(helix-disable)  ; turn it back off
```

> Helix and [evil](/docs/modal-editing) are two different modal flavors — turn on one or the other, not both.

## States

- **Normal** — keys are commands, and motions leave a visible selection. This is where you start.
- **Insert** — keys insert text. Enter with <kbd>i</kbd> / <kbd>a</kbd> / <kbd>o</kbd> (and capitals); leave with <kbd>Esc</kbd>.

<kbd>Esc</kbd> returns you to Normal — leaving select/extend, collapsing each selection to a bare cursor at its head. (It keeps any extra cursors you added, so a multi-cursor `c` / `i` / `a` survives.)

## Motions (these select)

A motion moves the head of every selection. Word and line motions leave a selection behind; plain character moves collapse to a bare cursor.

| Key | Selects / moves |
| --- | --- |
| <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> | left / down / up / right |
| <kbd>w</kbd> / <kbd>e</kbd> / <kbd>b</kbd> | next word / end of word / previous word |
| <kbd>0</kbd> / <kbd>$</kbd> | start / end of line |
| <kbd>x</kbd> | select the whole line — repeat to grow downward |
| <kbd>g g</kbd> / <kbd>g e</kbd> | top / bottom of the file |
| <kbd>g h</kbd> / <kbd>g l</kbd> | start / end of line |

## Selections

The core idea: build the selection you want, then act on it. These manage the selection set itself.

| Key | Action |
| --- | --- |
| <kbd>v</kbd> | toggle **select (extend)** — motions then grow each selection instead of replacing it |
| <kbd>C</kbd> | add a cursor on the line below |
| <kbd>;</kbd> | collapse every selection to a bare cursor at its head |
| <kbd>,</kbd> | keep only the primary selection (drop the rest) |
| <kbd>Esc</kbd> | leave extend and collapse to a single cursor |

With several selections active, every motion and every edit below runs on all of them at once. The renderer paints the primary selection and the secondaries distinctly.

## Acting on the selection

The verb acts on whatever is selected. Right after a plain move — when a selection is just a bare cursor — these act on the single character under it, so they still do something useful.

| Key | Action |
| --- | --- |
| <kbd>d</kbd> | delete the selection(s) |
| <kbd>c</kbd> | change — delete, then enter Insert |
| <kbd>y</kbd> | yank the selection(s) to the system clipboard |
| <kbd>p</kbd> / <kbd>P</kbd> | paste after / before |
| <kbd>r</kbd> | replace each selected character (`r` then a char) |
| <kbd>u</kbd> / <kbd>U</kbd> | undo / redo |

Yanks join multiple selections with newlines and copy to the system clipboard, so <kbd>y</kbd> then <kbd>cmd-v</kbd> in another app works.

## Entering insert state

| Key | Action |
| --- | --- |
| <kbd>i</kbd> / <kbd>a</kbd> | insert at the start / end of each selection |
| <kbd>o</kbd> / <kbd>O</kbd> | open a line below / above |

With multiple cursors, text you type replicates to every one of them.

## Search and Ex

| Key | Action |
| --- | --- |
| <kbd>/</kbd> | search forward |
| <kbd>n</kbd> / <kbd>N</kbd> | next / previous match |
| <kbd>:</kbd> | Ex command — e.g. `:%s/old/new/g` |

## Scrolling

| Key | Action |
| --- | --- |
| <kbd>C-d</kbd> / <kbd>C-u</kbd> | half-page down / up |

## Special buffers

Read-only buffers (Magit, grep, compile, occur, …) get a vim/helix-style motion overlay under helix-mode, mirroring [evil's](/docs/modal-editing) collection layer: <kbd>j</kbd> / <kbd>k</kbd> everywhere, plus the full read-only motion set in plain-text result buffers.

To copy text out of one, press <kbd>v</kbd> (or <kbd>V</kbd> for whole lines) where the buffer doesn't bind that key itself, extend the selection with the motions (<kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd>, <kbd>w</kbd> <kbd>b</kbd> <kbd>e</kbd>, <kbd>0</kbd> <kbd>$</kbd>, <kbd>g g</kbd> <kbd>G</kbd>), then <kbd>y</kbd> to copy it or <kbd>Esc</kbd> to cancel. A mouse drag selects too.

## Try it

Run the in-editor Helix tutorial with <kbd>C-h t</kbd> and pick **[helix]** (or follow the `[helix]` link on the welcome screen).

## Customizing

Helix bindings live in the `"helix-normal"` and `"helix-insert"` maps. Rebind exactly like any other key:

```scheme
(define-key "helix-normal" "shift-q" 'kill-buffer)
```

Because it's all Scheme, you can add your own motions, verbs, and multi-selection commands. See [Scripting with Steel](/docs/scripting).
