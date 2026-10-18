# Plan: Formula engine for grid buffers

Spreadsheet formulas for grid buffers (see [grid-buffer.md](grid-buffer.md)):
`=SUM(B2:B9)` in a cell shows its result, recalculates when its inputs change,
and survives saving. The **syntax is Excel's**, so `.xlsx` files, muscle memory,
and the world's documentation all carry over. The engine is **Emacs-like
through extensibility**: functions written in Scheme are callable from
formulas.

**Hard constraint: written from scratch.** Everything under `src/formula/`
uses only `std`. That covers the lexer, parser, evaluator, dependency graph,
recalculation, function library, criteria and wildcard matching, date
arithmetic, and number formatting. No formula, parsing, regex, date, or
decimal crates are used, including ones already in the dependency tree. The
engine may use the project's own modules (the grid `Workbook`, the Scheme
engine bridge).

## Goals

- **Excel formula language:**
  - `A1` references, `$` anchors, ranges, whole columns and rows, and
    `Sheet!A1`.
  - Excel's operators, precedence, coercion rules and error values.
- **Function library:** about 80 of the most-used functions in v1. Unknown
  functions evaluate to `#NAME?` but keep their text, so nothing is lost.
- **Recalculation:** incremental. Editing a cell recalculates only what
  depends on it, in dependency order. Cycles are detected and reported, never
  hang.
- **Scheme functions:**
  `(define-grid-function (MARGIN price cost) …)` makes `=MARGIN(E2,F2)` work
  in any grid.
- **Editing experience:** the formula bar shows the formula and the cell
  shows its result. References in the formula are coloured. Point mode,
  function-name completion and signature hints help while typing.
- **Speed targets:**
  - Editing one input of a 100k-formula sheet updates the visible results
    within a frame (under 16 ms) when fewer than about 10k formulas depend
    on it.
  - `=SUM(A:A)` over 1M rows takes under 20 ms.
  - A full recalculation of 100k simple formulas takes under 250 ms.

## Non-goals (v1)

- ~~**Dynamic arrays and spilling**~~ — added since v1: `FILTER`, `SORT`,
  `SORTBY`, `UNIQUE`, `SEQUENCE`, spilling, `#SPILL!`, `A1#` and `@` (see
  the *Spilling* notes in `src/formula/engine.rs` and
  `src/formula/implicit.rs` for old files). Functions of one value still
  don't spill element-wise (`ABS(A1:A3)` is one value).
- **Dynamic references:** `INDIRECT` and `OFFSET`. Volatile functions with
  fixed references (`NOW`, `TODAY`, `RAND`) are in scope.
- **Other gaps:** iterative calculation, localized function names and
  separators (en-US only), R1C1 entry mode, structured table references
  (`Table1[Col]`), data tables, and `LAMBDA`/`LET`. `LET` is a good v2
  candidate.

---

## Decisions

| Question | Decision |
|---|---|
| Syntax | Excel's, exactly. The formula text shown is what Excel would show |
| Extensibility | Scheme functions (`define-grid-function`), batched across the engine thread |
| Implementation | From scratch, `std` only, in `src/formula/` |
| Reference storage | Relative offsets internally (R1C1-style), so copying, sorting and moving cells keeps references correct; A1 text is regenerated for display |
| Cell content | A formula cell's stored value is its source text (`=SUM(B2:B9)`); its *result* lives in the formula table. Undo, copy/paste and the raw view work unchanged |
| Errors | Excel's error values plus an explicit `#CIRC!` for circular references. Excel instead shows 0 with a warning; an explicit error is clearer. `.xlsx` export keeps the formula and writes 0 as its cached value, since `#CIRC!` isn't an Excel error code; Excel recalculates and shows its own warning |
| CSV files | **CSV never stores formulas.** Adding a formula to a CSV buffer makes saving it require *Save As* `.xlsx` (§7). The same now applies to hand-set formatting (bold, colors, number formats), which `.xlsx` stores |
| Scheme-function timeout | One global option, `grid-function-timeout`, default **120 s** |
| Locale | Stored and parsed in Excel's English (en-US) form, exactly as Excel stores it. Typing is en-US only in v1 (§1, Locale) |

