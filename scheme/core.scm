;; requires:

;; Core Lisp-style helpers built on the new read primitives (point, goto-char,
;; buffer-string, …). Loaded early so later libs (magit, projectile) can use them.

;; save-excursion: run body, then restore point. Mirrors Emacs's special form.
;; `goto-char` is queued as a command, so point is restored after body's edits.
;; CAVEAT (snapshot model): reads see a pre-command snapshot and edits are
;; queued, so the saved point is the pre-body offset — if the body inserts or
;; deletes text BEFORE that offset, the restored point lands off by the edited
;; delta. Fine for read-only bodies; be wary around mutating ones.
(define-syntax save-excursion
  (syntax-rules ()
    ((_ body ...)
     (let ((saved-point (point)))
       (let ((result (begin body ...)))
         (goto-char saved-point)
         result)))))

;; --- small list/path utilities ---------------------------------------------

(define (last lst)
  (if (null? (cdr lst)) (car lst) (last (cdr lst))))

;; 0-based list indexing (Steel may lack list-ref; this is always available).
(define (nth lst i)
  (if (= i 0) (car lst) (nth (cdr lst) (- i 1))))

;; --- UTF-8 byte arithmetic ---------------------------------------------------
;; The editor's overlay/byte APIs (and string-match / regexp-captures offsets)
;; are BYTE-based, while substring / string-length are CHAR-indexed. These
;; helpers convert safely; never mix a byte offset into substring directly.

;; UTF-8 byte width of one character.
(define (char-bytes ch)
  (let ((cp (char->integer ch)))
    (cond ((< cp 128) 1) ((< cp 2048) 2) ((< cp 65536) 3) (else 4))))

;; UTF-8 byte length of a string — the byte-based twin of `string-length`.
(define (utf8-length s)
  (let loop ((chars (string->list s)) (n 0))
    (if (null? chars) n (loop (cdr chars) (+ n (char-bytes (car chars)))))))

;; Drop the first `n` BYTES of `s`, returning the remainder. Lets a scanner
;; advance past a match it located by byte offset (string-match).
(define (byte-drop s n)
  (if (<= n 0)
      s
      (let loop ((chars (string->list s)) (acc 0))
        (cond ((null? chars) "")
              ((>= acc n) (list->string chars))
              (else (loop (cdr chars) (+ acc (char-bytes (car chars)))))))))

