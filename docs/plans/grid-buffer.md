# Plan: Grid buffer (CSV/TSV spreadsheet view)

A new buffer type that shows delimited data as a spreadsheet-style grid. It
supports drag-and-drop to open, Excel-like infinite scrolling and zoom, mouse
resizing of rows and columns, and modal cell editing. It must stay fast on
multi-million-row files.

## Goals

- Drop a `.csv` / `.tsv` file on a pane, or `find-file` one, and it opens as a
  grid with the delimiter, quoting, header row and encoding detected
  automatically.
- **Excel feel:**
  - Default cell size matches Excel: 64 px wide (8.43 characters) by 20 px
    tall, at 100% zoom.
  - Lettered column headers (`A … Z, AA …`) and numbered row headers.
  - Infinite scrolling past the end of the data.
  - Zoom from 10% to 400%.
  - Resize columns and rows by dragging header borders. Double-click a border
    to auto-fit.
- **Modal editing:**
  - In *grid normal*, keys move between cells.
  - In *cell insert*, keys edit text inside one cell.
  - There is no nested third mode. Heavy edits go through a popup buffer (an
    escape hatch).
- **Scale targets:** 1M rows × 20 columns (about 100 MB):
  - First paint in under 100 ms.
  - Fully loaded in under 2 s.
  - Resident memory no more than about 1.3× the file size.
  - Scrolling at 120 fps.
- **Round trip:** saving a file you didn't edit gives back identical bytes.
  Saving an edited file changes only the edited records.

## Non-goals (v1)

- Evaluating formulas, and loading multiple sheets. The `Workbook` model and
  the formula bar are built in v1 so both can be added later without
  rework.
- `.xlsx` support. The model is designed so it can be added later as another
  loader.
- Cell formatting and styles beyond what is inferred (numbers right-aligned,
  header row bold).

---

## Architecture

The embedded terminal already has the same shape: a non-text buffer that owns
its own state, has its own element, and gets its own key layer. The grid
follows that precedent.

| Piece | Where | Precedent |
|---|---|---|
| Data model, parser, writer, interner | `src/grid/{model,parse,intern,write,dialect}.rs` (new) | `src/terminal.rs` |
| Editor integration (open, commands, undo, save) | `src/editor/grid.rs` (new) | `src/editor/terminal.rs` |
| Rendering | `GridElement` in `src/grid/element.rs` | `TerminalElement` (`src/element.rs:2248`) |
| Pane dispatch | `render_layout` branch on `pane_grid_id` | `pane_terminal_id` (`src/editor/panes.rs:818`) |
| Key routing | `key_grid` layer after `key_terminal` | `src/editor/input.rs:146` |
| Keymaps, Scheme API | `scheme/grid.scm` (new) | `scheme/tabulated.scm`, `scheme/evil.scm` |
| Drag and drop | `.on_drop::<ExternalPaths>` on the root div | `src/editor/mod.rs:5126` |

The editor keeps `workbooks: HashMap<u64, Workbook>`, the same way it keeps
`terminals`. Each sheet buffer refers to `(workbook id, sheet index)`, and a
CSV opens as a workbook with one sheet (see §1, Workbooks and sheets). The buffer list still gets a placeholder `Document`
with `special = Some("grid-mode")`. That keeps the buffer menu, perspectives,
the mode line, `kill-buffer` and session restore working without special
cases. The `Document`'s `Buffer` stays empty because the grid owns the data.

---

## 1. Data model (`src/grid/model.rs`)

### Cell references: 8 bytes per cell, almost no heap use

```rust
/// One cell, packed into 8 bytes. Most cells never get their own heap allocation.
#[repr(transparent)]
struct CellRef(u64);
// tag (top 2 bits):
//   00 Empty
//   01 Span   { offset: u40, len: u22 } -> borrows bytes from the source file
//   10 Inline { len: u3, bytes: [u8; 7] } -> short cells ("42", "TRUE", "NY")
//   11 Interned(u32)                  -> id in the StringPool (aliased strings)
```

- **Span:** used for cells that can be read straight from the original file.
  That means unquoted cells, or quoted cells with no doubled `""` inside. The
  source bytes are kept in one `Arc<[u8]>`, so this costs nothing to load.
- **Inline:** cells of 7 bytes or fewer are stored inside the reference
  itself. Numbers, flags and codes are a large share of real CSV data, and
  this avoids looking them up in a table or the source file.