---

## Architecture

| Piece | Where |
|---|---|
| Tokens and lexer | `src/formula/lex.rs` |
| AST, parser and printer (AST → A1 text) | `src/formula/{ast,parse,print}.rs` |
| References and adjustment (insert/delete/move/sort) | `src/formula/refs.rs` |
| Values, coercion and errors | `src/formula/value.rs` |
| Evaluator | `src/formula/eval.rs` |
| Dependency graph and recalculation scheduler | `src/formula/deps.rs`, `src/formula/recalc.rs` |
| Function library | `src/formula/func/{math,stat,logic,lookup,text,date,info}.rs` |
| Criteria (`">=10"`, wildcards) | `src/formula/criteria.rs` |
| Dates (serials, calendar math, parsing) | `src/formula/date.rs` |
| Number to text (General format) | `src/formula/num.rs` |
| Workbook integration | `src/grid/model.rs` (`FormulaTable` becomes real) and `src/formula/mod.rs` (`Engine`) |
| Editor integration | `src/editor/grid.rs`, `grid_element.rs` (display, editing, point mode) |
| Scheme API | `src/script/registry.rs`, `scheme/grid.scm` |

The engine depends only on the grid model's read API: `cell`,
`for_each_in_column` and dimensions. It doesn't depend on the editor, so it's
testable on its own.

---

## 1. Language

### Grammar

The grammar below is EBNF. It is parsed by hand: a recursive-descent parser
with precedence climbing for the operators.

```
formula    = "=" expr
expr       = comparison
comparison = concat { ("=" | "<>" | "<" | ">" | "<=" | ">=") concat }
concat     = additive { "&" additive }
additive   = term { ("+" | "-") term }
term       = power { ("*" | "/") power }
power      = unary { "^" unary }               ; left-assoc, as Excel
unary      = { "-" | "+" } percent             ; binds tighter than ^ in Excel: -2^2 = 4
percent    = range { "%" }
range      = primary [ ":" primary ]           ; A1:B5, A:A, 1:1, Sheet!A1:B2
primary    = number | string | bool | error | array
           | reference | call | name | "(" expr ")"
call       = ident "(" [ arg { "," arg } ] ")" ; empty args allowed: IF(A1,,1)
array      = "{" row { ";" row } "}"           ; row = const { "," const }
reference  = [ sheet "!" ] cell | col ":" col | row ":" row
sheet      = ident | "'" { char | "''" } "'"
cell       = [ "$" ] letters [ "$" ] digits
```

Excel quirks reproduced on purpose, each covered by tests:

- **Unary minus beats `^`:** `=-2^2` is `4`.
- **`^` is left-associative:** `=2^3^2` is `64`.
- **`%` is postfix:** `=50%` is `0.5`.
- **Comparisons use one precedence level, left to right:** `=1<2<3`
  compares `TRUE` with `3`, and booleans sort above numbers, so it's `FALSE`.

### Locale

Formulas are always stored, parsed and saved in en-US form: English
function names, `,` between arguments, and `.` as the decimal point. This is
the form Excel writes into every `.xlsx`, whatever the user's region. v1
also types and displays them this way. A translated typing and display layer
is a possible later option and wouldn't change files (see Open questions).

### References

- **Internal form:** each axis of a reference is stored as
  `Rel(offset)` or `Abs(index)`, relative to the formula's own cell. So
  `=A1+B1` in C1 is stored as `(row +0, col −2) + (row +0, col −1)`.
- **Copying, filling down and sorting** move the formula cell and keep the
  stored form. Relative references follow automatically, with no rewriting.