;; The first `n` BYTES of `s` — the head-side twin of `byte-drop`, for splitting
;; a string at a byte offset returned by `string-match`.
(define (byte-take s n)
  (if (<= n 0)
      ""
      (let loop ((chars (string->list s)) (acc 0) (out '()))
        (cond ((null? chars) s)
              ((>= acc n) (list->string (reverse out)))
              (else (loop (cdr chars)
                          (+ acc (char-bytes (car chars)))
                          (cons (car chars) out)))))))

;; Byte offset of the start of line `i` in a `lines` list (each line + newline).
(define (lines-byte-offset lines i)
  (let loop ((k 0) (acc 0))
    (if (>= k i) acc (loop (+ k 1) (+ acc (utf8-length (nth lines k)) 1)))))

(define (alist-ref alist key)
  (cond ((null? alist) #f)
        ((equal? (car (car alist)) key) (cdr (car alist)))
        (else (alist-ref (cdr alist) key))))

;; `alist` without any entry keyed `key` (order of the rest preserved).
(define (alist-remove alist key)
  (filter (lambda (p) (not (equal? (car p) key))) alist))

;; `alist` with `key` bound to `v`: the new entry goes to the FRONT and any old
;; entry for `key` is dropped. A record list works too — `(alist-set al id
;; (list a b))` stores the row `(id a b)`.
(define (alist-set alist key v)
  (cons (cons key v) (alist-remove alist key)))

;; The value after `key` in a flat (key value …) plist, else `default` (#f when
;; omitted). Keys compare with `equal?`, so symbols and strings both work.
(define (plist-get opts key . default)
  (let loop ((opts opts))
    (cond ((or (null? opts) (null? (cdr opts))) (if (null? default) #f (car default)))
          ((equal? (car opts) key) (cadr opts))
          (else (loop (cddr opts))))))

;; Safe field access on a parsed JSON object (string->jsexpr hashes key by
;; symbol): the value, or #f when `o` isn't a hash or lacks `k`.
(define (json-get o k) (if (hash? o) (hash-try-get o k) #f))
;; A JSON number (a float after parsing) as an exact integer; 0 if not a number.
(define (json-int x) (if (number? x) (exact (round x)) 0))

;; Does `s` end with any of the suffixes in `exts`?
(define (ends-with-any? s exts)
  (let loop ((es exts))
    (cond ((null? es) #f)
          ((ends-with? s (car es)) #t)
          (else (loop (cdr es))))))

;; The extension of a file name, after the last "." of its final path segment
;; ("src/main.rs" -> "rs", ".bashrc" -> "bashrc"; "" for "Makefile" or "a.d/x").
(define (file-extension name)
  (let loop ((i (- (string-length name) 1)))
    (cond ((< i 0) "")
          ((equal? (substring name i (+ i 1)) ".") (substring name (+ i 1) (string-length name)))
          ((equal? (substring name i (+ i 1)) "/") "")
          (else (loop (- i 1))))))

(define (non-empty-lines s)
  (remove-equal "" (split-many s "\n")))

;; Join a list of strings with `sep` in one pass (one string-append over the
;; interleaved pieces — no per-element recursion, so it's linear and safe on a
;; 50k-line list).
(define (string-join lst sep)
  (if (null? lst)
      ""
      (let loop ((rest (cdr lst)) (acc (list (car lst))))
        (if (null? rest)
            (apply string-append (reverse acc))
            (loop (cdr rest) (cons (car rest) (cons sep acc)))))))

;; Join a list of strings with newlines (inverse of split-many on "\n").
(define (join-lines lines) (string-join lines "\n"))

;; Concatenate a list of strings.
(define (string-concat lst) (apply string-append lst))

;; `s` repeated `n` times ("ab" 3 → "ababab"; n <= 0 → "").
(define (string-repeat s n)
  (if (<= n 0) "" (string-append s (string-repeat s (- n 1)))))

;; Pad `s` with trailing spaces to at least `width` CHARS (left-aligned; an
;; already-wide string is returned unchanged, never truncated).
(define (pad-right s width)
  (let ((len (string-length s)))
    (if (>= len width) s (string-append s (string-repeat " " (- width len))))))

;; Pad `s` with leading spaces to at least `width` CHARS (right-aligned; never
;; truncated).
(define (pad-left s width)
  (let ((len (string-length s)))
    (if (>= len width) s (string-append (string-repeat " " (- width len)) s))))

;; Distinct elements of `lst`, preserving first-seen order. With the optional
;; `key` procedure, an element is dropped when its key repeats an earlier one's.
(define (dedup lst . key)
  (let ((k (if (null? key) (lambda (x) x) (car key))))
    (let loop ((lst lst) (seen '()) (acc '()))
      (cond ((null? lst) (reverse acc))
            ((member (k (car lst)) seen) (loop (cdr lst) seen acc))
            (else (loop (cdr lst) (cons (k (car lst)) seen) (cons (car lst) acc)))))))

;; The last `n` elements of `lst` (all of it when shorter). The first `n` is
;; Steel's builtin `(take lst n)`.
(define (take-last lst n)
  (let ((len (length lst)))
    (if (<= len n) lst (list-tail lst (- len n)))))

;; Most-recently-used ring: `x` moved (or added) to the front of `lst`, any
;; other copy dropped, capped at `n` entries.
(define (mru-push x lst n)
  (take (cons x (remove-equal x lst)) n))

;; Trim leading and trailing spaces/tabs.
(define (string-trim s)
  (replace-regexp-in-string "^[ \t]+" ""
    (replace-regexp-in-string "[ \t]+$" "" s)))

;; Toggle the case of every cased character in `s` (vim `~`); uncased chars
;; pass through. Char-indexed — safe on multibyte text.
(define (string-toggle-case s)
  (string-concat
   (map (lambda (ch)
          (let* ((c (list->string (list ch)))
                 (up (string-upcase c))
                 (dn (string-downcase c)))
            (if (equal? c up) dn up)))
        (string->list s))))

;; --- line comments -----------------------------------------------------------
;; Shared by comment-dwim (M-;, edit.scm) and evil's `gc` operator (evil.scm).
;; The comment leader is the buffer-local "comment-string" when a mode set one
;; (e.g. steel-mode in edit.scm), else by language / major mode, else "// ".
(define *comment-strings*
  (list (cons "python" "# ") (cons "ruby" "# ") (cons "bash" "# ") (cons "yaml" "# ")
        (cons "toml" "# ") (cons "r" "# ") (cons "elixir" "# ") (cons "makefile" "# ")
        (cons "powershell" "# ")
        (cons "lua" "-- ") (cons "sql" "-- ") (cons "haskell" "-- ")
        (cons "scheme" ";; ") (cons "racket" ";; ") (cons "clojure" ";; ") (cons "steel" ";; ")
        (cons "latex" "% ") (cons "erlang" "% ") (cons "typst" "// ")
        (cons "ocaml" "(* ") (cons "html" "<!-- ") (cons "css" "/* ")
        (cons "markdown" "<!-- ")))
(define (comment-string)
  (let ((local (buffer-local-get "comment-string")))
    (if (not (equal? local ""))
        local
        (let ((by-lang (or (alist-ref *comment-strings* (buffer-language))
                           (alist-ref *comment-strings* (major-mode)))))
          (if by-lang by-lang "// ")))))
;; Block-style leaders close the line too.
(define (comment-closer cs)
  (cond ((equal? cs "(* ") " *)") ((equal? cs "<!-- ") " -->") ((equal? cs "/* ") " */") (else "")))

;; #t when `pred` holds for every element of `lst`.
(define (all? pred lst)
  (cond ((null? lst) #t) ((pred (car lst)) (all? pred (cdr lst))) (else #f)))

;; Toggle line comments over the lines spanning [s, e): when every non-blank
;; line is commented, uncomment them all; otherwise comment them all at the
;; shallowest indent (blank lines are left alone). Returns the start of the
;; first line.
(define (comment-lines s e)
  (let* ((cs (comment-string))
         (close (comment-closer cs))
         (bare (string-trim cs))
         (ls (line-start-pos s))
         (le (line-end-pos (max s (- e 1))))
         (lines (split-many (buffer-substring ls le) "\n"))
         (blank? (lambda (l) (equal? (string-trim l) "")))
         (indent-of (lambda (l) (let loop ((i 0) (cs (string->list l)))
                                  (if (and (pair? cs) (or (char=? (car cs) #\space) (char=? (car cs) #\tab)))
                                      (loop (+ i 1) (cdr cs))
                                      i))))
         (commented? (lambda (l) (starts-with? (trim-start l) bare)))
         (content (filter (lambda (l) (not (blank? l))) lines))
         (uncomment (and (pair? content) (all? commented? content)))
         (col (if (null? content) 0 (apply min (map indent-of content))))
         (new (map (lambda (l)
                     (cond ((blank? l) l)
                           (uncomment (comment-strip-line l bare (string-trim close)))
                           (else (string-append (substring l 0 col) cs
                                                (substring l col (string-length l)) close))))
                   lines)))
    (replace-region ls le (string-join new "\n"))
    ls))

;; Drop the first `bare` leader (and one following space) and a trailing
;; `closer`, keeping the indentation.
(define (comment-strip-line l bare closer)
  (let* ((n (string-length l))
         (ind (- n (string-length (trim-start l))))
         (after (+ ind (string-length bare)))
         (after (if (and (< after n) (equal? (substring l after (+ after 1)) " ")) (+ after 1) after))
         (body (substring l after n))
         (body (if (and (not (equal? closer "")) (ends-with? (trim-end body) closer))
                   (trim-end (substring body 0 (- (string-length (trim-end body)) (string-length closer))))
                   body)))
    (string-append (substring l 0 ind) body)))

;; Remove the comment leader `cs` (and its closer) from a commented `line`.
(define (uncomment-line line cs)
  (comment-strip-line line (string-trim cs) (string-trim (comment-closer cs))))

;; --- fill / word wrap ---------------------------------------------------------
;; Column the fill commands wrap at (Emacs `fill-column`, Vim `textwidth`).
;; Shared by fill-paragraph (M-q, editing.scm), evil's gq family (evil.scm) and
;; helix's gq reflow (helix.scm). Change it in init.scm with
;; (set! *fill-column* 100) or interactively with M-x set-fill-column.
(define *fill-column* 80)

;; Comment leaders that wrapping continues onto every new line (Vim keeps the
;; comment leader when formatting; Emacs calls this the fill prefix). Longest
;; first so ";;" is matched before ";".
(define *fill-leaders* (list ";;;" ";;" ";" "//" "#" "--" ">"))

;; The comment leader `rest` (a line minus its indentation) starts with, or #f.
;; The leader must be followed by whitespace or end the line — "# heading" has
;; a leader, "#hashtag" is just a word.
(define (fill-leader-of rest)
  (let loop ((ls *fill-leaders*))
    (cond ((null? ls) #f)
          ((and (starts-with? rest (car ls))
                (let ((r (substring rest (string-length (car ls)) (string-length rest))))
                  (or (equal? r "") (starts-with? r " ") (starts-with? r "\t"))))
           (car ls))
          (else (loop (cdr ls))))))

;; The fill prefix of `line`: its leading whitespace, plus "<leader> " when the
;; line opens with a comment leader ("  ;; aaa" → "  ;; ", "    aaa" → "    ").
(define (fill-prefix-of line)
  (let* ((rest   (trim-start line))
         (indent (substring line 0 (- (string-length line) (string-length rest))))
         (leader (fill-leader-of rest)))
    (if leader (string-append indent leader " ") indent)))

;; `line` with its indentation and comment leader (if any) stripped — the text
;; that becomes words when a paragraph is re-wrapped.
(define (fill-strip-leader line)
  (let* ((rest   (trim-start line))
         (leader (fill-leader-of rest)))
    (if leader
        (trim-start (substring rest (string-length leader) (string-length rest)))
        rest)))

;; The words of a line: split on runs of spaces/tabs, empties dropped.
(define (fill-words line)
  (filter (lambda (w) (not (equal? w "")))
          (split-many (replace-regexp-in-string "[ \t]+" " " line) " ")))

;; Re-wrap ONE paragraph (no blank lines in `text`) at `width` columns: the
;; words of every line are pooled and re-flowed greedily, each output line
;; starting with the FIRST line's fill prefix. A word that alone overflows
;; `width` gets its own line — never hard-broken. Pure and CHAR-indexed
;; throughout (columns are chars); callers translate buffer bytes.
(define (fill-string text width)
  (let* ((lines  (split-many text "\n"))
         (prefix (fill-prefix-of (car lines)))
         (words  (let loop ((ls lines))
                   (if (null? ls)
                       '()
                       (append (fill-words (fill-strip-leader (car ls)))
                               (loop (cdr ls)))))))
    (if (null? words)
        text
        (let loop ((ws (cdr words))
                   (cur (string-append prefix (car words)))
                   (out '()))
          (cond ((null? ws) (join-lines (reverse (cons cur out))))
                ((<= (+ (string-length cur) 1 (string-length (car ws))) width)
                 (loop (cdr ws) (string-append cur " " (car ws)) out))
                (else (loop (cdr ws) (string-append prefix (car ws)) (cons cur out))))))))

;; Re-wrap every blank-line-separated paragraph in `text`, preserving the blank
;; lines between them — the workhorse behind fill-region and visual-mode gq.
(define (fill-text text width)
  (let ((flush (lambda (para out)
                 (if (null? para)
                     out
                     (cons (fill-string (join-lines (reverse para)) width) out)))))
    (let loop ((lines (split-many text "\n")) (para '()) (out '()))
      (cond ((null? lines) (join-lines (reverse (flush para out))))
            ((equal? (string-trim (car lines)) "")
             (loop (cdr lines) '() (cons (car lines) (flush para out))))
            (else (loop (cdr lines) (cons (car lines) para) out))))))

;; --- paragraph bounds (buffer reads) ------------------------------------------
;; Is the buffer line holding byte `pos` blank (empty or only spaces/tabs)?
(define (blank-line-at? pos)
  (equal? (string-trim (buffer-substring (line-start-pos pos) (line-end-pos pos))) ""))

;; (start end) byte offsets of the blank-line-delimited paragraph containing
;; `pos` — start of its first line to end of its last (before that newline) —
;; or #f when pos sits on a blank line. Walks by lines via the line-start-pos /
;; line-end-pos bridges: a line's end is its "\n" (which belongs to that line),
;; so end±1 crosses into the neighbor.
(define (paragraph-bounds pos)
  (if (blank-line-at? pos)
      #f
      (list
       (let loop ((ls (line-start-pos pos)))
         (if (or (= ls 0) (blank-line-at? (- ls 1)))
             ls
             (loop (line-start-pos (- ls 1)))))
       (let ((mx (point-max)))
         (let loop ((le (line-end-pos pos)))
           (if (or (>= (+ le 1) mx) (blank-line-at? (+ le 1)))
               le
               (loop (line-end-pos (+ le 1)))))))))

;; The next paragraph boundary after `pos`: the start of the first blank line
;; past some non-blank text (Emacs M-}, vim `}`), or point-max at the end.
(define (paragraph-forward-pos pos)
  (let ((mx (point-max)))
    (let loop ((p pos) (seen (not (blank-line-at? pos))))
      (let ((next (+ (line-end-pos p) 1)))
        (cond ((>= next mx) mx)
              ((and seen (blank-line-at? next)) next)
              (else (loop next (or seen (not (blank-line-at? next))))))))))

;; The previous paragraph boundary before `pos`: the start of the first blank
;; line above some non-blank text (Emacs M-{, vim `{`), or 0 at the top.
(define (paragraph-backward-pos pos)
  (let loop ((ls (line-start-pos pos)) (seen (not (blank-line-at? pos))))
    (if (= ls 0)
        0
        (let ((prev (line-start-pos (- ls 1))))
          (if (and seen (blank-line-at? (- ls 1)))
              prev
              (loop prev (or seen (not (blank-line-at? (- ls 1))))))))))

;; Keyed, STABLE merge sort: order `lst` by `(key x)` under `less?`, keeping
;; equal keys in their original order (tabulated columns and helix selections
;; rely on that, so Steel's builtin quicksort `sort` won't do). O(n log n) with
;; an iterative merge: the old insertion sort was O(n²) and recursed once per
;; element, which `sort-lines` on a large buffer felt.
(define (merge-sorted-by a b key less?)
  (let loop ((a a) (b b) (acc '()))
    (cond ((null? a) (append (reverse acc) b))
          ((null? b) (append (reverse acc) a))
          ((less? (key (car b)) (key (car a))) (loop a (cdr b) (cons (car b) acc)))
          (else (loop (cdr a) b (cons (car a) acc))))))
(define (sort-by key less? lst)
  (let ((n (length lst)))
    (if (< n 2)
        lst
        (let ((half (quotient n 2)))
          (merge-sorted-by (sort-by key less? (take lst half))
                           (sort-by key less? (list-tail lst half))
                           key less?)))))
(define (sort-strings lst) (sort-by (lambda (x) x) string<? lst))

;; --- Gregorian calendar arithmetic (pure) ------------------------------------
;; Proleptic-Gregorian day count (days since 1970-01-01), via Howard Hinnant's
;; algorithm. All our dates are positive years, so plain `quotient` suffices.
;; Shared substrate for org scheduling/repeaters and org-roam dailies.
(define (ymd->days y m d)
  (let* ((y2 (if (<= m 2) (- y 1) y))
         (era (quotient y2 400))
         (yoe (- y2 (* era 400)))
         (doy (+ (quotient (+ (* 153 (+ m (if (> m 2) -3 9))) 2) 5) (- d 1)))
         (doe (+ (* yoe 365) (quotient yoe 4) (- (quotient yoe 100)) doy)))
    (- (+ (* era 146097) doe) 719468)))

(define (days->ymd z0)
  (let* ((z (+ z0 719468))
         (era (quotient z 146097))
         (doe (- z (* era 146097)))
         (yoe (quotient (+ (- doe (quotient doe 1460))
                           (quotient doe 36524) (- (quotient doe 146096)))
                        365))
         (y (+ yoe (* era 400)))
         (doy (- doe (+ (* 365 yoe) (quotient yoe 4) (- (quotient yoe 100)))))
         (mp (quotient (+ (* 5 doy) 2) 153))
         (d (+ (- doy (quotient (+ (* 153 mp) 2) 5)) 1))
         (m (+ mp (if (< mp 10) 3 -9))))
    (list (if (<= m 2) (+ y 1) y) m d)))

;; Two-digit zero-padded number ("07"), for date/time formatting.
(define (pad2 n) (if (< n 10) (string-append "0" (number->string n)) (number->string n)))

;; ISO dates. "YYYY-MM-DD" (anywhere in `s`) → (y m d), or #f.
(define (parse-iso-date s)
  (let ((c (regexp-captures "([0-9]{4})-([0-9]{2})-([0-9]{2})" s)))
    (if (null? c) #f
        (list (string->number (nth c 1)) (string->number (nth c 2)) (string->number (nth c 3))))))
;; The day count of the first "YYYY-MM-DD" in `s`, or #f.
(define (iso-date->days s)
  (let ((ymd (parse-iso-date s)))
    (if ymd (ymd->days (nth ymd 0) (nth ymd 1) (nth ymd 2)) #f)))
;; A day count as "YYYY-MM-DD".
(define (days->iso-date days)
  (let ((ymd (days->ymd days)))
    (string-append (number->string (nth ymd 0)) "-" (pad2 (nth ymd 1)) "-" (pad2 (nth ymd 2)))))

;; Minibuffer reads with an OPTIONAL initial value (Emacs `read-string` /
;; `completing-read` style). The Rust `*-impl` primitives take a required
;; initial; these wrappers default it to "" so existing 2-/3-arg calls keep
;; working while new callers may pre-fill the input:
;;   (read-string "New name: " "cb" old-name)
;;   (completing-read "Pick: " '("a" "b") "cb" "a")
;; `callback` may be a 'symbol / "string" naming a one-arg function, or a
;; PROCEDURE — a closure is stashed via `callback->name` (below), so a command
;; can capture its operands instead of smuggling them through globals:
;;   (read-string "Tags: " (lambda (input) (apply-tags H input)))
(define (read-string prompt callback . rest)
  (read-string-impl prompt (callback->name callback) (if (null? rest) "" (car rest))))
;; Masked read for a secret (Emacs `read-passwd`): echoed as •, never added
;; to minibuffer history, passed to `callback` untrimmed. The optional
;; `on-cancel` runs (with "") if the read is aborted or displaced by another,
;; so a caller waiting on the password can unwind. A callback error is
;; reported without its message (Steel quotes the offending value — the
;; password). Closures are one-shot: both are freed once either fires.
;;   (read-passwd "Password: " (lambda (pw) (connect user pw))
;;                (lambda (_) (message "login cancelled")))
(define (read-passwd prompt callback . rest)
  (let ((names (callback-group->names
                (list callback (if (null? rest) #f (car rest))))))
    (read-passwd-impl prompt (car names) (cadr names))))
(define (completing-read prompt candidates callback . rest)
  (completing-read-impl prompt candidates (callback->name callback)
                        (if (null? rest) "" (car rest))))

;; (set-line-image line path [max-width]) — display an image on rows below
;; `line`; the optional max-width (px) caps the displayed width (0 = default).
(define (set-line-image line path . rest)
  (set-line-image-impl line path (if (null? rest) 0 (car rest))))

;; Interactive-command registry: Scheme commands register here so M-x lists them
;; alongside the built-in commands (see help.scm). `name` is the canonical
;; 'symbol form, but a "string" still works (M-x runs it by name either way).
(define *interactive-commands* '())
(define (register-command name)
  (let ((name (if (symbol? name) (symbol->string name) name)))
    (when (not (member name *interactive-commands*))
      (set! *interactive-commands* (cons name *interactive-commands*)))))

;; (define-command (name args …) body …) — define a command AND register it for
;; M-x in one form, so the name isn't repeated as a separate string. Mirrors a
;; plain `define`; M-x invokes the command with no arguments. The two-form
;; variant `(define-command name value)` registers a value binding likewise.
;;
;;   (define-command (layout-demo) (render-layout …))   ; was: (define (layout-demo) …)
;;                                                       ;       (register-command "layout-demo")
(define-syntax define-command
  (syntax-rules ()
    ((_ (name . args) body ...)
     (begin
       (define (name . args) body ...)
       (register-command 'name)))
    ((_ name value)
     (begin
       (define name value)
       (register-command 'name)))))

;; `write-file` — two jobs under Emacs's name. With PATH and TEXT, a script's
;; file write (the `write-file-text` primitive; #t on success). With just PATH,
;; Emacs's `write-file`: save the current buffer as PATH and visit it from now
;; on (asking before overwriting another file). With nothing — M-x, C-x C-w —
;; read the path in the minibuffer, completing file names.
(define (write-file . args)
  (cond ((null? args) (read-file-name "Write file: " 'write-file-impl))
        ((null? (cdr args)) (write-file-impl (car args)))
        (else (write-file-text (car args) (cadr args)))))
(register-command 'write-file)

;; `load-theme` — load a built-in theme. With an argument (string or symbol) it
;; loads directly: (load-theme 'macros-dark). With no argument — e.g. M-x
;; load-theme — it prompts with completion over (theme-names), reusing itself as
;; the callback (the chosen name comes back as a string). Mirrors the
;; read-string / completing-read *-impl wrapper pattern above.
(define (load-theme . rest)
  (if (null? rest)
      (completing-read "Load theme: " (theme-names) 'load-theme)
      (load-theme-impl (car rest))))
(register-command 'load-theme)
;; In-buffer completion of `(load-theme 'mac…)` is wired in completion.scm, which
;; owns the argument-aware CAPF machinery.

;; Split a grep hit `path:line:text` into (path line-string text), or #f. The
;; path is the shortest prefix followed by `:<digits>:`, so a path that itself
;; contains a colon (`notes:2024.md`) still parses.
(define (grep-hit-parts hit)
  (let ((c (regexp-captures "^(.+?):([0-9]+):(.*)$" hit)))
    (if (null? c) #f (list (nth c 1) (nth c 2) (nth c 3)))))

(define (path-join a b)
  (if (ends-with? a "/") (string-append a b) (string-append a "/" b)))

;; Last path component, e.g. "/home/me/proj/" -> "proj".
(define (basename path)
  (let ((parts (filter (lambda (s) (not (equal? s "")))
                       (split-many path "/"))))
    (if (null? parts) path (last parts))))

;; --- project root resolution ------------------------------------------------
;; The Emacs `project.el` concept: walk up to the nearest marker. Lives in core
;; (loaded early) so BOTH magit and projectile can build on it. Steel resolves
;; free identifiers at definition time, so the shared resolver must precede them.

;; Files/dirs whose presence marks a project root (git first, like projectile).
;; VCS markers win over build-tool markers: a workspace member crate (its own
;; Cargo.toml) resolves to the enclosing repo, not itself, so sub-folders don't
;; show up as separate projects in switch-project.
(define *vcs-markers* (list ".git" ".hg"))
(define *project-markers* (list ".git" ".hg" ".projectile" "Cargo.toml" "package.json" "go.mod"))
(define *project-cache* '())     ; dir -> resolved-root
(define *project-known* '())     ; roots we've seen (for switch-project)

(define (project-has-marker? dir markers)
  (cond ((null? markers) #f)
        ((file-exists? (path-join dir (car markers))) #t)
        (else (project-has-marker? dir (cdr markers)))))

;; A `.git` FILE (`gitdir: …`) marks a linked worktree or submodule checkout:
;; a repo boundary of its own, even when nested inside another repo's tree.
(define (git-link-root? dir)
  (let ((git (path-join dir ".git")))
    (and (file-exists? git) (not (file-directory? git)))))

;; Walk up from `dir`, tracking two candidates: the OUTERMOST dir with a VCS
;; marker, and the NEAREST dir with any marker. Prefer the VCS root when one
;; exists (so nested build-tool roots collapse into the repo); otherwise fall
;; back to the nearest build marker. A git worktree/submodule (`.git` file)
;; stops the walk, so it never resolves to the enclosing checkout. #f outside
;; any project.
(define (project-root-walk dir)
  (let loop ((d dir) (vcs-root #f) (nearest #f))
    (if (equal? d "")
        (or vcs-root nearest)
        (let* ((vcs-root* (if (project-has-marker? d *vcs-markers*) d vcs-root))
               (nearest*  (if (and (not nearest) (project-has-marker? d *project-markers*))
                              d nearest))
               (parent (parent-directory d)))
          (if (or (equal? parent d) (git-link-root? d))
              (or vcs-root* nearest*)
              (loop parent vcs-root* nearest*))))))

;; Cached resolution; remembers each root for switch-project.
(define (project-root dir)
  (or (alist-ref *project-cache* dir)
      (let ((root (project-root-walk dir)))
        (when root
          (set! *project-cache* (cons (cons dir root) *project-cache*))
          (when (not (member root *project-known*))
            (set! *project-known* (cons root *project-known*))))
        root)))

;; The directory project-aware commands (compile, term, …) should default to.
;; Prefers the title-bar project: perspectives.scm installs *project-dir-hook*
;; as a thunk returning the current perspective's root (or #f). With no active
;; perspective the hook is #f and we fall back to the current buffer's project
;; root, then its own directory (a fileless / non-project buffer). This keeps
;; project commands pinned to the SELECTED project rather than following whatever
;; buffer happens to be current — the same intent as magit's *magit-scope-hook*.
(define *project-dir-hook* #f)   ; #f or a thunk -> project-root-or-#f
;; Installer for the hook. Installers MUST go through this setter: a raw
;; top-level `set!` from a later-loaded file mutates a binding `project-directory`
;; never reads (Steel resolves free identifiers at definition time); a set!
;; executed inside this core-defined function mutates the one it does.
(define (set-project-dir-hook! f) (set! *project-dir-hook* f))
(define (project-directory)
  (or (and *project-dir-hook* (*project-dir-hook*))
      (project-root (buffer-directory))
      (buffer-directory)))

;; --- updates ----------------------------------------------------------------

;; Enable or disable the launch-time auto-update check against macros.dev. On by
;; default. (set-auto-update #f) in init.scm turns it off: no network check at
;; launch, no background download, and no "restart to update" notice. Re-enable
;; with (set-auto-update #t). Backed by the "auto-update" option; the manual
;; M-x check-for-updates command still works regardless of this setting.
(define (set-auto-update enable)
  (set-option "auto-update" enable))

;; --- procedure dispatch -----------------------------------------------------
;; Lets the keymap API bind a *procedure* (an identity) as well as a
;; "command-name" string: a bound procedure is stashed here under a generated id,
;; and the keymap binds a `(run-thunk "id")` stub that dispatch evaluates.

(define *thunk-table* (hash))
(define *thunk-counter* 0)

;; Stash `proc` and return an id whose `(run-thunk "id")` stub calls it.
(define (register-thunk proc)
  (set! *thunk-counter* (+ *thunk-counter* 1))
  (let ((id (string-append "thunk-" (number->string *thunk-counter*))))
    (set! *thunk-table* (hash-insert *thunk-table* id proc))
    id))

;; Call the procedure registered under `id` (a no-op if there is none).
(define (run-thunk id)
  (let ((p (hash-try-get *thunk-table* id)))
    (when p (p))))

;; One-argument variant, for stashed callbacks that receive a value (e.g. the
;; minibuffer result).
(define (run-thunk-1 id arg)
  (let ((p (hash-try-get *thunk-table* id)))
    (when p (p arg))))

;; Normalize a one-arg callback (for read-string / completing-read) to the name
;; the minibuffer will call with the result. A procedure is stashed and exposed
;; through a uniquely-named one-arg stub — the same trick as `hook->name` below,
;; which handles the no-arg hook shape.
(define (callback->name proc)
  (cond
    ((string? proc) proc)
    ((symbol? proc) (symbol->string proc))
    (else
     (let* ((id (register-thunk proc))
            (name (string-append "callback-fn-" id)))
       (eval-string (string-append "(define (" name " x) (run-thunk-1 \"" id "\" x))"))
       name))))

;; One-shot callbacks for a read with alternative outcomes (submit vs cancel):
;; → one name per entry of `cbs` ("" for #f). Procedures are stashed like
;; `callback->name`, but the first of the group to fire frees EVERY closure in
;; it, so neither the fired one nor its never-to-fire sibling stays reachable
;; (with whatever it captured) for the life of the session.
(define (callback-group->names cbs)
  (let* ((ids (map (lambda (cb) (if (procedure? cb) (register-thunk cb) #f)) cbs))
         (group (string-append
                 "(list"
                 (apply string-append
                        (map (lambda (id) (if id (string-append " \"" id "\"") "")) ids))
                 ")")))
    (map (lambda (cb id)
           (cond
             ((not cb) "")
             (id (let ((name (string-append "callback-fn-" id)))
                   (eval-string (string-append "(define (" name " x) (run-thunk-group-1 \""
                                               id "\" " group " x))"))
                   name))
             (else (callback->name cb))))
         cbs ids)))

;; Call the thunk `id` with `arg` after unregistering every id in `group`.
(define (run-thunk-group-1 id group arg)
  (let ((p (hash-try-get *thunk-table* id)))
    (for-each (lambda (g) (set! *thunk-table* (hash-remove *thunk-table* g))) group)
    (when p (p arg))))

;; Run `thunk` with extra environment for every process it spawns — call-process*,
;; make-process*, make-terminal, dap-start (Emacs's `process-environment`
;; let-binding). `env` is a list of "KEY=VALUE" (bare "KEY" = unset); nested
;; extents layer, inner wins. Async spawns capture the env when queued.
;;   (with-process-env (list (string-append "PGPASSWORD=" pw))
;;     (lambda () (call-process-full "psql" args "")))
(define (with-process-env env thunk)
  (process-env-push! env)
  (let ((result (thunk)))
    (process-env-pop!)
    result))

;; --- debounce ----------------------------------------------------------------
;; (make-debounce name delay-ms thunk) → a schedule procedure. Each call bumps a
;; pending counter and starts a `delay-ms` timer; only the LAST timer to fire
;; (counter back to 0) runs `thunk` — so a burst of edits repaints once. The
;; timer calls by name, so the fire step is exposed as a named stub (the
;; register-thunk pattern). The schedule procedure accepts an optional delay
;; override: `(schedule)` or `(schedule ms)`.
(define *debounce-pending* (hash))
(define (debounce--pending key)
  (let ((v (hash-try-get *debounce-pending* key))) (if v v 0)))
(define (debounce--set-pending key v)
  (set! *debounce-pending* (hash-insert *debounce-pending* key v)))
(define (make-debounce name delay-ms thunk)
  (let* ((fire (lambda ()
                 (when (> (debounce--pending name) 0)
                   (debounce--set-pending name (- (debounce--pending name) 1)))
                 (when (= (debounce--pending name) 0) (thunk))))
         (id (register-thunk fire))
         (fire-name (string-append "debounce-fire-" id)))
    (eval-string (string-append "(define (" fire-name ") (run-thunk \"" id "\"))"))
    (lambda rest
      (debounce--set-pending name (+ (debounce--pending name) 1))
      (run-with-timer (if (null? rest) delay-ms (car rest)) fire-name))))

;; --- per-buffer state over a plain alist ---------------------------------------
;; The `buffer-local-*` primitives store STRINGS only, so per-buffer Scheme
;; values (fold states, flags, lists) live in module alists keyed by buffer
;; name. These two helpers give every such alist the same read/replace shape:
;;   (define (mode--state-get)   (buffer-state-get *mode-state* '()))
;;   (define (mode--state-set v) (set! *mode-state* (buffer-state-set *mode-state* v)))
;; (Entries for killed buffers linger until overwritten — same as before; a
;; buffer-local migration needs non-string value support in the primitive.)
(define (buffer-state-get alist default)
  (let ((e (alist-ref alist (current-buffer)))) (if e e default)))
(define (buffer-state-set alist v) (alist-set alist (current-buffer) v))

;; --- list-buffer keymap boilerplate --------------------------------------------
;; The conventional read-only list-buffer keys: RET visits, n/p move, q kills,
;; and no line-number gutter (these are list views, not code). Mode-specific
;; keys (sort, refresh, marks, a different q) layer on with `define-key` after.
(define (define-list-mode-keys mode visit-cmd)
  (when visit-cmd (define-key mode "enter" visit-cmd))
  (define-key mode "n" 'next-line)
  (define-key mode "p" 'previous-line)
  (define-key mode "q" 'kill-buffer)
  (set-option (string-append "line-numbers:" mode) "false"))

;; Bind `cmd` under `chord` in keymap `map`. Like `define-key`, but `cmd` may be
;; a 'name symbol (the canonical config form) or a procedure (an identity), as
;; well as a "command-name" string. A symbol defers resolution to keypress time,
;; so it works regardless of file load order. Passing the function directly means
;; a typo is an unbound-variable error at load time, and editor tooling
;; (go-to-definition, rename) follows the binding.
;;
;;   (bind-key "fundamental" "ctrl-c g" 'magit-status)  ; canonical: a 'name symbol
;;   (bind-key "fundamental" "ctrl-c g" magit-status)   ; a Scheme command identity
;;   (bind-key "fundamental" "ctrl-c d" (lambda () (insert-string "hi")))
;;   (bind-key "fundamental" "ctrl-x g" "magit-status") ; string still works
(define (bind-key map chord cmd)
  (cond
    ((symbol? cmd) (define-key map chord (symbol->string cmd)))
    ((string? cmd) (define-key map chord cmd))
    (else (define-key map chord (string-append "run-thunk \"" (register-thunk cmd) "\"")))))
;; Bind a table of keys in keymap `map`: each entry is (chord cmd), with `cmd`
;; anything `bind-key` takes. A quoted table keeps the 'name symbol form:
;;
;;   (define-keys "dired" '(("enter" dired-find-file)
;;                          ("g"     dired-refresh)
;;                          ("@"     "my-command %c")))
(define (define-keys map bindings)
  (for-each (lambda (b) (bind-key map (car b) (cadr b))) bindings))

;; `define-keys` into each keymap of `maps` — one table shared by several maps:
;;   (define-keys-in '("evil-visual" "evil-visual-line") '(("u" evil-visual-downcase)))
(define (define-keys-in maps bindings)
  (for-each (lambda (m) (define-keys m bindings)) maps))

;; --- hook argument plumbing -------------------------------------------------
;; The editor stores hooks BY NAME and calls them by name on the engine thread —
;; a live closure can't cross that boundary — so the raw builtins take a string.
;; Capture them here so the friendlier `add-hook` / `remove-hook` wrappers (in
;; hooks.scm, which must live in a *separate* file: redefining `add-hook` in this
;; file would make the line above a forward-reference to the not-yet-initialized
;; local binding) can accept a 'symbol or a procedure too, like `bind-key`.
(define raw-add-hook add-hook)
(define raw-remove-hook remove-hook)

;; Normalize a hook argument to the string name the editor will call. (`proc`,
;; not `fn` — `fn` is Steel's lambda shorthand and can't be a parameter name.)
(define (hook->name proc)
  (cond
    ((string? proc) proc)
    ((symbol? proc) (symbol->string proc))
    ;; Procedure: stash it, then emit a uniquely-named no-arg global stub that
    ;; runs it. The stub's source only references the thunk by id (a string), so
    ;; eval-string is enough — no live value need cross into the definition.
    (else
     (let* ((id (register-thunk proc))
            (name (string-append "hook-fn-" id)))
       (eval-string (string-append "(define (" name ") (run-thunk \"" id "\"))"))
       name))))

;; --- active keybinding style -------------------------------------------------
;; The Normal-state keymap actually in effect: "helix-normal" under helix-mode,
;; "evil-normal" under evil-mode, and #f under plain Emacs keys. Helix must be
;; tested FIRST — `evil-enabled?` is also true under Helix, because the two
;; styles share one Normal/Insert state machine and differ only in which keymaps
;; the router consults.
;;
;; Lives here (loaded before everything but completion.scm) so the help and
;; discovery commands can branch on the style without depending on evil.scm or
;; helix.scm having loaded.
(define (current-modal-keymap)
  (cond ((helix-enabled?) "helix-normal")
        ((evil-enabled?)  "evil-normal")
        (else #f)))

;; Every keymap a plain key could resolve against right now, most specific
;; first. The modal Normal map shadows `fundamental`, and the Emacs chords stay
;; reachable underneath it — so this is the list the help commands must search
;; rather than assuming "fundamental".
(define (active-keymaps)
  (let ((m (current-modal-keymap)))
    (if m (list m "fundamental") (list "fundamental"))))

;; --- deferred jump into a buffer that is still loading --------------------
;; Opening a file that isn't visited yet is ASYNC, so a goto queued in the same
;; batch would move point in the OLD buffer. Instead record the target and poll
;; on a timer until it is the current buffer, then jump against its fresh
;; snapshot. Pending: (bufname path line0 byte-col tries); a newer jump replaces
;; an older one, and it gives up after ~2s.
(define *visit-pending* #f)

;; How the last pending visit ended, for callers that must tell a landing apart
;; from a timeout (both clear *visit-pending*): 'pending while one is armed,
;; 'landed once point moved, 'timed-out when the ~2s gave up.
(define *visit-result* 'idle)

;; --- small shared helpers -----------------------------------------------------
;; Drop one trailing "/" (list-directory marks dirs with one; the editor reports
;; changed directories without one — normalize before comparing or keying).
(define (strip-trailing-slash p)
  (if (ends-with? p "/") (substring p 0 (- (string-length p) 1)) p))

;; `s` cut to `n` chars with a trailing "…" when it's longer.
(define (truncate-string s n)
  (if (> (string-length s) n) (string-append (substring s 0 n) "…") s))

;; `s` as a literal `replace-regexp-in-string` replacement (`$` would otherwise
;; reference a capture group).
(define (replacement-literal s) (replace-regexp-in-string "\\$" "$$$$" s))

;; Whether a buffer named `name` exists.
(define (buffer-live? name) (if (member name (buffer-names)) #t #f))

;; `lst` without the elements `equal?` to `x`.
(define (remove-equal x lst) (filter (lambda (y) (not (equal? y x))) lst))

;; Expand a path the way the file prompt does (Emacs `file-name-shadow`):
;;   "~" or "~/…"  -> your home directory   (the quick "go home")
;;   "//"          -> the filesystem root "/"
;; e.g. with the input sitting at /a/b/c/, typing "~/" jumps home and "//" jumps
;; to root.
(define (path-contains? s sub) (> (length (split-many s sub)) 1))
;; The path segments after the LAST segment that is exactly "~" (a "~" at the
;; start or right after a "/"), or #f when there is none — so "foo~bar" and
;; "backup.txt~" are left alone.
(define (tilde-tail segs)
  (let loop ((ss segs) (found #f))
    (cond ((null? ss) found)
          ((equal? (car ss) "~") (loop (cdr ss) (cdr ss)))
          (else (loop (cdr ss) found)))))
(define (expand-file-name path)
  (let ((home (getenv "HOME"))
        (tail (tilde-tail (split-many path "/"))))
    (cond
      (tail
       (string-append home "/" (string-join tail "/")))
      ((path-contains? path "//")
       (string-append "/" (last (split-many path "//"))))
      (else path))))

;; Quote `s` as ONE POSIX shell word: wrap in single quotes, each embedded ' as
;; '\''. Use for every path / user-supplied value spliced into a `sh -c` string.
(define (shell-quote s)
  (string-append "'" (string-replace s "'" "'\\''") "'"))

;; Whether two paths name the same file (spelled differently: relative vs
;; absolute, through a symlink, /var vs /private/var, ...).
(define (same-file? a b)
  (or (equal? a b)
      (and (string? a) (not (equal? a ""))
           (equal? (file-truename a) (file-truename b)))))

;; The name of an open buffer visiting `path` (compared with `same-file?`, so
;; a relative, symlinked or /private-prefixed spelling still matches), or #f.
(define (find-buffer-visiting path)
  (and (not (equal? path ""))
       (let loop ((names (buffer-names)))
         (cond ((null? names) #f)
               ((let ((f (buffer-file-name-of (car names))))
                  (and (string? f) (not (equal? f "")) (same-file? f path)))
                (car names))
               (else (loop (cdr names)))))))

;; The current buffer's visited path, or its name for a fileless buffer. Use
;; this for extension checks: a uniquified buffer is named `notes.org<dir>`,
;; which no longer ends in ".org".
(define (buffer-path-or-name)
  (let ((f (buffer-file-name)))
    (if (or (not (string? f)) (equal? f "")) (current-buffer) f)))

;; The current buffer's text as a list of lines (split on "\n").
(define (buffer-lines) (split-many (buffer-string) "\n"))

;; Put point on 0-based `line0`, byte column `col`, once buffer `bufname` (or
;; the buffer visiting `path`; either may be "") is current.
(define (goto-when-visiting bufname path line0 col)
  (set! *visit-pending* (list bufname path line0 col 0))
  (set! *visit-result* 'pending)
  (run-with-timer 0 'visit-location-step))

(define (visit-location-step)
  (let ((v *visit-pending*))
    (when v
      (let ((here? (or (and (not (equal? (list-ref v 1) ""))
                            (same-file? (buffer-file-name) (list-ref v 1)))
                       (equal? (current-buffer) (list-ref v 0)))))
        (cond
          (here?
           (set! *visit-pending* #f)
           (set! *visit-result* 'landed)
           (let ((ls (position->byte (list-ref v 2) 0)))
             (goto-char (min (+ ls (list-ref v 3)) (line-end-pos ls)))
             (recenter)))
          ((< (list-ref v 4) 100)
           (set! *visit-pending* (list (list-ref v 0) (list-ref v 1) (list-ref v 2)
                                       (list-ref v 3) (+ (list-ref v 4) 1)))
           (run-with-timer 20 'visit-location-step))
          (else (set! *visit-pending* #f)
                (set! *visit-result* 'timed-out)))))))

;; Open `path` in the selected window and land on 0-based `line0` once loaded.
(define (find-file-goto-line path line0)
  (open-file path)
  (goto-when-visiting "" path line0 0))
