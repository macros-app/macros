;; requires: core

;; grid.scm — key bindings for grid buffers: CSV/TSV files shown as a
;; spreadsheet. The grid itself (storage, rendering, cell editing) is Rust;
;; every *binding* lives here, so all of it is overridable with `define-key`.
;;
;; A grid has two states, decided in Rust:
;;   grid normal — keys are commands, resolved against the keymaps below;
;;   cell insert — keys edit the text of one cell (Esc/Enter/Tab commit,
;;                 C-g cancels; with a range selected, C-RET writes the entry
;;                 into every selected cell, like grid-fill-selection); that
;;                 small editor is native and not rebindable.
;;
;; Keymaps follow the special-buffer convention (see evil.scm's
;; evil-collection section):
;;   "grid-mode"              — the Emacs-style bindings, always consulted;
;;   "evil-normal:grid-mode"  — consulted FIRST under evil-mode;
;;   "helix-normal:grid-mode" — consulted FIRST under helix-mode.
;; Under Emacs bindings a printable key that isn't bound starts editing the
;; cell with that character (spreadsheet behaviour); under evil/helix it's
;; swallowed. Ctrl/Alt chords nobody here binds fall through to the global
;; maps, so C-x C-s, M-x, and friends work as usual.
;;
;; --- Scripting the grid ------------------------------------------------------
;; Rows are 0-based; a column is a 0-based index or a name ("C" or 'C). All
;; of these act on the current buffer when it's a grid.
;;
;; Reads (instant, from the published snapshot):
;;   (grid-buffer?) (grid-cursor) → (row col)   (grid-selection) → (r0 c0 r1 c1)
;;   (grid-dimensions) → (rows cols)   (grid-current-value)   (grid-header?)
;;   (grid-header-names)   (grid-column-type col) → "int"/"float"/"bool"/"date"/"text"
;;   (grid-frozen-rows)   (grid-zoom) → percent   (grid-editing?)
;;   (grid-sheets) → an .xlsx's sheet names   (grid-current-sheet) → the one shown
;;   (grid-cell row col)   (grid-row-values row)   (grid-column-values col)
;;     — any cell, synchronously, for grids up to a million cells (a copy
;;     made after each edit); a bigger grid raises an error: use
;;   (grid-read-range "A1:C50" 'fn) → calls (fn rows), rows a list of lists
;;     of strings (bulk data comes back by callback; ≤ 200,000 cells)
;;   (grid-filtered?)   (grid-row-visible? row)   (grid-visible-row-count)
;;
;; Edits — everything one command does is ONE undo step:
;;   (grid-set-cell row col "text")   (grid-set-cells (list (list row col "text") …))
;;   (grid-insert-rows at n) (grid-delete-rows at n)
;;   (grid-insert-columns at n) (grid-delete-columns at n)
;;   (grid-sort col descending?)          ; skips the header row
;;   (grid-sort-as col descending? 'numeric)   ; or 'alpha 'time 'auto
;;   (grid-map-cells f)  — each selected (visible) cell becomes (f value)
;;   (grid-fill-down) (grid-fill-right)  — Excel's Ctrl+D / Ctrl+R: copy the
;;     selection's first row (column) into the rest of it, formatting too;
;;     formulas move their relative references ($A$1 stays). With one row
;;     (column) or no selection, copy from the cell above (to the left).
;;   (grid-fill-selection)  — Excel's Ctrl+Enter: the cursor cell (or the
;;     entry being typed) into every selected cell, formulas adjusted.
;;   All of them skip rows a filter hides and hidden columns.
;;
;; View:
;;   (grid-move-to row col)   (grid-select-range r0 c0 r1 c1)
;;   (grid-set-column-width col px) (grid-set-row-height row px)   ; 100% px
;;   (grid-freeze-rows n)   (grid-set-header #t)   (grid-set-zoom 125)
;;   (grid-set-column-hidden col #t)
;;   (grid-open-sheet "Costs")  — show another sheet (see "Workbook sheets")
;;   (grid-filter-rows col op value)  — show only rows whose col passes; the
;;     ops are the highlight-rule ones; filters stack; (grid-widen) clears
;;   (grid-column-choices col '("open" "closed"))  — what grid-complete-cell
;;     offers for col (default: the values already in the column)
;;
;; Styling — display only, never changes the file:
;;   (grid-column-face col 'keyword)                 ; a theme face
;;   (grid-format-column col "#,##0.00")             ; also "0.0%", "$#,##0", "0.000 kg"
;;   (grid-highlight-rule col ">" 1000 'error)       ; col #f = every column;
;;     ops: > >= < <= = != contains matches (regex) empty not-empty
;;     between ("10..20") duplicate top bottom (top/bottom take a count).
;;     The last matching rule wins; a face's background tints the cell.
;;   (grid-row-highlight-rule "D" "=" "late" 'error) ; color the whole row
;;   (grid-set-cell-face row col 'warning)  (grid-clear-cell-faces)
;;     A cell's own face beats rules, which beat row rules and column faces.
;;   (grid-stripe-rows #t)       ; zebra rows (the grid-stripe face), or a face
;;   (grid-header-face 'keyword) (grid-column-align col 'center)  ; left right auto
;;   (grid-clear-styles)
;;
;; Formatting by hand — what the M-o keys (`f` under evil/helix) do; it
;; moves with its cells and is undoable. CSV can't hold it, so saving a
;; formatted CSV grid saves it as .xlsx, which keeps it:
;;   (grid-set-cell-bold row col #t)
;;   (grid-set-cell-text-color row col 'red)   ; #f or 'none removes it
;;   (grid-set-cell-fill-color row col 'yellow)
;;     colors: red orange yellow green blue purple — a theme face named
;;     grid-<color> overrides one (its background is the fill)
;;   (grid-set-cell-number-format row col 'currency)
;;     presets: general number currency percent date text — or any Excel
;;     format code, e.g. "#,##0.0", "d mmm yyyy", "0.0%". Display only: the
;;     stored value is untouched. 'general removes it.
;;   (grid-set-number-format 'percent)   ; the selection (or cursor cell)
;;   (grid-clear-cell-formats)
;;
;; Settings (editor-wide):
;;   (grid-default-column-width 80)   (grid-max-autofit-width 400)
;;   (grid-auto-header 'detect)       ; or #t / #f to force row 1's role
;;   (grid-truncate-indicator "…")    ; shown at the end of cut-off text
;;
;; Hooks (add with `add-hook`):
;;   grid-load-hook          (fn buffer-name)  after a grid opens — set widths,
;;                           formats, rules, or freezing per file here.
;;   grid-cell-changed-hook  (fn changes), changes = ((row col old new) …), after
;;                           the user edits cells (not after Scheme's own writes,
;;                           so a hook that writes cells can't loop).
;;   grid-selection-changed-hook  (fn) after the cursor or selection moves;
;;                           read (grid-cursor) / (grid-selection) in it.
;;   grid-mode-hook          (fn) once per grid buffer, like any mode hook.
;;
;; Example: in init.scm, highlight big orders in any sales CSV.
;;   (define (my-sales-grid name)
;;     (when (string-contains? name "sales")
;;       (grid-format-column "E" "$#,##0.00")
;;       (grid-highlight-rule "D" ">" 400 'warning)))
;;   (add-hook "grid-load-hook" "my-sales-grid")

;; File extensions that open as a grid instead of text.
(grid-extensions '("csv" "tsv" "tab" "psv" "xlsx"))

;; --- Emacs-style (always active) ---------------------------------------------
;; Formatting by hand, on the selection or the cursor cell, under `prefix`
;; (M-o, and also `f` under evil/helix). It follows its cells through sorts
;; and row/column edits and undoes like any edit; a formatted CSV saves as
;; .xlsx. The color keys step through red → orange → yellow → green → blue →
;; purple → none. The number-format keys are Excel's Ctrl+Shift ones:
;; ~ general, ! number, $ currency, % percent, # date, @ text; `.`/`,` add or
;; drop a decimal place, `n` prompts for a preset or format code. `.` repeats.
(define (grid-bind-format km prefix)
  (define-keys km
    (map (lambda (b) (list (string-append prefix " " (car b)) (cadr b)))
         '(("b" grid-toggle-bold)
           ("c" grid-cycle-text-color)
           ("f" grid-cycle-fill-color)
           ("d" grid-clear-format)
           ("n" grid-number-format)
           ("~" grid-format-general)
           ("!" grid-format-number)
           ("$" grid-format-currency)
           ("%" grid-format-percent)
           ("#" grid-format-date)
           ("@" grid-format-text)
           ("." grid-increase-decimals)
           ("," grid-decrease-decimals)))))

;; `M-o n` / `f n`: pick a number format for the selection — a preset, or
;; type any Excel format code ("#,##0.0", "d mmm yyyy").
(define-command (grid-number-format)
  (completing-read "Number format (preset or code): "
                   '("general" "number" "currency" "percent" "date" "text")
                   'grid-set-number-format))

(define (grid-bind-common km)
  (define-keys km
    '(("ctrl-f" grid-right)
      ("ctrl-b" grid-left)
      ("ctrl-n" grid-down)
      ("ctrl-p" grid-up)
      ("alt-f" grid-edge-right)
      ("alt-b" grid-edge-left)
      ("alt-n" grid-edge-down)
      ("alt-p" grid-edge-up)
      ("ctrl-a" grid-row-start)
      ("ctrl-e" grid-row-end)
      ("alt-<" grid-goto-first-cell)
      ("alt->" grid-goto-last-cell)
      ("ctrl-home" grid-goto-first-cell)
      ("ctrl-end" grid-goto-last-cell)
      ("ctrl-v" grid-page-down)
      ("alt-v" grid-page-up)
      ("pageup" grid-page-up)
      ("pagedown" grid-page-down)
      ("shift-tab" grid-previous-cell)
      ("shift-up" grid-extend-up)
      ("shift-down" grid-extend-down)
      ("shift-left" grid-extend-left)
      ("shift-right" grid-extend-right)
      ("ctrl-space" grid-select-cells)
      ("ctrl-g" grid-select-cancel)
      ("escape" grid-select-cancel)
      ("ctrl-/" grid-undo)
      ("ctrl-_" grid-undo)
      ("ctrl-shift-/" grid-redo)
      ("ctrl-d" grid-clear)
      ("ctrl-w" grid-cut)
      ("alt-w" grid-yank)
      ("ctrl-y" grid-paste)
      ("ctrl-s" grid-search-forward)
      ("ctrl-=" grid-zoom-in)
      ("ctrl--" grid-zoom-out)
      ("ctrl-0" grid-zoom-reset)
      ("cmd-=" grid-zoom-in)
      ("cmd--" grid-zoom-out)
      ("cmd-0" grid-zoom-reset)
      ("ctrl-c '" grid-edit-popup)
      ("ctrl-x g" grid-goto)
      ("ctrl-c ctrl-s" grid-sort-ascending)
      ("ctrl-c ctrl-r" grid-sort-descending)
      ("ctrl-c ctrl-h" grid-toggle-header)
      ("ctrl-c ctrl-z" grid-toggle-freeze)
      ("ctrl-c ctrl-f" grid-formula-bar)
      ("ctrl-c ctrl-t" grid-toggle-view)
      ("f9" grid-recalculate)
      ("ctrl-`" grid-show-formulas)
      ("ctrl-c ctrl-[" grid-trace-precedents)
      ("ctrl-c ctrl-]" grid-trace-dependents)
      ("ctrl-c ctrl-p" grid-next-problem)
      ;; Structure (Emacs has no convention here; C-c prefixed like org tables).
      ("ctrl-c ctrl-o" grid-insert-row-below)
      ("ctrl-c alt-o" grid-insert-row-above)
      ("ctrl-c ctrl-k" grid-delete-row)
      ("ctrl-c alt-k" grid-delete-column)
      ("ctrl-c ctrl-c" grid-insert-column-right)
      ("ctrl-c >" grid-widen-column)
      ("ctrl-c <" grid-narrow-column)
      ("ctrl-c =" grid-autofit-column)
      ;; Emacs habits.
      ("ctrl-x h" grid-select-all)
      ("ctrl-x ctrl-x" grid-exchange-point-and-mark)
      ("ctrl-l" grid-recenter)
      ("ctrl-r" grid-search-backward)
      ("ctrl-k" grid-kill-to-row-end)
      ("alt-u" grid-upcase)
      ("alt-l" grid-downcase)
      ("alt-c" grid-capitalize)
      ("alt-y" grid-yank-pop)
      ("ctrl-x r k" grid-cut)                         ; rectangles are grid ranges
      ("ctrl-x r y" grid-paste)
      ("ctrl-x r alt-w" grid-yank)
      ("ctrl-alt-i" grid-complete-cell)
      ("ctrl-c j" grid-jump-to-column)
      ;; org-table structure editing.
      ("alt-up" grid-move-row-up)
      ("alt-down" grid-move-row-down)
      ("alt-left" grid-move-column-left)
      ("alt-right" grid-move-column-right)
      ("shift-enter" grid-copy-down)
      ("ctrl-c d" grid-fill-down)                     ; Excel's Ctrl+D
      ("ctrl-c r" grid-fill-right)                    ; Excel's Ctrl+R
      ("ctrl-enter" grid-fill-selection)              ; Excel's Ctrl+Enter
      ("ctrl-c ^" grid-sort-prompt)
      ("ctrl-c ctrl-x h" grid-hide-column)
      ("ctrl-c ctrl-x s" grid-show-columns)
      ;; Narrowing (C-x n, as for text) and filtering.
      ("ctrl-x n n" grid-narrow-to-selection)
      ("ctrl-x n f" grid-filter)
      ("ctrl-x n v" grid-filter-by-value)
      ("ctrl-x n w" grid-widen)
      ;; Export (C-c C-e, like org's export dispatcher).
      ("ctrl-c ctrl-e o" grid-copy-as-org)
      ("ctrl-c ctrl-e m" grid-copy-as-markdown)
      ("ctrl-c ctrl-e j" grid-copy-as-json)
      ("ctrl-c ?" grid-dispatch)
      ("ctrl-c t" helm-grid-sheets)))
  ;; Formatting, on Emacs's facemenu prefix (M-o b is bold there too).
  (grid-bind-format km "alt-o"))

(grid-bind-common "grid-mode")
;; Plain-key navigation for Emacs users: RET edits, TAB moves (both handled as
;; actions), and the delete keys clear.
(define-keys "grid-mode"
  '(("enter" grid-edit-end)
    ("tab" grid-next-cell)
    ("backspace" grid-edit-replace)
    ("delete" grid-clear)
    ("f2" grid-edit-end)
    ("up" grid-up)
    ("down" grid-down)
    ("left" grid-left)
    ("right" grid-right)
    ("home" grid-row-start)
    ("end" grid-row-end)))

;; --- Evil overlay -------------------------------------------------------------
(define (grid-bind-vim km)
  (define-keys km
    '(("h" grid-left)
      ("j" grid-down)
      ("k" grid-up)
      ("l" grid-right)
      ("w" grid-next-cell)
      ("b" grid-previous-cell)
      ("shift-w" grid-edge-right)
      ("shift-b" grid-edge-left)
      ("}" grid-edge-down)
      ("{" grid-edge-up)
      ("0" grid-row-start)
      ("^" grid-row-first-nonempty)
      ("$" grid-row-end)
      ("g g" grid-goto-first-row)
      ("shift-g" grid-goto-last-row)
      ("|" grid-goto-column)
      ("ctrl-d" grid-half-page-down)
      ("ctrl-u" grid-half-page-up)
      ("ctrl-f" grid-page-down)
      ("ctrl-b" grid-page-up)
      (":" evil-ex-command)                  ; :w :q … and :12 / :C7 / :B2:D9 to move
      ("/" grid-search-forward)
      ("n" grid-search-next)
      ("shift-n" grid-search-previous)
      ("i" grid-edit-start)
      ("a" grid-edit-end)
      ("enter" grid-edit-end)
      ("c" grid-edit-replace)
      ("s" grid-edit-replace)
      ("x" grid-clear)
      ("delete" grid-clear)
      ("d d" grid-delete-row)
      ("d c" grid-delete-column)
      ("o" grid-insert-row-below)
      ("shift-o" grid-insert-row-above)
      ("g o" grid-insert-column-right)
      ("g shift-o" grid-insert-column-left)
      ("v" grid-select-cells)
      ("shift-v" grid-select-rows)
      ("ctrl-v" grid-select-columns)
      ("escape" grid-select-cancel)
      ("y" grid-yank)
      ("p" grid-paste)
      ("d x" grid-cut)
      (">" grid-widen-column)
      ("<" grid-narrow-column)
      ("=" grid-autofit-column)
      ("z >" grid-taller-row)
      ("z <" grid-shorter-row)
      ("shift-s" grid-sort-ascending)
      ("g shift-s" grid-sort-descending)
      ("u" grid-undo)
      ("ctrl-r" grid-redo)
      ("." grid-repeat)
      ("g e" grid-edit-popup)
      ("g n" grid-goto)
      ("g h" grid-toggle-header)
      ("g f" grid-toggle-freeze)
      ("g r" grid-toggle-view)
      ("g `" grid-show-formulas)
      ("g [" grid-trace-precedents)
      ("g ]" grid-trace-dependents)
      ("g p" grid-next-problem)
      ("tab" grid-next-cell)
      ("up" grid-up)
      ("down" grid-down)
      ("left" grid-left)
      ("right" grid-right)
      ("?" grid-dispatch)
      ("shift-d" grid-kill-to-row-end)
      ("ctrl-p" grid-yank-pop)               ; evil-paste-pop
      ("g shift-u" grid-upcase)
      ("g u" grid-downcase)
      ("g ~" grid-capitalize)
      ("g c" grid-complete-cell)
      ("g |" grid-jump-to-column)
      ("g s" grid-sort-prompt)
      ("alt-k" grid-move-row-up)
      ("alt-j" grid-move-row-down)
      ("alt-h" grid-move-column-left)
      ("alt-l" grid-move-column-right)
      ("alt-up" grid-move-row-up)
      ("alt-down" grid-move-row-down)
      ("alt-left" grid-move-column-left)
      ("alt-right" grid-move-column-right)
      ("shift-enter" grid-copy-down)
      ("alt-d" grid-fill-down)
      ("alt-r" grid-fill-right)
      ("ctrl-enter" grid-fill-selection)
      ("z z" grid-recenter)
      ("z c" grid-hide-column)
      ("z o" grid-show-columns)
      ("z n" grid-narrow-to-selection)
      ("z w" grid-widen)
      ("g /" grid-filter)
      ("g *" grid-filter-by-value)
      ("g b" grid-toggle-bold)
      ("g t" helm-grid-sheets)))
  (grid-bind-format km "alt-o")
  (grid-bind-format km "f"))

(grid-bind-vim "evil-normal:grid-mode")

;; --- Helix overlay ------------------------------------------------------------
;; Selection-first: the same cell motions, with `x`/`X` selecting rows the
;; way they select lines in text, `d` acting on the selection (clear), and
;; `c` replacing it.
(grid-bind-vim "helix-normal:grid-mode")
(define-keys "helix-normal:grid-mode"
  '(("x" grid-select-rows)
    ("d" grid-cut)
    ;; `d` acts at once under helix, so `d d` / `d c` can't be reached: delete
    ;; rows and columns on `g d` / `g D` instead.
    ("g d" grid-delete-row)
    ("g shift-d" grid-delete-column)
    ("c" grid-edit-replace)
    ("g g" grid-goto-first-row)
    ("g e" grid-goto-last-row)
    ("g h" grid-row-start)
    ("g l" grid-row-end)
    ("%" grid-select-all)
    ("shift-u" grid-redo)))

;; A grid switched to its raw text (`grid-toggle-view`) keeps the same key to
;; switch back.
(define-key "grid-text-mode" "ctrl-c ctrl-t" 'grid-toggle-view)

;; The `ge` / `C-c '` popup: a normal text buffer holding one cell's text.
(define-key "grid-cell-edit-mode" "ctrl-c ctrl-c" 'grid-cell-edit-commit)
(define-key "grid-cell-edit-mode" "ctrl-c ctrl-k" 'grid-cell-edit-abort)

;; `5G` jumps to row 5 and `3|` to column 3: these read the count instead of
;; repeating. Every other grid command under a count simply repeats (`3j`).
(for-each set-count-aware! '(grid-goto-last-row grid-goto-column))

;; --- Commands written in Scheme -----------------------------------------------

;; (grid-map-cells f) — replace every selected cell (skipping rows a filter
;; hides) with (f value), as one edit / one undo step. Reads synchronously,
;; so it needs a grid under grid-cell's size limit.
(define (grid-map-cells f)
  (let* ((sel (grid-selection))
         (dims (grid-dimensions))
         (r0 (list-ref sel 0))
         (c0 (list-ref sel 1))
         (r1 (min (list-ref sel 2) (- (car dims) 1)))
         (c1 (min (list-ref sel 3) (- (cadr dims) 1))))
    (define (row-cells r c acc)
      (if (< c c0)
          acc
          (row-cells r (- c 1) (cons (list r c (f (grid-cell r c))) acc))))
    (define (rows r acc)
      (cond ((< r r0) acc)
            ((grid-row-visible? r) (rows (- r 1) (row-cells r c1 acc)))
            (else (rows (- r 1) acc))))
    (grid-set-cells (rows r1 '()))))

;; Strip leading/trailing whitespace from the selected cells.
(define-command (grid-trim-cells)
  (grid-map-cells string-trim))

;; `C-c ^` / `gs` (and `C-u` before a sort key): org-table's sort prompt —
;; one key picks how the cursor's column compares; a capital sorts descending.
(define (grid-sort-prompt--key k)
  (define kinds '(("a" alpha #f) ("A" alpha #t) ("n" numeric #f) ("N" numeric #t)
                  ("t" time #f) ("T" time #t)))
  (let ((hit (assoc k kinds)))
    (if hit
        (grid-sort-as (cadr (grid-cursor)) (list-ref hit 2) (list-ref hit 1))
        (message "Sort cancelled"))))
(define-command (grid-sort-prompt)
  (read-char "Sort column by: [a]lpha [n]umeric [t]ime (capital: descending) "
             'grid-sort-prompt--key))

;; --- Workbook sheets (.xlsx tabs) ----------------------------------------------
;; (grid-sheets) lists an .xlsx's sheet names, (grid-current-sheet) names the
;; one shown, and (grid-open-sheet "Costs") shows another: the first sheet is
;; the file's own buffer, each other one opens as a buffer of its own,
;; "report.xlsx<Costs>" (reused while it's open).
;;
;; `C-c t` / `g t`: pick a sheet with Helm; the one shown is marked "current".
(define (helm-grid-sheets--candidate s)
  (if (equal? s (grid-current-sheet)) (list s "current") s))
(define-command (helm-grid-sheets)
  (let ((sheets (grid-sheets)))
    (if (null? sheets)
        (message "No sheets here: only .xlsx workbooks have them")
        (helm (list (list "Sheets" (map helm-grid-sheets--candidate sheets)
                          'grid-open-sheet))))))

;; `?` / `C-c ?`: every grid command in one menu (magit-style).
(define-command (grid-dispatch)
  (transient "Grid"
    (list
      (list "Move"
            (list "g" "Go to cell…" 'grid-goto)
            (list "G" "Go to sheet…" 'helm-grid-sheets)
            (list "j" "Jump to column…" 'grid-jump-to-column)
            (list "s" "Search…" 'grid-search-forward)
            (list "r" "Search backward…" 'grid-search-backward)
            (list "l" "Recenter" 'grid-recenter))
      (list "Edit"
            (list "e" "Edit cell in a buffer" 'grid-edit-popup)
            (list "c" "Complete cell…" 'grid-complete-cell)
            (list "d" "Copy down / fill series" 'grid-copy-down)
            (list "D" "Fill down from first row" 'grid-fill-down)
            (list "R" "Fill right from first column" 'grid-fill-right)
            (list "E" "Fill selection with this cell" 'grid-fill-selection)
            (list "U" "Upcase" 'grid-upcase)
            (list "L" "Downcase" 'grid-downcase)
            (list "C" "Capitalize" 'grid-capitalize)
            (list "W" "Trim whitespace" 'grid-trim-cells))
      (list "Rows & columns"
            (list "o" "Insert row below" 'grid-insert-row-below)
            (list "O" "Insert row above" 'grid-insert-row-above)
            (list "i" "Insert column right" 'grid-insert-column-right)
            (list "k" "Delete row" 'grid-delete-row)
            (list "K" "Delete column" 'grid-delete-column)
            (list "p" "Move row up" 'grid-move-row-up)
            (list "n" "Move row down" 'grid-move-row-down)
            (list "b" "Move column left" 'grid-move-column-left)
            (list "f" "Move column right" 'grid-move-column-right)
            (list "h" "Hide column" 'grid-hide-column)
            (list "H" "Show hidden columns" 'grid-show-columns))
      (list "Sort & filter"
            (list "S" "Sort…" 'grid-sort-prompt)
            (list "/" "Filter…" 'grid-filter)
            (list "=" "Filter by this value" 'grid-filter-by-value)
            (list "N" "Narrow to selection" 'grid-narrow-to-selection)
            (list "w" "Widen" 'grid-widen))
      (list "View"
            (list "t" "Header row on/off" 'grid-toggle-header)
            (list "z" "Freeze rows on/off" 'grid-toggle-freeze)
            (list "a" "Auto-fit column" 'grid-autofit-column)
            (list "B" "Formula bar on/off" 'grid-formula-bar)
            (list "v" "Raw text view" 'grid-toggle-view))
      (list "Format"
            (list "*" "Bold on/off" 'grid-toggle-bold)
            (list "T" "Next text color" 'grid-cycle-text-color)
            (list "F" "Next fill color" 'grid-cycle-fill-color)
            (list "#" "Number format…" 'grid-number-format)
            (list "+" "More decimals" 'grid-increase-decimals)
            (list "-" "Fewer decimals" 'grid-decrease-decimals)
            (list "x" "Clear formatting" 'grid-clear-format))
      (list "Copy as"
            (list "X" "Org table" 'grid-copy-as-org)
            (list "M" "Markdown table" 'grid-copy-as-markdown)
            (list "J" "JSON" 'grid-copy-as-json)))))

;; --- Formulas: Scheme functions ------------------------------------------------
;; Make a Scheme function callable from grid formulas:
;;
;;   (define-grid-function (MARGIN price cost)
;;     (/ (- price cost) price))
;;
;; and `=MARGIN(E2, F2)` works in every grid (names are case-insensitive in
;; formulas). Arguments arrive as numbers, strings, #t/#f, "" for an empty
;; cell, an error as its text ("#N/A"), and a range as a list of rows (each a
;; list of values). Return one value; a list is #VALUE!.
;;
;; Grid functions run in their own interpreter (so a slow one never freezes
;; the editor, and one running past `grid-function-timeout` — 120 s unless
;; set — is stopped). That interpreter knows only what's shared with it: a
;; function sees its arguments and nothing else, and helpers it calls must be
;; defined with `define-grid-helper`:
;;
;;   (define-grid-helper (money x) (string-append "$" (number->string x)))
;;
;; Both forms also define the procedure here, so you can try it with M-:.
;; While a call runs its cell shows #CALC!; results are cached per arguments.
(define-syntax define-grid-function
  (syntax-rules ()
    ((_ (name arg ...) body ...)
     (begin
       (define (name arg ...) body ...)
       (grid-register-function "function" (symbol->string 'name)
                               '(define (name arg ...) body ...))))))

(define-syntax define-grid-helper
  (syntax-rules ()
    ((_ (name arg ...) body ...)
     (begin
       (define (name arg ...) body ...)
       (grid-register-function "helper" (symbol->string 'name)
                               '(define (name arg ...) body ...))))))