- **Inserting or deleting rows and columns** runs a single adjustment pass
  over every formula (`refs.rs`). References that point across the edit
  shift. A reference whose target was deleted becomes `#REF!`. A range that
  loses one end shrinks, as in Excel.
- **Whole columns and rows** (`A:A`, `3:3`) are ranges with an open end.
  They are bounded by the used extent when evaluated, so they never walk
  into the infinite area.
- **Printing** regenerates the A1 text from the AST, so the formula bar
  always shows text that reflects the current position and structure.

### Values and coercion

```rust
enum Value { Number(f64), Text(Rc<str>), Bool(bool), Error(Err), Empty }
enum Err { Div0, NA, Name, Null, Num, Ref, Value, Circ, Calc /* Scheme fn pending/failed */ }
```

Evaluation produces `Value` or a range view that iterates lazily.
Excel's coercion rules, implemented in `value.rs`:

- **Arithmetic:**
  - Text that parses as a number counts as that number (`="3"+1` is `4`).
  - Other text gives `#VALUE!`.
  - Empty counts as 0. `TRUE` and `FALSE` count as 1 and 0.
- **`&`:** numbers become text in General format (`num.rs`), and booleans
  become `TRUE`/`FALSE`.
- **Comparison:**
  - Values of different types order as number < text < boolean.
  - Text compares without regard to case.
  - Empty equals both 0 and `""`.
- **Aggregates** (`SUM`, `AVERAGE`, …) skip text and booleans inside ranges,
  but coerce them when passed directly as arguments, as Excel does.
- **Errors propagate** through every operator and most functions. `IFERROR`,
  `ISERROR` and similar functions catch them.

### Number semantics

- **Storage:** IEEE `f64`, as Excel uses.
- **Display:** at most 15 significant digits, as Excel shows. This makes
  `=0.1+0.2-0.3` display `0` while storing 5.55e-17, like Excel.
- **General format** is implemented from scratch in `num.rs`: scientific
  notation past 11 characters, and trailing zeros trimmed.
- **`ROUND`** rounds half away from zero. It works on the decimal
  representation, so `ROUND(2.675, 2)` gives `2.68`, as in Excel.

---

## 2. Storage and the workbook

- **`FormulaTable`:** today a placeholder `HashMap<(sheet,row,col), u32>`.
  It becomes a map from position to `FormulaId` into a `Formulas` arena of
  `Formula { ast, result: Value, flags }`.
- **Sparse:** cells that aren't formulas cost nothing extra.
- **Writing `=…` into a cell** parses it and registers the formula. A parse
  error still stores the text and shows `#NAME?` or `#ERROR!`. The formula
  bar explains the problem, and the text is never lost.
- **Editing a formula cell to a plain value** unregisters the formula.
- **Displayed value:** a new model read, `display(sheet, r, c)`. It returns
  the formatted result for formula cells and the stored text for everything
  else. Grid painting, `bar_text`'s cell value, the range summary, and
  `grid-read-range` all switch to it.
- **The stored value stays the source text.** The formula bar, raw view,
  copy and save all see the source text, never the result.
- **Undo** needs nothing new. Formula edits are cell edits, and the formula
  table is rebuilt from the changed cells on undo and redo.
- **Results** aren't stored in `CellRef`s, so recalculation never creates
  undo steps or dirty rows.

---

## 3. Dependency graph and recalculation

### Precedents and dependents

- **Each formula records its precedents** when it's compiled: single cells
  and ranges, in absolute coordinates.
- **Reverse index, built from scratch:**
  - **Single-cell precedents:** a hash map from a cell to the formulas
    reading it.
  - **Range precedents:** for each column, a list of
    `(row_start, row_end, FormulaId)` intervals, sorted by start, with a
    running maximum of `row_end`. Finding the formulas whose range covers a
    cell is `O(log n + k)`. A whole-column range is an interval to
    `usize::MAX`.
