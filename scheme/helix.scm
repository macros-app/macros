;; requires: core evil

;; helix.scm — Helix-style selection-first modal editing, built on Macros'
;; keymaps and the multiple-selection core. Like evil.scm, the Rust side only
;; decides *when* plain keys are commands (Normal) vs. text (Insert); every
;; binding below is Scheme and fully overridable. Opt in with (helix-mode) in
;; your config (and (helix-disable) to turn it back off). Loaded after core.scm,
;; so (string-join) and friends exist.
;;
;; The Helix model, the part that makes it Helix: editing is SELECTION-FIRST. A
;; motion *selects* (noun), then a verb acts on the selection — `w` selects the
;; word, `d` deletes it. And there can be MANY selections at once: `C` adds one
;; on the next line, and every motion/verb applies to all of them. The renderer
;; paints each one (the primary in `region`, the rest in `secondary-region`).
;;
;; How it works under the hood: `(selections)` reads every selection from the
;; core as a flat list of anchor/head byte offsets; a Helix command maps each to
;; a new (anchor . head) and hands the whole set back with `(set-selections …)`.
;; The pure position bridges (grapheme-forward, word-forward-start, line-of-pos,
;; …) compute a fresh head per selection without disturbing the others. Reads
;; come from a pre-command snapshot, so a command computes every new position up
;; front, then queues one `set-selections`.

