---
title: Modal editing (evil)
description: Vim-style modal editing — motions, operators, and text objects, the way evil-mode works in Emacs.
---

# Modal editing (evil)

Macros offers modal editing in **two flavors**, both built entirely on the editor's keymaps, both opt-in, and both fully overridable from Scheme:

- **evil** (this page) — Vim-style, *verb-then-motion* (`d w` deletes a word). Enable with `(evil-mode)`.
- **[helix](/docs/helix)** — selection-first, *motion-then-verb* (`w` selects a word, `d` deletes it), with first-class multiple selections. Enable with `(helix-mode)`.

Turn on one or the other, not both. Macros uses standard Emacs bindings unless you opt in — modal editing is **not on by default**.

## evil-mode

Macros ships with **evil-mode**: Vim-style modal editing that mirrors GNU Emacs's evil-mode — plain Vim modal editing, **no leader key**.

Enable it from your `init.scm` (see [Configuration](/docs/configuration)):

```scheme
(evil-mode)     ; opt in to Vim-style modal editing
(evil-disable)  ; turn it back off
```

The Rust side only decides *when* plain keys are commands (Normal/Visual states) versus text (Insert state); every binding is Scheme and fully overridable.

## States

- **Normal** — keys are commands. This is where you start.
- **Insert** — keys insert text. Enter with <kbd>i</kbd> / <kbd>a</kbd> / <kbd>o</kbd> (and capitals); leave with <kbd>Esc</kbd>.
- **Visual** — character selection for operators. Enter with <kbd>v</kbd>.

<kbd>Esc</kbd> (`keyboard-quit`) always returns you to Normal.

## Normal-state motions

