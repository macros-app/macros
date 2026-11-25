;; requires: core

;; hex.scm — key bindings and prompts for hex buffers: a file's bytes shown as
;; offset | hex | ASCII rows, with a data inspector at the bottom. The buffer
;; itself (storage, rendering, typing) is Rust; every *binding* lives here, so
;; all of it is overridable with `define-key`.
;;
;; Opening one:
;;   - Any binary file (not UTF-8, or NUL bytes near the start) opens as hex,
;;     from C-x C-f, the command line, or dragging it onto the window.
;;   - (hex-find-file "path") / M-x hex-find-file-prompt opens any file as hex.
;;   - M-x hex-mode shows the current text buffer as hex; C-c C-t switches
;;     back (when the bytes are UTF-8).
;;
;; Typing (native, not rebindable):
;;   - Hex column: 0-9 a-f write the nibble under the cursor, high then low.
;;   - Text column (Tab switches): printable keys write characters.
;;   - Overwrite by default; Insert (or C-c C-o) toggles inserting bytes.
;;   - Under evil/helix, plain keys are commands until `i` / `R` arms typing;
;;     Escape disarms it.
;;
;; The usual Emacs motion and editing commands act byte-wise here, so C-f/C-b,
;; C-n/C-p, M-f/M-b (by group), C-a/C-e, C-v/M-v, M-</M->, C-SPC, C-x h, C-w,
;; M-w, C-y, C-/ and C-x C-s all work without hex-specific bindings.
;;
;; Keymaps follow the special-buffer convention (see grid.scm):
;;   "hex-mode"              — the Emacs-style bindings, always consulted;
;;   "evil-normal:hex-mode"  — consulted FIRST under evil-mode;
;;   "helix-normal:hex-mode" — consulted FIRST under helix-mode.
;;
;; --- Scripting ---------------------------------------------------------------
;; Reads: (hex-buffer?) (hex-offset) (hex-size) (hex-byte) → integer or #f
;;        (hex-selection) → (start end), end exclusive, or '()
;; Moves: (hex-goto-offset "0x1f0")  ; also "496", "+16", "-0x10", "50%"
;; Search: (hex-search-forward-for "de ad ?? ef") (hex-search-backward-for "\"PNG")
;;   A pattern is hex bytes (spaces and 0x optional, ?? matches any byte), or
;;   text in double quotes. Anything with a non-hex character is searched as
;;   text; an odd number of hex digits ("abc") is an error — quote it.
;; Edits: (hex-fill "00") fills the selection; (hex-insert "de ad") inserts at
;;   point; (hex-type "4142") types as from the keyboard.
;; View: (hex-set-row-width 16) — 0 fits the window; (hex-set-group-size 4).
;; Hook: hex-mode-hook runs once per hex buffer, like any mode hook.
;;
;; Faces: hex-null, hex-whitespace, hex-control, hex-high colour the bytes by
;; kind (printable ASCII uses the foreground). Without them NUL is a faded
;; foreground and the others take the theme's string, number and keyword
;; colours.

;; --- Prompts -----------------------------------------------------------------

(define *hex-last-search* "")

(define (hex-search--prompt dir)
  (string-append "Hex search" dir
                 (if (equal? *hex-last-search* "")
                     " (de ad ?? ef, or \"text\"): "
                     (string-append " (default " *hex-last-search* "): "))))