- **Interned:** used for decoded cells (escaped quotes, transcoded text),
  edited values, and cells from low-cardinality columns (see §2).
- A cell longer than 4 MiB, the limit of the 22-bit length field, falls back
  to Interned.

### String aliasing (`src/grid/intern.rs`)

`StringPool` gives every distinct string a single stored copy. A string is
stored once in an append-only arena, and the pool maps it to a `u32` id.

- Hashing uses `rustc-hash` (FxHash), which is fast for short keys. The hash
  table holds `(hash, id)` entries that point into the arena, so each string is
  stored once rather than once as a key and once as a value.
- Equal strings always get the same id. That makes comparing, sorting, finding
  distinct values, and "find all equal" cheap: they compare `u32`s. It also
  means the shaped-text cache can be keyed by id (§4).
- The arena never frees anything. Strings orphaned by edits are reclaimed by
  `grid-compact`, which runs on save or when they exceed a threshold.

### Workbooks and sheets

`.xlsx` support will eventually open each sheet (tab) as its own buffer. The
model is shaped for that now, even though v1 only loads CSV:

```rust
struct Workbook {
    path: Option<PathBuf>,
    format: Format,                   // Csv(Dialect) | Xlsx (later)
    pool: StringPool,                 // shared by every sheet
    sheets: Vec<Sheet>,               // a CSV has exactly one
    formulas: FormulaTable,           // empty in v1; see below
    modified: bool,                   // one flag for the whole workbook
}
```

- **One shared owner:** cross-sheet references (`Rates!B1`), the shared
  string table, and later the formula engine all live in one place.
- **Save covers the whole workbook:** saving from any sheet buffer writes
  every sheet. The unsaved marker, the kill-buffer prompt, and
  `save-some-buffers` apply to the workbook, so every sheet buffer from one
  file reports the same state.
- **Buffer names:** a sheet buffer of `book.xlsx` is named `book.xlsx:Rates`.
  A CSV buffer keeps the file name alone.
- **Formulas go in a separate table:** all four tags in the 8-byte `CellRef`
  are already used, and most cells are not formulas. So the cell holds the
  computed value, and the formula text lives in a sparse
  `FormulaTable: HashMap<(sheet, row, col), FormulaId>`. Cells without
  formulas stay 8 bytes. In v1 the table exists but is always empty.

The `Grid` below is one `Sheet`. Its `pool` field moves up to the `Workbook`.

### Layout: column-major chunks

```rust
struct Grid {
    source: Arc<[u8]>,                // original file bytes (owned, not mmapped; see Risks)
    pool: StringPool,
    columns: Vec<Column>,             // used columns only
    row_count: usize,                 // used rows
    dialect: Dialect,                 // delimiter, quote, line ending, BOM, encoding, header
    geometry: Geometry,               // column widths, row heights, zoom, frozen panes
    undo: Vec<GridEdit>, redo: Vec<GridEdit>,
    dirty_rows: RoaringBitmap-ish,    // rows to re-serialize on save
    load: LoadState,                  // Streaming { rows_so_far } | Done | Failed
}
struct Column {
    chunks: Vec<Chunk>,               // 4096 rows each
    stats: ColumnStats,               // inferred type, max width seen, distinct estimate
}
struct Chunk { cells: Box<[CellRef]> } // fixed capacity, so inserting a row costs O(chunk)
```

- **Why column-major:** sorting, type inference, auto-fit width and
  dictionary encoding all work one column at a time. Rendering only reads
  about 50 × 20 cells per frame, so the storage order doesn't affect drawing
  speed.
- **Why chunks:** inserting or deleting a row shifts one 4096-row chunk per
  column, not the whole column. A chunk that gets full splits, and one that
  gets small merges with its neighbour. Finding the chunk for a row uses
  prefix counts kept in a small Fenwick tree.
- **Sparse beyond the data:** cells outside the used area are not stored.
  Reading one returns Empty. Writing one grows `row_count` and `columns` to
  include it, and the gaps in between are filled with Empty.

---

## 2. Loading (`src/grid/parse.rs`, `src/grid/dialect.rs`)

### Pipeline

1. **Read** the file on the background executor, the way `find_file` does
   (`src/editor/buffers.rs:268`). Read raw bytes (`fs::read`) rather than a
   `String`. That skips the full-file UTF-8 validation pass, and the file may
   not be UTF-8 at all.
