;; requires: core

;; evil-mode — Vim-style modal editing, built on Macros' keymaps. The Rust side
;; only decides *when* plain keys are commands (Normal/Visual) vs. text (Insert);
;; every binding below is Scheme and fully overridable. Emacs keybindings are
;; the default; opt in to modal editing with (evil-mode) in your config (and
;; (evil-disable) to turn it back off). This mirrors GNU Emacs's evil-mode:
;; plain Vim modal editing, no leader key. Add your own leader in init.scm if
;; you want a Doom/Spacemacs-style menu.

;; --- composite operators (commands enqueue in order when a fn is called) ----
;; a — insert after the char under the cursor, but never past the line end: on
;; an empty line `forward-char` would step over the break onto the next line.
(define (evil-append)
  (when (< (point) (line-end-pos (point))) (forward-char))
  (evil-insert-state))
(define (evil-append-line) (move-end-of-line) (evil-insert-state))
(define (evil-insert-line) (evil-first-non-blank) (evil-insert-state)) ; I — first non-blank
(define (evil-open-below)  (move-end-of-line) (newline) (evil-insert-state))
(define (evil-open-above)  (move-beginning-of-line) (open-line) (evil-insert-state))
;; The typed count for a count-aware command (see `set-count-aware!` below),
;; at least 1. `(prefix-count)` itself is 0 when no count was typed.
(define (evil-count) (max 1 (prefix-count)))

;; The last line a linewise op can reach: a buffer ending in "\n" has a phantom
;; empty line after it that Vim doesn't count.
(define (evil-last-line)
  (let ((mx (point-max)) (n (line-count)))
    (if (and (> mx 0) (equal? (buffer-substring (- mx 1) mx) "\n") (> n 1))
        (- n 2)
        (- n 1))))