;; C-s: search forward. An empty answer repeats the last search.
(define-command (hex-search)
  (read-string (hex-search--prompt "") 'hex-search--forward))
(define (hex-search--forward s)
  (cond ((and (equal? s "") (equal? *hex-last-search* "")) (message "Nothing to search for"))
        ;; The last search may come from another hex buffer: search for it
        ;; afresh (from just past point, so a match at point isn't found
        ;; again) rather than asking this buffer for its own last search.
        ((equal? s "") (hex-search--again *hex-last-search* #t))
        (else (set! *hex-last-search* s) (hex-search-forward-for s))))

;; C-r: search backward.
(define-command (hex-search-backward)
  (read-string (hex-search--prompt " backward") 'hex-search--backward))
(define (hex-search--backward s)
  (cond ((and (equal? s "") (equal? *hex-last-search* "")) (message "Nothing to search for"))
        ((equal? s "") (hex-search--again *hex-last-search* #f))
        (else (set! *hex-last-search* s) (hex-search-backward-for s))))

(define (hex-search--again pattern forward?)
  (if forward?
      (begin (hex-forward-byte) (hex-search-forward-for pattern))
      (hex-search-backward-for pattern)))

;; M-g g: go to an offset.
(define-command (hex-goto)
  (read-string "Goto offset (0x1f0, 496, +16, -0x10, 50%): " 'hex-goto-offset))

;; C-c C-f: fill the selection with a repeated byte pattern.
(define-command (hex-fill-selection)
  (read-string "Fill selection with bytes (00, de ad, or \"text\"): " 'hex-fill))

;; C-c i: insert bytes at point.
(define-command (hex-insert-bytes)
  (read-string "Insert bytes (de ad be ef, or \"text\"): " 'hex-insert))

;; M-x hex-find-file-prompt: open any file as hex.
(define-command (hex-find-file-prompt)
  (read-file-name "Find file as hex: " 'hex-find-file))

(register-command 'hex-mode)
(register-command 'hex-view-as-text)
(register-command 'hex-toggle-inspector)
(register-command 'hex-toggle-endian)
(register-command 'hex-toggle-insert)
(register-command 'hex-copy-as-c-array)
(register-command 'hex-copy-as-base64)
(register-command 'hex-copy-as-text)
(register-command 'hex-wider-rows)
(register-command 'hex-narrower-rows)
(register-command 'hex-cycle-group)

;; --- Emacs-style (always active) ---------------------------------------------

(define-keys "hex-mode"
  '(("tab" hex-toggle-column)
    ("shift-tab" hex-toggle-column)
    ("left" hex-backward-byte)
    ("right" hex-forward-byte)
    ("up" hex-previous-row)
    ("down" hex-next-row)
    ("home" hex-row-start)
    ("end" hex-row-end)
    ("pageup" hex-page-up)
    ("pagedown" hex-page-down)
    ("backspace" hex-delete-backward-byte)
    ("delete" hex-delete-byte)
    ("insert" hex-toggle-insert)
    ("enter" hex-next-row)
    ("ctrl-s" hex-search)
    ("ctrl-r" hex-search-backward)
    ("f3" hex-search-next)
    ("shift-f3" hex-search-previous)
    ("alt-g g" hex-goto)
    ("alt-g alt-g" hex-goto)
    ("ctrl-c ctrl-t" hex-view-as-text)
    ("ctrl-c ctrl-o" hex-toggle-insert)
    ("ctrl-c ctrl-i" hex-toggle-inspector)
    ("ctrl-c ctrl-e" hex-toggle-endian)
    ("ctrl-c ctrl-f" hex-fill-selection)
    ("ctrl-c i" hex-insert-bytes)
    ("ctrl-c 0" hex-insert-null)
    ("ctrl-c w" hex-wider-rows)
    ("ctrl-c shift-w" hex-narrower-rows)
    ("ctrl-c g" hex-cycle-group)
    ("ctrl-c c h" hex-copy)
    ("ctrl-c c c" hex-copy-as-c-array)
    ("ctrl-c c b" hex-copy-as-base64)
    ("ctrl-c c t" hex-copy-as-text)
    ("ctrl-l" hex-recenter)))

;; A hex buffer switched to text (`hex-view-as-text`) keeps the same key to
;; switch back.
(define-key "hex-text-mode" "ctrl-c ctrl-t" 'hex-mode)

;; --- Evil / helix overlay -------------------------------------------------------
;; Plain keys are commands; `i` / `R` arm typing (Escape disarms it).
(define (hex-bind-vim km)
  (define-keys km
    '(("h" hex-backward-byte)
      ("j" hex-next-row)
      ("k" hex-previous-row)
      ("l" hex-forward-byte)
      ("w" hex-forward-group)
      ("b" hex-backward-group)
      ("0" hex-row-start)
      ("^" hex-row-start)
      ("$" hex-row-end)
      ("g g" hex-beginning-of-buffer)
      ("shift-g" hex-end-of-buffer)
      ("g o" hex-goto)
      ("ctrl-f" hex-page-down)
      ("ctrl-b" hex-page-up)
      ("ctrl-d" hex-page-down)
      ("ctrl-u" hex-page-up)
      ("i" hex-edit-start)
      ("shift-r" hex-edit-start)
      ("g i" hex-toggle-insert)
      ("v" hex-set-mark)
      ("y" hex-copy)
      ("x" hex-delete-byte)
      ("d" hex-cut)
      ("p" hex-paste)
      ("u" hex-undo)
      ("ctrl-r" hex-redo)
      ("/" hex-search)
      ("?" hex-search-backward)
      ("n" hex-search-next)
      ("shift-n" hex-search-previous)
      ("z z" hex-recenter)
      ;; Ex commands (:w, :wq, :q, …) as in any other buffer.
      (":" evil-ex-command))))

(hex-bind-vim "evil-normal:hex-mode")
(hex-bind-vim "helix-normal:hex-mode")
(define-keys "helix-normal:hex-mode"
  '(("g e" hex-end-of-buffer)
    ("g h" hex-row-start)
    ("g l" hex-row-end)
    ("shift-u" hex-redo)))