2. **Detect the dialect** from the first 64 KiB:
   - **Encoding:** a BOM identifies UTF-8 or UTF-16. Otherwise validate the
     sample as UTF-8 and fall back to Windows-1252 if it fails (via
     `encoding_rs`).
   - **Delimiter:** for each candidate (`,` `\t` `;` `|`), count how many
     fields each line has. Pick the one that gives the most consistent count
     greater than 1. A `.tsv` extension biases the choice toward tab.
   - **Quote and escape style:** `""` or `\"`.
   - **Line ending:** whichever of LF and CRLF is more common. It is used when
     writing.
   - **Header row:** treat row 1 as a header when it is all text and the rows
     below have numeric columns, or when every value in row 1 is unique and no
     cell is empty. A detected header is still **ordinary data**. It is row 1,
     it can be edited and deleted like any cell, and it is saved as it is.
     Detection only changes how it is displayed and treated:
     - It is drawn bold. It is **not** frozen by default; `grid-toggle-freeze`
       (`g f` / `C-c C-z`) freezes the rows above the cursor (row 1 when the
       cursor is on it), remembered per file.
     - Sort and type inference skip it.
     - Its values are shown as column names in the formula bar
       (`C7 · email`).

     Column letters stay `A`, `B`, `C`. `M-x grid-toggle-header` corrects a
     wrong guess, and the choice is remembered in the sidecar file.
3. **Fast first paint:** parse the first approximately 200 rows on the UI
   thread, which takes well under 1 ms. Install the buffer and paint. The mode
   line shows `Loading… 12,345 rows`.
4. **Parallel full parse** on background threads:
   - Find record boundaries quickly. Use `memchr` to find every `"`, `\n` and
     delimiter, then track whether each position is inside quotes with a
     running quote-parity count. This is the simdcsv technique.
   - Split the file into roughly 8 MiB pieces aligned to record boundaries.
   - Parse each piece on its own thread with `csv-core`, which doesn't
     allocate. Each thread builds `CellRef` chunks and a local
     `StringPool`.
   - Merge the local pools into the global one. Each thread gets a table
     mapping its local ids to global ids, and its chunks are rewritten with
     it.
   - Every completed batch of about 64k rows is published to the UI thread.
     The scrollbar and row count grow as rows arrive.
5. **Finish:** compute column statistics, infer types (§3), and set initial
   column widths (§4).

### Choosing how to store each cell during parsing

| Cell | Stored as |
|---|---|
| Empty | `Empty` |
| 7 bytes or fewer, no unescaping needed | `Inline` |
| Raw bytes are the value (no `""`, already UTF-8) | `Span` |
| Needs unescaping or transcoding | `Interned` |
| In a low-cardinality column (see below) | `Interned`, even when Span would work |

**Low-cardinality columns:** while sampling the first 10k rows, estimate each
column's number of distinct values with a HyperLogLog sketch. If the estimate
is under 5% of the rows sampled, intern all of the column's cells. This is the
main aliasing saving. Columns like `country`, `status` or `category` collapse
to a few dozen strings, so sort and filter compare ids instead of bytes.

### Other optimizations

- **Cancelling:** closing the buffer while it is still loading sets an
  `AtomicBool`, and the parser threads stop at their next batch.
- **Malformed rows:** a row with an unclosed quote or the wrong number of
  fields still loads. The row number gets a warning mark, and `grid-problems`
  lists every such row. Loading never fails because of bad data.
- **Large-file guard:** files over 1 GiB ask first, with the choices "open as
  grid", "open first N rows" or "open as text".
- **No-copy text mode:** `grid-view-as-text` opens the same source bytes as a
  normal rope buffer. Use it when detection gets the format wrong.

---

## 3. Type inference

For each column, sample up to 1000 non-empty cells and classify them as
`Int | Float | Bool | Date | Text`, accepting a column type if at least 95% of
the sample matches. The type controls:

- **Alignment:** numbers are right-aligned, text left-aligned, as in Excel.
- **Sorting:** numbers sort numerically. The `f64` value for a numeric cell is
  parsed lazily and cached per chunk the first time the column is sorted.
- **Auto-fit:** numeric columns are sized for their widest formatted value.

The original text is always what gets stored and saved. Inference only changes
how cells are shown and compared.

---

## 4. Rendering (`GridElement`)

### Excel-like geometry

