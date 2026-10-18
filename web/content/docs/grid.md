---
title: Grid (CSV & spreadsheets)
description: Open CSV and TSV files as a spreadsheet grid, flip between the grid and the raw text, edit cells with Emacs, evil, or helix keys, and save byte-exact.
---

# Grid: CSV & spreadsheets

Open a `.csv`, `.tsv`, `.tab`, `.psv`, or `.xlsx` file — with <kbd>C-x C-f</kbd> or by dragging it onto a window — and it opens as a **grid**: a spreadsheet with lettered columns, numbered rows, a formula bar, and Excel's default 64 × 20 px cells. The same file is one keystroke away from its raw text.

![A CSV of sales orders open as a grid: auto-fitted columns, prices formatted as currency, discounts as percentages, pending and late statuses colored, and a selected range with its count, sum, average, min and max in the mode line.](/screenshots/grid-overview.png)

- **Your keys.** The grid uses the bindings you already know from the rest of the editor (Emacs, evil, or helix), plus your own rebinds, so you don't have to learn a spreadsheet's shortcuts.
- **Infinite.** Scroll or move past the last row and column; the grid keeps going, and empty space you visit never reaches the file.
- **Byte-exact saves.** Rows you didn't touch are written back exactly as they were — quoting, line endings, encoding, BOM. Values are stored exactly as typed: `007` stays `007`, `12345678901234567890` never becomes `1.23E+19`.
- **Fast on big files.** Large files show their first rows at once and finish loading in the background; repeated values are stored once.
- **Formulas.** Type `=SUM(B2:B9)` in a cell. See [Grid formulas](/docs/grid-formulas).

## Grid or text: one keystroke

A CSV is text, and sometimes you want it as text. <kbd>C-c C-t</kbd> (evil: <kbd>g r</kbd>, or <kbd>M-x grid-toggle-view</kbd>) switches the *same buffer* between the grid and the raw file, keeping the file and any unsaved edits. Press it again to go back.

![The same orders.csv shown as raw comma-separated text in a normal text buffer.](/screenshots/grid-raw.png)

The delimiter, quoting, encoding (UTF-8, UTF-16, Windows-1252), and header row are detected when the file opens. `:w orders.tsv` saves a copy with tabs, `:w orders.csv` with commas. Run <kbd>M-x grid-mode</kbd> to view any text buffer as a grid.

## Workbooks with several sheets

An `.xlsx` opens on its first sheet. <kbd>C-c t</kbd> (evil: <kbd>g t</kbd>, or <kbd>M-x helm-grid-sheets</kbd>) lists the workbook's sheets in Helm, with the one you're on marked *current*; pick one to open it. Each other sheet opens as a buffer of its own, named like `report.xlsx<Costs>`, so you can switch between sheets with your usual buffer keys. A sheet that's already open is reused, and picking the first sheet returns to the file's own buffer.

Because saving would drop the other sheets, a workbook with several sheets opens protected (quietly — saving over it is what tells you why): you can edit any sheet and save it as a workbook of its own with `:w costs.xlsx`, but the original file is never overwritten. Formulas can read other sheets, as in Excel: `=Costs!B2`, `=SUM('Q1 Sales'!C:C)`. Until a sheet is open, its values are the ones last saved in the file; once it's open in a buffer, edits there update the formulas reading it right away, saved or not.

From Scheme, `(grid-sheets)` lists the sheet names, `(grid-current-sheet)` names the one shown, and `(grid-open-sheet "Costs")` opens one.

## Moving around