;; Sort the small selection lists by position (core's keyed `sort-by`).
(define (helix-sort lst less?) (sort-by (lambda (x) x) less? lst))

;; --- selection-wide character replace (`r`) --------------------------------
;; Helix's `r` overwrites EVERY character of EVERY selection with the typed
;; char (a bare cursor overwrites the grapheme under it). `evil-replace-char`
;; is the vim one-grapheme-at-point version, so Helix needs its own. Edits run
;; high→low so earlier offsets stay valid; newlines are preserved so a
;; multi-line selection keeps its shape.
(define (helix-replace-dispatch ch)
  (let ((desc (helix-sort (helix-ranges-asc) (lambda (x y) (> (car x) (car y)))))
        (c (car (string->list ch))))
    (for-each
     (lambda (r)
       (replace-region (car r) (cdr r)
         (list->string
          (map (lambda (x) (if (equal? x #\newline) x c))
               (string->list (buffer-substring (car r) (cdr r)))))))
     desc)))
(define (helix-replace-char) (read-char "r-" 'helix-replace-dispatch))

;; --- selection plumbing ----------------------------------------------------
;; `(selections)` → flat (a0 h0 a1 h1 …). Chunk it into ((a0 . h0) (a1 . h1) …).
(define (helix-chunk-pairs lst)
  (if (or (null? lst) (null? (cdr lst)))
      '()
      (cons (cons (car lst) (cadr lst)) (helix-chunk-pairs (cddr lst)))))

;; All selections as (anchor . head) pairs, primary first.
(define (helix-selections)
  (helix-chunk-pairs (selections)))

;; Flatten ((a . h) …) back to (a h …) and install as the new selection set.
(define (helix-flatten pairs)
  (if (null? pairs)
      '()
      (cons (caar pairs) (cons (cdar pairs) (helix-flatten (cdr pairs))))))
(define (helix-set! pairs)
  (when (not (null? pairs))
    (set-selections (helix-flatten pairs))))

;; --- select mode (extend) --------------------------------------------------
;; `v` toggles "extend": motions keep each selection's anchor and move only its
;; head, growing the selection instead of replacing it (Helix's select mode).
(define *helix-extend* #f)
(define (helix-toggle-extend)
  (set! *helix-extend* (not *helix-extend*))
  (message (if *helix-extend* "-- SELECT (extend) --" "")))

;; Selections are stored exclusive — a forward selection's head sits one PAST
;; its last char — but in Helix the cursor is ON that last char. This is the
;; char the cursor is on: what cursor-relative commands (h/l/j/k, A, o, gl, …)
;; must start from, or after `x`/`e`/`f` they'd act one char (or line) late.
(define (helix-cursor s)
  (let ((a (car s)) (h (cdr s)))
    (if (> h a) (grapheme-backward h) h)))

;; The generic motion driver. `head->new` maps a selection's position to its
;; new head. `select?` #t means a non-extend move still leaves a visible
;; selection from the old head to the new (word/object motions); #f collapses to
;; a bare cursor at the new position (plain h/j/k/l moves), which moves from
;; the cursor char (see `helix-cursor`).
(define (helix-move head->new select?)
  (helix-set!
    (map (lambda (s)
           (let* ((a (car s)) (h (cdr s)))
             (cond (*helix-extend* (cons a (head->new h)))
                   (select?        (cons h (head->new h)))
                   (else (let ((nh (head->new (helix-cursor s)))) (cons nh nh))))))
         (helix-selections))))

;; Forward motions whose target char is INCLUDED (e/E, f, t): `cursor->target`
;; maps the cursor char to the target char (or #f: no match, stay put); the new
;; head goes one past it. The selection starts just after the old one.
(define (helix-move-inclusive cursor->target)
  (helix-set!
    (map (lambda (s)
           (let* ((a (car s)) (h (cdr s)) (tg (cursor->target (helix-cursor s))))
             (if (not tg)
                 s
                 (let ((nh (grapheme-forward tg)))
                   (if *helix-extend* (cons a nh) (cons h nh))))))
         (helix-selections))))

;; Backward motions (b/B, F, T): head lands ON the target; the selection keeps
;; the cursor char (a bare cursor's own char is included — `b` mid-word
;; selects the whole word back to its start).
(define (helix-move-backward cursor->target)
  (helix-set!
    (map (lambda (s)
           (let* ((a (car s)) (h (cdr s)) (tg (cursor->target (helix-cursor s))))
             (cond ((not tg) s)
                   (*helix-extend* (cons a tg))
                   (else (cons (if (= a h) (grapheme-forward h) h) tg)))))
         (helix-selections))))

;; --- character / line motions ----------------------------------------------
(define (helix-char-left)  (helix-move grapheme-backward #f))
(define (helix-char-right) (helix-move grapheme-forward  #f))

;; Vertical motion preserves the CHARACTER column on each selection's head —
;; never the byte column. A byte column carried onto a line with multibyte text
;; points *inside* a character, and the rope refuses to slice at such an offset
;; (the same rule `line_offset` and `mc_add_line` follow on the Rust side).
;; `char-column` / `pos-at-char-column` are the pure bridges for it.
(define (helix-line-move dir)
  (let ((last (- (line-count) 1)))
    (helix-move
      (lambda (c)
        (let* ((ln  (line-of-pos c))
               (col (char-column c))
               (tgt (+ ln dir)))
          ;; Clamp rather than wrap: `pos-at-char-column` clamps silently, so
          ;; without this a `k` on line 0 would look like a no-op that still
          ;; reset the column.
          (if (or (< tgt 0) (> tgt last))
              c
              (pos-at-char-column tgt col))))
      #f)))
(define (helix-line-down) (helix-line-move 1))
(define (helix-line-up)   (helix-line-move -1))

(define (helix-goto-line-start) (helix-move line-start-pos #f))
;; gl / $ — ON the line's last char, not the line break (so `gl d` deletes that
;; char instead of joining lines). An empty line has only the break.
(define (helix-line-last-char p)
  (let ((ls (line-start-pos p)) (le (line-end-pos p)))
    (if (> le ls) (grapheme-backward le) le)))
(define (helix-goto-line-end)   (helix-move helix-line-last-char #f))

;; --- word motions (these select) -------------------------------------------
;; w / W select up to the next word start, but like Helix they stop at a line
;; ending: on a line's last word the selection ends before the newline (so
;; `w c` doesn't join lines). Starting on a line break, they first step past
;; it (and any blank lines), so the next `w` selects the next line's word.
(define (helix-skip-line-breaks p)
  (let ((end (point-max)))
    (let loop ((p p))
      (if (and (< p end) (= p (line-end-pos p))) (loop (grapheme-forward p)) p))))
(define (helix-word-start-move next-start)
  (helix-set!
    (map (lambda (s)
           (let* ((a (car s)) (h (cdr s))
                  (from (helix-skip-line-breaks h))
                  (le (line-end-pos from))
                  (nx (next-start from))
                  (tg (if (and (< from le) (> nx le)) le nx)))
             (if *helix-extend* (cons a tg) (cons from tg))))
         (helix-selections))))
(define (helix-word-next) (helix-word-start-move word-forward-start)) ; w
;; `e` / `b` use the CLASS-AWARE primitives (the mirrors of `w`), not Emacs's
;; `word-forward` / `word-backward` — those treat punctuation as a separator, so
;; `e` sailed past a `);` run and `b` disagreed with `w` about where a word starts.
(define (helix-word-end)  (helix-move-inclusive word-forward-end))  ; e
(define (helix-word-prev) (helix-move-backward word-backward-start)) ; b

;; W / E / B — whitespace-delimited WORDs (`foo.bar` is one WORD).
(define (helix-bigword-next) (helix-word-start-move bigword-forward-start))
(define (helix-bigword-end)  (helix-move-inclusive bigword-forward-end))
(define (helix-bigword-prev) (helix-move-backward bigword-backward-start))

;; --- line selection (x) ----------------------------------------------------
;; Extend each selection out to whole lines (incl. the trailing newline). Repeat
;; to grow downward — Helix's `x`.
(define (helix-select-line)
  (helix-set!
    (map (lambda (s)
           (let* ((a (car s)) (h (cdr s))
                  (lo (min a h)) (hi (max a h))
                  (ls (line-start-pos lo))
                  (le (grapheme-forward (line-end-pos hi))))
             (cons ls le)))
         (helix-selections))))

;; `X` — round each selection out to whole lines, but do NOT reach for the next
;; one. Unlike `x` this is idempotent: pressing it again leaves the selection
;; where it is (Helix's extend_to_line_bounds).
(define (helix-extend-to-line-bounds)
  (helix-set!
    (map (lambda (s)
           (let* ((a (car s)) (h (cdr s))
                  (lo (min a h)) (hi (max a h))
                  ;; `hi` is exclusive: when it already sits on a line start the
                  ;; selection ends at the previous line, so step back to find
                  ;; the last line actually covered.
                  (ls (line-start-pos lo))
                  ;; …and take the line break, like `x` (Helix's line bounds)
                  (le (grapheme-forward (line-end-pos (max lo (- hi 1))))))
             (cons ls le)))
         (helix-selections))))

;; --- collapsing / pruning selections ---------------------------------------
;; The effective edit range of a selection: itself, or — when empty (a bare
;; cursor) — the single grapheme under the head, so `d`/`c`/`y` act like Helix
;; even right after a plain move.
(define (helix-range s)
  (let ((a (car s)) (h (cdr s)))
    (if (= a h)
        (cons a (grapheme-forward a))
        (cons (min a h) (max a h)))))

;; `;` — collapse every selection to a bare cursor.
;; On the cursor char, not the exclusive head (which is past the selection).
(define (helix-collapse)
  (helix-set! (map (lambda (s) (let ((c (helix-cursor s))) (cons c c))) (helix-selections))))

;; `,` — drop all but the primary selection.
(define (helix-keep-primary)
  (let ((sels (helix-selections)))
    (when (not (null? sels)) (helix-set! (list (car sels))))))

;; Escape — leave select (extend) mode. Like Helix it keeps the selections:
;; `;` collapses them, `,` drops all but the primary.
(define (helix-escape)
  (when *helix-extend* (message ""))
  (set! *helix-extend* #f))

;; --- yank / delete / change ------------------------------------------------
;; Selection ranges in document order, deduped trivially by sort.
(define (helix-ranges-asc)
  (helix-sort (map helix-range (helix-selections))
              (lambda (x y) (< (car x) (car y)))))

;; Copy every selection (joined by newlines) to the clipboard, silently. `p`
;; then pastes it (evil-paste-after syncs the clipboard into the kill ring).
(define (helix-copy-ranges ranges)
  (clipboard-set
    (string-join (map (lambda (r) (buffer-substring (car r) (cdr r))) ranges) "\n")))

;; `y` — copy every selection and report how many.
(define (helix-yank)
  (set! *helix-extend* #f)          ; d / c / y leave select mode, as in Helix
  (let ((ranges (helix-ranges-asc)))
    (helix-copy-ranges ranges)
    (message (string-append (number->string (length ranges)) " selection(s) yanked"))))

;; Cursor positions left after deleting `asc` (ascending, non-overlapping): each
;; range start, shifted left by the total length deleted before it.
(define (helix-cursors-after-delete asc)
  (let loop ((rs asc) (dropped 0) (acc '()))
    (if (null? rs)
        (reverse acc)
        (let* ((r (car rs)) (s (car r)) (e (cdr r)) (ns (- s dropped)))
          (loop (cdr rs) (+ dropped (- e s)) (cons (cons ns ns) acc))))))

;; Merge ascending (start . end) ranges that overlap or touch into one, so a
;; byte is never deleted / transformed twice by two overlapping selections.
(define (helix-merge-ranges asc)
  (let loop ((rs asc) (acc '()))
    (cond ((null? rs) (reverse acc))
          ((and (not (null? acc)) (<= (car (car rs)) (cdr (car acc))))
           (loop (cdr rs)
                 (cons (cons (car (car acc))
                             (max (cdr (car acc)) (cdr (car rs))))
                       (cdr acc))))
          (else (loop (cdr rs) (cons (car rs) acc))))))

;; Delete every selection (yanking first), then leave one bare cursor per
;; deletion. Overlapping selections are merged first (else the second delete
;; would remove bytes the first already shifted into its range). Deletes run
;; high-offset-first so earlier edits don't shift later ranges; the resulting
;; cursors are computed up front from the snapshot.
(define (helix-delete-core)
  (set! *helix-extend* #f)
  (let* ((asc  (helix-merge-ranges (helix-ranges-asc)))
         (desc (helix-sort asc (lambda (x y) (> (car x) (car y))))))
    (when (not (null? asc))
      (helix-copy-ranges asc)
      (for-each (lambda (r) (delete-region (car r) (cdr r))) desc)
      (helix-set! (helix-cursors-after-delete asc)))))

(define (helix-delete) (helix-delete-core))       ; d
(define (helix-change) (helix-delete-core) (evil-insert-state)) ; c

;; --- entering insert -------------------------------------------------------
;; Collapse each selection to a bare cursor at its start (`i`) or end (`a`), then
;; switch to Insert. Typed text replicates to every cursor (multiple-cursors).
;; Entering Insert always leaves select mode.
(define (helix-insert-before)
  (set! *helix-extend* #f)
  (helix-set! (map (lambda (s) (let ((m (min (car s) (cdr s)))) (cons m m))) (helix-selections)))
  (evil-insert-state))
;; `a` — after the selection; a bare cursor appends after the char under it
;; (but not past the line end).
(define (helix-append-after)
  (set! *helix-extend* #f)
  (helix-set!
   (map (lambda (s)
          (let* ((a (car s)) (h (cdr s))
                 (m (if (= a h)
                        (min (grapheme-forward h) (max h (line-end-pos h)))
                        (max a h))))
            (cons m m)))
        (helix-selections)))
  (evil-insert-state))

;; `o` / `O` — open a line below / above the primary cursor's line and insert.
;; The selection is collapsed to a bare cursor FIRST: `newline` replaces an
;; active selection, so after `w`/`x` a plain move-end-of-line + newline would
;; delete the selected text (and `x o` wiped two lines).
(define (helix-open-below)
  (set! *helix-extend* #f)
  (let* ((s (car (helix-selections))) (le (line-end-pos (helix-cursor s))))
    (helix-set! (list (cons le le))))
  (newline) (evil-insert-state))
(define (helix-open-above)
  (set! *helix-extend* #f)
  (let* ((s (car (helix-selections))) (ls (line-start-pos (min (car s) (cdr s)))))
    (helix-set! (list (cons ls ls))))
  (open-line) (evil-insert-state))

;; `I` / `A` — collapse each selection to its line's first non-blank / end,
;; then insert; typed text lands on every line at once (multiple-cursors).
(define (helix-insert-line-start)
  (set! *helix-extend* #f)
  (helix-set!
   (map (lambda (s)
          (let* ((c (min (car s) (cdr s)))
                 (fnb (evil-skip-blanks-to (line-start-pos c) (line-end-pos c))))
            (cons fnb fnb)))
        (helix-selections)))
  (evil-insert-state))
(define (helix-append-line-end)
  (set! *helix-extend* #f)
  (helix-set!
   (map (lambda (s) (let ((le (line-end-pos (helix-cursor s)))) (cons le le)))
        (helix-selections)))
  (evil-insert-state))

;; --- whole-buffer / case / anchor operations ---------------------------------
;; `%` — one selection spanning the whole buffer (Helix select-all).
(define (helix-select-all) (helix-set! (list (cons (point-min) (point-max)))))

;; `~` / `` ` `` / `M-` ` — case-fold every selection in place (a bare cursor
;; folds the single grapheme under it). Edits run high→low; the selections are
;; left as they were (case folding is length-preserving for ASCII).
(define (helix-fold-ranges folder)
  (let ((desc (helix-sort (helix-ranges-asc) (lambda (x y) (> (car x) (car y))))))
    (for-each (lambda (r)
                (replace-region (car r) (cdr r)
                                (folder (buffer-substring (car r) (cdr r)))))
              desc)))
(define (helix-switch-case)  (helix-fold-ranges string-toggle-case))
(define (helix-to-lowercase) (helix-fold-ranges string-downcase))
(define (helix-to-uppercase) (helix-fold-ranges string-upcase))

;; `M-;` — flip each selection's anchor and head (Helix flip_selections).
(define (helix-flip-selections)
  (helix-set! (map (lambda (s) (cons (cdr s) (car s))) (helix-selections))))

;; --- f / t / F / T char find --------------------------------------------------
;; Selection-first flavor of the evil char find (evil.scm owns the line-local
;; scanner and the `read-char` plumbing): every head moves to the target on
;; its own line, selecting from the old head like the word motions; heads with
;; no match stay put.
(define *helix-find-till* #f)
(define *helix-find-forward* #t)
(define (helix-find-dispatch ch)
  ;; f/t include the target (t: the char before it); F/T land ON it.
  (let ((tgt (lambda (c) (evil-find-target-at c ch *helix-find-till* *helix-find-forward*))))
    (if *helix-find-forward*
        (helix-move-inclusive tgt)
        (helix-move-backward tgt))))
(define (helix-find-arm till forward prompt)
  (set! *helix-find-till* till)
  (set! *helix-find-forward* forward)
  (read-char prompt 'helix-find-dispatch))
(define (helix-find-char-forward)  (helix-find-arm #f #t "f-"))
(define (helix-find-char-backward) (helix-find-arm #f #f "F-"))
(define (helix-till-char-forward)  (helix-find-arm #t #t "t-"))
(define (helix-till-char-backward) (helix-find-arm #t #f "T-"))

;; gs — first non-blank of each head's line (Helix goto_first_nonwhitespace).
(define (helix-goto-first-nonblank)
  (helix-move
   (lambda (h) (evil-skip-blanks-to (line-start-pos h) (line-end-pos h)))
   #f))

;; --- goto (g …) ------------------------------------------------------------
(define (helix-goto-top)    (helix-set! (list (cons 0 0))))
;; ge — the start of the LAST LINE (not the buffer end; a trailing newline's
;; phantom empty line doesn't count).
(define (helix-goto-bottom)
  (let ((s (pos-at-char-column (evil-last-line) 0))) (helix-set! (list (cons s s)))))

;; Scrolling moves the cursor; collapse first so it doesn't drag a selection.
(define (helix-half-page-down) (helix-collapse) (evil-scroll-down))
(define (helix-half-page-up)   (helix-collapse) (evil-scroll-up))
(define (helix-page-down)      (helix-collapse) (scroll-up))
(define (helix-page-up)        (helix-collapse) (scroll-down))

;; --- whole-line block edits (gq reflow, J join) --------------------------------
;; Shared bookkeeping: expand each selection to whole lines (a bare cursor =
;; its current line), merge overlapping ranges, apply `transform` to each
;; block's text with edits running high→low (earlier offsets stay valid), and
;; leave a bare cursor at each block's (shifted) start — helix-delete-core's
;; pattern, except the shift is each block's new-minus-old BYTE length.
;; `extend-single` grows a one-line block to include the next line (for J).
(define (helix-transform-line-blocks transform extend-single)
  (let* ((mx (point-max))
         (asc (helix-sort
               (map (lambda (s)
                      (let* ((lo (min (car s) (cdr s)))
                             (hi (max (car s) (cdr s)))
                             (ls (line-start-pos lo))
                             (le (line-end-pos (max lo (- hi 1)))))
                        (if (and extend-single
                                 (= le (line-end-pos ls))     ; a single line…
                                 (< (+ le 1) mx))             ; …with one below
                            (cons ls (line-end-pos (+ le 1)))
                            (cons ls le))))
                    (helix-selections))
               (lambda (x y) (< (car x) (car y)))))
         (merged (helix-merge-ranges asc))
         ;; ((range . new-text) …) in ascending order.
         (blocks (map (lambda (r)
                        (cons r (transform (buffer-substring (car r) (cdr r)))))
                      merged)))
    (when (not (null? blocks))
      (for-each (lambda (b)
                  (let ((r (car b))) (replace-region (car r) (cdr r) (cdr b))))
                (reverse blocks))
      (helix-set!
       (let loop ((bs blocks) (delta 0) (acc '()))
         (if (null? bs)
             (reverse acc)
             (let* ((r (car (car bs))) (new (cdr (car bs)))
                    (s (+ (car r) delta)))
               (loop (cdr bs)
                     (+ delta (- (utf8-length new) (- (cdr r) (car r))))
                     (cons (cons s s) acc)))))))))

;; gq — Helix's `:reflow` on a key (`:` here is the vim ex bridge, which has no
;; reflow): re-wrap each selection's lines at *fill-column* (fill-text,
;; core.scm).
(define (helix-reflow)
  (helix-transform-line-blocks (lambda (text) (fill-text text *fill-column*)) #f))

;; J — join the selected lines (a bare cursor joins the next line up),
;; collapsing each newline + following indentation to one space.
(define (helix-join-lines)
  (helix-transform-line-blocks
   (lambda (text) (replace-regexp-in-string "\n[ \t]*" " " text))
   #t))

;; =====================================================================
;; Keymap — Normal (selection-first). Plain keys are commands here.
;; =====================================================================
;; motion
(define-keys "helix-normal"
  '(("h" helix-char-left)
    ("l" helix-char-right)
    ("j" helix-line-down)
    ("k" helix-line-up)
    ("w" helix-word-next)
    ("e" helix-word-end)
    ("b" helix-word-prev)
    ("0" helix-goto-line-start)
    ("$" helix-goto-line-end)
    ("x" helix-select-line)
    ;; X — extend to whole lines WITHOUT growing downward. `x` grows a line per
    ;; press; `X` just rounds the current selection out to line bounds, so pressing
    ;; it repeatedly is idempotent (Helix's extend_to_line_bounds). They were the
    ;; same command, which made `X` a second `x`.
    ("shift-x" helix-extend-to-line-bounds)
    ("%" helix-select-all)                   ; % — select whole buffer
    ("f" helix-find-char-forward)            ; f/t/F/T — char find (selects)
    ("shift-f" helix-find-char-backward)
    ("t" helix-till-char-forward)
    ("shift-t" helix-till-char-backward)
    ("g g" helix-goto-top)                   ; gg — top of file
    ("g e" helix-goto-bottom)                ; ge — bottom of file
    ("g h" helix-goto-line-start)
    ("g l" helix-goto-line-end)
    ("g s" helix-goto-first-nonblank)        ; gs — first non-blank
    ("g w" avy-goto-char)                    ; gw — jump by char (avy)
    ("g q" helix-reflow)                     ; gq — reflow selected lines (:reflow)
    ;; selection management
    ("v" helix-toggle-extend)                ; toggle select/extend
    (";" helix-collapse)                     ; collapse to cursors
    ("," helix-keep-primary)                 ; keep only primary
    ("shift-c" mc-add-cursor-below)          ; C — cursor below
    ("escape" helix-escape)
    ;; changes (act on the selection — noun→verb)
    ("d" helix-delete)
    ("c" helix-change)
    ("y" helix-yank)
    ("p" helix-paste-after)                  ; p / P — paste after/before each selection
    ("shift-p" helix-paste-before)
    ("r" helix-replace-char)                 ; r<char> — replace EVERY selected char
    ("u" undo)
    ("shift-u" redo)                         ; U — redo
    ("shift-j" helix-join-lines)             ; J — join selected lines
    ("~" helix-switch-case)                  ; ~ — toggle case
    ("`" helix-to-lowercase)                 ; ` — lowercase
    ("alt-`" helix-to-uppercase)             ; M-` — uppercase
    ("alt-;" helix-flip-selections)          ; M-; — flip anchor/head
    ;; enter insert
    ("i" helix-insert-before)
    ("a" helix-append-after)
    ("shift-i" helix-insert-line-start)      ; I — insert at line start
    ("shift-a" helix-append-line-end)        ; A — append at line end
    ("o" helix-open-below)
    ("shift-o" helix-open-above)
    ;; search & ex
    ("/" evil-search-forward)
    ("n" evil-search-next)
    ("shift-n" evil-search-previous)
    (":" evil-ex-command)
    ;; scrolling (shared with evil)
    ("ctrl-d" helix-half-page-down)
    ("ctrl-u" helix-half-page-up)
    ("ctrl-f" helix-page-down)               ; full page forward
    ("ctrl-b" helix-page-up)                 ; full page back
    ;; Arrow/editing keys move like h/j/k/l. Unbound, they'd fall through to the
    ;; Emacs motions, which EXTEND the active selection instead of moving.
    ("left" helix-char-left)
    ("right" helix-char-right)
    ("up" helix-line-up)
    ("down" helix-line-down)
    ("backspace" helix-char-left)
    ("home" helix-goto-line-start)
    ("end" helix-goto-line-end)
    ("z z" recenter)                         ; zz — center cursor line
    ;; WORD motions (Helix's shifted trio) — whitespace-delimited, selecting.
    ("shift-w" helix-bigword-next)
    ("shift-e" helix-bigword-end)
    ("shift-b" helix-bigword-prev)
    ;; indent / outdent the selected lines
    (">" indent-region)
    ("<" outdent-region)
    ;; LSP navigation — the evil map has had these since day one; Helix's own
    ;; keymap uses the same `gd` and shows docs on `K`.
    ("g d" lsp-find-definition)
    ("g r" lsp-find-references)
    ("shift-k" lsp-hover)
    ;; Same Emacs-chord shadowing as evil-normal: these four Emacs defaults EDIT the
    ;; buffer, and `C-o` in particular silently inserted a newline in Normal state.
    ("ctrl-o" xref-pop-marker)
    ("ctrl-k" evil-undefined)
    ("ctrl-w" evil-undefined)
    ("ctrl-y" evil-undefined)
    ("ctrl-t" evil-undefined)))

;; =====================================================================
;; Keymap — Insert. Plain keys self-insert; Escape returns to Normal,
;; KEEPING any extra cursors (so multi-selection `c`/`i`/`a` survive).
;; =====================================================================
(define-key "helix-insert" "escape" 'helix-normal-state)

;; =====================================================================
;; helix-collection — vim/helix keys on read-only special buffers
;; =====================================================================
;; Mirrors evil-collection (evil.scm): the editor consults a `helix-normal:<major>`
;; overlay BEFORE a special buffer's own keymap, but only under helix-mode (see
;; `modal_collection_prefix` in the core). So whatever we bind here shadows a
;; default key under Helix and is inert otherwise. Reuses evil.scm's mode list
;; and motion helpers (evil.scm loads first); per-buffer specifics (e.g. magit's
;; `x`=discard) live next to each mode's keymap, like the evil overlay does.

;; Vim motion shared by every special buffer: j/k by default (single `g` is
;; refresh in several modes, so a `g g` here would shadow it — left to each
;; mode's own overlay). n/p keep working from the base keymap. The binding set
;; itself is evil.scm's, parameterized over the overlay prefix.
(define (helix-collection-bind-mode mode)
  (modal-collection-bind-mode "helix-normal:" mode))
(for-each helix-collection-bind-mode *evil-collection-modes*)

;; Plain-text result buffers (compile / grep / occur) are read-only text you
;; scroll like a file, so they get the full read-only motion set — the same
;; evil.scm binding set, on the helix overlay prefix. `g g` is left off so
;; single `g` stays free for each mode's own key; jump to the top with C-u or k.
(define (helix-collection-bind-readonly-motion mode)
  (modal-collection-bind-readonly-motion "helix-normal:" mode))
(for-each helix-collection-bind-readonly-motion
          (list "compile-mode" "grep-mode" "occur-mode"))

;; helix-mode / helix-disable flip the modal state machine in the core; expose
;; them to M-x so they're discoverable like evil-mode. Opt in from your config
;; with (helix-mode) in ~/.config/macros/init.scm.
(register-command 'helix-mode)
(register-command 'helix-disable)