- **Updates:** registering or unregistering a formula edits only its own
  entries.

### Recalculation

1. **Mark.** An edit marks the edited cell's dependents dirty. Marking is
   transitive, a breadth-first walk over the reverse index, and stops at
   formulas that are already dirty.
2. **Order.** The dirty set is topologically sorted (Kahn's algorithm over
   the dirty subgraph). Any nodes left over are in a cycle and become
   `#CIRC!`, along with everything downstream of them.
3. **Evaluate in order.** Range arguments iterate the column-major chunks
   directly (`for_each_in_column`), so `SUM(B:B)` is a straight scan. Numbers
   parsed from text cells are cached per chunk and invalidated by edits.
4. **Budget.** A recalculation that would run past about 8 ms continues in
   later frames. A `cx.spawn` task processes the rest in slices, and stale
   results draw dimmed until they're updated. Small edits never notice this.
5. **Volatile functions** (`NOW`, `TODAY`, `RAND`, `RANDBETWEEN`) are marked
   dirty on every recalculation. `M-x grid-recalculate` (`F9`) recalculates
   everything.

### Loading

Opening a file compiles every formula and builds the index once, then runs a
full recalculation in time slices. Cells show their cached values from an
`.xlsx`, or blank, until they're computed.

---

## 4. Function library (v1)

All functions are written from scratch. Each one follows Excel's documented
behaviour, including edge cases and error values, and has tests drawn from
Excel's documentation examples.

| Category | Functions |
|---|---|
| Math | `SUM`, `PRODUCT`, `ABS`, `ROUND`, `ROUNDUP`, `ROUNDDOWN`, `INT`, `TRUNC`, `MOD`, `POWER`, `SQRT`, `EXP`, `LN`, `LOG`, `LOG10`, `CEILING`, `FLOOR`, `SIGN`, `PI`, `RAND`, `RANDBETWEEN`, `SUMPRODUCT` |
| Conditional aggregates | `SUMIF`, `SUMIFS`, `COUNTIF`, `COUNTIFS`, `AVERAGEIF`, `AVERAGEIFS`, `MAXIFS`, `MINIFS` |
| Statistics | `AVERAGE`, `COUNT`, `COUNTA`, `COUNTBLANK`, `MIN`, `MAX`, `MEDIAN`, `MODE`, `STDEV`, `STDEV.P`, `VAR`, `VAR.P`, `LARGE`, `SMALL`, `RANK` |
| Logic | `IF`, `IFS`, `AND`, `OR`, `NOT`, `XOR`, `IFERROR`, `IFNA`, `SWITCH`, `TRUE`, `FALSE` |
| Lookup | `VLOOKUP`, `HLOOKUP`, `XLOOKUP`, `INDEX`, `MATCH`, `CHOOSE`, `ROW`, `COLUMN`, `ROWS`, `COLUMNS` |
| Text | `LEN`, `LEFT`, `RIGHT`, `MID`, `UPPER`, `LOWER`, `PROPER`, `TRIM`, `CONCAT`, `TEXTJOIN`, `SUBSTITUTE`, `REPLACE`, `FIND`, `SEARCH`, `VALUE`, `TEXT`, `REPT`, `EXACT` |
| Date | `DATE`, `TODAY`, `NOW`, `YEAR`, `MONTH`, `DAY`, `WEEKDAY`, `EDATE`, `EOMONTH`, `DATEDIF`, `DAYS`, `NETWORKDAYS`, `DATEVALUE` |
| Information | `ISBLANK`, `ISNUMBER`, `ISTEXT`, `ISERROR`, `ISNA`, `ISLOGICAL`, `NA` |

**Supporting pieces, also from scratch:**
- **`criteria.rs`:** the mini-language behind the `*IF(S)` functions:
  - comparison criteria (`">=10"`, `"<>x"`);
  - wildcards (`*`, `?`, with `~` to escape) via a matcher that backtracks
    in linear time;
  - case-insensitive equality, and numbers matching numbers.