| Key | Moves |
| --- | --- |
| <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> | left / down / up / right |
| <kbd>w</kbd> <kbd>b</kbd> <kbd>e</kbd> <kbd>g e</kbd> (and <kbd>W</kbd> <kbd>B</kbd> <kbd>E</kbd> <kbd>g E</kbd>) | word / WORD motions |
| <kbd>0</kbd> <kbd>^</kbd> <kbd>$</kbd> | start / first non-blank / end of line |
| <kbd>g g</kbd> / <kbd>G</kbd> | first / last line (<kbd>5 G</kbd>: line 5) |
| <kbd>{</kbd> <kbd>}</kbd> / <kbd>(</kbd> <kbd>)</kbd> | paragraph / sentence |
| <kbd>%</kbd> | matching bracket |
| <kbd>f</kbd> <kbd>F</kbd> <kbd>t</kbd> <kbd>T</kbd> + char, <kbd>;</kbd> <kbd>,</kbd> | find on the line, repeat |
| <kbd>H</kbd> <kbd>M</kbd> <kbd>L</kbd> | top / middle / bottom of the window |
| <kbd>] m</kbd> <kbd>[ m</kbd> (<kbd>] f</kbd> <kbd>[ f</kbd>), <kbd>] M</kbd> <kbd>[ M</kbd> | next / previous function start, end (tree-sitter) |
| <kbd>] ]</kbd> <kbd>[ [</kbd>, <kbd>] [</kbd> <kbd>[ ]</kbd> | next / previous class (section) start, end |
| <kbd>] d</kbd> <kbd>[ d</kbd> | next / previous diagnostic |

Counts work everywhere: <kbd>3 w</kbd>, <kbd>5 j</kbd>, <kbd>2 ] m</kbd>.

## Entering insert state

| Key | Action |
| --- | --- |
| <kbd>i</kbd> / <kbd>I</kbd> | insert before cursor / at first non-blank |
| <kbd>a</kbd> / <kbd>A</kbd> | append after cursor / at end of line |
| <kbd>o</kbd> / <kbd>O</kbd> | open a line below / above |

In Insert state, <kbd>C-r</kbd> + a register name inserts that register.

## Operators

Operators combine with **any** motion or text object, with a count on either side — Vim's `{count}{operator}{count}{motion}`: <kbd>d 2 w</kbd>, <kbd>2 d 3 w</kbd>, <kbd>c i "</kbd>, <kbd>y a p</kbd>, <kbd>g U i w</kbd>, <kbd>&gt; i p</kbd>, <kbd>d / foo</kbd> <kbd>RET</kbd>, <kbd>d ' a</kbd>. Doubling the operator acts on lines (<kbd>d d</kbd>, <kbd>3 &gt; &gt;</kbd>, <kbd>g U U</kbd>, <kbd>g c c</kbd>), and <kbd>v</kbd> / <kbd>V</kbd> after the operator force the motion charwise / linewise (<kbd>d v j</kbd>). Motions are exclusive, inclusive, or linewise exactly as in Vim.

| Operator | Does |
| --- | --- |
| <kbd>d</kbd> <kbd>c</kbd> <kbd>y</kbd> | delete / change / yank |
| <kbd>&gt;</kbd> <kbd>&lt;</kbd> <kbd>=</kbd> | shift right / left, reindent |
| <kbd>g u</kbd> <kbd>g U</kbd> <kbd>g ~</kbd> <kbd>g ?</kbd> | lowercase / uppercase / toggle case / rot13 |
| <kbd>g q</kbd> <kbd>g w</kbd> | format to `*fill-column*` (`gw` keeps the cursor) |
| <kbd>g c</kbd> | toggle line comments |

Text objects: <kbd>i w</kbd> <kbd>a w</kbd>, quotes (<kbd>i "</kbd> <kbd>i '</kbd> <kbd>i `</kbd>), brackets (<kbd>i (</kbd> / <kbd>i b</kbd>, <kbd>i {</kbd> / <kbd>i B</kbd>, <kbd>i [</kbd>, <kbd>i &lt;</kbd>), paragraphs (<kbd>i p</kbd>), and — from the syntax tree — functions (<kbd>a f</kbd> <kbd>i f</kbd>), classes / types (<kbd>a c</kbd> <kbd>i c</kbd>), arguments / parameters (<kbd>a a</kbd> <kbd>i a</kbd>) and comments (<kbd>a /</kbd> <kbd>i /</kbd>). In Visual state the same keys select the object, and operator keys act on the selection.

Single-key edits: <kbd>x</kbd> <kbd>s</kbd> <kbd>S</kbd> <kbd>D</kbd> <kbd>C</kbd> <kbd>Y</kbd> <kbd>r</kbd> <kbd>~</kbd> <kbd>J</kbd> <kbd>g J</kbd> <kbd>p</kbd> <kbd>P</kbd>, <kbd>u</kbd> / <kbd>C-r</kbd> undo / redo. <kbd>C-a</kbd> increments the number at or after the cursor (decimal, hex, binary; with a count) and <kbd>g -</kbd> decrements; in Visual, <kbd>C-a</kbd> / <kbd>g C-a</kbd> / <kbd>g C-x</kbd> work per line. Vim's <kbd>C-x</kbd> decrement is off by default because <kbd>C-x</kbd> is the Emacs prefix — enable it with `(evil-enable-ctrl-x-decrement)`.

## Repeat with `.`

<kbd>.</kbd> repeats the last change — an operator with its motion, an Insert session (<kbd>c w</kbd> … <kbd>Esc</kbd>, <kbd>o</kbd> …, <kbd>A</kbd> …), <kbd>x</kbd>, <kbd>r</kbd>, <kbd>p</kbd>, <kbd>J</kbd>, <kbd>&gt; &gt;</kbd>, and so on. A count replaces the original one (<kbd>3 .</kbd>). Each change, including its Insert session, undoes as one step.

## Registers

<kbd>"</kbd> + a name picks the register for the next yank, delete, or put: `"a`–`"z` (`"A`–`"Z` append), `"0` (last yank), `"1`–`"9` (deleted lines), `"-` (small deletes), `"_` (black hole), `"+` / `"*` (system clipboard), and the read-only `".` (last insert), `":` (last command line), `"/` (last search). The unnamed register is the kill ring, kept in sync with the system clipboard. `:registers` lists them.

## Macros

<kbd>q a</kbd> records into register a (<kbd>q A</kbd> appends), <kbd>q</kbd> stops; <kbd>@ a</kbd> replays (<kbd>3 @ a</kbd>), <kbd>@ @</kbd> repeats the last one, <kbd>@ :</kbd> the last ex command.

## Marks and jumps

<kbd>m a</kbd> sets a mark (<kbd>m A</kbd> a file mark), <kbd>' a</kbd> jumps to its line, <kbd>` a</kbd> to the exact spot. Marks move with edits. Automatic marks: <kbd>' '</kbd> (before the last jump), `'[` `']` (last change/yank), `'<` `'>` (last Visual area — <kbd>g v</kbd> reselects it), `'.` and `'^`. `:marks` lists them.

Jumps (<kbd>G</kbd>, <kbd>/</kbd>, <kbd>n</kbd>, <kbd>%</kbd>, <kbd>{</kbd>, marks, <kbd>g d</kbd>, …) are recorded: <kbd>C-o</kbd> goes back, <kbd>C-i</kbd> (or <kbd>Tab</kbd>) forward; `:jumps` lists them.

## Windows

<kbd>C-w</kbd> then: <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> move between windows, <kbd>w</kbd> / <kbd>W</kbd> cycle, <kbd>p</kbd> previous; <kbd>s</kbd> / <kbd>v</kbd> split, <kbd>n</kbd> new buffer, <kbd>c</kbd> / <kbd>q</kbd> close, <kbd>o</kbd> only; <kbd>=</kbd> balance, <kbd>+</kbd> <kbd>-</kbd> <kbd>&lt;</kbd> <kbd>&gt;</kbd> resize, <kbd>_</kbd> <kbd>|</kbd> maximize; <kbd>H</kbd> <kbd>J</kbd> <kbd>K</kbd> <kbd>L</kbd> move the window to the far side, <kbd>x</kbd> exchange, <kbd>r</kbd> rotate. Emacs users get `windmove-left`/`-right`/`-up`/`-down` and `balance-windows` (<kbd>C-x +</kbd>).

## Folds

<kbd>z a</kbd> toggle, <kbd>z c</kbd> / <kbd>z o</kbd> close / open, <kbd>z M</kbd> / <kbd>z R</kbd> fold / open everything, <kbd>z j</kbd> / <kbd>z k</kbd> next / previous fold.

## Search

| Key | Action |
| --- | --- |
| <kbd>/</kbd> / <kbd>?</kbd> | search forward / backward |
| <kbd>n</kbd> / <kbd>N</kbd> | next / previous match |
| <kbd>*</kbd> / <kbd>#</kbd> | search the word under the cursor |

## Ex commands

Press <kbd>:</kbd> to enter an Ex command (from Visual it starts with `'<,'>`). Ranges: `%`, `.`, `$`, line numbers, `'a` marks, `/pat/` and `?pat?`, with `+n` / `-n` offsets and `,` or `;` between.

```
:%s/old/new/g          substitute (flags: g c i I n e &, and a count)
:'<,'>s/x/y/c          …asking y/n/a/q/l for each match
:&&  &  g&             repeat the last substitute
:g/pat/d   :v/pat/d    run a command on (non-)matching lines
:g/TODO/normal A!      …including Normal-state keys
:%norm A;              run keys on every line of a range
:5   :$   :.+3         go to a line
:d  :y  :m0  :t.  :co$  :j  :>  :<  :sort[!] [inu]  :put
:noh  :registers  :marks  :jumps  :new  :vnew  :wincmd h
```

## Scrolling

| Key | Action |
| --- | --- |
| <kbd>C-d</kbd> / <kbd>C-u</kbd> | half-page down / up |

## Copying from special buffers

Special buffers (magit, dired, dashboards) take every key through their own keymap, so evil's states don't apply there. To copy text out of one, press <kbd>v</kbd> (or <kbd>V</kbd> for whole lines) where the buffer doesn't bind that key itself, extend the selection with the motions (<kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd>, <kbd>w</kbd> <kbd>b</kbd> <kbd>e</kbd>, <kbd>0</kbd> <kbd>$</kbd>, <kbd>g g</kbd> <kbd>G</kbd>), then <kbd>y</kbd> to copy it or <kbd>Esc</kbd> to cancel. A mouse drag selects too, and <kbd>Cmd-C</kbd> copies a dragged selection.

## No leader key by default

Like vanilla evil-mode in Emacs, Macros' evil has **no <kbd>Space</kbd> leader bindings**. A leader menu is a Doom/Spacemacs convention, not part of evil itself, so it's left to you. Adding one is a few lines in your `init.scm`:

```scheme
;; Roll your own Spacemacs-style leader. Commands are 'name symbols (a bare
;; command identity or a string name work too).
(evil-set-leader "space")                    ; the default; set BEFORE binding
(evil-leader-set "space" 'helm-execute-command) ; SPC SPC
(evil-leader-set "f f" 'helm-find-files)
(evil-leader-set "b b" 'helm-buffers)
(evil-leader-set "n o" 'delete-other-windows)
```

Leader chords bound with `evil-leader-set` work **everywhere**, not only in file buffers:

- **Special buffers** (magit, dired, package dashboards like `k8s-dashboard`) — <kbd>SPC</kbd> reaches your leader even though these buffers drive their own keymaps.
- **Terminals and Insert state**, where <kbd>SPC</kbd> has to type a space — use the **alt leader** instead, <kbd>M-SPC</kbd> by default (Doom's `doom-leader-alt-key`): <kbd>M-SPC n o</kbd> in a terminal runs the same command as <kbd>SPC n o</kbd> in a file. Change it with `(evil-set-leader-alt "ctrl-space")`, or disable it with `(evil-set-leader-alt "")`, before your bindings.

Per-mode leader bindings shadow the global ones in that mode, including special-buffer modes:

```scheme
(evil-leader-set-for-modes '("rust" "go")
  (list (cons "g g" 'lsp-find-definition)))
(evil-leader-set-for-modes '("k8s-mode")
  (list (cons "k r" 'k8s-refresh)))
```

## Customizing

evil bindings live in the `"evil-normal"`, `"evil-insert"`, `"evil-visual"` (and `-line` / `-block`) maps, and motions / text objects for operators in `"evil-operator"`. Rebind exactly like any other key:

```scheme
(define-key "evil-normal" "shift-q" 'kill-buffer)
```

Because it's all Scheme, you can add operators, motions, and text objects of your own:

```scheme
;; An operator: bind a key to (evil-operator "name") and define what it does
;; with the range [start, end) of kind "char" or "line".
(evil-define-operator "sort-lines"
  (lambda (kind start end) (evil-ex (string-append (number->string (+ 1 (line-of-pos start))) ","
                                                    (number->string (+ 1 (line-of-pos (- end 1)))) "sort"))))
(define-key "evil-normal" "g s" "evil-operator \"sort-lines\"")

;; A motion for operators: return (list type (point) target).
(evil-define-motion "next-blank-line"
  (lambda () (list "line" (point) (paragraph-forward-pos (point)))))
(define-key "evil-operator" "g b" "evil-motion \"next-blank-line\"")

;; Tree-sitter node kinds for a language the defaults miss.
(evil-ts-set-kinds "kotlin:function" '("function_declaration"))
```

A key binding whose command contains `%c` reads one more key as its argument (that is how <kbd>m</kbd>, <kbd>"</kbd>, <kbd>@</kbd>, and <kbd>f</kbd> under an operator work). Commands registered with `(set-jump-command! 'name)` are recorded in the jump list. See [Scripting with Steel](/docs/scripting).
