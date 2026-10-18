---
title: Grid formulas
description: An Excel-compatible formula engine for grid buffers — 150 functions, point mode, completion, dependency tracking, .xlsx files, and formulas written in Scheme.
---

# Grid formulas

Type `=` in a grid cell and you're writing an Excel formula. The engine was written from scratch for Macros, but it follows Excel's rules closely enough that the formulas you already know — and the workbooks you already have — just work. It covers roughly **95% of everyday spreadsheet use**; the rest is listed under [What's not supported](#whats-not-supported).

![A grid computing revenue per order with =F2*G2*(1-H2), and a summary table using SUMIFS by region with each region's share of the total. The cell being edited shows its formula with each reference colored and the referenced columns outlined in the same colors.](/screenshots/grid-formulas.png)

## Excel compatible

- **The same syntax.** `=A1+B1`, `$A$1`, `A:A`, `3:3`, `B2:D9`, `Costs!B2`, `'Q1 Sales'!A:A`, `"text"&A1`, `{1,2;3,4}`, `%`, `^`, and Excel's operator precedence, including its quirks (`=-2^2` is `4`).
- **The same 150 functions** — math, statistics, logic, lookup, text, dates, and information — with Excel's argument order and edge cases. See [the list](#functions).
- **The same values.** Numbers are IEEE doubles shown to 15 significant digits, so `=0.1+0.2-0.3` shows `0` just like Excel. `ROUND(2.675, 2)` is `2.68`. Dates are serial numbers from 1900, `TRUE` counts as 1, `"3"+1` is `4`, and comparisons ignore case.
- **The same errors.** `#DIV/0!`, `#N/A`, `#NAME?`, `#NULL!`, `#NUM!`, `#REF!`, `#VALUE!`, and `#CALC!` behave as in Excel, and `IFERROR`, `IFNA`, and `ISERROR` catch them. A circular reference shows **`#CIRC!`** in the cells involved, rather than a dialog.
- **The same files.** Formulas are stored in Excel's English form (`=SUM(A1:A3)`), so `.xlsx` files open with their formulas intact and save back with results cached — Excel, Numbers, and Google Sheets show values the moment they open the file.

## Writing formulas

![Typing =SUMI in a cell: a dropdown lists SUMIF and SUMIFS with their argument lists, and a side panel shows the selected function's signature.](/screenshots/grid-completion.png)

- **Completion.** Start typing a function name and a dropdown lists matching functions — built-in and your own — with their arguments. <kbd>↑</kbd> <kbd>↓</kbd> choose, <kbd>Tab</kbd> or <kbd>RET</kbd> insert `NAME(`, <kbd>Esc</kbd> closes the list.
- **Signature hints.** Inside the parentheses, the echo area shows the signature with the current argument marked: `SUMIF(range, «criteria», [sum_range])`.
- **Point mode.** Right after `=`, an operator, `(`, or `,`, the arrow keys (or <kbd>h</kbd> <kbd>j</kbd> <kbd>k</kbd> <kbd>l</kbd>) insert a reference and move it around the grid; hold <kbd>Shift</kbd> to stretch it into a range. Clicking or dragging over cells does the same.
- **Colored references.** Each reference in the formula gets a color, and the cell or range it points to is outlined in that color.
- **Closing parentheses.** Commit `=SUM(B4,B5` and the missing `)` is added for you, as in Excel.
- **Literal text.** Start a cell with `'` to store text that begins with `=`: `'=not a formula`.

## Working with formulas

| Emacs | Evil | Action |
| --- | --- | --- |
| <kbd>C-`</kbd> | <kbd>g`</kbd> | Show formulas instead of results (Excel's <kbd>Ctrl+`</kbd>) |
| <kbd>C-c C-[</kbd> | <kbd>g[</kbd> | Trace precedents: outline the cells this formula reads |
| <kbd>C-c C-]</kbd> | <kbd>g]</kbd> | Trace dependents: outline the formulas that read this cell |
| <kbd>C-c C-p</kbd> | <kbd>gp</kbd> | Jump to the next error |
| <kbd>F9</kbd> | <kbd>F9</kbd> | Recalculate everything (for `NOW`, `RAND`, …) |

The formula bar shows a cell's formula; the cell shows its result. Copying, pasting, filling down (<kbd>S-RET</kbd>, <kbd>C-c d</kbd>) or right (<kbd>C-c r</kbd>), filling a selection (<kbd>C-RET</kbd>), and sorting move formulas with their relative references adjusted, and inserting or deleting rows and columns updates every reference that crosses the edit — a reference to a deleted cell becomes `#REF!`. Copied ranges reach other apps as values, and paste back into a grid as formulas.

Recalculation follows a dependency graph, so an edit only recomputes the formulas downstream of it: 100,000 formulas recalculate in about 50 ms, and an edit feeding 10,000 dependents in about 5 ms.

## Saving

- **`.xlsx`** stores formulas with their results, and the grid's formatting: bold, the palette's text and fill colors, and number formats. Files that hold something the engine can't write back — charts, other cell styles such as borders, italics or other colors, extra sheets — open **protected**: you can view and edit them, and save a copy with `:w copy.xlsx`, but the original is never overwritten and silently damaged.
- **CSV can't store formulas or formatting**, so a CSV never gets formula text written into it, and bold, colors and number formats are never silently dropped. Formulas in a CSV evaluate normally while you work; when you save a CSV that has formulas or any formatting, <kbd>C-x C-s</kbd> becomes **Save As `.xlsx`** (the prompt says which) and the buffer then visits the new workbook, formatting included. The original CSV is left untouched. Remove every formula and all formatting (<kbd>M-o d</kbd>) and the buffer saves as plain CSV again.

## Functions written in Scheme

Any Scheme function can be called from a formula:

```scheme
;; init.scm
(define-grid-function (MARGIN price cost)
  (/ (- price cost) price))

(define-grid-function (TOTAL rows)          ; a range arrives as a list of rows
  (apply + (map (lambda (r) (apply + r)) rows)))
```

`=MARGIN(G2, K2)` and `=TOTAL(F2:F400)` now work in every grid, and appear in completion. Names are case-insensitive in formulas.

- **Arguments** arrive as numbers, strings, `#t`/`#f`, `""` for an empty cell, an error as its text (`"#N/A"`), and a range as a list of rows. Return one value; a list gives `#VALUE!`.
- **They never freeze the editor.** Grid functions run in their own interpreter on a separate thread. While a call runs, its cell — and everything that depends on it — shows `#CALC!`. Results are cached per argument set, so a function filled down 10,000 rows runs once per distinct input.
- **Runaway calls are stopped.** A call that runs longer than `grid-function-timeout` — 120 seconds unless you set `(grid-function-timeout 30)` — is interrupted and shows `#CALC!`.
- **They see only their arguments**, which keeps the dependency graph exact. Helper procedures a grid function calls are shared with `define-grid-helper`:

```scheme
(define-grid-helper (money x) (string-append "$" (number->string x)))
```

From Scheme, `(grid-formula)` reads the cursor cell's formula, `(grid-current-value)` its result, `(grid-set-cell 4 "J" "=SUM(J2:J4)")` writes one, and `(grid-recalculate)` recalculates.

## Functions

| Category | Functions |
| --- | --- |
| Logic | `IF` `IFS` `IFERROR` `IFNA` `CHOOSE` `SWITCH` `AND` `OR` `XOR` `NOT` `TRUE` `FALSE` |
| Math | `SUM` `PRODUCT` `SUMPRODUCT` `ABS` `ROUND` `ROUNDUP` `ROUNDDOWN` `MROUND` `INT` `TRUNC` `MOD` `QUOTIENT` `POWER` `SQRT` `EXP` `LN` `LOG` `LOG10` `CEILING` `CEILING.MATH` `FLOOR` `FLOOR.MATH` `SIGN` `PI` `RAND` `RANDBETWEEN` `GCD` `LCM` `FACT` `EVEN` `ODD` |
| Conditional | `SUMIF` `SUMIFS` `COUNTIF` `COUNTIFS` `AVERAGEIF` `AVERAGEIFS` `MAXIFS` `MINIFS` |
| Statistics | `AVERAGE` `AVERAGEA` `COUNT` `COUNTA` `COUNTBLANK` `MIN` `MAX` `MINA` `MAXA` `MEDIAN` `MODE` `MODE.SNGL` `STDEV` `STDEV.S` `STDEV.P` `STDEVP` `VAR` `VAR.S` `VAR.P` `VARP` `LARGE` `SMALL` `RANK` `RANK.EQ` `RANK.AVG` `PERCENTILE` `PERCENTILE.INC` `QUARTILE` `QUARTILE.INC` |
| Lookup | `VLOOKUP` `HLOOKUP` `XLOOKUP` `MATCH` `INDEX` `ROW` `COLUMN` `ROWS` `COLUMNS` |
| Text | `LEN` `LEFT` `RIGHT` `MID` `UPPER` `LOWER` `PROPER` `TRIM` `CLEAN` `CONCAT` `CONCATENATE` `TEXTJOIN` `SUBSTITUTE` `REPLACE` `FIND` `SEARCH` `VALUE` `NUMBERVALUE` `TEXT` `FIXED` `DOLLAR` `REPT` `EXACT` `CHAR` `CODE` |
| Date & time | `DATE` `TIME` `TODAY` `NOW` `YEAR` `MONTH` `DAY` `HOUR` `MINUTE` `SECOND` `WEEKDAY` `WEEKNUM` `EDATE` `EOMONTH` `DATEDIF` `DAYS` `NETWORKDAYS` `WORKDAY` `DATEVALUE` `TIMEVALUE` `YEARFRAC` |
| Information | `ISBLANK` `ISNUMBER` `ISTEXT` `ISNONTEXT` `ISLOGICAL` `ISERROR` `ISERR` `ISNA` `ISEVEN` `ISODD` `NA` `ERROR.TYPE` `TYPE` `N` `T` |

## What's not supported

The remaining few percent, mostly newer or rarer Excel features:

- **Dynamic arrays and spilling** — `FILTER`, `SORT`, `UNIQUE`, `SEQUENCE`. Array constants and array-taking functions like `SUMPRODUCT` do work.
- **`INDIRECT` and `OFFSET`**, `LAMBDA` and `LET`, structured table references (`Table1[Col]`), and data tables.
- **Iterative calculation** — circular references are always `#CIRC!`.
- **Localized** function names and separators; formulas use Excel's English form, as Excel stores them.
- Small differences inside individual functions, such as `TEXT()` without fraction formats.