| Emacs | Evil / Helix | Action |
| --- | --- | --- |
| <kbd>C-f</kbd> <kbd>C-b</kbd> <kbd>C-n</kbd> <kbd>C-p</kbd> | <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> | One cell (arrows work too) |
| <kbd>Tab</kbd> / <kbd>S-Tab</kbd> | <kbd>w</kbd> / <kbd>b</kbd> | Next / previous cell, wrapping across rows |
| <kbd>M-f</kbd> <kbd>M-b</kbd> <kbd>M-n</kbd> <kbd>M-p</kbd> | <kbd>W</kbd> <kbd>B</kbd> <kbd>}</kbd> <kbd>{</kbd> | Jump to the edge of the data (Excel's Ctrl+Arrow) |
| <kbd>C-a</kbd> / <kbd>C-e</kbd> | <kbd>0</kbd> <kbd>^</kbd> <kbd>$</kbd> | Start of row / first value / last value |
| <kbd>M-&lt;</kbd> / <kbd>M-&gt;</kbd> | <kbd>gg</kbd> / <kbd>G</kbd> | First / last row |
| <kbd>C-v</kbd> / <kbd>M-v</kbd> | <kbd>C-d</kbd> <kbd>C-u</kbd> <kbd>C-f</kbd> <kbd>C-b</kbd> | Page |
| <kbd>C-x g</kbd> | <kbd>gn</kbd>, `:C7`, `:B2:D9`, `:12` | Go to an address, select a range, go to a row |
| <kbd>C-s</kbd> / <kbd>C-r</kbd> | <kbd>/</kbd> <kbd>n</kbd> <kbd>N</kbd> | Search cell contents |
| <kbd>C-c j</kbd> | <kbd>g\|</kbd> | Jump to a column by header name |

The **name box** at the left of the formula bar shows the cell (`C7 · email`, with the header name when the file has one); click it to type an address.

## Selecting

| Emacs | Evil | Action |
| --- | --- | --- |
| <kbd>S-arrows</kbd>, <kbd>C-SPC</kbd> | <kbd>v</kbd> | Select a range of cells |
| | <kbd>V</kbd> / <kbd>C-v</kbd> | Whole rows / whole columns |
| <kbd>C-x h</kbd> | <kbd>%</kbd> (helix) | Everything |
| drag, click a header, <kbd>S-click</kbd> | | The mouse works as in a spreadsheet |

Moving up from the first row selects the cursor's whole column, and moving left from the first column selects its whole row — as if the cursor stepped onto the header. With a column selected, left and right add the neighbouring columns (moving back toward where you started gives them back); with a row selected, up and down add rows. Down from a selected column, or right from a selected row, steps back off the header and drops the selection. This works with every motion key: arrows, <kbd>C-p</kbd> / <kbd>C-b</kbd>, and <kbd>k</kbd> / <kbd>h</kbd>.

With a range selected, the mode line shows `24 cells · Sum 1,204 · Avg 50.2 · Min 3 · Max 249`.

## Editing cells

| Emacs | Evil | Action |
| --- | --- | --- |
| just type, <kbd>RET</kbd>, <kbd>F2</kbd> | <kbd>i</kbd> <kbd>a</kbd> <kbd>RET</kbd> | Edit the cell (typing replaces it in Emacs mode) |
| <kbd>Backspace</kbd> | <kbd>c</kbd> <kbd>s</kbd> <kbd>cc</kbd> | Clear the cell and type |
| <kbd>C-d</kbd> / <kbd>Delete</kbd> | <kbd>x</kbd> | Clear the selection |
| <kbd>C-c '</kbd> | <kbd>ge</kbd> | Edit the cell in a full buffer (<kbd>C-c C-c</kbd> writes it back) |

While editing a cell: <kbd>Esc</kbd> saves the edit, <kbd>C-g</kbd> cancels it, <kbd>RET</kbd> / <kbd>Tab</kbd> save and move down / right (<kbd>S-RET</kbd> / <kbd>S-Tab</kbd> go back), <kbd>M-RET</kbd> inserts a newline, <kbd>C-RET</kbd> with a range selected writes the entry into every selected cell (Excel's <kbd>Ctrl</kbd>+<kbd>Enter</kbd>; see [Filling ranges](#filling-ranges)), and the usual line-editing keys (<kbd>C-a</kbd> <kbd>C-e</kbd> <kbd>M-f</kbd> <kbd>M-b</kbd> <kbd>C-w</kbd> <kbd>C-k</kbd>) work.

### Multiple cursors (helix)

Under helix, a grid has helix's multiple selections, one cell each:

| Helix | Action |
| --- | --- |
| <kbd>C</kbd> / <kbd>M-C</kbd> | Add a cursor on the cell below / above each cursor (the new one becomes the primary) |
| <kbd>s</kbd> | A cursor on every selected cell matching a regex (the whole sheet when one cell is selected) |
| <kbd>,</kbd> / <kbd>Esc</kbd> | Keep only the primary cursor |
| <kbd>M-,</kbd> | Drop the primary cursor |
| <kbd>(</kbd> / <kbd>)</kbd> | Make the previous / next cursor the primary |

The other cursors are drawn more faintly than the primary, and <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd> move them all. <kbd>c</kbd> edits every cursor's cell at once: what you type shows in each cell as you type it and is saved into all of them with <kbd>Esc</kbd>. <kbd>i</kbd> and <kbd>a</kbd> add what you type before or after each cell's own text, and <kbd>d</kbd> clears every cursor's cell. A formula typed this way moves with each cell, the way copy-down moves it: `=A4*2` in row 4 becomes `=A2*2` in row 2. Each of these edits undoes in one step.

## Find & replace

| Emacs | Evil / Helix | Action |
| --- | --- | --- |
| <kbd>M-%</kbd> | <kbd>g%</kbd>, `:s/old/new/c` | Query replace: ask at each cell |
| <kbd>C-M-%</kbd> | <kbd>g&amp;</kbd> | Query replace a regexp |
| `M-x grid-replace` / `grid-replace-regexp` | `:s/old/new/g`, `:%s/old/new/g` | Replace every match without asking |

Replacing works on the selection, or the whole sheet when nothing is selected (under evil, `:s` is the selection or the cursor cell and `:%s` the whole sheet). Rows hidden by a filter are skipped. At each cell the prompt shows the cell's text before and after: <kbd>y</kbd> / <kbd>SPC</kbd> replaces, <kbd>n</kbd> / <kbd>DEL</kbd> skips, <kbd>!</kbd> replaces all the rest, <kbd>.</kbd> replaces this one and stops, and <kbd>q</kbd> / <kbd>RET</kbd> stops. A whole replace, a query session included, undoes in one step.

- **Formulas are matched by their formula text**, like Excel's Replace with *Look in: Formulas*: replacing `A2` with `A3` turns `=A2*2` into `=A3*2`, and a value a formula shows is never matched.
- **A cell is the unit**: <kbd>y</kbd> replaces every match in the cell (`:s` without `g`, only the first).
- **Case and syntax follow Emacs**: a search with no capitals matches any case, and the replacement copies each match's case (`paris` → `lyon` makes `Paris` → `Lyon`). In a regexp's replacement `\1`…`\9` insert groups and `\&` the whole match. `:s` uses Vim's syntax instead (`\1`, `&`, the `i` and `I` flags, `\c`).
- An empty search reuses the previous pair.

## Rows, columns, and structure

| Emacs | Evil | Action |
| --- | --- | --- |
| <kbd>C-c C-o</kbd> / <kbd>C-c M-o</kbd> | <kbd>o</kbd> / <kbd>O</kbd> | Insert a row below / above |
| <kbd>C-c C-c</kbd> | <kbd>go</kbd> / <kbd>gO</kbd> | Insert a column right / left |
| <kbd>C-c C-k</kbd> / <kbd>C-c M-k</kbd> | <kbd>dd</kbd> / <kbd>dc</kbd> | Delete row / column |
| <kbd>C-c C-d</kbd> | <kbd>du</kbd> (helix: <kbd>gx</kbd>) | [Remove duplicate rows](#removing-duplicates) |
| <kbd>M-↑</kbd> <kbd>M-↓</kbd> <kbd>M-←</kbd> <kbd>M-→</kbd> | <kbd>M-k</kbd> <kbd>M-j</kbd> <kbd>M-h</kbd> <kbd>M-l</kbd> | Move a row / column (org-table style) |
| <kbd>C-c C-s</kbd> / <kbd>C-c C-r</kbd> | <kbd>S</kbd> / <kbd>gS</kbd> | Sort by the column, ascending / descending |
| <kbd>C-c ^</kbd> | <kbd>gs</kbd> | Sort, choosing alpha, numeric, or time |
| <kbd>S-RET</kbd> | <kbd>S-RET</kbd> | Copy down, continuing a series (`1, 2, …`, dates, `Q1, Q2, …`) |
| <kbd>C-c d</kbd> / <kbd>C-c r</kbd> | <kbd>M-d</kbd> / <kbd>M-r</kbd> | Fill the selection down / right from its first row / column |
| <kbd>C-RET</kbd> | <kbd>C-RET</kbd> | Fill the selection with the cursor cell |
| <kbd>C-w</kbd> <kbd>M-w</kbd> <kbd>C-y</kbd> <kbd>M-y</kbd> | <kbd>y</kbd> <kbd>p</kbd> | Cut / copy / paste through the kill ring |
| <kbd>C-/</kbd> / <kbd>C-?</kbd> | <kbd>u</kbd> / <kbd>C-r</kbd>, <kbd>.</kbd> | Undo / redo (every command is one step), repeat |

### Filling ranges

`grid-fill-down` (<kbd>C-c d</kbd>, evil and helix <kbd>M-d</kbd>) is Excel's <kbd>Ctrl</kbd>+<kbd>D</kbd>: it copies the selection's first row into every other row of it, and `grid-fill-right` (<kbd>C-c r</kbd>, <kbd>M-r</kbd>) copies its first column across, like <kbd>Ctrl</kbd>+<kbd>R</kbd>. Values are copied as they are — <kbd>S-RET</kbd> is the one that continues a series — along with the cells' formatting, and formulas move their relative references the way a paste does: `=B2*$C$1` filled down becomes `=B3*$C$1`, `=B4*$C$1`, … With only one row selected, or nothing selected, the cells copy from the row above (or the column to the left).

To put one entry in many cells, select them, type the value or formula, and press <kbd>C-RET</kbd> instead of <kbd>RET</kbd> (Excel's <kbd>Ctrl</kbd>+<kbd>Enter</kbd>): every selected cell gets it, a formula adjusted for each cell. Outside the cell editor, <kbd>C-RET</kbd> (`grid-fill-selection`) does the same with the cursor cell. On a single cell, <kbd>C-RET</kbd> while editing still inserts a line break.

All three skip rows hidden by a filter (and hidden columns), never write over the header row, and undo as one step.

Copies go to the system clipboard as tab-separated values, so pasting into Excel, Numbers, or Google Sheets — or from them — just works. Copy a range out as a table with <kbd>C-c C-e o</kbd> (org), <kbd>C-c C-e m</kbd> (Markdown), or <kbd>C-c C-e j</kbd> (JSON).

## Removing duplicates

<kbd>C-c C-d</kbd> (evil: <kbd>d u</kbd>, helix: <kbd>g x</kbd>, or <kbd>M-x grid-remove-duplicates</kbd>, also in the <kbd>C-c ?</kbd> menu) is Excel's **Data › Remove Duplicates**: it deletes every row that repeats an earlier row, keeping the first, and reports `Removed 12 duplicate rows, 340 remain`.

It asks which columns to compare, offering the header names (or the column letters when there's no header row). Type one, or several separated by commas — `email` or `name, city` — or press <kbd>RET</kbd> on an empty prompt to compare every column. Two rows are duplicates when they match in all the columns you name; the other columns don't matter, and the whole row goes.

- **Which rows.** With several rows selected, only those rows are checked, and an empty prompt compares just the selected columns; otherwise the whole sheet is. The header row is never removed.
- **Filtered rows.** Rows a filter hides take no part: they are never removed and don't count as the first occurrence, so nothing you can't see is deleted. Widen first (<kbd>C-x n w</kbd>) to check every row.
- **How values compare** — as Excel does, on what each cell shows:
  - text ignores case: `Ann` and `ANN` are duplicates;
  - numbers compare by value: `1`, `1.0` and `1e0` are the same, `1` and `one` are not;
  - spaces count: `Ann ` (with a trailing space) differs from `Ann` — run <kbd>M-x grid-trim-cells</kbd> on the column first to ignore them;
  - a formula compares by its result, so `=1+1` duplicates a typed `2`;
  - empty cells match each other; dates, booleans and errors compare as their text.

The `duplicate` highlight rule (`(grid-highlight-rule "B" "duplicate" "" 'warning)`) compares the same way, so you can see what would go before removing it.

Removing is one undo step. Formatting moves with the rows that stay, and formulas update as for a row delete: a reference to a removed row becomes `#REF!`, and a range shrinks. It is fast on large files — a million rows take a second or so. From Scheme, name the columns by header, letter, or index: `(grid-remove-duplicates '("email"))`, `(grid-remove-duplicates '("A" "C"))`, or `(grid-remove-duplicates '())` for the default columns.

## Viewing

| Emacs | Evil | Action |
| --- | --- | --- |
| <kbd>C-c ></kbd> <kbd>C-c <</kbd> <kbd>C-c =</kbd> | <kbd>&gt;</kbd> <kbd>&lt;</kbd> <kbd>=</kbd> | Widen / narrow / auto-fit the column |
| drag a header border, double-click it | | Resize, or auto-fit |
| <kbd>C-=</kbd> <kbd>C--</kbd> <kbd>C-0</kbd> | | Zoom in / out / reset |
| <kbd>C-c C-z</kbd> | <kbd>gf</kbd> | Freeze the rows above the cursor (or row 1) |
| <kbd>C-c M-z</kbd> | <kbd>gF</kbd> | Freeze panes: the rows above and the columns left of the cursor |
| <kbd>C-c z</kbd> | <kbd>zf</kbd> | Freeze the first column |
| <kbd>C-c C-h</kbd> | <kbd>gh</kbd> | Treat row 1 as a header (or not) |
| <kbd>C-x n f</kbd> / <kbd>C-x n v</kbd> / <kbd>C-x n w</kbd> | <kbd>g/</kbd> / <kbd>g*</kbd> / <kbd>zw</kbd> | Filter rows / by this value / show all |
| <kbd>C-c C-x h</kbd> / <kbd>C-c C-x s</kbd> | <kbd>zc</kbd> / <kbd>zo</kbd> | Hide a column / show hidden ones |
| <kbd>C-c ?</kbd> | <kbd>?</kbd> | A menu of everything |

Frozen rows and columns stay put while the rest scrolls under them; each freeze key pressed again unfreezes. From Scheme, `(grid-freeze-rows n)` and `(grid-freeze-columns n)` freeze exactly `n` (0 unfreezes).

Column widths, zoom, frozen rows and columns, and the header choice are remembered per file, outside the CSV.

## Formatting cells

Format cells as you move around, the way you would with Excel's bold, font-color, fill and number-format buttons. The keys use Emacs's facemenu prefix, <kbd>M-o</kbd>, in every editing style; under evil and helix the same keys also work after <kbd>f</kbd> (<kbd>f b</kbd>, <kbd>f $</kbd>, …):

| Keys | Command | Action |
| --- | --- | --- |
| <kbd>M-o b</kbd> (evil: also <kbd>g b</kbd>) | `grid-toggle-bold` | Bold on / off |
| <kbd>M-o c</kbd> | `grid-cycle-text-color` | Next text color |
| <kbd>M-o f</kbd> | `grid-cycle-fill-color` | Next fill color |
| <kbd>M-o n</kbd> | `grid-number-format` | Pick a number format, or type a format code |
| <kbd>M-o ~</kbd> | `grid-format-general` | General (no number format) |
| <kbd>M-o !</kbd> | `grid-format-number` | Number: `1,234.50` |
| <kbd>M-o $</kbd> | `grid-format-currency` | Currency: `$1,234.50` |
| <kbd>M-o %</kbd> | `grid-format-percent` | Percent: `12.50%` |
| <kbd>M-o #</kbd> | `grid-format-date` | Date: `2024-03-01` |
| <kbd>M-o @</kbd> | `grid-format-text` | Text: shown exactly as typed |
| <kbd>M-o .</kbd> / <kbd>M-o ,</kbd> | `grid-increase-decimals` / `grid-decrease-decimals` | One decimal place more / fewer |
| <kbd>M-o d</kbd> | `grid-clear-format` | Back to plain |

The number-format keys are Excel's <kbd>Ctrl</kbd>+<kbd>Shift</kbd> ones. A number format only changes how a cell looks: the value you typed is kept exactly, and the formula bar shows it. <kbd>M-o n</kbd> offers the presets above and takes any Excel format code too — `#,##0.0`, `0.000 "kg"`, `d mmm yyyy`, `[Red]-0.00;0.00`. A date format shows a number as an Excel date serial and reads text such as `3/1/2024` as a date. Numbers and dates under a format align right; text stays left.

To put them on other keys, bind the commands in the grid keymaps — for example, on an evil leader in grids only:

```scheme
(evil-leader-set-for-modes '("grid-mode")
  (list (cons "g g" 'grid-toggle-bold)
        (cons "g p" 'grid-cycle-text-color)
        (cons "g k" 'grid-clear-format)))
```

The color keys step through red, orange, yellow, green, blue, purple, and back to none, and <kbd>.</kbd> repeats the last one in evil. With a range selected, every cell gets the same result, decided by the cell the selection started on. The commands are also in the <kbd>C-c ?</kbd> menu, under Format.

Formatting moves with its cells when you sort, move rows and columns, or insert and delete them, and each change is one undo step. CSV can't store formatting, so saving a formatted CSV grid becomes a **Save As `.xlsx`**, which keeps it; an `.xlsx` saves it in place and opens with it again. From Scheme: `(grid-set-cell-bold row col #t)`, `(grid-set-cell-text-color row col 'red)`, `(grid-set-cell-fill-color row col 'yellow)` (`#f` removes a color), `(grid-set-cell-number-format row col 'currency)` (a preset or a format code; `'general` removes it), `(grid-set-number-format 'percent)` for the selection, and `(grid-clear-cell-formats)`. A theme can change any palette color by setting the face `grid-red`, `grid-blue`, and so on.

## Styling from Scheme

Styling only changes how cells look — the file is never touched. Put it in a `grid-load-hook` to apply it to files by name:

```scheme
(define (my-sales-grid name)
  (when (string-contains? name "orders")
    (grid-format-column "G" "$#,##0.00")          ; also "0.0%", "#,##0", "0.000 kg"
    (grid-format-column "H" "0%")
    (grid-highlight-rule "I" "=" "late" 'diagnostic-error)
    (grid-highlight-rule "H" ">=" 0.15 'diagnostic-warning)
    (grid-stripe-rows #t)
    (grid-freeze-rows 1)))
(add-hook "grid-load-hook" "my-sales-grid")
```

Rules take `>` `>=` `<` `<=` `=` `!=` `contains` `matches` (regex) `empty` `not-empty` `between` `duplicate` `top` `bottom`; `grid-row-highlight-rule` colors whole rows, and `grid-set-cell-face` a single cell.

## Scripting

Everything above is a Scheme command (`grid-left`, `grid-sort-ascending`, …) bound in `scheme/grid.scm`, so every key can be rebound with `define-key` on `"grid-mode"`, `"evil-normal:grid-mode"`, or `"helix-normal:grid-mode"`. The grid is also readable and writable from Scheme:

```scheme
(grid-cursor)                    ; => (6 2)
(grid-cell 6 "C")                ; any cell, synchronously
(grid-set-cells (list (list 1 "B" "42") (list 2 "B" "=B2*2")))   ; one undo step
(grid-read-range "A1:C500" 'my-callback)                         ; bulk data
(grid-sort "units" #t)
(grid-filter-rows "region" "=" "West")
(grid-remove-duplicates '("email"))
(grid-replace-all "N/A" "" 'literal 'sheet)                      ; => cells changed
(grid-replace-all "(\\d+)kg" "\\1 kg" 'regexp 'auto)              ; 'auto: the selection, else the sheet
```

Hooks: `grid-load-hook`, `grid-cell-changed-hook` (the cells the user edited), `grid-selection-changed-hook`, and `grid-mode-hook`. The full API is documented at the top of `scheme/grid.scm`.

## Saving

<kbd>C-x C-s</kbd> and `:w` save as usual; an edit still being typed is saved too. Only the records you changed are rewritten. A grid that holds formulas or formatting can't be saved as CSV — CSV can't store either — so the save becomes a **Save As `.xlsx`**; see [Grid formulas](/docs/grid-formulas#saving).