| Setting | Default at 100% zoom | Notes |
|---|---|---|
| Column width | **64 px** (8.43 characters of the default font) | Excel's default |
| Row height | **20 px** (15 pt) | Excel's default |
| Row header width | Fits the largest visible row number, at least 40 px | Grows as you scroll deeper |
| Column header height | 20 px | Letters `A`, `B`, …, `AA`, … |
| Cell padding | 3 px left and right | Text is clipped at the cell edge |
| Gridlines | 1 px, `grid-line` face | Can be turned off |

- Each column width and row height can be overridden. Overrides are stored
  sparsely in `Geometry` as `BTreeMap<usize, f32>`, holding only the
  non-default sizes.
- Finding the x position of column *n*: `n × default + Σ(override − default)`
  for the overrides before *n*. Finding the column at a given x is a binary
  search over the overrides. The same works for rows. Both stay correct at any
  depth into the infinite area without scanning every column.
- All sizes are stored at 100% zoom and multiplied by the zoom level when
  drawn.

### Infinite scroll

- The area you can scroll through is
  `max(used_rows, last_visited_row) + one viewport` in each direction. So
  scrolling or moving past the end of the data keeps showing empty rows and
  columns, and the scrollbar grows with you as in Excel. There is no fixed
  row or column limit. Rows and columns are `usize` indices, and the only
  practical ceiling is memory. Column letters continue past `XFD`
  (`XFE`, …).
- Saving to `.xlsx` (once it exists) checks Excel's 1,048,576 × 16,384 limit
  and warns before writing a sheet that Excel couldn't open. That is a check
  when exporting, not a limit on the grid.
- The scrollbar thumb is sized against that area, not against the used area.
  `C-End` / `G` jump to the last used cell. `C-Home` / `gg` jump to `A1`.
- Scrolling is smooth to the pixel. The scroll offset is stored as a cell
  index plus a pixel offset into that cell, not as an absolute pixel value.
  Absolute values lose `f32` precision a million rows down.

### Zoom (scaling)

- `C-=` / `C--` / `C-0` change the zoom, as does `Cmd`/`Ctrl` + mouse wheel.
  Zoom steps run from 10% to 400%. The mode line shows the level, for example
  `85%`.
- Zoom is per grid. It is not tied to the editor's text scale. Font size,
  padding, gridlines and header sizes all scale together.
- Below 40% zoom, cell text is skipped and each filled cell is drawn as a
  tinted block. That keeps zoomed-out overviews of large files fast.

### Resizing with the mouse

- **Hit area:** a 4 px band centered on each column-header right edge and each
  row-header bottom edge. Hovering it shows `ResizeLeftRight` or
  `ResizeUpDown`, the same cursors the split dividers use in
  `src/editor/panes.rs`.
- **Drag:** shows a live guide line across the whole grid and a tooltip with
  the size in pixels and characters (`Width: 12.50 (105 px)`), as Excel
  does. The size is applied when the mouse is released. The minimum size is
  4 px, and dragging below it hides the column or row, as in Excel. One undo
  step restores the previous size.
- **Double-click a border:** auto-fits to the widest cell in the column, using
  only the visible rows plus the first 1000 rows, so it stays fast on huge
  files. `grid-autofit-column-full` measures every row.
- **Multiple selected columns:** if the dragged column is part of a column
  selection, every selected column is resized to the same width, as in Excel.
- **Keyboard equivalents:** `>` / `<` widen or narrow the column by one
  character. `=` auto-fits it. `z>` / `z<` resize the row.
- **Persistence:** sizes are saved in a sidecar file under the session
  directory, keyed by the file's canonical path, and restored on reopen. They
  are never written into the CSV.

### Layers drawn each frame

All of these cover visible cells only:

0. The formula bar (§5) across the top of the pane, above the column
   headers.
1. Background, banded rows (optional), and the selection-range fill.
2. Gridlines, batched into one quad list.
3. Cell text.
4. Headers, which stay fixed while the grid scrolls, plus any rows the user
   froze with `grid-toggle-freeze`. Nothing is frozen by default. Headers
   are deliberately quiet: no tinted background, small low-contrast labels,
   with the active row/column's label one step brighter.
5. Cursor cell border (2 px), the fill handle, the resize guide, and the
   inline cell editor.

### Text shaping cache

- Shaped text is cached in an LRU of about 8k entries keyed by
  `(CellKey, zoom_bucket)`. `CellKey` is the interned id for Interned cells
  and the packed `u64` for Inline cells, so aliased strings share one entry.
  Span cells use `(offset, len)` as the key.