;; The `n` whole lines from point's line down (clamped at the last line), as
;; (start end eof?): `end` is past the last line's newline, or — when that line
;; is the buffer's last and has no newline — the buffer end, with eof? #t.
(define (evil-lines-range n)
  (let* ((p (point))
         (ln2 (min (evil-last-line) (+ (line-of-pos p) (- n 1))))
         (s (line-start-pos p))
         (le (line-end-pos (pos-at-char-column ln2 0))))
    (if (< le (point-max))
        (list s (+ le 1) #f)
        (list s le #t))))

;; dd / 3dd — delete whole lines linewise (with their newlines) into the
;; register. On the buffer's last line (no newline after it) the newline BEFORE
;; the lines goes instead, so no phantom empty line is left behind; the
;; register still gets plain "line" text and linewise paste adds the break.
(define (evil-delete-line)
  (let ((r (evil-lines-range (evil-count))))
    (evil-delete-lines (car r) (cadr r))))
;; Visual-mode delete/yank are character-wise (the selection's exact range).
(define (evil-visual-delete) (kill-region) (evil-set-charwise) (evil-normal-state))
(define (evil-visual-yank)
  ;; Vim parks the cursor at the START of the yanked text, not at the end where
  ;; the motion left it — `goto-char` before leaving visual state (the region is
  ;; read from the pre-command snapshot, so `region-beginning` is still live).
  (let ((rb (region-beginning)))
    (kill-ring-save) (evil-set-charwise)
    (when (>= rb 0) (goto-char rb)))
  (evil-normal-state))
;; Visual-LINE delete/yank are line-wise: the visual region already covers whole
;; lines (incl. the trailing newline), so a following `p`/`P` pastes the vim way.
;; On the buffer's last line (no newline after it) the region stops at the
;; buffer end; drop the newline BEFORE it too, like `dd` there, so no empty
;; line is left behind.
(define (evil-visual-line-eof-start)   ; region start when the region hits EOF, else #f
  (let ((rb (region-beginning)) (re (region-end)) (mx (point-max)))
    (and (>= rb 0) (= re mx) (> rb 0)
         (not (equal? (buffer-substring (- re 1) re) "\n"))
         rb)))
(define (evil-visual-line-delete)
  (let ((eof (evil-visual-line-eof-start)))
    (kill-region) (evil-set-linewise)
    (when eof (delete-region (- eof 1) eof)))
  (evil-normal-state))
(define (evil-visual-line-yank)
  (let ((rb (region-beginning)))
    (kill-ring-save) (evil-set-linewise)
    (when (>= rb 0) (goto-char rb)))
  (evil-normal-state))
;; Normal-mode yanks (copy → kill ring AND the system clipboard). `kill-ring-save`
;; syncs the OS clipboard, so these reach cmd-v in other apps. Cursor is restored
;; to where the yank started, vim-style. Each marks the yank line- or char-wise
;; so a following `p`/`P` pastes the vim way.
(define (evil-yank-line)
  ;; yy / 3yy — line-wise: grab whole lines *including* their trailing newline,
  ;; so `p` drops them on fresh lines below. The cursor stays put (Vim).
  (let* ((p (point)) (r (evil-lines-range (evil-count))))
    (set-region (car r) (cadr r)) (kill-ring-save) (evil-set-linewise)
    (goto-char p)))

;; --- more operators & motions (the rest of the common Vim verbs) -------------
;; NOTE on the snapshot model: a command script reads buffer state (point,
;; line-bounds, buffer-substring, …) from a snapshot taken BEFORE it runs; the
;; motion/edit commands it calls are queued and applied afterwards. So a helper
;; must compute every position from synchronous reads up front — it can't move
;; then read the new point. `goto-char` also COLLAPSES any selection (it's
;; move-to), so the helpers that use it are bound in normal state only; visual
;; motions go through the extend-aware command procs (backward-char, forward-word…).

;; ^ — first non-blank of the current line. Where `^` and `I` land (Vim leaves
;; the cursor on the first non-blank, not column 0).
(define (evil-skip-blanks-to from end)   ; first non-blank byte in [from end), else end
  (let loop ((i from))
    (if (and (< i end)
             (let ((c (buffer-substring i (+ i 1)))) (or (equal? c " ") (equal? c "\t"))))
        (loop (+ i 1))
        i)))
(define (evil-first-non-blank)
  (let* ((b (line-bounds)) (s (car b)) (e (cadr b)))
    (goto-char (evil-skip-blanks-to s e))))

;; gg / G — first / last line, on its first non-blank. Scanned directly: point
;; reads are a pre-command snapshot, so we can't jump-then-read-the-new-line.
;; `5G` / `5gg` — line 5 (1-based), on its first non-blank.
(define (evil-goto-line-count n)
  (let* ((ln (min (evil-last-line) (max 0 (- n 1))))
         (s (pos-at-char-column ln 0)))
    (goto-char (evil-skip-blanks-to s (line-end-pos s)))))
(define (evil-goto-first-line)
  (if (> (prefix-count) 0) (evil-goto-line-count (prefix-count)) (evil-goto-first-line-0)))
(define (evil-goto-last-line)
  (if (> (prefix-count) 0) (evil-goto-line-count (prefix-count)) (evil-goto-last-line-0)))
(define (evil-goto-first-line-0)
  (let* ((mx (point-max))
         (end (let loop ((i 0))
                (cond ((>= i mx) mx)
                      ((equal? (buffer-substring i (+ i 1)) "\n") i)
                      (else (loop (+ i 1)))))))
    (goto-char (evil-skip-blanks-to 0 end))))
(define (evil-goto-last-line-0)
  (let* ((mx (point-max))
         ;; ignore one trailing newline so a file ending in "\n" doesn't land us
         ;; on a phantom empty line below the text.
         (end (if (and (> mx 0) (equal? (buffer-substring (- mx 1) mx) "\n")) (- mx 1) mx))
         (start (let loop ((i end))
                  (cond ((<= i 0) 0)
                        ((equal? (buffer-substring (- i 1) i) "\n") i)
                        (else (loop (- i 1)))))))
    (goto-char (evil-skip-blanks-to start end))))

;; e — end of word: land ON the last char of the current/next word. Backed by
;; the class-aware `forward-word-end` primitive (Vim `e`), NOT the old
;; forward-char + Emacs `forward-word` + backward-char dance: Emacs's motion
;; treats punctuation as a separator, so that form walked straight past a `);`
;; run instead of stopping on it, and `e` disagreed with `w` about where words
;; are. Kept as a wrapper so the visual maps can bind the same name.
(define (evil-forward-word-end) (forward-word-end))

;; x — delete the char under the cursor INTO the register (Vim `x` = `dl`), so a
;; following `p` can put it back. Char-wise; a no-op on an empty line.
(define (evil-delete-char)
  ;; 3x deletes up to 3 chars, never past the line end.
  (let* ((b (line-bounds)) (eol (cadr b)) (p (point))
         (e (let loop ((i p) (n (evil-count)))
              (if (or (<= n 0) (>= i eol)) i (loop (grapheme-forward i) (- n 1))))))
    (when (< p e)
      (kill-region-between p e) (evil-set-charwise))))

;; D / d$ — delete to end of line.  C / c$ — change to end of line.
(define (evil-delete-to-eol)
  (set-mark) (move-end-of-line) (kill-region) (evil-set-charwise))
(define (evil-change-to-eol)
  (set-mark) (move-end-of-line) (kill-region) (evil-insert-state))

;; cc / S / 3cc — change whole lines: clear their text (keeping one line) and
;; insert on it.
(define (evil-change-line)
  (let* ((r (evil-lines-range (evil-count))) (s (car r))
         (e (if (list-ref r 2) (cadr r) (- (cadr r) 1))))
    (when (< s e) (kill-region-between s e) (evil-set-linewise))
    (goto-char s)
    (evil-insert-state)))

;; s — substitute the char under the cursor (delete it, then insert).
(define (evil-substitute-char) (evil-delete-char) (evil-insert-state))

;; dw / db, cw / cb — word operators. `dw` runs to the next word start (Vim `w`,
;; trailing space included); `cw` stops at the word end (Vim treats `cw` as `ce`).
;; Where a `w` operator stops: the next word start, but clamped to the line end
;; when that start is on a later line (Vim: `dw` on a line's last word deletes
;; to the end of the line; it never joins the next one).
(define (evil-word-op-end p)
  (let ((nx (word-forward-start p)) (le (line-end-pos p)))
    (if (and (> nx le) (< p le)) le nx)))
(define (evil-delete-word)
  (let ((p (point)))
    (kill-region-between p (evil-word-op-end p)) (evil-set-charwise)))
;; cw — Vim treats it as `ce`, but from INSIDE a word it stops at that word's
;; end (on its last char, `ce` would jump to the next word). So: kill to the end
;; of the char run under point (word, punctuation, or blanks).
(define (evil-change-word)
  (let ((p (point)) (r (evil-object-word #t)))
    (when (and r (< p (cdr r)))
      (kill-region-between p (cdr r)) (evil-set-charwise))
    (evil-insert-state)))

;; ~ — toggle the case of the char under the cursor; the replace leaves the
;; cursor on the next char, matching Vim's advance. Only cased letters change.
(define (evil-toggle-case)
  (let* ((b (line-bounds)) (eol (cadr b)) (p (point)))
    (when (< p eol)
      ;; One grapheme, not one byte: `(+ p 1)` inside a multibyte letter (é)
      ;; read as "" and nothing changed. An uncased char just advances, as Vim.
      (let* ((q (grapheme-forward p))
             (c (buffer-substring p q)) (up (string-upcase c)) (dn (string-downcase c)))
        (if (not (equal? up dn))
            (replace-region p q (if (equal? c up) dn up))
            (when (< q eol) (goto-char q)))))))

;; J — join the next line onto this one, collapsing its leading indentation to a
;; single space (Vim `J`). Kept self-contained here (evil.scm doesn't require
;; editing.scm).
;; With a count, `3J` joins 3 lines (2 joins). Like Vim, no space is added when
;; the next line is empty (after its indent), starts with `)`, or the line
;; already ends in a blank. Edits run bottom-up so earlier offsets stay valid.
(define (evil-join-sep e i mx)
  (let ((next (if (< i mx) (buffer-substring i (+ i 1)) "\n")))
    (if (or (equal? next "\n") (equal? next ")")
            (and (> e (line-start-pos e)) (evil-blank-pos? (- e 1))))
        ""
        " ")))
(define (evil-join-line)
  (let* ((mx (point-max))
         (joins (max 1 (- (evil-count) 1)))
         ;; each join, top-down: (eol first-non-blank-of-next-line separator)
         (spots (let loop ((pos (point)) (k joins) (acc '()))
                  (let ((e (line-end-pos pos)))
                    (if (or (<= k 0) (>= e mx))
                        (reverse acc)
                        (let ((i (let skip ((i (+ e 1)))
                                   (if (and (< i mx) (evil-blank-pos? i)) (skip (+ i 1)) i))))
                          (loop (+ e 1) (- k 1)
                                (cons (list e i (evil-join-sep e i mx)) acc))))))))
    (when (not (null? spots))
      (for-each (lambda (sp) (replace-region (car sp) (cadr sp) (list-ref sp 2)))
                (reverse spots))
      ;; Vim leaves the cursor at the last join point: its eol, shifted left by
      ;; what the earlier joins removed.
      (let loop ((ss spots) (removed 0))
        (let ((sp (car ss)))
          (if (null? (cdr ss))
              (goto-char (- (car sp) removed))
              (loop (cdr ss)
                    (+ removed (- (- (cadr sp) (car sp))
                                (string-length (list-ref sp 2)))))))))))

;; gq / gw — Vim's format operator: re-wrap text at *fill-column* (80 by
;; default), continuing indentation and comment leaders. The wrap engine
;; (fill-text) and paragraph-bounds live in core.scm — loaded before this file,
;; and by the tests too. The evil layer binds concrete chords, so the common
;; forms are bound below: gqq / gqgq (current line), gqip / gqap (paragraph at
;; point), gq on a visual selection; gww is gqq with the cursor kept in place.
;; Snapshot model: read the range, wrap it, issue ONE replace-region.
(define (evil-fill-range s e keep-point)
  (when (< s e)
    (let ((new (fill-text (buffer-substring s e) *fill-column*)))
      (replace-region s e new)
      ;; gq leaves the cursor at the start of the formatted text; gw clamps the
      ;; old point into the new text (Vim restores it exactly).
      (goto-char (if keep-point (min (point) (+ s (utf8-length new))) s)))))
(define (evil-fill-line)      (let ((b (line-bounds))) (evil-fill-range (car b) (cadr b) #f)))
(define (evil-fill-paragraph)
  (let ((b (paragraph-bounds (point))))
    (when b (evil-fill-range (car b) (cadr b) #f))))
;; Visual gq: format the selected lines (the selection is rounded out to whole
;; lines; region-end is exclusive, so end-1 finds the last selected line).
(define (evil-visual-fill)
  (let ((rb (region-beginning)) (re (region-end)))
    (when (and (>= rb 0) (< rb re))
      (evil-fill-range (line-start-pos rb) (line-end-pos (max rb (- re 1))) #f)))
  (evil-normal-state))

;; --- paragraph motions ({ / }) ------------------------------------------------
;; Boundary positions live in core.scm (shared with Emacs M-{ / M-}). The
;; visual variants extend via `select-to` (goto-char would collapse).
(define (evil-forward-paragraph)         (goto-char  (paragraph-forward-pos (point))))
(define (evil-backward-paragraph)        (goto-char  (paragraph-backward-pos (point))))
(define (evil-forward-paragraph-visual)  (select-to  (paragraph-forward-pos (point))))
(define (evil-backward-paragraph-visual) (select-to  (paragraph-backward-pos (point))))

;; --- gJ ------------------------------------------------------------------------
;; gJ — join the next line up with NO space inserted (vim gJ): remove just the
;; newline, keeping the next line's indentation.
(define (evil-join-line-no-space)
  (let* ((b (line-bounds)) (e (cadr b)))
    (when (< e (point-max)) (delete-region e (+ e 1)))))

;; --- visual p — paste over the selection ---------------------------------------
;; Replace the selection with the register: delete WITHOUT touching the kill
;; ring (delete-region, not kill-region), then paste before the collapsed
;; cursor. (Vim also yanks the replaced text; that nuance is skipped.)
(define (evil-visual-paste)
  (let ((rb (region-beginning)) (re (region-end)))
    (when (and (>= rb 0) (< rb re))
      (delete-region rb re)
      (evil-paste-before)))
  (evil-normal-state))

;; --- q / @ — keyboard macros in registers -----------------------------------
;; Vim-style recording on the kbd-macro engine (C-x ( / C-x ) / C-x e): `qa`
;; records into register a (`qA` appends), `q` again stops, `@a` replays it
;; (`3@a` three times), `@@` replays the last one, `@:` the last ex command.
;; The toggle's name contains "kbd-macro" so the recorder skips it (exprs
;; mentioning kbd-macro are never recorded into the macro itself). The
;; recording flag is READ FROM THE EDITOR, not tracked here, so `q` stays right
;; when the Emacs chords (C-x ( / C-x )) are used instead.
(define (evil-kbd-macro-toggle)
  (if (kbd-macro-recording?) (end-kbd-macro) (read-char "q-" 'evil-macro-record)))

;; --- f / F / t / T char find (+ ; and , repeat) ---------------------------------
;; Built on the core `read-char` primitive: pressing `f` arms a one-keystroke
;; capture; the next key arrives in the dispatch callback as a string. The
;; motion is line-local, vim-style. `;` repeats the last find, `,` reverses it;
;; the visual variants extend the selection instead of moving.
(define *evil-find-char* #f)     ; last sought char, for ; and ,
(define *evil-find-till* #f)     ; t/T (stop short) vs f/F (land on it)
(define *evil-find-forward* #t)
(define *evil-find-count* 1)     ; `2fx` — the count, captured when armed

;; Byte offset the find lands on, scanning the line of `pos`, or #f. `till`
;; stops one grapheme before (after, when backward) the match. Steps by
;; GRAPHEMES, not bytes: buffer-substring floors a mid-codepoint offset to the
;; char's start, so a byte-stepping backward scan could "match" a multibyte
;; `ch` at a mid-codepoint offset and hand goto-char an unaligned position.
(define (evil-find-target-at pos ch till forward)
  (let* ((ls (line-start-pos pos)) (le (line-end-pos pos)) (w (utf8-length ch)))
    (if forward
        (let loop ((i (grapheme-forward pos)))
          (cond ((or (<= i pos) (>= i le)) #f)
                ((equal? (buffer-substring i (+ i w)) ch)
                 (if till (grapheme-backward i) i))
                (else (loop (grapheme-forward i)))))
        (let loop ((i (grapheme-backward pos)))
          (cond ((or (>= i pos) (< i ls)) #f)
                ((equal? (buffer-substring i (+ i w)) ch)
                 (if till (+ i w) i))
                ((<= i ls) #f)
                (else (loop (grapheme-backward i))))))))

;; The `n`th match from `pos`, or #f when there are fewer (Vim then doesn't
;; move). A repeated `t` would re-find the char it's parked before, so each
;; further step searches from one grapheme on.
(define (evil-find-nth pos ch till forward n)
  (let loop ((p pos) (k n))
    (let ((tgt (evil-find-target-at p ch till forward)))
      (cond ((not tgt) #f)
            ((<= k 1) tgt)
            (else (loop (if till (if forward (grapheme-forward tgt) (grapheme-backward tgt)) tgt)
                        (- k 1)))))))
(define (evil-find-go tgt extend)
  (when tgt (if extend (select-to tgt) (goto-char tgt))))
(define (evil-find-dispatch ch)         ; armed from normal state
  (set! *evil-find-char* ch)
  (evil-find-go (evil-find-nth (point) ch *evil-find-till* *evil-find-forward* *evil-find-count*) #f))
(define (evil-find-dispatch-visual ch)  ; armed from visual states — extends
  (set! *evil-find-char* ch)
  (evil-find-go (evil-find-nth (point) ch *evil-find-till* *evil-find-forward* *evil-find-count*) #t))
(define (evil-find-arm till forward callback prompt)
  (set! *evil-find-till* till)
  (set! *evil-find-forward* forward)
  (set! *evil-find-count* (evil-count))
  (read-char prompt callback))
(define (evil-find-char-forward)         (evil-find-arm #f #t 'evil-find-dispatch "f-"))
(define (evil-find-char-backward)        (evil-find-arm #f #f 'evil-find-dispatch "F-"))
(define (evil-till-char-forward)         (evil-find-arm #t #t 'evil-find-dispatch "t-"))
(define (evil-till-char-backward)        (evil-find-arm #t #f 'evil-find-dispatch "T-"))
(define (evil-find-char-forward-visual)  (evil-find-arm #f #t 'evil-find-dispatch-visual "f-"))
(define (evil-find-char-backward-visual) (evil-find-arm #f #f 'evil-find-dispatch-visual "F-"))
(define (evil-till-char-forward-visual)  (evil-find-arm #t #t 'evil-find-dispatch-visual "t-"))
(define (evil-till-char-backward-visual) (evil-find-arm #t #f 'evil-find-dispatch-visual "T-"))
;; ; and , — repeat in the stored / opposite direction. `,` does not flip the
;; stored direction (vim: two `,` in a row go the same way, not back and forth).
(define (evil-find-repeat-dir forward extend)
  (when *evil-find-char*
    ;; After `tx` point is parked just before the `x`, so searching from point
    ;; re-finds that same `x` and never moves: a repeated till starts one
    ;; grapheme further on (Vim's `;` after `t` goes to before the NEXT `x`).
    (let* ((p (point))
           (from (if *evil-find-till*
                     (if forward (grapheme-forward p) (grapheme-backward p))
                     p)))
      (evil-find-go (evil-find-nth from *evil-find-char* *evil-find-till* forward (evil-count))
                    extend))))
(define (evil-find-repeat)                (evil-find-repeat-dir *evil-find-forward* #f))
(define (evil-find-repeat-reverse)        (evil-find-repeat-dir (not *evil-find-forward*) #f))
(define (evil-find-repeat-visual)         (evil-find-repeat-dir *evil-find-forward* #t))
(define (evil-find-repeat-reverse-visual) (evil-find-repeat-dir (not *evil-find-forward*) #t))

;; --- * / # — search the word under the cursor -----------------------------------
;; Builds a whole-word regex from the word at (or after) point, stores it as
;; the active search via `set-search-query` (so n/N repeat it), and jumps.
(define (evil-word-char-pos? i)   ; byte at i is a word char [A-Za-z0-9_]
  ;; Plain codepoint ranges, not a regex: this runs per BYTE in the word scans
  ;; (`*`, iw/aw), and a regex match per byte is measurably slow on long lines.
  (let ((c (buffer-substring i (+ i 1))))
    (and (> (string-length c) 0)
         (let ((n (char->integer (car (string->list c)))))
           (or (and (>= n 97) (<= n 122))   ; a-z
               (and (>= n 65) (<= n 90))    ; A-Z
               (and (>= n 48) (<= n 57))    ; 0-9
               (= n 95))))))                ; _
(define (evil-word-at-point)      ; (start . end) of the word at/after point, or #f
  (let* ((b (line-bounds)) (ls (car b)) (le (cadr b))
         (start (let loop ((i (point)))   ; scan right to the next word char
                  (cond ((>= i le) #f)
                        ((evil-word-char-pos? i) i)
                        (else (loop (+ i 1)))))))
    (and start
         (let ((s (let loop ((i start))
                    (if (and (> i ls) (evil-word-char-pos? (- i 1))) (loop (- i 1)) i)))
               (e (let loop ((i start))
                    (if (and (< i le) (evil-word-char-pos? i)) (loop (+ i 1)) i))))
           (cons s e)))))
(define (evil-search-word forward)
  (let ((w (evil-word-at-point)))
    (when w
      (set-search-query
       (string-append "\\b" (regexp-quote (buffer-substring (car w) (cdr w))) "\\b")
       forward)
      ;; `#` searches back from the word's START, or mid-word it would just
      ;; find the word under the cursor.
      (when (not forward) (goto-char (car w)))
      (evil-search-next))))
(define (evil-search-word-forward)  (evil-search-word #t))
(define (evil-search-word-backward) (evil-search-word #f))

;; --- % — matching bracket --------------------------------------------------------
;; Jump between a bracket and its match, nesting-aware. The bracket is the char
;; under the cursor, or the first one to the right on the current line (vim).
;; A plain char scan: strings and comments are not understood.
(define (evil-matchit)        (let ((tg (evil-matchit-target))) (when tg (goto-char tg))))
(define (evil-matchit-visual) (let ((tg (evil-matchit-target))) (when tg (select-to tg))))
(define (evil-matchit-target)
  (let* ((b (line-bounds)) (le (cadr b)) (mx (point-max))
         (brackets (list "(" ")" "[" "]" "{" "}"))
         (start (let loop ((i (point)))
                  (cond ((>= i le) #f)
                        ((member (buffer-substring i (+ i 1)) brackets) i)
                        (else (loop (+ i 1)))))))
    (when start
      ;; One buffer read + a local char walk (byte offsets tracked through
      ;; multibyte chars via char-bytes) — not one buffer-substring FFI call
      ;; per byte, which is what a naive scan costs on a large file.
      (let* ((c (buffer-substring start (+ start 1)))
             (pair (cond ((or (equal? c "(") (equal? c ")")) (cons "(" ")"))
                         ((or (equal? c "[") (equal? c "]")) (cons "[" "]"))
                         (else (cons "{" "}"))))
             (open (car pair)) (close (cdr pair))
             (tgt (if (equal? c open)
                      ;; forward to the matching close
                      (let loop ((chars (cdr (string->list (buffer-substring start mx))))
                                 (i (+ start 1)) (d 1))
                        (if (null? chars)
                            #f
                            (let ((cs (list->string (list (car chars))))
                                  (w (char-bytes (car chars))))
                              (cond ((equal? cs open) (loop (cdr chars) (+ i w) (+ d 1)))
                                    ((equal? cs close)
                                     (if (= d 1) i (loop (cdr chars) (+ i w) (- d 1))))
                                    (else (loop (cdr chars) (+ i w) d))))))
                      ;; backward to the matching open (reversed walk, offsets
                      ;; descending: each char occupies [i-w, i))
                      (let loop ((chars (reverse (string->list (buffer-substring 0 start))))
                                 (i start) (d 1))
                        (if (null? chars)
                            #f
                            (let* ((cs (list->string (list (car chars))))
                                   (w (char-bytes (car chars)))
                                   (j (- i w)))
                              (cond ((equal? cs close) (loop (cdr chars) j (+ d 1)))
                                    ((equal? cs open)
                                     (if (= d 1) j (loop (cdr chars) j (- d 1))))
                                    (else (loop (cdr chars) j d)))))))))
        tgt))))

;; --- text objects (iw/aw, quotes, pairs, ip/ap) ----------------------------------
;; `d i w`, `c a (`, `y i "` , visual `i w`, … — each object computes a byte
;; range from the snapshot; the operator issues the edit. ASCII word class and
;; plain char scans (escapes/strings are not understood) — the vim 90% case.

;; Blank / word-char byte predicates (word = [A-Za-z0-9_], vim's default-ish
;; iskeyword). A mid-codepoint byte reads as "" — treated as neither.
(define (evil-blank-pos? i)
  (let ((c (buffer-substring i (+ i 1)))) (or (equal? c " ") (equal? c "\t"))))

;; iw/aw — the run of word chars (or of non-blank punctuation) under point;
;; `a` adds trailing blanks, or leading ones when there are none trailing.
(define (evil-object-word inner)
  (let* ((b (line-bounds)) (ls (car b)) (le (cadr b)) (p (point)))
    (and (< p le)
         (let* ((pred (cond ((evil-blank-pos? p) evil-blank-pos?)
                            ((evil-word-char-pos? p) evil-word-char-pos?)
                            (else (lambda (i) (and (not (evil-blank-pos? i))
                                                   (not (evil-word-char-pos? i)))))))
                (s (let loop ((i p)) (if (and (> i ls) (pred (- i 1))) (loop (- i 1)) i)))
                (e (let loop ((i p)) (if (and (< i le) (pred i)) (loop (+ i 1)) i))))
           (if inner
               (cons s e)
               (let ((e2 (let loop ((i e))
                           (if (and (< i le) (evil-blank-pos? i)) (loop (+ i 1)) i))))
                 (if (> e2 e)
                     (cons s e2)
                     (cons (let loop ((i s))
                             (if (and (> i ls) (evil-blank-pos? (- i 1))) (loop (- i 1)) i))
                           e))))))))

;; i"/a" (and '/`) — the quote pair on the current line containing point, else
;; the next pair to the right; quotes pair up left to right (escapes ignored).
(define (evil-object-quote qch inner)
  (let* ((b (line-bounds)) (ls (car b)) (le (cadr b)) (p (point))
         (positions (let loop ((i ls) (acc '()))
                      (cond ((>= i le) (reverse acc))
                            ((equal? (buffer-substring i (+ i 1)) qch) (loop (+ i 1) (cons i acc)))
                            (else (loop (+ i 1) acc))))))
    (let loop ((ps positions))
      (cond ((or (null? ps) (null? (cdr ps))) #f)
            ((or (and (<= (car ps) p) (<= p (cadr ps)))  ; contains point,
                 (> (car ps) p))                          ; or first pair past it
             (if inner (cons (+ (car ps) 1) (cadr ps)) (cons (car ps) (+ (cadr ps) 1))))
            (else (loop (cddr ps)))))))

;; i(/a( and friends — the innermost open/close pair containing point,
;; nesting-aware, scanning the whole buffer once (byte offsets tracked through
;; multibyte chars via char-bytes).
(define (evil-object-pair open close inner)
  (let ((p (point)))
    (let loop ((chars (string->list (buffer-substring 0 (point-max))))
               (i 0) (stack '()) (best #f))
      (if (null? chars)
          (and best
               (if inner (cons (+ (car best) 1) (cdr best))
                   (cons (car best) (+ (cdr best) 1))))
          (let* ((ch (car chars)) (w (char-bytes ch)) (cs (list->string (list ch))))
            (cond ((equal? cs open) (loop (cdr chars) (+ i w) (cons i stack) best))
                  ((equal? cs close)
                   (if (null? stack)
                       (loop (cdr chars) (+ i w) stack best)
                       (let* ((o (car stack))
                              (hit (and (<= o p) (<= p i)
                                        (or (not best) (> o (car best))))))
                         (loop (cdr chars) (+ i w) (cdr stack)
                               (if hit (cons o i) best)))))
                  (else (loop (cdr chars) (+ i w) stack best))))))))

;; ip/ap — the paragraph as lines (linewise, incl. trailing newline); `a` also
;; takes the blank lines after it.
(define (evil-object-paragraph inner)
  (let ((b (paragraph-bounds (point))) (mx (point-max)))
    (and b
         (let* ((s (car b)) (e (min mx (+ (cadr b) 1))))
           (if inner
               (cons s e)
               (cons s (let loop ((i e))
                         (cond ((>= i mx) mx)
                               ((blank-line-at? i) (loop (min mx (+ (line-end-pos i) 1))))
                               (else i)))))))))

;; A text object as (type start end), type "line" or "char". The tree-sitter
;; objects decide their own type; of the rest only paragraphs are linewise.
(define (evil-object-range-typed kind obj)
  (let ((inner (equal? kind "i")))
    (cond ((equal? obj "function") (evil-ts-typed (evil-ts-object "function" inner)))
          ((equal? obj "class")    (evil-ts-typed (evil-ts-object "class" inner)))
          ((equal? obj "argument") (evil-ts-typed (evil-ts-argument inner)))
          ((equal? obj "comment")  (evil-ts-typed (evil-ts-comment inner)))
          (else
           (let ((r (evil-object-range kind obj)))
             (and r (list (if (equal? obj "para") "line" "char") (car r) (cdr r))))))))
(define (evil-ts-typed r) (and r (list (car r) (cadr r) (cddr r))))
(define (evil-object-range kind obj)
  (let ((inner (equal? kind "i")))
    (cond ((member obj '("function" "class" "argument" "comment"))
           (let ((r (evil-object-range-typed kind obj))) (and r (cons (cadr r) (caddr r)))))
          ((equal? obj "word")    (evil-object-word inner))
          ((equal? obj "para")    (evil-object-paragraph inner))
          ((equal? obj "dquote")  (evil-object-quote "\"" inner))
          ((equal? obj "squote")  (evil-object-quote "'" inner))
          ((equal? obj "btick")   (evil-object-quote "`" inner))
          ((equal? obj "paren")   (evil-object-pair "(" ")" inner))
          ((equal? obj "brace")   (evil-object-pair "{" "}" inner))
          ((equal? obj "bracket") (evil-object-pair "[" "]" inner))
          ((equal? obj "angle")   (evil-object-pair "<" ">" inner))
          (else #f))))

;; The operator half: op is "change" / "delete" / "yank" / "select" (visual).
;; Deletes go through kill-region-between (register + clipboard, like d/x);
;; yanks select-then-save so the kill ring and OS clipboard both get it.
(define (evil-object-cmd op kind obj)
  (let* ((typed (evil-object-range-typed kind obj))
         (r (and typed (cons (cadr typed) (caddr typed)))))
    (when (and r (< (car r) (cdr r)))
      (let ((s (car r)) (e (cdr r)))
        (if (equal? (car typed) "line") (evil-set-linewise) (evil-set-charwise))
        (cond ((equal? op "delete") (kill-region-between s e))
              ((equal? op "change") (kill-region-between s e) (evil-insert-state))
              ((equal? op "yank")   (set-region s e) (kill-ring-save) (goto-char s))
              ;; Visual is inclusive of the char under the cursor, so park the
              ;; cursor ON the object's last char, not just past it.
              ((equal? op "select") (set-region s (grapheme-backward e))))))))

;; Object keys → object names; every (c/d/y) × (i/a) × key chord is generated
;; below, plus visual-mode `i <key>` / `a <key>` selection. b/B are the vim
;; aliases for (/{.
(define *evil-text-objects*
  (list (cons "w" "word") (cons "p" "para")
        (cons "\"" "dquote") (cons "'" "squote") (cons "`" "btick")
        (cons "(" "paren") (cons ")" "paren") (cons "b" "paren")
        (cons "{" "brace") (cons "}" "brace") (cons "shift-b" "brace")
        (cons "[" "bracket") (cons "]" "bracket")
        (cons "<" "angle") (cons ">" "angle")
        ;; tree-sitter objects (af if ac ic aa ia a/ i/)
        (cons "f" "function") (cons "c" "class") (cons "a" "argument") (cons "/" "comment")))
(for-each
 (lambda (to)
   (for-each
    (lambda (kind)
      (define-key "evil-visual"
        (string-append kind " " (car to))
        (string-append "evil-object-cmd \"select\" \"" kind "\" \"" (cdr to) "\"")))
    (list "i" "a")))
 *evil-text-objects*)

;; --- the operator grammar (d/c/y/>/</=/gu/gU/g~/gq/gw/g?/gc × any motion) -----
;; Vim's `{count}{operator}{count}{motion}`: an operator key is bound to
;; `(evil-operator "name")`, which the editor turns into OPERATOR-PENDING state;
;; the next keys resolve against the "evil-operator" keymap (motions + text
;; objects, below), with digits as the motion count and `v`/`V` forcing the
;; motion charwise/linewise. The doubled operator (`dd`, `>>`, `gUU`) is "the
;; current line(s)". The editor then dispatches ONE self-contained call:
;;
;;   (evil-operate "delete" 6 "" (lambda () (evil-motion "w")))   ; 2d3w
;;
;; so a macro or `.` replays it with no pending state. `evil-operate` runs the
;; motion thunk (which returns `(type from to)` computed from the snapshot),
;; turns it into a byte range per Vim's exclusive/inclusive/linewise rules, and
;; applies the operator. Add your own operator with `evil-define-operator` and
;; bind a key to `evil-operator "name"`; add a motion with `evil-define-motion`.

(define *evil-motion-count* 0)        ; count for the running motion (0 = none)
(define *evil-op-name* "")            ; the operator the motion runs under
(define (evil-mcount) (max 1 *evil-motion-count*))

;; Motion results: (list type from to). `type` is "exclusive", "inclusive",
;; "line", "exclusive!" (exclusive, but skip Vim's end-of-line adjustment), or
;; "obj-char" / "obj-line" (a text object: an absolute range).
(define (evil-mres type to) (list type (point) to))

;; Apply `f` (pos → pos) `n` times, stopping early once it stops moving.
(define (evil-repeat-pos f p n)
  (let loop ((p p) (k n))
    (if (<= k 0)
        p
        (let ((q (f p)))
          (if (= q p) p (loop q (- k 1)))))))

;; First non-blank byte of the line holding `pos`.
(define (evil-first-nonblank-pos pos)
  (evil-skip-blanks-to (line-start-pos pos) (line-end-pos pos)))

;; Byte offset at char column `col` (clamped) on line `ln`.
(define (evil-line-col-pos ln col) (pos-at-char-column ln col))

;; Target of a vertical motion to line `ln`, keeping point's column.
(define (evil-vertical-target ln)
  (evil-line-col-pos (max 0 (min ln (evil-last-line))) (char-column (point))))

;; `w`/`W` under an operator: the n-th next word start, except that when the
;; last word moved over ends its line the range stops at that line's end (Vim:
;; `dw` on a line's last word never joins the next line).
(define (evil-word-op-target p n step)
  (let* ((q (evil-repeat-pos step p (- n 1)))
         (nx (step q))
         (le (line-end-pos q)))
    (if (and (> nx le) (< q le)) le nx)))

;; `cw` / `cW`: Vim's special case — like `ce`, but from inside a word only
;; to that word's end (the char run under point).
(define (evil-change-word-target p n bigword)
  (let* ((run-end (if bigword
                      (let ((le (line-end-pos p)))
                        (let loop ((i p))
                          (if (and (< i le) (not (evil-blank-pos? i))) (loop (+ i 1)) i)))
                      (cdr (or (evil-object-word #t) (cons p (grapheme-forward p))))))
         (end-step (if bigword bigword-forward-end word-forward-end)))
    (let loop ((r run-end) (k (- n 1)))
      (if (<= k 0)
          r
          (loop (grapheme-forward (end-step (grapheme-backward r))) (- k 1))))))

;; --- sentences ( ) -------------------------------------------------------------
;; A sentence ends at . ! or ? (plus any closing )]"' chars) followed by
;; whitespace; a blank line also starts one. The next sentence starts at the
;; first non-blank after that.
(define *evil-sentence-re* "[.!?][)\\]\"']*[ \t\n]+[^ \t\n]|\n[ \t]*\n[ \t\n]*[^ \t\n]")
(define (evil-sentence-starts from to)
  (let loop ((m (buffer-regex-matches *evil-sentence-re* from to)) (acc '()))
    (if (or (null? m) (null? (cdr m)))
        (reverse acc)
        (loop (cddr m) (cons (grapheme-backward (cadr m)) acc)))))
(define (evil-sentence-forward-pos p)
  (let* ((mx (point-max))
         (starts (evil-sentence-starts p (min mx (+ p 20000)))))
    (let loop ((ss starts))
      (cond ((null? ss) mx)
            ((> (car ss) p) (car ss))
            (else (loop (cdr ss)))))))
(define (evil-sentence-backward-pos p)
  (let* ((from (max 0 (- p 20000)))
         (starts (cons from (evil-sentence-starts from p))))
    (let loop ((ss starts) (best 0))
      (cond ((null? ss) best)
            ((< (car ss) p) (loop (cdr ss) (car ss)))
            (else best)))))
(define (evil-forward-sentence)
  (goto-char (evil-repeat-pos evil-sentence-forward-pos (point) (evil-count))))
(define (evil-backward-sentence)
  (goto-char (evil-repeat-pos evil-sentence-backward-pos (point) (evil-count))))
(define (evil-forward-sentence-visual)
  (select-to (evil-repeat-pos evil-sentence-forward-pos (point) (evil-count))))
(define (evil-backward-sentence-visual)
  (select-to (evil-repeat-pos evil-sentence-backward-pos (point) (evil-count))))

;; H / M / L — screen lines (top / middle / bottom of the window).
;; `(visible-lines)` is (first end) with `end` EXCLUSIVE and overscanned: Rust
;; computes end = first + ceil(viewport/line-height) + 1, clamped to the line
;; count. So unclamped, the last FULLY visible line is end - 3 (the partial
;; bottom row and the overscan row excluded; off by one only when the viewport
;; is an exact multiple of the line height). When clamped at the buffer end the
;; buffer's last line is (at most a row or two past) the bottom — use it.
(define (evil-visible-bottom vl)
  (let ((top (car vl)) (end (cadr vl)))
    (if (>= end (line-count))
        (evil-last-line)
        (min (evil-last-line) (max top (- end 3))))))
(define (evil-screen-line which n)
  (let* ((vl (visible-lines)) (top (car vl)) (bot (evil-visible-bottom vl)))
    (cond ((equal? which "H") (min bot (+ top (- n 1))))
          ((equal? which "L") (max top (- bot (- n 1))))
          (else (quotient (+ top bot) 2)))))

;; User-defined motions: name → thunk returning a motion result.
(define *evil-user-motions* (hash))
(define (evil-define-motion name thunk)
  (set! *evil-user-motions* (hash-insert *evil-user-motions* name thunk)))

;; The motion table. Every motion computes from the snapshot (point, buffer)
;; and the motion count; #f means "fails" (Vim beeps; the operator is dropped).
;; Built-ins live in their own hash, name → (lambda (p n raw) …) with p = point,
;; n = the motion count (at least 1) and raw = the typed count (0 when none).
;; They are consulted BEFORE `evil-define-motion` entries, so a user motion can
;; add a name but not shadow a built-in one.
(define *evil-builtin-motions* (hash))
(define (evil-builtin-motion name proc)
  (set! *evil-builtin-motions* (hash-insert *evil-builtin-motions* name proc)))
(define (evil-motion name)
  (let ((builtin (hash-try-get *evil-builtin-motions* name)))
    (if builtin
        (builtin (point) (evil-mcount) *evil-motion-count*)
        (let ((user (hash-try-get *evil-user-motions* name)))
          (and user (user))))))

;; The n-th line below point (clamped to the last line), at column 0.
(define (evil-motion-nth-line-start p n)
  (pos-at-char-column (min (evil-last-line) (+ (line-of-pos p) (- n 1))) 0))

;; Motions that just step a position function `n` times: (name type step).
(for-each
 (lambda (row)
   (let ((type (cadr row)) (step (caddr row)))
     (evil-builtin-motion (car row)
                          (lambda (p n raw) (evil-mres type (evil-repeat-pos step p n))))))
 (list (list "b"  "exclusive" word-backward-start)
       (list "B"  "exclusive" bigword-backward-start)
       (list "e"  "inclusive" word-forward-end)
       (list "E"  "inclusive" bigword-forward-end)
       (list "ge" "inclusive" word-backward-end)
       (list "gE" "inclusive" bigword-backward-end)
       (list "}"  "exclusive" paragraph-forward-pos)
       (list "{"  "exclusive" paragraph-backward-pos)
       (list ")"  "exclusive" evil-sentence-forward-pos)
       (list "("  "exclusive" evil-sentence-backward-pos)))

(evil-builtin-motion "line"                    ; dd / cc / yy / >> …
  (lambda (p n raw) (list "line" p (evil-motion-nth-line-start p n))))
(evil-builtin-motion "_"
  (lambda (p n raw) (list "line" p (evil-motion-nth-line-start p n))))
(evil-builtin-motion "h"
  (lambda (p n raw)
    (let ((ls (line-start-pos p)))
      (and (> p ls)
           (evil-mres "exclusive"
                      (evil-repeat-pos (lambda (q) (if (> q ls) (grapheme-backward q) q)) p n))))))
(evil-builtin-motion "l"
  (lambda (p n raw)
    (let ((le (line-end-pos p)))
      (and (< p le)
           (evil-mres "exclusive"
                      (evil-repeat-pos (lambda (q) (if (< q le) (grapheme-forward q) q)) p n))))))
(evil-builtin-motion "j"
  (lambda (p n raw)
    (let ((ln (line-of-pos p)))
      (and (< ln (evil-last-line)) (list "line" p (evil-vertical-target (+ ln n)))))))
(evil-builtin-motion "k"
  (lambda (p n raw)
    (let ((ln (line-of-pos p)))
      (and (> ln 0) (list "line" p (evil-vertical-target (- ln n)))))))
(evil-builtin-motion "+"
  (lambda (p n raw)
    (let ((ln (line-of-pos p)))
      (and (< ln (evil-last-line))
           (list "line" p (evil-first-nonblank-pos (evil-vertical-target (+ ln n))))))))
(evil-builtin-motion "-"
  (lambda (p n raw)
    (let ((ln (line-of-pos p)))
      (and (> ln 0) (list "line" p (evil-first-nonblank-pos (evil-vertical-target (- ln n))))))))
;; w / W: under `c` on a non-blank, Vim's cw acts like ce.
(define (evil-word-motion p n bigword)
  (if (and (equal? *evil-op-name* "change") (not (evil-blank-pos? p)) (< p (line-end-pos p)))
      (evil-mres "exclusive!" (evil-change-word-target p n bigword))
      (evil-mres "exclusive" (evil-word-op-target p n (if bigword bigword-forward-start word-forward-start)))))
(evil-builtin-motion "w" (lambda (p n raw) (evil-word-motion p n #f)))
(evil-builtin-motion "W" (lambda (p n raw) (evil-word-motion p n #t)))
(evil-builtin-motion "0" (lambda (p n raw) (evil-mres "exclusive" (line-start-pos p))))
(evil-builtin-motion "^" (lambda (p n raw) (evil-mres "exclusive" (evil-first-nonblank-pos p))))
(evil-builtin-motion "$"
  (lambda (p n raw) (evil-mres "exclusive!" (line-end-pos (evil-motion-nth-line-start p n)))))
(evil-builtin-motion "g_"
  (lambda (p n raw)
    (let* ((ls (evil-motion-nth-line-start p n))
           (le (line-end-pos ls))
           (last (let loop ((i le))
                   (if (and (> i ls) (evil-blank-pos? (- i 1))) (loop (- i 1)) i))))
      (evil-mres "exclusive!" last))))
(evil-builtin-motion "|"
  (lambda (p n raw) (evil-mres "exclusive" (evil-line-col-pos (line-of-pos p) (- n 1)))))
(evil-builtin-motion "gg"
  (lambda (p n raw)
    (list "line" p (evil-first-nonblank-pos
                    (pos-at-char-column (if (> raw 0) (min (evil-last-line) (- raw 1)) 0) 0)))))
(evil-builtin-motion "G"
  (lambda (p n raw)
    (list "line" p (evil-first-nonblank-pos
                    (pos-at-char-column (if (> raw 0) (min (evil-last-line) (- raw 1)) (evil-last-line)) 0)))))
(evil-builtin-motion "%"
  (lambda (p n raw)
    (if (> raw 0)
        ;; N% → line ceil(N × lines / 100), 1-based, over the real lines (not
        ;; the phantom one after a trailing newline); `evil-last-line` is 0-based.
        (let* ((nlines (+ (evil-last-line) 1))
               (ln (min nlines (quotient (+ (* raw nlines) 99) 100))))
          (list "line" p (pos-at-char-column (max 0 (- ln 1)) 0)))
        (let ((tg (evil-matchit-target))) (and tg (evil-mres "inclusive" tg))))))
(for-each
 (lambda (which)
   (evil-builtin-motion which
     (lambda (p n raw)
       (list "line" p (evil-first-nonblank-pos (pos-at-char-column (evil-screen-line which n) 0))))))
 (list "H" "M" "L"))
(evil-builtin-motion ";" (lambda (p n raw) (evil-find-motion *evil-find-forward* n #t)))
(evil-builtin-motion "," (lambda (p n raw) (evil-find-motion (not *evil-find-forward*) n #t)))
(evil-builtin-motion "n" (lambda (p n raw) (evil-search-motion (search-query) (search-forward?) n)))
(evil-builtin-motion "N" (lambda (p n raw) (evil-search-motion (search-query) (not (search-forward?)) n)))
(evil-builtin-motion "*" (lambda (p n raw) (evil-search-word-motion #t n)))
(evil-builtin-motion "#" (lambda (p n raw) (evil-search-word-motion #f n)))

;; f/F/t/T as operator motions: `(evil-motion-find "f" "x")` (the editor fills
;; in the char). f/t are inclusive, F/T exclusive — as in Vim.
(define (evil-motion-find kind ch)
  (set! *evil-find-char* ch)
  (set! *evil-find-till* (or (equal? kind "t") (equal? kind "T")))
  (set! *evil-find-forward* (or (equal? kind "f") (equal? kind "t")))
  (evil-find-motion *evil-find-forward* (evil-mcount) #f))
;; `repeat` (; and ,): a till re-search starts one grapheme on, as in
;; `evil-find-repeat-dir`, or it re-finds the char point is parked before.
(define (evil-find-motion forward n repeat)
  (and *evil-find-char*
       (let* ((p (point))
              (from (if (and repeat *evil-find-till*)
                        (if forward (grapheme-forward p) (grapheme-backward p))
                        p))
              (tg (evil-find-nth from *evil-find-char* *evil-find-till* forward n)))
         (and tg (evil-mres (if forward "inclusive" "exclusive") tg)))))

;; Mark motions under an operator: `d'a` (linewise) / `` d`a `` (exclusive).
(define (evil-motion-mark m exact)
  (let ((pos (mark-position (if (equal? m "`") "'" m))))
    (and (>= pos 0)
         (if exact (evil-mres "exclusive" pos) (list "line" (point) pos)))))

;; H / M / L in Normal / Visual: the top / middle / bottom screen line (with a
;; count, that many lines in from the top / bottom), on its first non-blank.
(define (evil-screen-goto which extend)
  (let ((pos (evil-first-nonblank-pos (pos-at-char-column (evil-screen-line which (evil-count)) 0))))
    (if extend (select-to pos) (goto-char pos))))
(define (evil-window-top)           (evil-screen-goto "H" #f))
(define (evil-window-middle)        (evil-screen-goto "M" #f))
(define (evil-window-bottom)        (evil-screen-goto "L" #f))
(define (evil-window-top-visual)    (evil-screen-goto "H" #t))
(define (evil-window-middle-visual) (evil-screen-goto "M" #t))
(define (evil-window-bottom-visual) (evil-screen-goto "L" #t))

;; Search motions (`d/foo`, `dn`): exclusive, to the n-th match.
(define (evil-search-motion pattern forward n)
  (and (not (equal? pattern ""))
       (let loop ((from (point)) (k n))
         (let ((hit (regex-search-pos pattern from forward)))
           (cond ((< hit 0) #f)
                 ((<= k 1) (evil-mres "exclusive" hit))
                 (else (loop hit (- k 1))))))))
(define (evil-motion-search pattern forward)
  (set-search-query pattern forward)
  (evil-search-motion pattern forward (evil-mcount)))
(define (evil-search-word-motion forward n)
  (let ((w (evil-word-at-point)))
    (and w
         (let ((pat (string-append "\\b" (regexp-quote (buffer-substring (car w) (cdr w))) "\\b")))
           (set-search-query pat forward)
           (evil-search-motion pat forward n)))))

;; Text objects under an operator: `(evil-object "i" "word")`.
(define (evil-object kind obj)
  (let ((r (evil-object-range-typed kind obj)))
    (and r (< (cadr r) (caddr r))
         (list (if (equal? (car r) "line") "obj-line" "obj-char") (cadr r) (caddr r)))))

;; The range of a motion result: (list kind start end), kind "char" or
;; "line" (a line range runs from a line start to past the last line's
;; newline, or to the buffer end). `force` is "", "v" or "V".
(define (evil-line-range a b)
  (let ((s (line-start-pos a)) (le (line-end-pos b)))
    (list "line" s (if (< le (point-max)) (+ le 1) le))))
(define (evil-motion->range m force)
  (let* ((a0 (cadr m)) (b0 (caddr m))
         (a (min a0 b0)) (b (max a0 b0))
         (type (car m))
         (type (cond ((equal? force "V") (if (equal? type "obj-char") "obj-line!" "line"))
                     ((equal? force "v")
                      (cond ((equal? type "line") "exclusive!")
                            ((equal? type "inclusive") "exclusive!")
                            ((equal? type "obj-line") "obj-char")
                            ((equal? type "obj-char") "obj-char")
                            (else "inclusive")))
                     (else type))))
    (cond
      ((equal? type "obj-char") (list "char" a b))
      ((equal? type "obj-line") (list "line" a b))
      ((equal? type "obj-line!") (evil-line-range a (max a (- b 1))))
      ((equal? type "line") (evil-line-range a b))
      ((equal? type "inclusive")
       (list "char" a (if (< b (point-max)) (grapheme-forward b) b)))
      ((equal? type "exclusive!") (list "char" a b))
      ;; Exclusive, with Vim's `:help exclusive` rules: an end in column 0 of
      ;; a later line moves back to the end of the previous line; if the start
      ;; is also at/before its line's first non-blank, the motion turns
      ;; linewise (`d}` from a paragraph's first column deletes whole lines).
      ((and (> b a) (= b (line-start-pos b)) (> (line-of-pos b) (line-of-pos a)))
       (let ((b2 (line-end-pos (- b 1))))
         (if (<= a (evil-first-nonblank-pos a))
             (evil-line-range a b2)
             (list "char" a b2))))
      (else (list "char" a b)))))

;; The linewise delete shared by `dd`, `dj`, `dip`, … over [s, e): on the
;; buffer's last line (no newline after it) the newline BEFORE the lines goes
;; too, so no phantom empty line is left; the cursor lands on the first
;; non-blank of the line that slides into place.
(define (evil-delete-lines s e)
  (let ((eof (and (= e (point-max))
                  (or (= e s) (not (equal? (buffer-substring (- e 1) e) "\n"))))))
    (kill-region-between s e)
    (evil-set-linewise)
    (if (and eof (> s 0))
        (let ((ls (line-start-pos (- s 1))))
          (delete-region (- s 1) s)
          (goto-char (evil-skip-blanks-to ls (- s 1))))
        (if (and (= e (point-max)) (> s 0))
            (goto-char (evil-skip-blanks-to (line-start-pos (- s 1)) (- s 1)))
            (goto-char (+ s (- (evil-skip-blanks-to e (line-end-pos e)) e)))))))

;; Case / rot13 folds over a string.
(define (evil-rot13-char c)
  (let ((n (char->integer c)))
    (cond ((and (>= n 97) (<= n 122)) (integer->char (+ 97 (modulo (+ (- n 97) 13) 26))))
          ((and (>= n 65) (<= n 90))  (integer->char (+ 65 (modulo (+ (- n 65) 13) 26))))
          (else c))))
(define (evil-rot13 s) (list->string (map evil-rot13-char (string->list s))))

;; Replace [s, e) with (folder text), leaving point at `goto`.
(define (evil-fold-range s e folder goto)
  (when (< s e)
    (let* ((old (buffer-substring s e)) (new (folder old)))
      (when (not (equal? old new)) (replace-region s e new))))
  (goto-char goto))

;; Shift the lines of [s, e) one step right (dir > 0) or left: the region-
;; aware indent commands do the work (they skip blank lines, honor the
;; buffer's indent width and land on the first non-blank).
(define (evil-shift-lines s e dir)
  (set-region s (max s (- e 1)))
  (if (> dir 0) (indent-region) (outdent-region))
  (goto-char (evil-first-nonblank-pos s)))

;; User-defined operators: name → (lambda (kind start end) …).
(define *evil-user-operators* (hash))
(define (evil-define-operator name proc)
  (set! *evil-user-operators* (hash-insert *evil-user-operators* name proc)))

;; Built-in operators: name → (lambda (line? s e p) …), where line? is whether
;; the range is linewise and p is point before the operator ran. Consulted
;; before `evil-define-operator` entries, like the built-in motions.
(define *evil-builtin-operators* (hash))
(define (evil-builtin-operator name proc)
  (set! *evil-builtin-operators* (hash-insert *evil-builtin-operators* name proc)))

;; Apply operator `op` to [s, e) of kind "char"/"line".
(define (evil-apply-operator op kind s e)
  (let ((builtin (hash-try-get *evil-builtin-operators* op)))
    (if builtin
        (builtin (equal? kind "line") s e (point))
        (let ((user (hash-try-get *evil-user-operators* op)))
          (if user
              (user kind s e)
              (message (string-append "Unknown operator: " op)))))))

(evil-builtin-operator "delete"
  (lambda (line s e p)
    (if line
        (evil-delete-lines s e)
        (when (< s e) (kill-region-between s e) (evil-set-charwise)))))
(evil-builtin-operator "change"
  (lambda (line s e p)
    (if line
        (let* ((eof (and (= e (point-max))
                         (or (= e s) (not (equal? (buffer-substring (- e 1) e) "\n")))))
               (e2 (if eof e (- e 1))))
          (when (< s e2) (kill-region-between s e2) (evil-set-linewise))
          (goto-char s))
        (when (< s e) (kill-region-between s e) (evil-set-charwise)))
    (evil-insert-state)))
(evil-builtin-operator "yank"
  (lambda (line s e p)
    (when (< s e)
      (set-region s e) (kill-ring-save)
      (if line (evil-set-linewise) (evil-set-charwise))
      (goto-char (if (and line (= (line-of-pos p) (line-of-pos s))) p s)))))
(evil-builtin-operator "indent"   (lambda (line s e p) (evil-shift-lines s e 1)))
(evil-builtin-operator "outdent"  (lambda (line s e p) (evil-shift-lines s e -1)))
(evil-builtin-operator "reindent" (lambda (line s e p) (evil-reindent-lines s e)))
;; Case / rot13 folds: (name folder). Point stays put on a linewise range.
(for-each
 (lambda (row)
   (let ((folder (cadr row)))
     (evil-builtin-operator (car row)
                            (lambda (line s e p) (evil-fold-range s e folder (if line p s))))))
 (list (list "downcase" string-downcase)
       (list "upcase" string-upcase)
       (list "togglecase" string-toggle-case)
       (list "rot13" evil-rot13)))
(evil-builtin-operator "fill"
  (lambda (line s e p) (evil-fill-range (line-start-pos s) (line-end-pos (max s (- e 1))) #f)))
(evil-builtin-operator "fill-keep"
  (lambda (line s e p) (evil-fill-range (line-start-pos s) (line-end-pos (max s (- e 1))) #t)))
(evil-builtin-operator "comment" (lambda (line s e p) (evil-comment-lines s e)))

;; The entry point the editor dispatches for `{count}{op}{count}{motion}`.
(define (evil-operate op count force thunk)
  (set! *evil-motion-count* count)
  (set! *evil-op-name* op)
  (let ((m (thunk)))
    (set! *evil-motion-count* 0)
    ;; A failed motion (#f) aborts the whole command — `c` doesn't even enter
    ;; Insert — as in Vim.
    (when m
      (let ((r (evil-motion->range m force)))
        (evil-apply-operator op (car r) (cadr r) (caddr r))))))

;; An operator key in a Visual state: apply it to the selection, then leave
;; Visual (change enters Insert instead).
(define (evil-visual-operator op)
  (let* ((rb (region-beginning)) (re (region-end))
         (st (evil-state))
         (kind (if (equal? st "visual-line") "line" "char")))
    (when (and (>= rb 0) (< rb re))
      (if (equal? op "yank")
          ;; the live selection IS the range (Visual is inclusive in the core)
          (begin (kill-ring-save)
                 (if (equal? kind "line") (evil-set-linewise) (evil-set-charwise))
                 (goto-char rb))
          (evil-apply-operator op kind rb re)))
    (when (not (equal? op "change")) (evil-normal-state))))

;; `=` — re-indent lines: structurally (tree-sitter indent queries, the Lisp
;; aligner) where the language has them, else to the previous line's indent.
(define (evil-reindent-lines s e)
  (reindent-lines (line-of-pos s) (line-of-pos (max s (- e 1)))))


;; --- the operator-pending keymap -------------------------------------------------
(define (evil-omap chord expr) (define-key "evil-operator" chord expr))
(for-each
 (lambda (b) (evil-omap (car b) (string-append "evil-motion \"" (cdr b) "\"")))
 (list (cons "h" "h") (cons "l" "l") (cons "space" "l") (cons "j" "j") (cons "k" "k")
       (cons "+" "+") (cons "-" "-") (cons "_" "_")
       (cons "w" "w") (cons "shift-w" "W") (cons "b" "b") (cons "shift-b" "B")
       (cons "e" "e") (cons "shift-e" "E") (cons "g e" "ge") (cons "g shift-e" "gE")
       (cons "0" "0") (cons "^" "^") (cons "$" "$") (cons "g _" "g_") (cons "|" "|")
       (cons "g g" "gg") (cons "shift-g" "G")
       (cons "{" "{") (cons "}" "}") (cons "(" "(") (cons ")" ")") (cons "%" "%")
       (cons "shift-h" "H") (cons "shift-m" "M") (cons "shift-l" "L")
       (cons ";" ";") (cons "," ",") (cons "n" "n") (cons "shift-n" "N")
       (cons "*" "*") (cons "#" "#")))
(evil-omap "f" "evil-motion-find \"f\" %c")
(evil-omap "shift-f" "evil-motion-find \"F\" %c")
(evil-omap "t" "evil-motion-find \"t\" %c")
(evil-omap "shift-t" "evil-motion-find \"T\" %c")
(evil-omap "/" "evil-op-search \"/\"")
(evil-omap "'" "evil-motion-mark %c #f")
(evil-omap "`" "evil-motion-mark %c #t")
(evil-omap "?" "evil-op-search \"?\"")
;; Text objects: `d i w`, `c a (`, … (and the visual `i w` selections below).
(for-each
 (lambda (to)
   (for-each
    (lambda (kind)
      (evil-omap (string-append kind " " (car to))
                 (string-append "evil-object \"" kind "\" \"" (cdr to) "\"")))
    (list "i" "a")))
 *evil-text-objects*)

;; Operator keys (normal state → operator-pending; visual → apply to selection).
(define (evil-bind-operator chord op)
  (define-key "evil-normal" chord (string-append "evil-operator \"" op "\"")))
(evil-bind-operator "d" "delete")
(evil-bind-operator "c" "change")
(evil-bind-operator "y" "yank")
(evil-bind-operator ">" "indent")
(evil-bind-operator "<" "outdent")
(evil-bind-operator "=" "reindent")
(evil-bind-operator "g u" "downcase")
(evil-bind-operator "g shift-u" "upcase")
(evil-bind-operator "g ~" "togglecase")
(evil-bind-operator "g ?" "rot13")
(evil-bind-operator "g q" "fill")
(evil-bind-operator "g w" "fill-keep")
(evil-bind-operator "g c" "comment")
(for-each
 (lambda (km)
   (define-keys km
     '(("=" "evil-visual-operator \"reindent\"")
       ("g u" "evil-visual-operator \"downcase\"")
       ("g shift-u" "evil-visual-operator \"upcase\"")
       ("g ~" "evil-visual-operator \"togglecase\"")
       ("g ?" "evil-visual-operator \"rot13\"")
       ("g c" "evil-visual-operator \"comment\""))))
 (list "evil-visual" "evil-visual-line"))

;; --- tree-sitter text objects and motions ---------------------------------------
;; af/if (function), ac/ic (class / type), aa/ia (argument / parameter), a/ / i/
;; (comment), and ]m [m ]M [M / ]f [f ]F [F (function starts / ends), ]] [[ ][
;; [] (class / section starts / ends). Built on the synchronous `ts-*` reads of
;; the buffer's syntax tree; the node kinds per language live in
;; `*evil-ts-kinds*` — extend it with `evil-ts-set-kinds`.

;; Group a flat list into k-element lists: (k s e k s e …) → ((k s e) …).
(define (evil-ts-group flat k)
  (let loop ((xs flat) (acc '()))
    (if (< (length xs) k)
        (reverse acc)
        (loop (list-tail xs k)
              (cons (let take ((ys xs) (i 0) (out '()))
                      (if (= i k) (reverse out) (take (cdr ys) (+ i 1) (cons (car ys) out))))
                    acc)))))
(define (evil-ts-triples flat) (evil-ts-group flat 3))   ; (kind start end)
(define (evil-ts-quads flat) (evil-ts-group flat 4))     ; (kind start end named?)
(define (evil-ts-pairs flat)
  (map (lambda (l) (cons (car l) (cadr l))) (evil-ts-group flat 2)))
(define (evil-named? q) (list-ref q 3))

;; Node kinds per category, with per-language overrides ("lang:category").
(define *evil-ts-kinds* (hash))
(define (evil-ts-set-kinds key kinds)
  (set! *evil-ts-kinds* (hash-insert *evil-ts-kinds* key kinds)))
(define (evil-ts-kinds category)
  (let ((specific (hash-try-get *evil-ts-kinds* (string-append (buffer-language) ":" category))))
    (if specific specific (or (hash-try-get *evil-ts-kinds* category) '()))))
(evil-ts-set-kinds "function"
  '("function_item" "function_definition" "function_declaration" "method_declaration"
    "method_definition" "func_literal" "function_expression" "arrow_function"
    "generator_function_declaration" "generator_function" "closure_expression"
    "lambda" "lambda_expression" "constructor_declaration" "method" "singleton_method"
    "function_signature_item" "fn_declaration" "local_function" "function"
    "test_declaration" "anonymous_function" "value_definition" "function_clause"))
(evil-ts-set-kinds "class"
  '("struct_item" "enum_item" "impl_item" "trait_item" "union_item" "mod_item"
    "class_definition" "class_declaration" "class" "interface_declaration"
    "type_alias_declaration" "enum_declaration" "struct_specifier" "class_specifier"
    "union_specifier" "enum_specifier" "namespace_definition" "type_declaration"
    "struct_declaration" "union_declaration" "opaque_declaration" "module"
    "record_declaration" "object_definition" "trait_definition" "class_body_declaration"))
(evil-ts-set-kinds "arguments"
  '("arguments" "argument_list" "parameters" "parameter_list" "formal_parameters"
    "type_arguments" "type_parameters" "closure_parameters" "lambda_parameters"
    "template_argument_list" "template_parameter_list" "function_parameters"
    "tuple_type" "type_parameter_list"))
(evil-ts-set-kinds "comment" '("comment" "line_comment" "block_comment" "doc_comment"))
(evil-ts-set-kinds "body"
  '("block" "body" "compound_statement" "statement_block" "function_body"
    "declaration_list" "class_body" "field_declaration_list" "enum_variant_list"
    "interface_body" "enum_body" "struct_body" "container_field_list"
    "field_declaration_list" "do_block" "block_statement"))
;; Lisps: every form is a `list`; functions/classes are recognized by head.
(evil-ts-set-kinds "scheme:function" '("list"))
(evil-ts-set-kinds "racket:function" '("list"))
(evil-ts-set-kinds "clojure:function" '("list_lit"))
(define *evil-lisp-function-heads*
  '("(define " "(define-" "(lambda " "(λ " "(defn " "(fn " "(defun " "(let " "(define(" ))
(define (evil-ts-lisp?) (member (buffer-language) '("scheme" "racket" "clojure")))
(define (evil-ts-function-node? n)
  (if (evil-ts-lisp?)
      (let ((head (buffer-substring (cadr n) (min (caddr n) (+ (cadr n) 9)))))
        (let loop ((hs *evil-lisp-function-heads*))
          (cond ((null? hs) #f)
                ((starts-with? head (car hs)) #t)
                (else (loop (cdr hs))))))
      #t))

;; The innermost node at `pos` of `category` (passing `ok?`), as (kind s e).
(define (evil-ts-enclosing pos category ok?)
  (let ((kinds (evil-ts-kinds category)))
    (let loop ((ns (evil-ts-triples (ts-ancestors pos))))
      (cond ((null? ns) #f)
            ((and (member (car (car ns)) kinds) (ok? (car ns))) (car ns))
            (else (loop (cdr ns)))))))
(define (evil-ts-any n) #t)

;; A range as a text-object result: linewise when it covers whole lines (from
;; its line's first non-blank to its line end), charwise otherwise.
(define (evil-ts-object-range s e)
  (let ((le (line-end-pos (max s (- e 1)))))
    (if (and (<= s (evil-first-nonblank-pos s)) (>= e le))
        (cons "line" (cons (line-start-pos s) (if (< le (point-max)) (+ le 1) le)))
        (cons "char" (cons s e)))))

;; The "inside" of a function/class node: its body child minus braces (or,
;; for a Lisp form, everything after the head and name).
(define (evil-ts-inner node)
  (let* ((s (cadr node)) (e (caddr node))
         (kids (evil-ts-quads (ts-children s e)))
         (bodies (evil-ts-kinds "body"))
         (body (let loop ((ks kids))
                 (cond ((null? ks) #f)
                       ((member (car (car ks)) bodies) (car ks))
                       (else (loop (cdr ks)))))))
    (cond
      (body
       (let* ((bs (cadr body)) (be (caddr body))
              (braced (member (buffer-substring bs (+ bs 1)) '("{" "(" "[")))
              (is (if braced (+ bs 1) bs))
              (ie (if braced (- be 1) be)))
         (evil-ts-trim-inner is ie)))
      ((evil-ts-lisp?)
       ;; (define (name args) body…) → the body forms
       (let ((named (filter evil-named? kids)))
         (if (> (length named) 2)
             (evil-ts-trim-inner (cadr (list-ref named 2)) (- e 1))
             #f)))
      (else #f))))
;; Shrink [s, e) past surrounding blanks; when it spans several lines, make it
;; the whole lines strictly inside (Vim `i{` style).
(define (evil-ts-trim-inner s e)
  (let* ((s2 (let loop ((i s))
               (if (and (< i e) (member (buffer-substring i (+ i 1)) '(" " "\t" "\n"))) (loop (+ i 1)) i)))
         (e2 (let loop ((i e))
               (if (and (> i s2) (member (buffer-substring (- i 1) i) '(" " "\t" "\n"))) (loop (- i 1)) i))))
    (and (< s2 e2)
         ;; linewise only when the body has whole lines to itself (the opener's
         ;; and closer's lines hold nothing of it)
         (if (and (> (line-of-pos s2) (line-of-pos s)) (< (line-of-pos e2) (line-of-pos e)))
             (let ((ls (line-start-pos s2)) (le (line-end-pos e2)))
               (cons "line" (cons ls (if (< le (point-max)) (+ le 1) le))))
             (cons "char" (cons s2 e2))))))

;; af / if, ac / ic.
(define (evil-ts-object category inner)
  (let ((n (evil-ts-enclosing (point) category
                              (if (equal? category "function") evil-ts-function-node? evil-ts-any))))
    (and n
         (if inner
             (evil-ts-inner n)
             (evil-ts-object-range (cadr n) (caddr n))))))

;; aa / ia — the argument / parameter under point: its node (inner), plus the
;; separating comma and blanks after it — or, for the last one, before it.
(define (evil-ts-argument inner)
  (let* ((p (point))
         (lisp (evil-ts-lisp?))
         (container (if lisp
                        (evil-ts-enclosing p "function" (lambda (n) #t))
                        (evil-ts-enclosing p "arguments" evil-ts-any)))
         (container (or container (and lisp (evil-ts-lisp-list p)))))
    (and container
         (let* ((kids (filter evil-named?
                              (evil-ts-quads (ts-children (cadr container) (caddr container)))))
                ;; a Lisp form's first element is the operator, not an argument
                (args (if (and lisp (pair? kids)) (cdr kids) kids)))
           (let loop ((as args) (prev #f))
             (cond
               ((null? as) #f)
               ((or (<= p (caddr (car as))) (null? (cdr as)))
                (let* ((a (car as)) (s (cadr a)) (e (caddr a))
                       (next (and (pair? (cdr as)) (cadr as))))
                  (cond
                    (inner (cons "char" (cons s e)))
                    (next (cons "char" (cons s (cadr next))))
                    (prev (cons "char" (cons (caddr prev) e)))
                    (else (cons "char" (cons s e))))))
               (else (loop (cdr as) (car as)))))))))
(define (evil-ts-lisp-list p)
  (let loop ((ns (evil-ts-triples (ts-ancestors p))))
    (cond ((null? ns) #f)
          ((member (car (car ns)) '("list" "list_lit" "vector")) (car ns))
          (else (loop (cdr ns))))))

;; a/ / i/ — the comment under point; `a/` takes a run of adjacent line
;; comments (linewise), `i/` the text inside the comment markers.
(define (evil-ts-comment inner)
  (let ((n (evil-ts-enclosing (point) "comment" evil-ts-any)))
    (and n
         (let ((s (cadr n)) (e (caddr n)))
           (if inner
               (let* ((text (buffer-substring s e))
                      (lead (let loop ((i s))
                              (if (and (< i e)
                                       (member (buffer-substring i (+ i 1))
                                               '("/" "*" "#" ";" "-" "!" " " "\t" "%")))
                                  (loop (+ i 1))
                                  i)))
                      (tail (let loop ((i e))
                              (if (and (> i lead)
                                       (member (buffer-substring (- i 1) i)
                                               '("/" "*" " " "\t" "\n" "#" "|")))
                                  (loop (- i 1))
                                  i))))
                 (and (< lead tail) (cons "char" (cons lead tail))))
               ;; extend over adjacent comment-only lines
               (let* ((first (let loop ((ls (line-start-pos s)))
                               (if (and (> ls 0) (evil-ts-comment-line? (line-start-pos (- ls 1))))
                                   (loop (line-start-pos (- ls 1)))
                                   ls)))
                      (last (let loop ((le (line-end-pos e)))
                              (if (and (< (+ le 1) (point-max)) (evil-ts-comment-line? (+ le 1)))
                                  (loop (line-end-pos (+ le 1)))
                                  le))))
                 (if (and (<= s (evil-first-nonblank-pos s)) (>= (line-end-pos (max s (- e 1))) (- e 0)))
                     (cons "line" (cons first (if (< last (point-max)) (+ last 1) last)))
                     (cons "char" (cons s e)))))))))
;; Is the line at `ls` only a comment (its first non-blank starts a comment node)?
(define (evil-ts-comment-line? ls)
  (let ((fnb (evil-first-nonblank-pos ls)))
    (and (< fnb (line-end-pos ls))
         (let ((n (evil-ts-enclosing fnb "comment" evil-ts-any)))
           (and n (= (cadr n) fnb))))))

;; Motions: the n-th node start / end of `category` after / before point.
;; The nearest spot past `p` in direction `forward` (a linear scan, no sort).
(define (evil-nearest-spot spots p forward)
  (let loop ((ss spots) (best #f))
    (if (null? ss)
        best
        (let ((x (car ss)))
          (loop (cdr ss)
                (if (if forward
                        (and (> x p) (or (not best) (< x best)))
                        (and (< x p) (or (not best) (> x best))))
                    x
                    best))))))
(define (evil-ts-motion-target category forward ends n)
  (let* ((kinds (evil-ts-kinds category))
         (ranges (filter (lambda (r) (or (not (equal? category "function"))
                                         (evil-ts-function-node? (list "" (car r) (cdr r)))))
                         (evil-ts-pairs (ts-ranges-of-kinds kinds))))
         (spots (map (lambda (r) (if ends (max (car r) (- (cdr r) 1)) (car r))) ranges)))
    (let loop ((p (point)) (k n))
      (if (<= k 0)
          p
          (let ((next (evil-nearest-spot spots p forward)))
            (if next (loop next (- k 1)) (if (= k n) #f p)))))))
(define (evil-ts-goto category forward ends extend)
  (let ((tg (evil-ts-motion-target category forward ends (evil-count))))
    (if tg
        (if extend (select-to tg) (goto-char tg))
        (message (string-append "No " (if forward "next " "previous ") category)))))
(define (evil-next-function-start)      (evil-ts-goto "function" #t #f #f))
(define (evil-previous-function-start)  (evil-ts-goto "function" #f #f #f))
(define (evil-next-function-end)        (evil-ts-goto "function" #t #t #f))
(define (evil-previous-function-end)    (evil-ts-goto "function" #f #t #f))
(define (evil-next-class-start)         (evil-ts-goto "class" #t #f #f))
(define (evil-previous-class-start)     (evil-ts-goto "class" #f #f #f))
(define (evil-next-class-end)           (evil-ts-goto "class" #t #t #f))
(define (evil-previous-class-end)       (evil-ts-goto "class" #f #t #f))
(define (evil-next-class-start-visual)     (evil-ts-goto "class" #t #f #t))
(define (evil-previous-class-start-visual) (evil-ts-goto "class" #f #f #t))
(define (evil-next-class-end-visual)       (evil-ts-goto "class" #t #t #t))
(define (evil-previous-class-end-visual)   (evil-ts-goto "class" #f #t #t))
(define (evil-next-function-start-visual)     (evil-ts-goto "function" #t #f #t))
(define (evil-previous-function-start-visual) (evil-ts-goto "function" #f #f #t))
(define (evil-next-function-end-visual)       (evil-ts-goto "function" #t #t #t))
(define (evil-previous-function-end-visual)   (evil-ts-goto "function" #f #t #t))
;; As operator motions: exclusive to a start, inclusive to an end.
(define (evil-ts-motion category forward ends)
  (let ((tg (evil-ts-motion-target category forward ends (evil-mcount))))
    (and tg (evil-mres (if ends "inclusive" "exclusive") tg))))

;; --- ]d / [d — diagnostics ----------------------------------------------------
;; The n-th LSP diagnostic after / before point (records "line\tcol\t…").
(define (evil-diagnostic-positions)
  (map (lambda (rec)
         (let ((f (split-many rec "\t")))
           (pos-at-char-column (string->number (car f)) (string->number (cadr f)))))
       (diagnostics)))
(define (evil-diagnostic-goto forward)
  (let ((spots (evil-diagnostic-positions)))
    (let loop ((p (point)) (k (evil-count)) (moved #f))
      (let ((next (and (> k 0) (evil-nearest-spot spots p forward))))
        (cond (next (loop next (- k 1) #t))
              (moved (goto-char p))
              (else (message (if forward "No more diagnostics below" "No more diagnostics above"))))))))
(define (evil-next-diagnostic)     (evil-diagnostic-goto #t))
(define (evil-previous-diagnostic) (evil-diagnostic-goto #f))

;; --- gc — comment operator (vim-commentary) -------------------------------------
;; Toggle line comments over [s, e): when every non-blank line is commented,
;; uncomment them all; otherwise comment them all at the shallowest indent.
;; The toggle, the per-language leader table and the buffer-local
;; "comment-string" override are shared with M-; (comment-lines in core.scm).
(define (evil-comment-lines s e)
  (goto-char (evil-first-nonblank-pos (comment-lines s e))))

;; --- C-a / C-x — increment / decrement the number at or after the cursor ------
;; Decimal (with a leading `-`), hex `0x…` (case and width kept) and binary
;; `0b…`; a count adds that much. Visual: every selected line's first number
;; (g C-a: progressively, 1×count, 2×count, …).
(define *evil-number-re* "0[xX][0-9a-fA-F]+|0[bB][01]+|-?[0-9]+")
(define (evil-digit-value c)
  (let ((n (char->integer c)))
    (cond ((and (>= n 48) (<= n 57)) (- n 48))
          ((and (>= n 97) (<= n 102)) (+ 10 (- n 97)))
          ((and (>= n 65) (<= n 70)) (+ 10 (- n 65)))
          (else 0))))
(define (evil-parse-radix s radix)
  (let loop ((cs (string->list s)) (acc 0))
    (if (null? cs) acc (loop (cdr cs) (+ (* acc radix) (evil-digit-value (car cs)))))))
(define (evil-format-radix n radix upper)
  (if (= n 0)
      "0"
      (let loop ((n n) (acc '()))
        (if (= n 0)
            (list->string acc)
            (let* ((d (modulo n radix))
                   (c (if (< d 10) (integer->char (+ 48 d))
                          (integer->char (+ (if upper 65 97) (- d 10))))))
              (loop (quotient n radix) (cons c acc)))))))
(define (evil-pad-left s width)
  (if (>= (string-length s) width) s (evil-pad-left (string-append "0" s) width)))
;; The new text for number `text` plus `delta`.
(define (evil-number-add text delta)
  (cond
    ((or (starts-with? text "0x") (starts-with? text "0X"))
     (let* ((digits (substring text 2 (string-length text)))
            (upper (let loop ((cs (string->list digits)))
                     (cond ((null? cs) #f)
                           ((and (char>=? (car cs) #\A) (char<=? (car cs) #\F)) #t)
                           (else (loop (cdr cs))))))
            (width (string-length digits))
            (modulus (expt 16 width))
            (v (modulo (+ (evil-parse-radix digits 16) delta) (max modulus (expt 16 8)))))
       (string-append (substring text 0 2) (evil-pad-left (evil-format-radix v 16 upper) width))))
    ((or (starts-with? text "0b") (starts-with? text "0B"))
     (let* ((digits (substring text 2 (string-length text)))
            (v (max 0 (+ (evil-parse-radix digits 2) delta))))
       (string-append (substring text 0 2)
                      (evil-pad-left (evil-format-radix v 2 #f) (string-length digits)))))
    (else (number->string (+ (string->number text) delta)))))
;; The number at or after `pos` on its line: (start . end), or #f. A `-` only
;; counts as a sign when it doesn't follow a word character (`x-1` is 1).
(define (evil-number-at pos)
  (let* ((ls (line-start-pos pos)) (le (line-end-pos pos)))
    (let loop ((m (buffer-regex-matches *evil-number-re* ls le)))
      (cond ((or (null? m) (null? (cdr m))) #f)
            ((> (cadr m) pos)
             (let* ((s (car m)) (e (cadr m))
                    (s (if (and (equal? (buffer-substring s (+ s 1)) "-") (> s ls)
                                (evil-word-char-pos? (- s 1)))
                           (+ s 1) s)))
               (cons s e)))
            (else (loop (cddr m)))))))
(define (evil-increment-by delta)
  (let ((r (evil-number-at (point))))
    (if (not r)
        (message "No number on this line")
        (let* ((s (car r)) (e (cdr r))
               (new (evil-number-add (buffer-substring s e) delta)))
          (replace-region s e new)
          (goto-char (+ s (- (utf8-length new) 1)))))))
(define (evil-increment) (evil-increment-by (evil-count)))
(define (evil-decrement) (evil-increment-by (- 0 (evil-count))))
;; Visual: each selected line's first number at/after the selection start.
(define (evil-visual-increment-by step progressive)
  (let ((rb (region-beginning)) (re (region-end)))
    (when (and (>= rb 0) (< rb re))
      (let loop ((ln (line-of-pos (max rb (- re 1)))) (edits '()))
        (if (>= ln (line-of-pos rb))
            (let* ((ls (pos-at-char-column ln 0))
                   (r (evil-number-at (if (= ln (line-of-pos rb)) rb ls)))
                   (r (and r (= (line-of-pos (car r)) ln) r)))
              (loop (- ln 1) (if r (cons r edits) edits)))
            ;; edits are top-down; apply bottom-up so offsets hold
            (let go ((es (reverse edits)) (k (length edits)))
              (when (pair? es)
                (let* ((r (car es))
                       (delta (if progressive (* step k) step)))
                  (replace-region (car r) (cdr r) (evil-number-add (buffer-substring (car r) (cdr r)) delta)))
                (go (cdr es) (- k 1))))))
        (goto-char rb)))
  (evil-normal-state))
(define (evil-visual-increment)             (evil-visual-increment-by (evil-count) #f))
(define (evil-visual-decrement)             (evil-visual-increment-by (- 0 (evil-count)) #f))
(define (evil-visual-increment-progressive) (evil-visual-increment-by (evil-count) #t))
(define (evil-visual-decrement-progressive) (evil-visual-increment-by (- 0 (evil-count)) #t))

;; Visual-mode change (charwise selection's exact range) and case folding. Case
;; ops work the region text directly (builtins only) rather than via editing.scm.
(define (evil-visual-change) (kill-region) (evil-set-charwise) (evil-insert-state))
(define (evil-visual-fold-case folder)
  (let ((s (region-beginning)) (e (region-end)))
    (when (and (>= s 0) (< s e))
      (replace-region s e (folder (buffer-substring s e)))))
  (evil-normal-state))
(define (evil-visual-downcase)    (evil-visual-fold-case string-downcase))    ; u
(define (evil-visual-upcase)      (evil-visual-fold-case string-upcase))      ; U
(define (evil-visual-toggle-case) (evil-visual-fold-case string-toggle-case)) ; ~
;; Visual-LINE change: drop the whole-line selection, then open a fresh line in
;; its place and insert (the selection already spans entire lines).
(define (evil-visual-line-change)
  ;; At EOF the kill already leaves an empty last line: insert there instead of
  ;; opening another one above it.
  (let ((eof (evil-visual-line-eof-start)))
    (kill-region) (evil-set-linewise)
    (if eof (evil-insert-state) (evil-open-above))))

;; Visual `>` / `<` — shift the selected lines one indent step. Vim drops back to
;; Normal after a single shift (`3>` repeats instead, which the count prefix now
;; handles), so these end in normal state.
(define (evil-visual-indent)  (indent-region)  (evil-normal-state))
(define (evil-visual-outdent) (outdent-region) (evil-normal-state))

;; Visual `J` — join every selected line into one, collapsing each newline plus
;; the following indentation to a single space (vim `J` over a selection). One
;; replace-region over the whole selected line span, computed from the snapshot.
(define (evil-visual-join)
  (let ((rb (region-beginning)) (re (region-end)))
    (when (and (>= rb 0) (< rb re))
      (let* ((s (line-start-pos rb))
             (e (line-end-pos (max rb (- re 1)))))
        (when (< s e)
          (replace-region s e
            (replace-regexp-in-string "\n[ \t]*" " " (buffer-substring s e)))
          (goto-char s)))))
  (evil-normal-state))

;; Visual `r<char>` — overwrite EVERY character of the selection with the typed
;; char (vim). Newlines are preserved, so a multi-line selection keeps its
;; shape. Armed through the same `read-char` capture as normal-state `r`.
(define (evil-visual-replace-dispatch ch)
  (let ((rb (region-beginning)) (re (region-end)))
    (when (and (>= rb 0) (< rb re))
      (replace-region rb re
        (list->string
         (map (lambda (c) (if (equal? c #\newline) c (car (string->list ch))))
              (string->list (buffer-substring rb re)))))
      (goto-char rb)))
  (evil-normal-state))
(define (evil-visual-replace-char) (read-char "r-" 'evil-visual-replace-dispatch))
;; Block-wise `r` overwrites only the rectangle, not the stream between its
;; corners (the core walks the block's columns).
(define (evil-block-replace-dispatch ch) (evil-block-replace ch))
(define (evil-block-replace-char) (read-char "r-" 'evil-block-replace-dispatch))

;; Visual `l` stops on the line's last char (Vim), so repeated `l` can't creep
;; onto the line break and make `d` join lines. `$` still reaches it.
(define (evil-visual-forward-char)
  (let ((p (point)))
    (when (< (grapheme-forward p) (line-end-pos p)) (forward-char))))

;; The three Visual keymaps (charwise, linewise, blockwise), for bindings they
;; all share.
(define *evil-visual-maps* '("evil-visual" "evil-visual-line" "evil-visual-block"))

;; --- normal state -----------------------------------------------------------
;; motion
(define-keys "evil-normal"
  '(("h" backward-char)
    ("l" forward-char)
    ("j" next-line)
    ("k" previous-line)
    ("w" forward-word-start)
    ;; `b` is the exact mirror of `w` (class-aware), NOT Emacs `backward-word` —
    ;; that one skips punctuation as a separator, so it disagreed with `w` about
    ;; where a word starts and `b` could never land on a `).` run.
    ("b" backward-word-start)
    ;; W / B / E — whitespace-delimited WORD motions (vim's shifted trio).
    ("shift-w" forward-bigword-start)
    ("shift-b" backward-bigword-start)
    ("shift-e" forward-bigword-end)
    ("0" move-beginning-of-line)
    ;; macOS clears shift for non-letter keys and reports the produced char, so
    ;; bind the literal symbol ($, :, ?) — NOT shift-4 / shift-; / shift-/.
    ("$" move-end-of-line)
    ("e" evil-forward-word-end)            ; e — end of word
    ("^" evil-first-non-blank)             ; ^ — first non-blank
    ("g g" evil-goto-first-line)           ; gg — first line, first non-blank
    ("shift-g" evil-goto-last-line)        ; G  — last line, first non-blank
    ("{" evil-backward-paragraph)          ; { — previous paragraph boundary
    ("}" evil-forward-paragraph)           ; } — next paragraph boundary
    ("(" evil-backward-sentence)           ; ( — previous sentence start
    (")" evil-forward-sentence)            ; ) — next sentence start
    ("%" evil-matchit)                     ; % — matching bracket
    ("f" evil-find-char-forward)           ; f<char> — find char on line
    ("shift-f" evil-find-char-backward)    ; F<char> — find backward
    ("t" evil-till-char-forward)           ; t<char> — till char
    ("shift-t" evil-till-char-backward)    ; T<char>
    (";" evil-find-repeat)                 ; ; — repeat last f/t
    ("," evil-find-repeat-reverse)         ; , — repeat, reversed
    ("*" evil-search-word-forward)         ; * — search word under cursor
    ("#" evil-search-word-backward)        ; # — same, backward
    ;; enter insert
    ("i" evil-insert-state)
    ("shift-i" evil-insert-line)           ; I
    ("a" evil-append)
    ("shift-a" evil-append-line)           ; A
    ("o" evil-open-below)
    ("shift-o" evil-open-above)            ; O
    ;; edit
    ("x" evil-delete-char)                 ; x — delete char (into register)
    ("r" evil-replace-char)                ; r<char> — replace char under cursor
    ("s" evil-substitute-char)             ; s — substitute char
    ("shift-s" evil-change-line)           ; S — substitute line (= cc)
    ("~" evil-toggle-case)                 ; ~ — toggle case of char
    ("shift-j" evil-join-line)             ; J — join next line up
    ("g shift-j" evil-join-line-no-space)  ; gJ — join, no space
    ("g d" lsp-find-definition)            ; gd — go to definition (LSP)
    ("shift-k" lsp-hover)                  ; K  — doc at point (LSP)
    ("q" evil-kbd-macro-toggle)            ; q — record macro (toggle)
    ("@" "evil-macro-play %c")             ; @a — replay macro a (@@ last, @: ex)
    ;; "x — use register x for the next yank / delete / put ("_ black hole, "+ / "*
    ;; clipboard, "0 last yank, "1-"9 deletes, "- small delete, ". : / read-only).
    ("\"" "evil-use-register %c")))
(define-keys-in *evil-visual-maps* '(("\"" "evil-use-register %c")))
(define-keys "evil-normal"
  '(("shift-d" evil-delete-to-eol)      ; D  (= d$)
    ("shift-c" evil-change-to-eol)      ; C  (= c$)
    ("shift-y" evil-yank-line)          ; Y (= yy)
    ("p" evil-paste-after)              ; p  (line-wise → below)
    ("shift-p" evil-paste-before)       ; P  (line-wise → above)
    ;; marks: ma sets (mA = file mark), 'a jumps to its line, `a to the exact spot;
    ;; '' / `` return to before the last jump. gv reselects the last Visual area.
    ("m" "evil-set-mark %c")
    ("'" "evil-goto-mark-line %c")
    ("`" "evil-goto-mark %c")
    ("g v" evil-reselect-visual)
    ("&" "evil-ex \"s\"")               ; & — repeat the last :s on this line
    ("g &" "evil-ex \"%&&\"")           ; g& — …on every line, same flags
    ("shift-h" evil-window-top)         ; H / M / L — screen lines
    ("shift-m" evil-window-middle)
    ("shift-l" evil-window-bottom)
    ("u" undo)
    ("." evil-repeat)                   ; . — repeat the last change (3. = new count)
    ("ctrl-r" redo)                     ; C-r — redo (else it falls
    ; through to isearch-backward)
    ("v" evil-visual-state)
    ("shift-v" evil-visual-line-state)  ; V (line-wise visual)
    ("ctrl-v" evil-visual-block-state)  ; C-v (block-wise)
    (":" evil-ex-command)))             ; :  (e.g. :%s/a/b/g)
;; From Visual, `:` starts on the selected lines (`:'<,'>`).
(define-keys-in *evil-visual-maps* '((":" evil-ex-command)))
(define-keys "evil-normal"
  '(("escape" keyboard-quit)
    ;; search
    ("/" evil-search-forward)
    ("?" evil-search-backward)
    ("n" evil-search-next)
    ("shift-n" evil-search-previous))) ; N

;; Scrolling — vanilla evil's half-page scroll on C-d / C-u. (No SPC leader by
;; default: that's a Spacemacs/Doom addition, not part of Emacs's evil-mode.
;; Define your own in init.scm, e.g. (define-key "evil-normal" "space space"
;; "helm-M-x").)
(define-keys "evil-normal"
  '(("ctrl-d" evil-scroll-down)   ; half page down
    ("ctrl-u" evil-scroll-up)     ; half page up
    ("ctrl-f" scroll-up)          ; full page forward
    ("ctrl-b" scroll-down)        ; full page back
    ("z z" recenter)              ; zz — center cursor line
    ;; Folds (scheme/folding.scm; org/markdown use their own fold styles): za
    ;; toggle, zc close, zo open, zM fold everything, zR open everything, zj / zk
    ;; the next / previous fold header.
    ("z a" fold-toggle-at-point)
    ("z c" fold-close-at-point)
    ("z o" fold-open-at-point)
    ("z shift-m" fold-all)
    ("z shift-r" fold-show-all)
    ("z j" fold-next-header)
    ("z k" fold-previous-header)))

;; Structural motions (tree-sitter): ]m [m / ]f [f — next / previous function
;; start, ]M [M / ]F [F — function end; ]] [[ — next / previous class (section)
;; start, ][ [] — its end. ]d [d — next / previous diagnostic.
;; Each row is (key category forward? end?); the command names follow from the
;; row — `evil-{next,previous}-{category}-{start,end}` (+ `-visual`, defined
;; above) — and the operator motion is `evil-ts-motion` on the same arguments.
(define (evil-ts-motion-name category forward ends)
  (string-append "evil-" (if forward "next" "previous") "-" category
                 "-" (if ends "end" "start")))
(define (evil-scheme-bool b) (if b "#t" "#f"))
(for-each
 (lambda (b)
   (let* ((key (car b)) (category (cadr b)) (forward (caddr b)) (ends (list-ref b 3))
          (name (evil-ts-motion-name category forward ends)))
     (define-key "evil-normal" key name)
     (define-keys-in '("evil-visual" "evil-visual-line") (list (list key (string-append name "-visual"))))
     (define-key "evil-operator" key
                 (string-append "evil-ts-motion \"" category "\" "
                                (evil-scheme-bool forward) " " (evil-scheme-bool ends)))))
 (list
  (list "] m"       "function" #t #f)
  (list "[ m"       "function" #f #f)
  (list "] shift-m" "function" #t #t)
  (list "[ shift-m" "function" #f #t)
  (list "] f"       "function" #t #f)
  (list "[ f"       "function" #f #f)
  (list "] shift-f" "function" #t #t)
  (list "[ shift-f" "function" #f #t)
  (list "] ]"       "class" #t #f)
  (list "[ ["       "class" #f #f)
  (list "] ["       "class" #t #t)
  (list "[ ]"       "class" #f #t)))
(define-key "evil-normal" "] d" 'evil-next-diagnostic)
(define-key "evil-normal" "[ d" 'evil-previous-diagnostic)

;; C-a — increment the number at/after the cursor (decimal, hex, binary; a count
;; adds that much). Visual: C-a on each selected line, g C-a / g C-x
;; progressively. Vim's C-x (decrement) is off by default because C-x is the
;; Emacs prefix (C-x C-s, C-x b, …) that stays live in Normal state — turn it
;; on with (evil-enable-ctrl-x-decrement); `g -` decrements regardless.
(define-key "evil-normal" "ctrl-a" 'evil-increment)
(define-key "evil-normal" "g -" 'evil-decrement)
(define-keys-in '("evil-visual" "evil-visual-line")
  '(("ctrl-a" evil-visual-increment)
    ("g ctrl-a" evil-visual-increment-progressive)
    ("g ctrl-x" evil-visual-decrement-progressive)
    ("g -" evil-visual-decrement)))
(define (evil-enable-ctrl-x-decrement)
  (define-key "evil-normal" "ctrl-x" 'evil-decrement)
  (define-keys-in '("evil-visual" "evil-visual-line") '(("ctrl-x" evil-visual-decrement))))

;; (`>>` / `<<` and every other operator + motion pair live in the operator
;; grammar above: `>` / `<` are operators, doubled for the current line.)

;; --- Emacs chords vim owns in Normal state ----------------------------------
;; Unbound Ctrl chords fall through to the Emacs dispatch, which is what keeps
;; C-x C-s / M-x / C-g working under evil. But four of those Emacs defaults
;; EDIT the buffer, and in Normal state a vim user pressing them expects
;; something else entirely — `C-o` above all, which is vim's jump-back and Emacs's
;; `open-line`, so it silently inserted a newline. Shadow them:
;;   C-o / C-i (and TAB, the same key in a terminal) → the jump list: back
;;        to where G, /, n, %, '', gd, … started, and forward again
;;   C-k / C-y / C-t → undefined, like vim (beep instead of edit)
(define (evil-undefined)
  (message "Not a Normal-state command (Emacs binding shadowed by evil)"))
(define-key "evil-normal" "ctrl-o" 'evil-jump-backward) ; C-o — older jump position
(define-key "evil-normal" "ctrl-i" 'evil-jump-forward)  ; C-i — newer (TAB too, unless a mode binds it)
(define-key "evil-normal" "ctrl-k" 'evil-undefined)     ; was kill-line

;; --- windows: C-w … (Vim's window commands) -----------------------------------
;; h/j/k/l move by direction (also C-h/C-j/C-k/C-l after C-w), w/W cycle, p the
;; previous window, t/b top-left/bottom-right; s/v split, n new buffer, c/q
;; close, o only; = balance, +/- height, </> width, _/| maximize; H/J/K/L move
;; the window to the far side, x exchange, r/R rotate. Counts repeat (3 C-w +).
;; Emacs users get the same as M-x windmove-left/… and balance-windows (C-x +).
(for-each
 (lambda (b)
   (define-key "evil-normal" (string-append "ctrl-w " (car b)) (cdr b)))
 (list (cons "h" "windmove-left") (cons "j" "windmove-down")
       (cons "k" "windmove-up") (cons "l" "windmove-right")
       (cons "ctrl-h" "windmove-left") (cons "ctrl-j" "windmove-down")
       (cons "ctrl-k" "windmove-up") (cons "ctrl-l" "windmove-right")
       (cons "left" "windmove-left") (cons "down" "windmove-down")
       (cons "up" "windmove-up") (cons "right" "windmove-right")
       (cons "w" "other-window") (cons "ctrl-w" "other-window")
       (cons "shift-w" "previous-window-any-frame")
       (cons "p" "evil-window-mru") (cons "ctrl-p" "evil-window-mru")
       (cons "t" "evil-window-top-left") (cons "b" "evil-window-bottom-right")
       (cons "s" "split-window-below") (cons "shift-s" "split-window-below")
       (cons "ctrl-s" "split-window-below")
       (cons "v" "split-window-right") (cons "ctrl-v" "split-window-right")
       (cons "n" "evil-window-new") (cons "ctrl-n" "evil-window-new")
       (cons "c" "delete-window") (cons "q" "delete-window") (cons "ctrl-q" "delete-window")
       (cons "o" "delete-other-windows") (cons "ctrl-o" "delete-other-windows")
       (cons "=" "balance-windows")
       (cons "+" "enlarge-window") (cons "-" "shrink-window")
       (cons ">" "enlarge-window-horizontally") (cons "<" "shrink-window-horizontally")
       (cons "_" "evil-window-set-height") (cons "|" "evil-window-set-width")
       (cons "shift-h" "evil-window-move-far-left") (cons "shift-j" "evil-window-move-very-bottom")
       (cons "shift-k" "evil-window-move-very-top") (cons "shift-l" "evil-window-move-far-right")
       (cons "x" "evil-window-exchange") (cons "ctrl-x" "evil-window-exchange")
       (cons "r" "evil-window-rotate-downwards") (cons "ctrl-r" "evil-window-rotate-downwards")
       (cons "shift-r" "evil-window-rotate-upwards")))
(define-key "evil-normal" "ctrl-y" 'evil-undefined)     ; was yank
(define-key "evil-normal" "ctrl-t" 'evil-undefined)     ; was transpose-chars

;; --- insert state -----------------------------------------------------------
(define-key "evil-insert" "escape" 'evil-normal-state)
(define-key "evil-insert" "ctrl-r" "evil-insert-register %c") ; C-r x — insert register x

;; --- visual states (motions extend; the mark is active) ---------------------
;; One motion table shared by all three Visual maps, then what charwise and
;; linewise share, then each map's own keys. Charwise-only motions (e ( ) f t ;
;; , B %) are not bound in the line / block maps, and the case / shift / join /
;; fill / paragraph / mark / screen-line keys are not bound in the block map.
(define-keys-in *evil-visual-maps*
  '(("h" backward-char)
    ("j" next-line)
    ("k" previous-line)
    ("w" forward-word-start)
    ("b" backward-word-start)
    ("0" move-beginning-of-line)
    ("$" move-end-of-line)
    ("g g" beginning-of-buffer)
    ("shift-g" end-of-buffer)
    ("o" exchange-point-and-mark)       ; o — swap selection ends / block corners
    ("shift-w" forward-bigword-start)   ; W / E — WORD motions
    ("shift-e" forward-bigword-end)
    ("escape" evil-normal-state)))

;; Charwise + linewise Visual.
(define-keys-in '("evil-visual" "evil-visual-line")
  '(("{" evil-backward-paragraph-visual) ; { / } — extend by paragraph
    ("}" evil-forward-paragraph-visual)
    ("p" evil-visual-paste)              ; p — paste over selection
    ("u" evil-visual-downcase)           ; u — lowercase selection
    ("shift-u" evil-visual-upcase)       ; U — uppercase selection
    ("~" evil-visual-toggle-case)        ; ~ — toggle case
    ("g q" evil-visual-fill)             ; gq — format selected lines
    ("g w" evil-visual-fill)
    (">" evil-visual-indent)             ; > / < — shift selected lines
    ("<" evil-visual-outdent)
    ("shift-j" evil-visual-join)         ; J — join selected lines
    ("r" evil-visual-replace-char)       ; r<char> — replace every char
    ("ctrl-d" evil-scroll-down)          ; half-page, extending
    ("ctrl-u" evil-scroll-up)
    ("'" "evil-goto-mark-line %c")
    ("`" "evil-goto-mark %c")
    ("shift-h" evil-window-top-visual)
    ("shift-m" evil-window-middle-visual)
    ("shift-l" evil-window-bottom-visual)))

;; Charwise Visual (`v`).
(define-keys "evil-visual"
  '(("l" evil-visual-forward-char)
    ("e" evil-forward-word-end)          ; e — end of word (extends)
    ("(" evil-backward-sentence-visual)
    (")" evil-forward-sentence-visual)
    ("f" evil-find-char-forward-visual)  ; f/F/t/T — extend to char
    ("shift-f" evil-find-char-backward-visual)
    ("t" evil-till-char-forward-visual)
    ("shift-t" evil-till-char-backward-visual)
    (";" evil-find-repeat-visual)        ; ; , — repeat f/t (extends)
    ("," evil-find-repeat-reverse-visual)
    ("d" evil-visual-delete)
    ("x" evil-visual-delete)
    ("y" evil-visual-yank)
    ("c" evil-visual-change)             ; c/s — change selection
    ("s" evil-visual-change)
    ("shift-b" backward-bigword-start)
    ("%" evil-matchit-visual)            ; % — extend to matching bracket
    ("v" evil-normal-state)              ; v again exits
    ("shift-v" evil-visual-line-state)   ; switch to line-wise
    ("ctrl-v" evil-visual-block-state))) ; switch to block-wise

;; --- visual line state (`V`: whole-line selection; j/k extend by line) -------
;; The selection is rounded out to whole lines in the core, so the motions here
;; just move point; `d`/`y` act line-wise. `I`/`A` are vim's blockwise insert /
;; append: they drop a cursor at the start / end of every selected line, so the
;; text you type (then Escape) lands on all of them at once.
(define-keys "evil-visual-line"
  '(("l" forward-char)
    ("d" evil-visual-line-delete)
    ("x" evil-visual-line-delete)
    ("y" evil-visual-line-yank)
    ("c" evil-visual-line-change)        ; c/s — change selected lines
    ("s" evil-visual-line-change)
    ("shift-i" evil-visual-insert)       ; I — insert at each line start
    ("shift-a" evil-visual-append)       ; A — append at each line end
    ("v" evil-visual-state)              ; switch to charwise visual
    ("shift-v" evil-normal-state)        ; V again exits
    ("ctrl-v" evil-visual-block-state))) ; switch to block-wise

;; --- visual block state (`C-v`: rectangular column selection) ----------------
;; The selection is a rectangle between two corners; the renderer highlights only
;; the column strip (not whole lines). Motions move the cursor corner. `I`/`A`
;; insert/append down the block's left/right column on every row at once (vim's
;; block-insert); `d`/`x` delete the rectangle and `y` yanks it.
(define-keys "evil-visual-block"
  '(("l" forward-char)
    ("d" evil-block-delete)
    ("x" evil-block-delete)
    ("y" evil-block-yank)
    ("c" evil-block-change)              ; c — change the rectangle
    ("s" evil-block-change)
    ("r" evil-block-replace-char)        ; r<char> — replace the rectangle
    ("shift-i" evil-block-insert)        ; I — insert down the left column
    ("shift-a" evil-block-append)        ; A — append down the right column
    ("v" evil-visual-state)              ; switch to charwise visual
    ("shift-v" evil-visual-line-state)   ; switch to line-wise
    ("ctrl-v" evil-normal-state)))       ; C-v again exits

;; --- binding commands by identity (not by name string) ----------------------
;; A keymap stores a NAME string and evals "(name)" on the keypress. To let you
;; bind the *command itself* — `lsp-find-references`, not "lsp-find-references" —
;; so editor tooling (go-to-def, find-refs, typo/unbound warnings) sees a real
;; reference, we keep a table of bound procedures and dispatch through it.
;; Every command (builtins included) is an ordinary Steel procedure, so a bare
;; identifier is a value we can store here.
(define *evil-cmd-table* (hash))
(define (evil-run-key key)
  (let ((cmd (hash-try-get *evil-cmd-table* key)))
    (when cmd (cmd))))
;; Bind `cmd` under `chord` in keymap `km`. `cmd` is a 'name symbol (the canonical
;; config form), a procedure (a bare identifier — lets editor tooling resolve the
;; reference), or a "name" string for back-compat.
(define (evil-bind-key km chord cmd)
  (cond
    ((symbol? cmd) (define-key km chord (symbol->string cmd))) ; 'name → (name)
    ((string? cmd) (define-key km chord cmd))                  ; "name" → (name), back-compat
    (else                                                      ; identity → table dispatch
     (let ((key (string-append km " " chord)))
       (set! *evil-cmd-table* (hash-insert *evil-cmd-table* key cmd))
       (define-key km chord (string-append "evil-run-key \"" key "\""))))))

;; --- per-major-mode evil bindings (Emacs `evil-define-key` / evil-leader) ----
;; Bindings in the keymap "evil-<state>:<major-mode>" shadow the global
;; "evil-<state>" keymap, but only in buffers of that major mode. The major mode
;; is the language name for code buffers ("rust", "go", "typescript", "steel",
;; "python", …) — the same name the highlighter uses.
;;
;;   (evil-define-key "normal" "rust" "space g g" 'lsp-find-definition)
;;
;; binds SPC g g to go-to-definition in Rust buffers only (pass a 'name symbol; a
;; bare command identifier or a "name" string also work). Elsewhere SPC g g keeps
;; whatever the global evil-normal keymap says.

;; Bind one key (procedure or name) in a major-mode-scoped evil keymap.
(define (evil-define-key state mode chord command)
  (evil-bind-key (string-append "evil-" state ":" mode) chord command))

;; Bind many (chord . command) pairs across several major modes for one state —
;; the analogue of Emacs's `(dolist (mode …) (evil-leader/set-key-for-mode …))`.
(define (evil-define-keys-for-modes state modes bindings)
  (for-each
   (lambda (mode)
     (for-each
      (lambda (b) (evil-define-key state mode (car b) (cdr b)))
      bindings))
   modes))

;; --- leader key -------------------------------------------------------------
;; Doom/Spacemacs-style leader (Emacs `evil-leader`). Plain evil has no leader;
;; this opt-in layer lets you bind chords *relative* to a leader key and swap
;; that key in one place. The leader is a normal config variable — `SPC` by
;; default; override it in init.scm with `evil-set-leader` BEFORE defining your
;; leader bindings, since each chord is expanded when it's bound (changing the
;; leader afterward does not move bindings already made):
;;
;;   (evil-set-leader "comma")                    ; or "space" (default), "space space", …
;;   (evil-set-leader-alt "ctrl-space")           ; or "alt-space" (default), "" for none
;;   (evil-leader-set "n n" 'next-buffer)         ; <leader> n n   (global, normal state)
;;   (evil-leader-set-for-modes '("rust" "go")    ; <leader> g g / g r in those modes only
;;     (list (cons "g g" 'lsp-find-definition)
;;           (cons "g r" 'lsp-rename)))
;;
;; Commands may be a 'name symbol (canonical), a "name" string, or a bare
;; procedure identifier (the latter lets editor tooling resolve the reference),
;; same as `evil-define-key`.
;;
;; Global leader chords work EVERYWHERE, not just in file buffers: each one is
;; also bound in the "evil-leader" keymap, which the editor consults in special
;; buffers (magit, dired, package dashboards — whose own keymaps otherwise hide
;; the global evil-normal map) and terminals. Where a plain key must type — a
;; terminal, Insert state — the ALT leader (Doom's `doom-leader-alt-key`, M-SPC
;; by default) opens the same chords: `M-SPC n o` in a pod shell works like
;; `SPC n o` in a file.
(define *evil-leader* "space")
(define *evil-leader-alt* "alt-space")

;; Set the leader to a chord string ("space", "comma", "space space", …).
(define (evil-set-leader key) (set! *evil-leader* key))

;; Set the alt leader (a Ctrl/Alt chord, so it can't collide with typing), or
;; "" to disable it. Like the leader, set it BEFORE your bindings.
(define (evil-set-leader-alt key) (set! *evil-leader-alt* key))

;; Prefix a chord with the current leader; "" yields the bare leader key.
(define (evil-leader-chord chord)
  (if (equal? chord "") *evil-leader* (string-append *evil-leader* " " chord)))

;; Every full sequence a leader chord is reachable by: the leader, plus the alt
;; leader when one is set.
(define (evil-leader-sequences chord)
  (let ((main (evil-leader-chord chord)))
    (if (equal? *evil-leader-alt* "")
        (list main)
        (list main
              (if (equal? chord "")
                  *evil-leader-alt*
                  (string-append *evil-leader-alt* " " chord))))))

;; Bind one leader chord in the global normal-state map (`evil-leader/set-key`)
;; and in the "evil-leader" map special buffers / terminals / Insert consult.
(define (evil-leader-set chord command)
  (for-each
   (lambda (seq)
     (evil-bind-key "evil-normal" seq command)
     (evil-bind-key "evil-leader" seq command))
   (evil-leader-sequences chord)))

;; Bind many (chord . command) leader pairs across major modes for normal state
;; (`evil-leader/set-key-for-mode` over a list of modes). Works for special
;; buffer modes too ("k8s-mode", "magit-status", …): their evil-normal:<mode>
;; overlay shadows the global leader there.
(define (evil-leader-set-for-modes modes bindings)
  (evil-define-keys-for-modes "normal" modes
    (apply append
           (map (lambda (b)
                  (map (lambda (seq) (cons seq (cdr b)))
                       (evil-leader-sequences (car b))))
                bindings))))

;; --- evil-collection: special read-only buffers -----------------------------
;; Macros' "special" buffers (magit, dired, dired-sidebar, grep, …) drive their own
;; keymaps, so the global evil keymaps don't apply there. Like Emacs's
;; `evil-collection`, this layer overlays vim-style keys onto those buffers.
;;
;; The editor consults an `evil-normal:<major>` keymap for a special buffer ONLY
;; when evil-mode is active, and BEFORE the buffer's own keymap. So whatever we
;; bind here is (a) inert under plain Emacs — the buffer's Emacs-default keys
;; show through untouched — and (b) able to shadow a default key under evil
;; (e.g. magit's `k`=discard becomes `k`=up; magit then re-exposes discard on
;; `x` in its own `evil-normal:magit-status` overlay). It's all just keymaps:
;; override any of this in init.scm with `(define-key "evil-normal:<mode>" …)`.

;; Read-only special modes that get vim motion. Extend in init.scm by
;; `(set! *evil-collection-modes* (cons "my-mode" *evil-collection-modes*))`.
;; (Editable special buffers like `git-commit-mode` are intentionally absent —
;; you type into them, so they use the normal insert/normal states.)
(define *evil-collection-modes*
  (list "magit-status" "magit-log-mode" "dired-mode" "dired-sidebar-mode"
        "ibuffer-mode" "grep-mode" "occur-mode" "compile-mode"
        "diagnostics-mode" "help-mode" "diff-mode" "welcome-mode"
        "org-roam-backlinks-mode" "org-roam-unlinked-mode"
        "org-roam-graph-mode" "org-agenda-mode" "org-columns-mode"))

;; Vim motion shared by every special buffer. Only `j`/`k` by default: single
;; `g` is `refresh` in several of these modes, so a `g g` top-motion would
;; shadow it — left out deliberately. `n`/`p` keep working from the base keymap.
;; Parameterized over the overlay prefix so the helix collection (helix.scm,
;; which requires this file) shares the exact same binding set.
(define (modal-collection-bind-mode prefix mode)
  (let ((km (string-append prefix mode)))
    (define-key km "j" 'next-line)
    (define-key km "k" 'previous-line)))
(define (evil-collection-bind-mode mode)
  (modal-collection-bind-mode "evil-normal:" mode))

;; Install the overlay across every mode in `*evil-collection-modes*`. Called
;; once below as part of loading the evil layer; safe to re-run after extending
;; the list.
(define (evil-collection-init)
  (for-each evil-collection-bind-mode *evil-collection-modes*))
(evil-collection-init)

;; Plain-text result buffers (compile / grep / occur output) are read-only text
;; you read and scroll like a file, so they earn the full read-only motion set on
;; top of the shared `j`/`k`: `h`/`l`, word motions, line ends, `G`, and half-page
;; `C-d`/`C-u` (the editor routes those ctrl chords through this overlay too).
;; `g g` is deliberately left off so a single `g` stays free for each mode's own
;; key (compile's recompile, grep's refresh); jump to the top with `C-u` or `k`.
;; Not folded into the shared `evil-collection-bind-mode` because the modal
;; buffers (magit/dired) already bind several of these keys (`b`, `l`, `$`, `^`).
(define (modal-collection-bind-readonly-motion prefix mode)
  (let ((km (string-append prefix mode)))
    (define-keys km
      '(("h" backward-char)
        ("l" forward-char)
        ("w" forward-word-start)
        ("b" backward-word-start)
        ("e" evil-forward-word-end)
        ("0" move-beginning-of-line)
        ("$" move-end-of-line)
        ("^" evil-first-non-blank)
        ("shift-g" evil-goto-last-line)
        ("ctrl-d" evil-scroll-down)
        ("ctrl-u" evil-scroll-up)))))
(define (evil-collection-bind-readonly-motion mode)
  (modal-collection-bind-readonly-motion "evil-normal:" mode))

(for-each evil-collection-bind-readonly-motion
          (list "compile-mode" "grep-mode" "occur-mode"))

;; Selecting text in a special buffer from the keyboard (evil or helix): `v` /
;; `V`, when the buffer leaves them unbound, start a charwise / linewise
;; selection, `y` copies it and `escape` ends it (the core owns those keys, see
;; special_select.rs). While it is live these motions extend it, ahead of the
;; buffer's own keys; anything unbound here still reaches the buffer.
(define-keys "special-select"
  '(("h" backward-char)
    ("j" next-line)
    ("k" previous-line)
    ("l" evil-visual-forward-char)
    ("w" forward-word-start)
    ("b" backward-word-start)
    ("e" evil-forward-word-end)
    ("shift-w" forward-bigword-start)
    ("shift-e" forward-bigword-end)
    ("0" move-beginning-of-line)
    ("$" move-end-of-line)
    ("^" evil-first-non-blank)
    ("g g" beginning-of-buffer)
    ("shift-g" end-of-buffer)
    ("ctrl-d" evil-scroll-down)
    ("ctrl-u" evil-scroll-up)
    ("o" exchange-point-and-mark)))

;; --- counts ------------------------------------------------------------------
;; These take their count as `(prefix-count)` and run once (`5G` = line 5,
;; `3yy` = three lines, `2fx` = second x). Everything else under a count simply
;; repeats (`3w`, `3dw`, …), squashed into one undo step by the core.
(for-each set-count-aware!
          '(evil-delete-line evil-yank-line evil-change-line evil-delete-char
            evil-join-line evil-goto-first-line evil-goto-last-line
            evil-replace-char indent-region outdent-region
            evil-find-char-forward evil-find-char-backward
            evil-till-char-forward evil-till-char-backward
            evil-find-char-forward-visual evil-find-char-backward-visual
            evil-till-char-forward-visual evil-till-char-backward-visual
            evil-find-repeat evil-find-repeat-reverse
            evil-find-repeat-visual evil-find-repeat-reverse-visual
            evil-forward-sentence evil-backward-sentence
            evil-forward-sentence-visual evil-backward-sentence-visual
            evil-repeat evil-macro-play evil-replace-with
            evil-window-top evil-window-bottom
            evil-window-top-visual evil-window-bottom-visual
            evil-increment evil-decrement evil-visual-increment evil-visual-decrement
            evil-visual-increment-progressive evil-visual-decrement-progressive
            evil-next-diagnostic evil-previous-diagnostic
            evil-next-function-start evil-previous-function-start
            evil-next-function-end evil-previous-function-end
            evil-next-class-start evil-previous-class-start
            evil-next-class-end evil-previous-class-end
            evil-next-function-start-visual evil-previous-function-start-visual
            evil-next-function-end-visual evil-previous-function-end-visual
            evil-next-class-start-visual evil-previous-class-start-visual
            evil-next-class-end-visual evil-previous-class-end-visual))

;; --- jumps ---------------------------------------------------------------------
;; Commands that are "jumps" in Vim's sense: run from a key, they record the
;; jump list (C-o / C-i) and the '' mark first. Extend with your own
;; `(set-jump-command! 'my-command)`.
(for-each set-jump-command!
          '(evil-goto-first-line evil-goto-last-line beginning-of-buffer end-of-buffer
            evil-search-forward evil-search-backward evil-search-next evil-search-previous
            evil-search-word-forward evil-search-word-backward
            evil-matchit evil-forward-paragraph evil-backward-paragraph
            evil-forward-sentence evil-backward-sentence
            evil-window-top evil-window-middle evil-window-bottom
            lsp-find-definition lsp-find-references lsp-find-implementation
            lsp-find-type-definition imenu
            evil-next-class-start evil-previous-class-start
            evil-next-diagnostic evil-previous-diagnostic))