- **`date.rs`:**
  - Excel serial dates in both the 1900 and 1904 systems. The 1900 system
    reproduces the fake 29 Feb 1900, so serials agree with Excel.
  - Calendar conversion using Howard Hinnant's day-count algorithm,
    re-implemented.
  - Weekday maths and month arithmetic with end-of-month clamping.
  - `DATEVALUE` parsing of the common formats: ISO, `M/D/YYYY`, `D-Mon-YYYY`.
- **`TEXT(value, format)`** reuses the grid's number-format code
  (`grid/style.rs`), extended with date codes (`yyyy-mm-dd`, `mmm d`) and
  the `[colour]`/section syntax kept to the common subset.
- **`SEARCH`** wildcards share the matcher with `criteria.rs`.
- **Lookups:**
  - Exact matches build a per-range hash index on first use. It's cached
    until the range changes, so `VLOOKUP` over 100k rows inside 100k
    formulas isn't O(n²).
  - Approximate matches use binary search, with Excel's sorted-data
    semantics.

---

## 5. Scheme functions

```scheme
;; init.scm
(define-grid-function (MARGIN price cost)
  (/ (- price cost) price))
(define-grid-function (TAGS . cells)          ; variadic
  (string-join (filter (lambda (s) (> (string-length s) 0)) cells) ", "))
```

`=MARGIN(E2, F2)` and `=TAGS(A2:C2)` then work in every grid.

- **Registration:** `define-grid-function` is a `grid.scm` macro. It defines
  the procedure and calls the primitive `(grid-register-function "MARGIN"
  'margin-proc arity)`. Names are case-insensitive, as Excel's are, and can
  shadow built-in functions only with an explicit `#:override #t`.
- **Calling them is asynchronous.** Scheme runs on its own thread, and
  recalculation runs on the UI thread.
  1. In a recalculation, each Scheme-function call evaluates its arguments
     and records a request. Identical requests are merged (see
     memoization below).
  2. The cell shows `#CALC!` (drawn as `…`) for now.
  3. At the end of the recalculation, the requests are queued to the
     function engine (see the next point), **one job per distinct call**,
     in order.
  4. Each result is applied when its job finishes, so one slow function
     never holds back the others. The cell takes its value, and its
     dependents recalculate as a new wave.
  5. A function that raises an error gives `#VALUE!`, with the Scheme error
     in the echo area.
  6. A call still running after `grid-function-timeout` seconds is
     interrupted. Its cell shows `#CALC!`, and the echo area says
     "MARGIN timed out after 120 s".
- **The timeout is one global option**, in seconds, defaulting to 120:
  `(set-option "grid-function-timeout" "120")`. There is no per-function
  timeout.
- **A separate interpreter runs grid functions.** With a 2-minute timeout,
  running them on the config engine would freeze every Scheme-backed key
  binding and `M-x` command while a slow function runs. Instead they run on
  a second Steel interpreter, the same arrangement as the ANVIL REPL
  (`src/anvil.rs`):
  - `define-grid-function` sends each definition's source to that
    interpreter as well as defining it in the config.
  - That interpreter can be interrupted, which is what makes the timeout
    enforceable. The config engine can't be interrupted.
  - Functions that need helpers from the user's config `require` them in
    their own definition. This is the one rule users have to know, and the
    `define-grid-function` docstring states it.
