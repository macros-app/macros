;; requires: core editing expand-region help

;; editing-extras.scm — the rest of Emacs's everyday editing commands: sexp and
;; defun motion in every language, whitespace and line utilities, word counts,
;; dabbrev expansion and insert-char. Requires expand-region so its old M-=
;; binding loads first and count-words-region takes the key over.
;;
;;   C-M-f / C-M-b   forward-sexp / backward-sexp        (Rust builtins)
;;   C-M-u / C-M-d   backward-up-list / down-list        (Rust builtins)
;;   C-M-n / C-M-p   forward-list / backward-list        (Rust builtins)
;;   C-M-k           kill-sexp                           (Rust builtin)
;;   C-M-a / C-M-e   beginning-of-defun / end-of-defun
;;   C-M-h           mark-defun
;;   C-M-SPC, C-M-@  mark-sexp
;;   C-M-t           transpose-sexps
;;   M-\             delete-horizontal-space
;;   C-S-backspace   kill-whole-line
;;   C-x TAB         indent-rigidly (then ←/→, S-←/S-→)
;;   M-=             count-words-region (M-x count-words: whole buffer)
;;   C-x =           what-cursor-position
;;   M-/             dabbrev-expand (repeat to cycle)
;;   C-x 8 RET       insert-char
;;   M-x             delete-trailing-whitespace, align-regexp, keep-lines,
;;                   flush-lines, delete-duplicate-lines
;;
;; Sexps: Lisp buffers read as Lisp; every other buffer reads as
;; bracket-balanced code — `(…)`, `[…]`, `{…}`, identifiers and string
;; literals, with strings and comments taken from the syntax tree — so the
;; same keys work in Rust, Python or JSON.

;; --- small helpers --------------------------------------------------------------
(define (extras--blank? c) (or (equal? c " ") (equal? c "\t")))

;; Letters and digits (any non-ASCII char counts as a letter).
(define (extras--alnum? c)
  (let ((k (char->integer c)))
    (or (and (>= k 48) (<= k 57)) (and (>= k 65) (<= k 90)) (and (>= k 97) (<= k 122)) (> k 127))))

;; Byte offsets [s e) covering whole lines: the region's lines, else the
;; fallback range. `e` stops before the last line's newline.
(define (extras--line-span a b)
  (let* ((r (region-or a b)) (s (car r)) (e (cadr r)))
    (list (line-start-pos s)
          (line-end-pos (if (and (> e s) (= e (line-start-pos e))) (- e 1) e)))))