- Repeated values in a low-cardinality column are shaped once.
- Long cells are truncated before shaping, at about
  `column_px / min_char_px + 1` characters, so a 10 KB cell never gets
  shaped.

---

## 5. Modes and keybindings (`scheme/grid.scm`)

All bindings are written in Scheme against Rust primitives, which matches
`evil.scm` and `helix.scm`. The Rust side only decides whether the grid is in
*grid normal* or *cell insert*.

### Grid normal (evil)

| Keys | Action |
|---|---|
| `h j k l`, arrow keys | Move one cell |
| `w` / `b`, `Tab` / `S-Tab` | Next/previous cell in reading order, wrapping across rows |
| `W` / `B`, `{` / `}` | Jump to the edge of the filled block (Excel's `Ctrl+Arrow`) |
| `0` / `^` / `$` | First column / first non-empty cell / last non-empty cell in the row |
| `gg` / `G` / `N G` / `N \|` | Top / last used row / row N / column N |
| `C-d` `C-u` `C-f` `C-b` | Half page / full page |
| `/` `n` `N` | Search cell contents (regex) |
| `i` / `a` / `Enter` | Edit with the cursor at the start / end / end |
| `c` / `s` / `cc` | Clear the cell and edit it |
| `x` | Clear the cell |
| `dd` / `dc` | Delete the row / delete the column |
| `o` / `O`, `go` / `gO` | Insert a row below/above, a column right/left |
| `v` / `V` / `C-v` | Select a cell range / rows / columns |
| `y` `p` | Yank and paste. Pasted text in TSV form spreads into a block of cells |
| `>` `<` `=` | Widen / narrow / auto-fit the column |
| `S` | Sort by this column (`tabulated.scm` uses `s` for sort; here `s` edits, so sort is capital `S`) |
| `u` `C-r` `.` | Undo / redo / repeat |
| `ge` | Edit the cell in a popup buffer (the escape hatch) |

### Cell insert

| Keys | Action |
|---|---|
| `Esc` | Save the edit and stay on the cell |
| `C-g` | Cancel and restore the original value |
| `Enter` / `S-Enter` | Save and move down/up |
| `Tab` / `S-Tab` | Save and move right/left |
| `Up` / `Down` | Save and move |
| `Left` `Right` `C-a` `C-e` `M-f` `M-b` `C-w` `C-u` `Backspace` | Edit text inside the cell |
| `M-Enter` | Insert a literal newline in the cell |

The inline editor is a small `Buffer` (`src/buffer.rs`) holding just that
cell's text. That lets it reuse the grapheme and word motion code. Its own
small key handler covers the table above. It does not route through the full
`key_evil_insert` path, which assumes `self.active`.

**Escape hatch (`ge`):** opens `*cell C7*` in a popup buffer with full evil or
Helix editing. `C-c C-c` writes the text back, and `C-c C-k` cancels. This
mirrors Emacs's `org-edit-special`.

### Emacs and Helix variants

- **Emacs:**
  - `C-f`/`C-b`/`C-n`/`C-p` move between cells, and `M-f`/`M-b`/`M-n`/`M-p`
    jump to block edges.
  - `C-a`/`C-e` go to the start/end of the row, and `M-<`/`M->` to the
    top/bottom.
  - `RET` edits. Typing a printable character replaces the cell, as in a
    spreadsheet.
- **Helix:** motions select cells, `C` adds a cursor on the next row, and
  `d`/`c`/`y` apply to every selection. This gives multi-cell column edits.

### Formula bar (one per grid pane)

Every grid pane draws its own formula bar as a strip above the column
headers, inside `GridElement`. Two grids split side by side each have their
own bar showing their own cell. The minibuffer is left for prompts and
messages.

```
┌───────────┬────┬──────────────────────────────────────────┐
│ C7 · email│ fx │ Acme Corporation, Inc. — "Northwest" div… │
└───────────┴────┴──────────────────────────────────────────┘
      A          B            C
```

- **Name box (left):**
  - Shows the current cell's address, plus the column name when the sheet
    has a header row.
  - A range selection shows its address (`B2:D9`).
  - Clicking it, or pressing `gn` (`C-x g` in Emacs bindings), lets you type
    an address and jump there.
  - In evil, `:C7`, `:B2:D9` and later `:Rates!B1` do the same, which fits
    the existing `:` commands.
- **Contents (right):**
  - Updates whenever the cursor lands on a cell, by keyboard, mouse, search
    or jump.
  - Shows the full raw contents. The grid truncates text at the cell edge,
    but the bar does not. Once formulas exist, the bar shows the formula and
    the cell shows its result.
  - A cell with newlines is shown with `↵` markers on one line. `M-Enter`
    while editing grows the bar to several lines.
  - Very long values are cut with `…`. Editing the cell or pressing `ge`
    shows the rest.
- **One editor shown in two places:**
  - The inline cell editor and the bar share the same small editing
    `Buffer`. Typing in the cell updates the bar, and the reverse.
  - Clicking the bar during cell insert moves the text cursor into the bar
    without ending the edit.
  - Starting an edit from the bar works the same as starting it in the cell.
- **Range summary:** with a range selected, the pane's mode line shows
  `24 cells · Sum 1,204 · Avg 50.2`, like Excel's status bar. Sum and
  average cover numeric cells only.
- **Fast under key repeat:** an update costs one cell lookup. If a key is
  held down, the bar redraws at most once per frame and always shows the last
  cell the cursor reached.
- **Hiding the bar:** `M-x grid-formula-bar` toggles it per pane, and
  `(set-option 'grid-formula-bar #f)` sets the default. While it's hidden,
  the same text goes to the echo area instead.
  - Those echoes aren't logged in `*Messages*`: add an echo that skips the
    log, like Emacs's `message` with `message-log-max` bound to `nil`.
  - They replace the echo text only when it came from the grid. That uses
    the same ownership check as `hover_message` (`src/editor/mod.rs`), so an
    error already showing isn't erased by the next cursor move.
- **Later, for formulas:**
  - References in the formula being edited (`B2:B9`) are colored, and a
    matching colored box is drawn around those cells.
  - Right after `=`, an operator, `(` or `,`, arrow keys and `hjkl` pick a
    cell and insert its reference (Excel's point mode). A mouse click does the
    same. Anywhere else, arrows move inside the text as usual.

### Mouse

- Click selects a cell. Drag selects a range. `S-click` extends the
  selection.
- Clicking a header selects the whole row or column.
- Double-clicking a cell edits it. Double-clicking a header border auto-fits.
- Dragging a header border resizes (§4).
- Wheel scrolls vertically, `S`+wheel scrolls horizontally, and `Cmd`+wheel
  zooms.

---

## 6. Editing, undo, save (`src/editor/grid.rs`)

- **Undo:** `GridEdit` is one of:
  - `SetCells { cells: Vec<(row, col, old, new)> }`
  - `InsertRows`, `DeleteRows { data }`, `InsertCols`, `DeleteCols { data }`
  - `Resize { axis, idx, old, new }`, `Sort { permutation }`

  A command that changes many cells (paste, `x` over a range, Helix
  multi-cursor edits) records one `GridEdit` and undoes in one step. Sort
  stores its row permutation rather than copies of the cells.
- **Save behaves like any other buffer.** Edit a cell, press `C-x C-s` /
  `:w` / `Cmd-S`, and the CSV on disk contains the new value. Nothing else
  changes. In detail:
  - **Pending edits are saved too:** if the user saves while still typing in
    a cell, the edit is committed first. A value they can see on screen is
    never left out of the file.
  - **Unsaved-changes tracking:** the mode line shows the usual `**` marker,
    and the `Document`'s `modified` flag follows the grid's state. So
    `kill-buffer`, quitting, and `save-some-buffers` prompt as they do for
    text. Undoing back to the last saved state clears the flag.
  - **No reformatting:** values are written exactly as typed, with no type
    conversion. `007` stays `007`, `1e5` stays `1e5`, and long numbers like
    `12345678901234567890` don't turn into `1.23E+19`. Excel does all three
    of these, and they surprise users.
  - **Infinite scroll adds nothing:** scrolling or moving into empty space
    past the data writes nothing. Only rows and columns that contain a value
    are written. If the user empties the last data cells, the file shrinks
    back. Gaps between filled cells are written as empty fields, with every
    row padded to the widest row's column count, as Excel and Google Sheets
    do.
  - **Format is kept:** the original delimiter, quote style, line ending,
    BOM, encoding, and whether the file ended with a newline are all
    preserved.
  - **Save-as by extension:** `write-file` to `foo.tsv` writes tabs, and to
    `foo.csv` writes commas. An unknown extension keeps the current dialect.
  - **Saving during load:** a save requested while the file is still loading
    waits for the load to finish. Saving a partial file would truncate it.
    The user sees `Saving after load completes…`.
  - **Backups and auto-save:** these use the text buffers' existing settings
    and paths.
  - **Feedback:** the echo shows `Wrote data.csv (1,204 rows)`.
- **Save mechanics:** uses `save::write_sequenced(ticket, path, &[u8])`
  (`src/save.rs:109`), so grid saves are atomic and kept in order like text
  buffer saves.
  - The writer streams one record at a time. A row that hasn't changed and
    whose record bytes are still intact in `source` is copied from the source
    bytes unchanged. That keeps the original quoting, whitespace and line
    endings.
  - Edited rows are re-serialized with the detected dialect. Fields are
    quoted only when they need it.
  - The original BOM and encoding are kept. Text is transcoded back to the
    source encoding.
- **After a successful save:** `source` is not swapped for the newly written
  bytes, because that would need a re-read. `dirty_rows` is cleared, but the
  edited cells stay as Interned ids.
- **Auto-revert and file watching:** these reuse `reconcile_watches`. A
  change on disk re-parses the file in the background and swaps it in, unless
  the grid has unsaved changes. In that case the user gets the usual prompt.

---

## 7. Opening and drag-and-drop

- **Root drop handler:** add `.on_drop::<ExternalPaths>(…)` to the root div
  at `src/editor/mod.rs:5126`. Use `drag_over` to highlight the pane under
  the pointer while dragging. On drop:
  1. Find which pane is under the drop position.
  2. Select it.
  3. Call `find_file` for each path. Dropping several files opens them all
     and leaves the last one shown.

  This adds drag-and-drop for every file type, not only CSV.
- **Choosing grid or text:** `find_file` decides by extension through a Scheme
  hook, `(grid-extensions '("csv" "tsv" "tab" "psv"))`, similar to how
  `set-extension` works for language servers. Files with other extensions can
  be opened as a grid with `M-x grid-mode`, which re-parses the current text
  buffer's contents.
- **Avoiding a second read:** the background read in `find_file` produces
  bytes. When the path maps to grid mode, those bytes go straight to the
  grid loader without becoming a `String`.

---

## 8. Scheme API

These primitives are registered in `src/script/` and used by `grid.scm`:

- **Movement and edits:** `grid-move`, `grid-jump-edge`, `grid-goto`,
  `grid-edit-begin`, `grid-edit-commit`, `grid-edit-cancel`, `grid-set-cell`,
  `grid-cell`, `grid-insert-row`, `grid-delete-row`, `grid-insert-col`,
  `grid-delete-col`.
- **Sizing and view:** `grid-resize`, `grid-autofit`, `grid-zoom`, `grid-sort`,
  `grid-freeze`.
- **Info:** `grid-selection`, `grid-dimensions`.
- **Hooks:** `grid-mode-hook` and `grid-cell-changed-hook`. The latter is
  called with the row, column, old value and new value, and is meant for
  validation in user code.

---

## Phases

Each phase can ship on its own.

1. **Model and loader (no UI).** `CellRef`, `StringPool`, chunked columns,
   dialect detection, single-threaded `csv-core` parse, and a writer that
   reproduces the input byte for byte.
   *Tests:*
   - Round-trip property tests that generate random CSV with `""`, embedded
     newlines and CRLF, and check that load → save gives identical bytes.
   - Dialect detection fixtures.
   - Interning dedupe.
   - The edge cases of the packed `CellRef` layout.
2. **Read-only rendering.** `GridElement` with headers, gridlines, Excel
   default sizes, infinite scroll, the zoom levels, and the shaping cache.
   Adds `pane_grid_id` dispatch and drag-and-drop opening.
   *Tests:* e2e tests in `src/editor/e2e_tests.rs` that open a fixture, scroll
   past the end of the data, and zoom.
3. **Navigation and resizing.** The `key_grid` layer, `grid.scm` keymaps for
   evil, Emacs and Helix, mouse selection, and dragging or double-clicking
   header borders. The per-pane formula bar shows the name box and the cell's
   contents, with the minibuffer as the fallback when the bar is hidden. A
   range summary appears in the mode line. Column and row sizes persist in
   the sidecar file.
4. **Editing.** The inline cell editor, which shares its text with the
   formula bar, row and column insert and delete,
   yank and paste (with TSV clipboard interop with Excel and Google Sheets),
   undo and redo, saving, and the `ge` popup.
   *Tests:*
   - Edit one cell, save, and diff against the original: exactly one record
     changes.
   - Save while in cell insert: the pending edit is in the file.
   - Values such as `007` and long numbers are written exactly as typed.
   - Visiting row 5000 of a 10-row file and saving leaves the file
     unchanged.
   - The `modified` flag drives the kill and quit prompts.
5. **Scale.** Parallel boundary scan and parse, progressive loading,
   low-cardinality dictionary encoding, cancellation, the large-file guard,
   and drawing filled cells as blocks at low zoom.
   *Benchmark:* a `criterion` bench on generated 1M×20 and 10M×5 files that
   checks the goals above.
6. **Polish.** Sorting with typed comparison, freeze panes, `grid-problems`
   for malformed rows, `grid-view-as-text`, session restore, and a tutorial
   section.

## New dependencies

| Crate | Why |
|---|---|
| `csv-core` | Parsing and writing CSV without allocation (the core of BurntSushi's `csv` crate) |
| `memchr` | SIMD byte search for finding record boundaries (already a dependency of `regex`) |
| `rustc-hash` | FxHash for the interner |
| `encoding_rs` | Detecting and transcoding UTF-16 and Windows-1252 |
| `rayon` (optional) | Parallel parsing. `std::thread::scope` would also work and avoids a dependency |

Each one needs a `deny.toml` license check and an entry in
`THIRD-PARTY-LICENSES.md`.

## Risks and decisions

- **Owned bytes, not mmap:** mapping the file avoids a copy. But if another
  process truncates the file in place, the editor crashes with `SIGBUS`.
  Owned `Arc<[u8]>` bytes are safe, and for files under 1 GiB the copy costs
  about 100 ms. mmap could become an opt-in later for very large files.
- **Uniform row heights by default:** keeping rows the same height makes
  finding a row by position fast. Cells with newlines show a `↵` marker and
  don't grow their row. Users can still set a row's height by hand, which is
  stored as an override.
- **The 40-bit Span offset** limits Span cells to the first 1 TiB of a file.
  That is fine.
- **Inline editor vs. full evil:** keeping the cell editor separate is what
  makes the two-state design work. If users ask for more vim power inside a
  cell, `ge` is the answer. A nested mode is not.
- **Interned strings never freed:** many edits leave orphaned strings in the
  arena. `grid-compact` fixes this. The plan is to run it automatically when
  orphaned bytes exceed 25% of the arena.

## Decisions

1. **Row limit:** none. See §4, Infinite scroll.
2. **Header row:** it is ordinary data, drawn bold and not frozen by
   default (freezing is opt-in). See §2,
   Pipeline.
3. **Formula bar:** one per grid pane, drawn inside `GridElement`, with the
   minibuffer as the fallback when it's hidden. See §5.
4. **`.xlsx`:** planned for later. Each sheet opens as its own buffer, and
   all of them belong to one `Workbook`. See §1, Workbooks and sheets.

## As built (deviations from this plan)

- **Parser:** hand-written on `memchr` rather than `csv-core`, because
  `csv-core` neither reports field byte offsets (needed for Span cells and
  byte-exact saves) nor keeps blank lines. `csv-core` is a dev-dependency;
  a test checks both agree field for field on generated input. Parsing is
  single-threaded (about 630 MB/s), which meets the load-time goals without
  the parallel boundary scan.
- **Memory:** about 1.9× the file size for a 100 MB, 10-column file, not
  1.3×. Plain 8-byte cells can't get lower at about 10 bytes per field.
- **Shaping cache:** no grid-specific LRU. GPUI's line-layout cache already
  reuses identical shaped lines between frames.
- **Positions** are `f64` pixels, not "cell index + pixel offset". `f64` is
  exact far past any reachable row.
- **Raw view toggle:** `grid-toggle-view` (`g r` / `C-c C-t`) switches the
  same buffer between grid and raw text in place. It keeps the file and any
  unsaved changes, but not the undo history. It works for UTF-8 files only.
- **Helix:** the overlay shares the vim motions, plus `x` for rows, `%` for
  all and `d` to clear. Multiple cursors (`src/editor/grid_cursors.rs`): `C`
  / `M-C` add one below / above each cursor, `s` selects matching cells as
  cursors, `,` / `Esc` keep the primary, `M-,` drops it, and `(` / `)`
  rotate it. Motions move them all; `i` / `a` / `c` show the typed text in
  every cursor's cell live and write them all on commit (formulas shifted
  per cell, like copy-down), and `d` clears them, each one undo step.
- **Scrollbars** are position indicators only (not draggable).