- **Arguments as Scheme values:**

  | Formula value | Scheme value |
  |---|---|
  | number | number |
  | text | string |
  | boolean | `#t` / `#f` |
  | empty | `""` |
  | error | the symbol `'#N/A` (and so on) |
  | range | a list of rows, each a list of values |

  Returned values map back the same way. Lists become `#VALUE!` (a Scheme
  function doesn't spill).
- **Dependencies:** a Scheme function's arguments are precedents in the
  dependency graph (§3), like any built-in function's. Editing an argument
  cell re-runs the call. When a result arrives, the cell's dependents
  recalculate. While a call is pending, its cell and everything downstream
  show `#CALC!`, never a stale value.
- **A Scheme function sees only its arguments.** It has no other way to
  read cells: the grid-function interpreter doesn't have the grid API. A
  read the graph can't see would leave results stale, so this rule rules
  it out. A function that needs more data takes a range argument, such as
  `=TOTAL(A2:A100)`. A function whose result depends on something outside
  the sheet declares `#:volatile #t`.
- **Purity:** Scheme functions are assumed pure. Results are memoized per
  recalculation by function and arguments, so `=MARGIN(E2,F2)` filled down a
  10k-row column sends each distinct argument set only once. A function
  that's deliberately impure can opt into being recalculated every time with
  `#:volatile #t`.

---

## 6. Editing experience

- **Display:** the formula bar shows the formula text and the cell shows its
  result. Errors render in the `error` face. Opening the cell editor or
  using `ge` edits the formula text.
- **Reference colouring:** while editing, each reference in the formula text
  gets a colour. The cell or range it points to gets an outline in the
  same colour (as planned in grid-buffer.md §5).
- **Point mode:** right after `=`, an operator, `(` or `,`:
  - Arrows and `hjkl` insert a reference and move it.
  - Shift extends it to a range.
  - A mouse click or drag inserts the reference.
  - Any other key leaves point mode.
- **Completion:**
  - After an identifier, the completion popup lists functions, built-in and
    Scheme-defined.
  - `TAB` accepts and inserts `NAME(`.
  - Inside the parentheses, the echo area shows the signature with the
    current argument in bold.
- **Showing formulas:** `M-x grid-show-formulas` (Excel's `` Ctrl+` ``)
  toggles cells between showing results and formula text.
- **Tracing:** `M-x grid-trace-precedents` and `grid-trace-dependents`
  outline the cells a formula reads, or the formulas that read the current
  cell.
- **Filling and pasting:**
  - Fill down and right: `C-d`-style commands in Emacs bindings; `g d` and
    `g l` under evil.
  - Copying a formula cell and pasting it elsewhere keeps references
    relative.
  - Pasted text that starts with `=` becomes a formula.
- **Escaping:** text that starts with `=` but isn't meant as a formula is
  typed with a leading `'`, as in Excel. The `'` isn't stored.

---

## 7. Saving

- **`.xlsx`** (once the reader and writer exist): formulas are written as
  A1 text with their results cached, so Excel opens the file with values
  already shown.
- **CSV never stores formulas.** A CSV with formulas would be misleading:
  other tools would read formula text as data, and storing formulas beside
  the file could silently diverge from it. So:
  - **Formulas in a CSV buffer evaluate normally**, so you can build and
    check them. The first `=` typed shows a one-time message: "CSV can't
    store formulas — this file will need to be saved as .xlsx".
  - **Saving requires Save As `.xlsx`.** `C-x C-s` / `:w` on a CSV buffer
    holding any formula opens "Save as (.xlsx): `<name>.xlsx`" instead of
    writing. After it saves, the buffer visits the new `.xlsx` file. The
    original CSV is left untouched on disk.
  - **Paths that can't prompt never write formula text into a CSV.**
    `save-some-buffers`, quit-time save-all, `:wq` and auto-save skip such
    a buffer and report "data.csv has formulas — save it as .xlsx". A quit
    then stops instead of discarding the changes.
  - **Removing every formula makes the buffer a plain CSV again**, and it
    saves normally.
  - **`:w other.csv` refuses** with the same message. `:w other.xlsx` is
    the save-as.
  - **Dependency:** this needs the `.xlsx` writer (a separate plan). Until
    it exists, a CSV with formulas can't be saved, only reverted, or saved
    after its formulas are removed. Shipping formulas therefore waits for a
    minimal one-sheet `.xlsx` writer that stores values and formulas
    (phase 7).

---

## 8. Scheme API additions

- **`(grid-formula row col)`:** the formula text, or `#f` for a plain cell.
- **`(grid-cell-value row col)`:** the displayed value, which is the result
  for formula cells. `grid-read-range` takes `#:values #t` to return results
  instead of source text.
- **`(grid-recalculate)`**, `(grid-register-function …)`, and the
  `define-grid-function` macro.
- **Writing formulas:** `(grid-set-cell r c "=SUM(B2:B9)")` already works,
  since a formula is just cell text.
- **`grid-recalculated-hook`:** runs after a recalculation wave settles. It
  receives the list of changed result cells, capped like
  `grid-cell-changed-hook`.

---

## 9. Testing

- **Parser:**
  - Parsing then printing gives back the same text for every grammar form.
  - Precedence tests for each of the quirks listed in §1.
  - Garbage input never panics.
- **Evaluator:**
  - A table of golden tests, `(formula, expected display)`, taken from
    Excel's function documentation.
  - At least five cases per function, including the error edge cases.
- **Reference adjustment:**
  - Insert, delete, move and sort scenarios against hand-computed results.
  - Deleted targets become `#REF!`.
  - Ranges shrink correctly when they lose an end.
- **Recalculation:**
  - Dependency order.
  - Diamond dependencies are evaluated once.
  - Cycles become `#CIRC!`, and so does everything downstream.
  - Volatile functions recalculate.
  - Time-sliced recalculation gives the same results as a synchronous one.
- **Property tests** with the existing deterministic generator:
  - Random formula DAGs give the same results whether recalculated
    incrementally or in full.
- **Scheme functions:**
  - End-to-end tests: merged requests, memoization, results applying as
    each call finishes, errors, and dependents recalculating.
  - A deliberately slow function is interrupted at the timeout (set low in
    the test), and Scheme key bindings keep working while it runs.
- **CSV guard:**
  - Saving a CSV with a formula prompts for `.xlsx`.
  - Save-all, quit and `:wq` refuse and report.
  - `:w x.csv` refuses.
  - Removing every formula restores normal CSV saving.
  - The original CSV is byte-identical after a Save As.
- **Performance:** an `#[ignore]`d benchmark for each goal-line number,
  in the style of the grid's existing perf test.

---

## Phases

1. **Language core:** lexer, parser, AST, printer, references, values,
   coercion, and General number formatting. Operators only, no functions.
   *Done when:* the round-trip and precedence suites pass.
2. **Evaluation in the grid:**
   - The formula table and `display()`; cells show results.
   - The dependency index and incremental recalculation with cycle
     detection.
   - Math and logic functions.
   - The formula bar shows source text; editing works.
3. **Function library:** statistics, criteria and the `*IF(S)` functions,
   lookups with the hash index, text functions, and dates with the serial
   system and `TEXT`.
4. **Structure:** reference adjustment on insert, delete and sort; filling
   down and right; pasting formulas; `#REF!`.
5. **Editing experience:** reference colouring, point mode, function
   completion, signature hints, `grid-show-formulas`, and tracing.
6. **Scheme functions:** `define-grid-function`, batched asynchronous calls,
   memoization, and timeouts; the `grid-formula`/`grid-cell-value`/
   `grid-recalculate` API and `grid-recalculated-hook`.
7. **Persistence and scale:** a minimal one-sheet `.xlsx` writer (values,
   formulas and cached results) or the full `.xlsx` work, whichever lands
   first. Then the CSV rules: required Save As `.xlsx`, guarded save-all
   and quit, and `:w` checks. Also time-sliced recalculation and the
   performance benchmarks. Formulas ship to users only once this phase is
   done.

## Resolved questions

1. **Formulas in CSV:** CSV never stores them. Saving a CSV buffer that
   holds formulas requires Save As `.xlsx` (§7).
2. **Circular references:** an explicit `#CIRC!` error. An `.xlsx` export
   writes 0 as the cached value, keeping the formula.
3. **Locale:** formulas are stored in Excel's English form, as Excel does.
   v1 typing and display are en-US. A translated display and typing layer
   (`grid-formula-locale`) is a possible later addition and wouldn't
   change files.
4. **Scheme-function timeout:** a global `grid-function-timeout`, default
   120 s, enforced by running grid functions on their own interruptible
   interpreter.

## Open questions

None.

## As built

Implemented on `feat/grid-buffer`. Every phase is in, with these deviations:

- **References are stored with absolute coordinates plus `$` flags**, not
  as relative offsets. Inserting or deleting rows is then a plain shift.
  Copying, filling and sorting translate the unanchored axes explicitly
  (`refs::translate`), and moving rows or columns (org-table `M-arrows`)
  remaps every reference to the moved cells. The A1 text stored in each cell
  is the canonical form.
- **How a cell is marked as a formula:** its stored text starts with a
  control-character marker (`U+0001`) followed by `=`. This is how CSV
  "never stores formulas" is enforced. A `=…` loaded from a CSV is plain
  text, and the marker travels with the cell through undo, sort and copy.
  `'=…` typed into a cell stays literal text.
- **After a structural edit, the engine re-indexes from the cells**, rather
  than shifting its own index in place. Grids that have never had a formula
  skip that scan.
- **Recalculation needs no recursion.** It uses an explicit work stack: a
  formula that reads a still-dirty input re-queues itself behind it, and
  reading a formula that is waiting on the reader is a cycle (`#CIRC!`).
- **Scheme functions:**
  - Defining one starts the grid-function interpreter, which runs on its
    own thread. A watchdog interrupts calls that run past
    `(grid-function-timeout secs)`.
  - Helpers must be shared with `define-grid-helper`.
  - v1 functions take fixed arguments (no rest arguments).
  - A single-cell argument arrives as its value, a range as a list of rows.
- **`.xlsx` support:** a std-only ZIP, inflate/deflate and XML reader and
  writer (`src/xlsx`).
  - Opening shows the first sheet, and shared formulas are expanded.
    Every other sheet opens as a buffer of its own (`grid-open-sheet`,
    `helm-grid-sheets`), named like `report.xlsx<Costs>`.
  - References to other sheets (`Costs!B2`) read the file's cached
    values, then live values from open sheet buffers: each engine knows
    which sheets its formulas read (`Engine::sheet_changed`), and the
    editor hands a sheet's values to the workbook's other open sheets
    after it changes.
  - A file holding anything the writer would drop opens protected: it can
    be edited and saved as a copy, but never written back over itself.
- **Scheme API:**
  - `grid-formula` reads the cursor cell's formula, and
    `grid-current-value` its displayed result.
  - The per-cell `(grid-formula row col)` and `grid-cell-value`, and
    `grid-recalculated-hook`, are not built yet.
- **Speed** (dev profile, `cargo test formula_perf_targets -- --ignored`):

  | Measurement | Target | Result |
  |---|---|---|
  | Full recalculation of 100k formulas | < 250 ms | 48 ms |
  | Index those 100k formulas (load) | — | 61 ms |
  | An edit feeding 10k dependents | < 16 ms | 5 ms |
  | `SUM(A:A)` over 1M rows | < 20 ms | 26 ms (slightly over; cell text is parsed on each read) |

- **Function library:** 144 built-ins plus the 6 evaluator forms. The
  deviations from Excel are recorded in the functions' doc comments, for
  example `COUNTBLANK(A:A)` counting only to the used extent and `TEXT()`
  not supporting fraction formats.
- **Display:** a number too wide for its column drops decimal places to fit
  before falling back to `###`, as Excel's General format does.