;; Split `text` into lines; a trailing newline does not make a last "" line.
(define (extras--lines text)
  (if (equal? text "") '() (split-many text "\n")))

;; Lower-case hex digits of a non-negative integer.
(define (extras--hex n)
  (let loop ((n n) (acc '()))
    (let* ((d (modulo n 16))
           (acc (cons (string-ref "0123456789abcdef" d) acc)))
      (if (< n 16) (list->string acc) (loop (quotient n 16) acc)))))

;; At least four upper-case hex digits, as in U+00E9.
(define (extras--hex4 n)
  (let ((h (string-upcase (extras--hex n))))
    (string-append (string-repeat "0" (- 4 (string-length h))) h)))

;; Octal digits, for what-cursor-position's Emacs-style #o field.
(define (extras--octal n)
  (let loop ((n n) (acc '()))
    (let ((acc (cons (string-ref "01234567" (modulo n 8)) acc)))
      (if (< n 8) (list->string acc) (loop (quotient n 8) acc)))))

;; A hex string ("1F600", "U+1F600", "#x41") as an integer, or #f.
(define (extras--parse-hex s)
  (let* ((t (string-upcase (string-trim s)))
         (t (cond ((starts-with? t "U+") (substring t 2 (string-length t)))
                  ((starts-with? t "#X") (substring t 2 (string-length t)))
                  ((starts-with? t "0X") (substring t 2 (string-length t)))
                  (else t))))
    (and (> (string-length t) 0)
         (<= (string-length t) 6)
         (let loop ((cs (string->list t)) (n 0))
           (if (null? cs)
               n
               (let ((c (car cs)))
                 (let ((k (char->integer c)))
                   (cond ((and (>= k 48) (<= k 57)) (loop (cdr cs) (+ (* n 16) (- k 48))))
                         ((and (>= k 65) (<= k 70)) (loop (cdr cs) (+ (* n 16) (- k 55))))
                         (else #f)))))))))

;; A scalar value `integer->char` accepts.
(define (extras--valid-code-point? n)
  (and n (>= n 0) (<= n 1114111) (not (and (>= n 55296) (<= n 57343)))))

;; Case-insensitive unless the pattern has an upper-case letter (Emacs's
;; case-fold-search default for keep-lines and friends).
(define (extras--fold-case pattern)
  (if (equal? pattern (string-downcase pattern)) (string-append "(?i)" pattern) pattern))

;; --- defuns ----------------------------------------------------------------------
;; Named function-like nodes across the bundled grammars. Lambdas and closures
;; are left out: C-M-a should find the enclosing named function, not the
;; callback inside it. Extend per language with `(set! *defun-node-kinds* …)`.
(define *defun-node-kinds*
  '("function_item" "function_definition" "function_declaration" "method_declaration"
    "method_definition" "generator_function_declaration" "constructor_declaration"
    "method" "singleton_method" "function_signature_item" "fn_declaration"
    "local_function" "test_declaration" "value_definition" "function_clause"
    "decorated_definition"))

(define (extras--flat->pairs flat)
  (if (or (null? flat) (null? (cdr flat)))
      '()
      (cons (cons (car flat) (cadr flat)) (extras--flat->pairs (cddr flat)))))

;; Every defun as (start . end), in document order. Lisp: the top-level forms.
;; Code with a syntax tree: its function nodes. Anything else: top-level `{…}`
;; blocks, widened back to the start of the line that opens them.
(define (defun-ranges)
  (cond
    ((lisp-buffer?) (extras--flat->pairs (sexp-top-level-ranges)))
    (else
     (let ((ts (extras--flat->pairs (ts-ranges-of-kinds *defun-node-kinds*))))
       (if (not (null? ts))
           ts
           (filter-map-pairs
            (lambda (r)
              (and (equal? (buffer-substring (car r) (+ (car r) 1)) "{")
                   (cons (line-start-pos (car r)) (cdr r))))
            (extras--flat->pairs (sexp-top-level-ranges))))))))

(define (filter-map-pairs f xs)
  (let loop ((xs xs) (acc '()))
    (if (null? xs)
        (reverse acc)
        (let ((v (f (car xs)))) (loop (cdr xs) (if v (cons v acc) acc))))))

;; The innermost defun containing `p`, else the first one after it, or #f.
(define (defun-at p)
  (let loop ((rs (defun-ranges)) (inner #f) (next #f))
    (if (null? rs)
        (or inner next)
        (let ((r (car rs)))
          (loop (cdr rs)
                (if (and (<= (car r) p) (< p (cdr r))
                         (or (not inner) (>= (car r) (car inner))))
                    r
                    inner)
                (if (and (not next) (> (car r) p)) r next))))))

;; Past a defun's end, through its newline — where Emacs leaves point.
(define (extras--after-newline e)
  (if (equal? (buffer-substring e (+ e 1)) "\n") (+ e 1) e))

;; C-M-a: to the start of the defun around point, or of the previous one.
(define (beginning-of-defun)
  (let* ((p (point))
         (starts (filter (lambda (s) (< s p)) (map car (defun-ranges)))))
    (if (null? starts)
        (message "No previous defun")
        (goto-char (apply max starts)))))

;; C-M-e: to the end of the defun around point, or of the next one.
(define (end-of-defun)
  (let* ((p (point))
         (ends (filter (lambda (e) (> (extras--after-newline e) p))
                       (map cdr (defun-ranges)))))
    (if (null? ends)
        (message "No next defun")
        ;; The nearest end ahead is the innermost defun's end, or the next one's.
        (goto-char (extras--after-newline (apply min ends))))))

;; C-M-h: region around the defun at point (point at its start).
(define (mark-defun)
  (let ((r (defun-at (point))))
    (if r
        (set-region (extras--after-newline (cdr r)) (car r))
        (message "No defun here"))))

;; --- sexps -----------------------------------------------------------------------
;; C-M-SPC / C-M-@: mark the sexp after point (point stays). With the region
;; already active, extend it by one more sexp — repeat to grow.
(define (mark-sexp)
  (let* ((p (point))
         (from (if (region-active?) (max (region-beginning) (region-end)) p))
         (anchor (if (region-active?) (min (region-beginning) (region-end)) p))
         (e (sexp-forward-pos from)))
    (if (< e 0)
        (message "No further sexp")
        (set-region e anchor))))

;; C-M-t: swap the sexps before and after point; point ends after both.
(define (transpose-sexps)
  (let* ((p (point))
         (next-end (sexp-forward-pos p))
         (next-start (if (< next-end 0) -1 (sexp-backward-pos next-end)))
         (prev-start (sexp-backward-pos p))
         (prev-end (if (< prev-start 0) -1 (sexp-forward-pos prev-start))))
    (if (and (>= prev-start 0) (>= next-start 0) (<= prev-end next-start) (< prev-start next-start))
        (begin
          (replace-region prev-start next-end
                          (string-append (buffer-substring next-start next-end)
                                         (buffer-substring prev-end next-start)
                                         (buffer-substring prev-start prev-end)))
          (goto-char next-end))
        (message "Don't have two things to transpose"))))

;; --- whitespace and lines -----------------------------------------------------------
;; M-\: delete all spaces and tabs around point.
(define (delete-horizontal-space)
  (let* ((mx (point-max))
         (s (let loop ((i (point)))
              (if (and (> i 0) (extras--blank? (buffer-substring (- i 1) i))) (loop (- i 1)) i)))
         (e (let loop ((i (point)))
              (if (and (< i mx) (extras--blank? (buffer-substring i (+ i 1)))) (loop (+ i 1)) i))))
    (when (< s e) (delete-region s e))))

;; C-S-backspace: kill the whole line, newline included, wherever point is.
(define (kill-whole-line)
  (let* ((s (line-start-pos (point)))
         (e (line-end-pos (point)))
         (mx (point-max)))
    (cond ((< e mx) (kill-region-between s (+ e 1)))
          ((> s 0) (kill-region-between (- s 1) e))   ; last line: take the newline before
          (else (kill-region-between s e)))))

;; delete-trailing-whitespace: strip trailing blanks from every line of the
;; region, or of the buffer — which also drops blank lines at its end.
(define (extras--strip-trailing text) (replace-regexp-in-string "(?m)[ \t]+$" "" text))
(define (delete-trailing-whitespace)
  (let* ((whole (not (region-active?)))
         (span (if whole (list 0 (point-max)) (extras--line-span (point) (point))))
         (s (car span)) (e (cadr span)) (p (point))
         (text (buffer-substring s e))
         (stripped (extras--strip-trailing text))
         (new (if whole (replace-regexp-in-string "\n\\s*\\z" "\n" stripped) stripped)))
    (if (equal? new text)
        (message "No trailing whitespace")
        (let* ((ls (max s (min e (line-start-pos p))))
               (before (utf8-length (extras--strip-trailing (buffer-substring s ls))))
               (line (extras--strip-trailing (buffer-substring ls (line-end-pos ls)))))
          (replace-region s e new)
          ;; Keep point on its line rather than at the end of the rewrite.
          (goto-char (min (+ s (utf8-length new))
                          (+ s before (min (- p ls) (utf8-length line)))))))))

;; keep-lines / flush-lines: keep (or delete) the lines matching a regexp, in
;; the region or from the line after point to the end of the buffer.
(define (extras--rest-span)
  (if (region-active?)
      (extras--line-span (point) (point))
      (let* ((p (point)) (s (if (= p (line-start-pos p)) p (min (point-max) (+ (line-end-pos p) 1)))))
        (list s (point-max)))))

(define (extras--filter-lines keep? pattern)
  (let* ((span (extras--rest-span)) (s (car span)) (e (cadr span))
         (text (buffer-substring s e))
         (trail (ends-with? text "\n"))
         (body (if trail (substring text 0 (- (string-length text) 1)) text))
         (lines (extras--lines body))
         (re (extras--fold-case pattern))
         (kept (filter (lambda (l) (eq? keep? (>= (string-match re l) 0))) lines))
         (gone (- (length lines) (length kept))))
    (when (> gone 0)
      (replace-region s e (string-append (join-lines kept) (if (and trail (pair? kept)) "\n" "")))
      (goto-char s))
    (message (string-append "Deleted " (number->string gone)
                            (if (= gone 1) " line" " lines")))))

(define (keep-lines) (read-string "Keep lines containing match for regexp: " 'keep-lines-finish))
(define (keep-lines-finish pattern)
  (unless (equal? pattern "") (extras--filter-lines #t pattern)))
(define (flush-lines) (read-string "Flush lines containing match for regexp: " 'flush-lines-finish))
(define (flush-lines-finish pattern)
  (unless (equal? pattern "") (extras--filter-lines #f pattern)))

;; delete-duplicate-lines: drop every repeat of an earlier line in the region
;; (whole buffer without one), keeping the first.
(define (extras--dedup-lines lines)
  (let loop ((ls lines) (seen (hash)) (acc '()))
    (cond ((null? ls) (reverse acc))
          ((hash-try-get seen (car ls)) (loop (cdr ls) seen acc))
          (else (loop (cdr ls) (hash-insert seen (car ls) #t) (cons (car ls) acc))))))
(define (delete-duplicate-lines)
  (let* ((span (if (region-active?) (extras--line-span (point) (point)) (list 0 (point-max))))
         (s (car span)) (e (cadr span))
         (text (buffer-substring s e))
         (trail (ends-with? text "\n"))
         (body (if trail (substring text 0 (- (string-length text) 1)) text))
         (lines (extras--lines body))
         (kept (extras--dedup-lines lines))
         (gone (- (length lines) (length kept))))
    (when (> gone 0)
      (replace-region s e (string-append (join-lines kept) (if trail "\n" "")))
      (goto-char s))
    (message (string-append "Deleted " (number->string gone) " duplicate"
                            (if (= gone 1) " line" " lines")))))

;; align-regexp: pad the whitespace before the first match of a regexp on each
;; line of the region (or paragraph) so the matches line up one space past
;; the longest text before them.
(define (align-regexp) (read-string "Align regexp: " 'align-regexp-finish))
(define (extras--align-split line re)
  ;; (before rest) around the first match, the blanks before it dropped; #f when none.
  (let ((m (string-match re line)))
    (if (< m 0)
        #f
        (let* ((head (byte-take line m))
               (before (trim-end head)))
          (list before (byte-drop line m))))))
(define (align-text text re)
  (let* ((lines (extras--lines text))
         (splits (map (lambda (l) (extras--align-split l re)) lines))
         (col (apply max (cons 0 (map (lambda (sp) (if sp (string-length (car sp)) 0)) splits)))))
    (let loop ((ls lines) (sps splits) (acc '()))
      (if (null? ls)
          (join-lines (reverse acc))
          (let ((l (car ls)) (sp (car sps)))
            (loop (cdr ls) (cdr sps)
                  (cons (if (and sp (> (string-length (car sp)) 0))
                            (string-append (car sp)
                                           (string-repeat " " (+ 1 (- col (string-length (car sp)))))
                                           (cadr sp))
                            l)
                        acc)))))))
(define (align-regexp-finish re)
  (unless (equal? re "")
    (let* ((para (paragraph-bounds (point)))
           (span (if (region-active?)
                     (extras--line-span (point) (point))
                     (if para (list (car para) (cadr para)) (extras--line-span (point) (point)))))
           (s (car span)) (e (cadr span))
           (text (buffer-substring s e))
           (new (align-text text re)))
      (unless (equal? new text) (replace-region s e new)))))

;; C-x TAB: indent-rigidly. Shifts every line of the region (else the line at
;; point) by a prefix count when one was given; otherwise it reads keys:
;; ← / → move one column, S-← / S-→ four, and any other key stops. Blank
;; lines are left alone, as in Emacs.
(define *indent-rigidly-span* #f)   ; (start line-count) while adjusting
(define (extras--shift-line line n)
  (cond ((equal? line "") line)
        ((> n 0) (string-append (string-repeat " " n) line))
        (else
         (let loop ((l line) (k (- n)))
           (if (and (> k 0) (> (string-length l) 0) (extras--blank? (substring l 0 1)))
               (loop (substring l 1 (string-length l)) (- k 1))
               l)))))
(define (extras--count-lines text) (length (extras--lines text)))
(define (indent-rigidly-by n)
  (when *indent-rigidly-span*
    (let* ((s (car *indent-rigidly-span*))
           (count (cadr *indent-rigidly-span*))
           (e (let loop ((i s) (k 1))
                (if (or (>= k count) (>= (line-end-pos i) (point-max)))
                    (line-end-pos i)
                    (loop (+ (line-end-pos i) 1) (+ k 1)))))
           (text (buffer-substring s e))
           (new (join-lines (map (lambda (l) (extras--shift-line l n)) (extras--lines text)))))
      (unless (equal? new text)
        (replace-region s e new)
        (set-region s (+ s (utf8-length new)))))))
(define (indent-rigidly)
  (let* ((span (extras--line-span (point) (point))) (s (car span)) (e (cadr span))
         (n (prefix-count)))
    (set! *indent-rigidly-span* (list s (max 1 (extras--count-lines (buffer-substring s e)))))
    (if (> n 0)
        ;; With a count (C-u 8 C-x TAB) shift by it at once, no key loop.
        (begin (indent-rigidly-by n) (set! *indent-rigidly-span* #f))
        (begin
          (set-region s e)
          (read-key-start "Indent rigidly: ←/→ by 1, S-←/S-→ by 4, any other key to stop"
                          'indent-rigidly-key)))))
(define (indent-rigidly-key key)
  (cond ((equal? key "left") (indent-rigidly-by -1))
        ((equal? key "right") (indent-rigidly-by 1))
        ((equal? key "shift-left") (indent-rigidly-by -4))
        ((equal? key "shift-right") (indent-rigidly-by 4))
        (else (set! *indent-rigidly-span* #f) (read-key-stop))))

;; --- counting and position ---------------------------------------------------------
;; (lines words chars) of a text. Words are runs of letters and digits.
(define (count-text text)
  (let loop ((cs (string->list text)) (in-word #f) (words 0) (nl 0) (chars 0))
    (if (null? cs)
        (list (+ nl (if (or (equal? text "") (ends-with? text "\n")) 0 1)) words chars)
        (let* ((c (car cs)) (w (extras--alnum? c)))
          (loop (cdr cs) w (if (and w (not in-word)) (+ words 1) words)
                (if (char=? c #\newline) (+ nl 1) nl) (+ chars 1))))))
(define (extras--count-message what text)
  (let ((c (count-text text)))
    (message (string-append what " has " (number->string (car c)) " line"
                            (if (= (car c) 1) "" "s") ", "
                            (number->string (cadr c)) " word" (if (= (cadr c) 1) "" "s")
                            ", and " (number->string (caddr c)) " character"
                            (if (= (caddr c) 1) "" "s") "."))))
;; M-=: lines, words and characters in the region (the buffer without one).
(define (count-words-region)
  (if (region-active?)
      (extras--count-message "Region" (buffer-substring (region-beginning) (region-end)))
      (count-words)))
(define (count-words) (extras--count-message "Buffer" (buffer-string)))

;; How a character reads in what-cursor-position: control chars as C-x.
(define (extras--char-label ch)
  (let ((n (char->integer ch)))
    (cond ((= n 10) "C-j")
          ((= n 9) "TAB")
          ((= n 32) "SPC")
          ((< n 32) (string-append "C-" (string (integer->char (+ n 96)))))
          (else (string ch)))))
;; C-x =: the character after point and where point is.
(define (what-cursor-position)
  (let* ((p (point)) (mx (point-max))
         (pos (+ 1 (string-length (buffer-substring 0 p))))
         (total (+ 1 (string-length (buffer-string))))
         (pct (if (<= mx 0) 0 (quotient (* 100 p) mx)))
         (col (string-length (buffer-substring (line-start-pos p) p)))
         (where (string-append "point=" (number->string pos) " of " (number->string total)
                               " (" (number->string pct) "%) column=" (number->string col))))
    (if (>= p mx)
        (message where)
        (let* ((ch (string-ref (buffer-substring p (grapheme-forward p)) 0))
               (n (char->integer ch)))
          (message (string-append "Char: " (extras--char-label ch)
                                  " (" (number->string n) ", #o" (extras--octal n)
                                  ", #x" (extras--hex n) ", U+"
                                  (extras--hex4 n)
                                  ") " where))))))

;; --- dabbrev-expand (M-/) ----------------------------------------------------------
;; Expand the word before point to the nearest word that starts with it:
;; first backward then forward through this buffer, then the other file
;; buffers. Repeating M/ right away replaces the expansion with the next
;; candidate; after the last one the original text comes back.
(define *dabbrev-state* #f)   ; (start prefix candidates index end)

(define (extras--word-char? c)
  (or (extras--alnum? c) (char=? c #\_)))

;; The word-ish text right before point.
(define (dabbrev--prefix)
  (let* ((p (point))
         (back (buffer-substring (max 0 (- p 256)) p)))
    (let loop ((cs (reverse (string->list back))) (acc '()))
      (if (and (pair? cs) (extras--word-char? (car cs)))
          (loop (cdr cs) (cons (car cs) acc))
          (list->string acc)))))

(define (dabbrev--candidates prefix p)
  (let* ((re (string-append "\\b" (regexp-quote prefix) "\\w+"))
         (strings (lambda (recs) (map caddr (filter pair? recs))))
         (before (let ((r (buffer-search-all re 0 p))) (if (list? r) (reverse (strings r)) '())))
         (after (let ((r (buffer-search-all re p (point-max)))) (if (list? r) (strings r) '())))
         (others (apply append
                        (map (lambda (b)
                               (let ((f (buffer-file-name-of b)))
                                 (if (or (equal? f "") (equal? b (current-buffer)))
                                     '()
                                     (regexp-all-matches re (read-file f)))))
                             (buffer-names)))))
    (filter (lambda (w) (not (equal? w prefix)))
            (extras--dedup-lines (append before after others)))))

(define (dabbrev--insert start old-end word)
  (replace-region start old-end word)
  (goto-char (+ start (utf8-length word))))

(define (dabbrev--continuing?)
  (and *dabbrev-state*
       (let* ((st *dabbrev-state*) (start (car st)) (end (list-ref st 4))
              (cands (caddr st)) (i (list-ref st 3)))
         (and (= (point) end)
              (equal? (buffer-substring start end) (list-ref cands i))))))

(define (dabbrev-expand)
  (if (dabbrev--continuing?)
      (let* ((st *dabbrev-state*) (start (car st)) (prefix (cadr st))
             (cands (caddr st)) (i (+ 1 (list-ref st 3))) (end (list-ref st 4)))
        (if (>= i (length cands))
            (begin
              (dabbrev--insert start end prefix)
              (set! *dabbrev-state* #f)
              (message "No further dynamic expansions"))
            (let ((w (list-ref cands i)))
              (dabbrev--insert start end w)
              (set! *dabbrev-state* (list start prefix cands i (+ start (utf8-length w)))))))
      (let* ((prefix (dabbrev--prefix)) (p (point)) (start (- p (utf8-length prefix))))
        (if (equal? prefix "")
            (message "No dynamic expansion here")
            (let ((cands (dabbrev--candidates prefix p)))
              (if (null? cands)
                  (begin (set! *dabbrev-state* #f)
                         (message (string-append "No dynamic expansion for \"" prefix "\"")))
                  (let ((w (car cands)))
                    (dabbrev--insert start p w)
                    (set! *dabbrev-state* (list start prefix cands 0 (+ start (utf8-length w)))))))))))

;; --- insert-char (C-x 8 RET) -------------------------------------------------------
;; Type a hex code point (`1F600`, `U+00E9`) or pick from a table of common
;; characters by name. There is no full Unicode name table in the build, so
;; the list is short; any code point works through hex.
(define *insert-char-names*
  '(("NO-BREAK SPACE" . 160) ("EN DASH" . 8211) ("EM DASH" . 8212)
    ("HORIZONTAL ELLIPSIS" . 8230) ("BULLET" . 8226) ("MIDDLE DOT" . 183)
    ("LEFT SINGLE QUOTATION MARK" . 8216) ("RIGHT SINGLE QUOTATION MARK" . 8217)
    ("LEFT DOUBLE QUOTATION MARK" . 8220) ("RIGHT DOUBLE QUOTATION MARK" . 8221)
    ("LEFT-POINTING DOUBLE ANGLE QUOTATION MARK" . 171)
    ("RIGHT-POINTING DOUBLE ANGLE QUOTATION MARK" . 187)
    ("SECTION SIGN" . 167) ("PILCROW SIGN" . 182) ("DAGGER" . 8224)
    ("COPYRIGHT SIGN" . 169) ("REGISTERED SIGN" . 174) ("TRADE MARK SIGN" . 8482)
    ("DEGREE SIGN" . 176) ("PLUS-MINUS SIGN" . 177) ("MULTIPLICATION SIGN" . 215)
    ("DIVISION SIGN" . 247) ("NOT EQUAL TO" . 8800) ("LESS-THAN OR EQUAL TO" . 8804)
    ("GREATER-THAN OR EQUAL TO" . 8805) ("ALMOST EQUAL TO" . 8776)
    ("IDENTICAL TO" . 8801) ("INFINITY" . 8734) ("SQUARE ROOT" . 8730)
    ("N-ARY SUMMATION" . 8721) ("N-ARY PRODUCT" . 8719) ("INTEGRAL" . 8747)
    ("PARTIAL DIFFERENTIAL" . 8706) ("NABLA" . 8711) ("INCREMENT" . 8710)
    ("FOR ALL" . 8704) ("THERE EXISTS" . 8707) ("ELEMENT OF" . 8712)
    ("NOT AN ELEMENT OF" . 8713) ("EMPTY SET" . 8709) ("SUBSET OF" . 8834)
    ("SUPERSET OF" . 8835) ("INTERSECTION" . 8745) ("UNION" . 8746)
    ("LOGICAL AND" . 8743) ("LOGICAL OR" . 8744) ("NOT SIGN" . 172)
    ("LEFTWARDS ARROW" . 8592) ("UPWARDS ARROW" . 8593) ("RIGHTWARDS ARROW" . 8594)
    ("DOWNWARDS ARROW" . 8595) ("LEFT RIGHT ARROW" . 8596)
    ("RIGHTWARDS DOUBLE ARROW" . 8658) ("LEFT RIGHT DOUBLE ARROW" . 8660)
    ("MICRO SIGN" . 181) ("PER MILLE SIGN" . 8240) ("PRIME" . 8242)
    ("DOUBLE PRIME" . 8243) ("CHECK MARK" . 10003) ("BALLOT X" . 10007)
    ("BLACK STAR" . 9733) ("WHITE STAR" . 9734) ("BLACK HEART SUIT" . 9829)
    ("SNOWMAN" . 9731) ("EURO SIGN" . 8364) ("POUND SIGN" . 163) ("YEN SIGN" . 165)
    ("CENT SIGN" . 162) ("INDIAN RUPEE SIGN" . 8377) ("BITCOIN SIGN" . 8383)
    ("INVERTED QUESTION MARK" . 191) ("INVERTED EXCLAMATION MARK" . 161)
    ("GREEK SMALL LETTER ALPHA" . 945) ("GREEK SMALL LETTER BETA" . 946)
    ("GREEK SMALL LETTER GAMMA" . 947) ("GREEK SMALL LETTER DELTA" . 948)
    ("GREEK SMALL LETTER EPSILON" . 949) ("GREEK SMALL LETTER THETA" . 952)
    ("GREEK SMALL LETTER LAMDA" . 955) ("GREEK SMALL LETTER MU" . 956)
    ("GREEK SMALL LETTER PI" . 960) ("GREEK SMALL LETTER SIGMA" . 963)
    ("GREEK SMALL LETTER TAU" . 964) ("GREEK SMALL LETTER PHI" . 966)
    ("GREEK SMALL LETTER OMEGA" . 969) ("GREEK CAPITAL LETTER DELTA" . 916)
    ("GREEK CAPITAL LETTER SIGMA" . 931) ("GREEK CAPITAL LETTER OMEGA" . 937)
    ("LATIN SMALL LETTER E WITH ACUTE" . 233) ("LATIN SMALL LETTER N WITH TILDE" . 241)
    ("LATIN SMALL LETTER SHARP S" . 223) ("LATIN SMALL LETTER U WITH DIAERESIS" . 252)
    ("ZERO WIDTH SPACE" . 8203) ("SMILING FACE WITH SMILING EYES" . 128522)
    ("THUMBS UP SIGN" . 128077) ("FIRE" . 128293) ("ROCKET" . 128640)))

(define (extras--u+ n) (string-append "U+" (extras--hex4 n)))

;; Live candidates: a hex code point first (when the input is one), then the
;; table entries whose name contains every typed word.
(define (insert-char-candidates pattern)
  (let* ((n (extras--parse-hex pattern))
         (hex (if (extras--valid-code-point? n)
                  (list (list (extras--u+ n) (string (integer->char n))))
                  '()))
         (named (filter (lambda (e) (all-words-match? pattern (car e))) *insert-char-names*)))
    (append hex
            (map (lambda (e) (list (car e) (string-append (string (integer->char (cdr e)))
                                                          "  " (extras--u+ (cdr e)))))
                 named))))

(define (insert-char-by-name choice)
  (let* ((entry (assoc choice *insert-char-names*))
         (n (if entry (cdr entry) (extras--parse-hex choice))))
    (if (extras--valid-code-point? n)
        (insert (string (integer->char n)))
        (message (string-append "Not a character: " choice)))))

(define (insert-char)
  (helm (list (list "Insert character (name or hex code point)"
                    (list 'raw "insert-char-candidates")
                    'insert-char-by-name))))

;; --- keys and M-x --------------------------------------------------------------------
(define-key "fundamental" "ctrl-alt-f" 'forward-sexp)
(define-key "fundamental" "ctrl-alt-b" 'backward-sexp)
(define-key "fundamental" "ctrl-alt-u" 'backward-up-list)
(define-key "fundamental" "ctrl-alt-d" 'down-list)
(define-key "fundamental" "ctrl-alt-n" 'forward-list)
(define-key "fundamental" "ctrl-alt-p" 'backward-list)
(define-key "fundamental" "ctrl-alt-k" 'kill-sexp)
(define-key "fundamental" "ctrl-alt-a" 'beginning-of-defun)
(define-key "fundamental" "ctrl-alt-e" 'end-of-defun)
(define-key "fundamental" "ctrl-alt-h" 'mark-defun)
(define-key "fundamental" "ctrl-alt-space" 'mark-sexp)
(define-key "fundamental" "ctrl-alt-@" 'mark-sexp)
(define-key "fundamental" "ctrl-alt-t" 'transpose-sexps)
(define-key "fundamental" "alt-\\" 'delete-horizontal-space)
(define-key "fundamental" "ctrl-shift-backspace" 'kill-whole-line)
(define-key "fundamental" "ctrl-x tab" 'indent-rigidly)
(define-key "fundamental" "alt-=" 'count-words-region)
(define-key "fundamental" "ctrl-x =" 'what-cursor-position)
(define-key "fundamental" "alt-/" 'dabbrev-expand)
(define-key "fundamental" "ctrl-x 8 enter" 'insert-char)

(register-command 'beginning-of-defun)
(register-command 'end-of-defun)
(register-command 'mark-defun)
(register-command 'mark-sexp)
(register-command 'transpose-sexps)
(register-command 'delete-horizontal-space)
(register-command 'kill-whole-line)
(register-command 'indent-rigidly)
(register-command 'count-words-region)
(register-command 'count-words)
(register-command 'what-cursor-position)
(register-command 'delete-trailing-whitespace)
(register-command 'align-regexp)
(register-command 'keep-lines)
(register-command 'flush-lines)
(register-command 'delete-duplicate-lines)
(register-command 'dabbrev-expand)
(register-command 'insert-char)
