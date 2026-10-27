;; requires: core folding bookmarks narrow

;; org.scm — Org mode, implemented entirely in Scheme on the editor primitives.
;;
;; Five layers, each independently useful:
;;   1. fontification + heading visibility cycling (TAB / S-TAB)
;;   2. structure editing — TODO/tags/priority cycling, promote/demote, move
;;      subtree, insert heading, toggle checkbox
;;   3. an Org AST parser (`org-parse`) that both export and agenda consume
;;   4. agenda + capture across a set of .org files
;;   5. tables (align) + babel (execute source blocks)
;;
;; A `.org` file activates Org mode via `find-file-hook` (it calls
;; `set-major-mode`, so the `org-mode` keymap below takes effect). Fontification
;; paints the shared overlay-face layer (like rainbow-delimiters); rainbow skips
;; .org buffers so the two don't fight over that layer.

;; ===========================================================================
;; Faces
;; ===========================================================================
;; Heading levels cycle through this palette (1-based, wraps at 8).
(define *org-level-colors*
  (list "#8ec07c" "#83a598" "#d3869b" "#fabd2f" "#fe8019" "#b8bb26" "#fb4934" "#458588"))

(define (org--register-level-faces)
  (let loop ((cs *org-level-colors*) (i 1))
    (when (not (null? cs))
      (set-face (string-append "org-level-" (number->string i)) (car cs))
      (loop (cdr cs) (+ i 1)))))
(org--register-level-faces)

(set-face "org-todo" "#fb4934")
(set-face "org-done" "#8ec07c")
(set-face "org-tag" "#83a598")
(set-face "org-priority" "#d3869b")
(set-face "org-meta" "#928374")
(set-face "org-drawer" "#665c54")
(set-face "org-block" "#bdae93")
(set-face "org-table" "#83a598")
(set-face "org-link" "#458588")
(set-face "org-date" "#b16286")
(set-face "org-bold" "#ebdbb2")
(set-face "org-italic" "#d5c4a1")
(set-face "org-verbatim" "#b8bb26")
(set-face "org-code" "#b8bb26")
(set-face "org-checkbox" "#fabd2f")
(set-face "org-checkbox-done" "#8ec07c")
(set-face "org-strike" "#928374")
(set-face "org-underline" "#83a598")
(set-face "org-footnote" "#d3869b")

;; The TODO keyword vocabulary, and the order C-c C-t rotates through it. Like
;; Emacs `org-todo-keywords`, every keyword here is part of the cycle, so active
;; states come first and "done" states last (DONE/CANCELLED — see below). The
;; cycle is nothing → each of these → each custom keyword → nothing. Trim or
;; reorder this list from init.scm to shape the cycle.
(define *org-todo-keywords*
  (list "TODO" "NEXT" "WAITING" "DOING" "HOLD" "DONE" "CANCELLED"))
;; Keywords that count as closed (green face, treated as done in the agenda).
(define *org-done-keywords* (list "DONE" "CANCELLED"))

;; Custom heading keywords: a user-defined array of labels, appended to the
;; C-c C-t cycle after the TODO vocabulary above. Empty by default — the
;; built-in ships no labels; define your own from init.scm. Each is recognized
;; on a headline like a TODO word but carries NO color of its own (it renders in
;; the heading title color) unless one is set in *org-custom-keyword-colors*.
;; A typical init.scm:
;;   (set! *org-custom-keywords* (list "BUG" "FEATURE" "IDEA"))
;;   (set! *org-custom-keyword-colors*               ; optional per-label colors
;;     (list (cons "BUG" "#fb4934") (cons "FEATURE" "#83a598")))
;;   (org--register-custom-keyword-faces)            ; apply the colors
;; They join the C-c C-t rotation automatically; bind a key only if you also
;; want to jump straight to one, e.g.
;;   (define (org-toggle-bug) (org-set-keyword "BUG"))
;;   (define-key "org-mode" "ctrl-c ctrl-b" "org-toggle-bug")
;;   (register-command "org-toggle-bug")
(define *org-custom-keywords* '())

;; Per-keyword colors: an alist (LABEL . "#rrggbb"). Empty by default, so every
;; custom keyword inherits the title color. Set from init.scm, then call
;; `org--register-custom-keyword-faces` to apply.
(define *org-custom-keyword-colors* '())

;; The face name carrying a custom keyword's color (e.g. "org-kw-BUG").
(define (org--keyword-face-name kw) (string-append "org-kw-" kw))

(define (org--custom-keyword? kw) (and (member kw *org-custom-keywords*) #t))

;; The user-defined color for `kw`, or #f if none is set (⇒ inherit the title).
(define (org--keyword-color kw)
  (let loop ((es *org-custom-keyword-colors*))
    (cond ((null? es) #f)
          ((equal? (car (car es)) kw) (cdr (car es)))
          (else (loop (cdr es))))))

;; Recognized on a headline: a TODO word or any custom keyword.
(define (org--known-keyword? kw)
  (or (member kw *org-todo-keywords*) (org--custom-keyword? kw)))

;; Register (or refresh) one face per *colored* custom keyword. Call again after
;; re-setting *org-custom-keyword-colors* to apply new colors.
(define (org--register-custom-keyword-faces)
  (for-each (lambda (e) (set-face (org--keyword-face-name (car e)) (cdr e)))
            *org-custom-keyword-colors*))
(org--register-custom-keyword-faces)

;; Collapse the leading stars on every headline from display (the text stays in
;; the buffer), so a heading shows only its colored title — all stars hidden, not
;; just the all-but-one of Emacs's `org-hide-leading-stars`. Disable from init.scm
;; with
;;   (set! *org-hide-leading-stars* #f)  ; then M-x org-mode, or reopen the file
(define *org-hide-leading-stars* #t)

;; Virtual indentation (Emacs `org-indent-mode`): indent each heading by its depth
;; and the body text beneath it one level further, all display-only — no spaces
;; are written to the buffer, so there is nothing for the cursor to step through.
;; A heading of level L is indented (L-1)*per-level columns and its body L*per.
(define *org-indent* #t)
(define *org-indent-per-level* 4)

;; ===========================================================================
;; Tiny utilities
;; ===========================================================================
;; The generic byte/string helpers (char-bytes, byte-drop, byte-take,
;; string-trim, string-concat, string-repeat) live in core.scm.

;; Pad `s` with spaces to `w` display columns. Width is CHARS, not bytes
;; (string-length, not utf8-length): a multibyte cell like "café" occupies 4
;; columns on screen but 5 bytes, and byte-width padding left every column
;; containing non-ASCII text visibly short of its pipe.
(define (org--pad s w) (string-append s (string-repeat " " (- w (string-length s)))))

;; Inclusive sublist `lines[a..b]`, clamped; '() when the range is empty.
(define (org--slice lines a b)
  (let ((n (length lines)))
    (if (or (> a b) (>= a n) (< b 0))
        '()
        (let loop ((ls lines) (i 0) (acc '()))
          (cond ((null? ls) (reverse acc))
                ((> i b) (reverse acc))
                ((>= i a) (loop (cdr ls) (+ i 1) (cons (car ls) acc)))
                (else (loop (cdr ls) (+ i 1) acc)))))))

;; ===========================================================================
;; Line predicates (pure; operate on a single line of text)
;; ===========================================================================
;; Heading depth (number of leading stars) if `line` is a headline, else #f.
(define (org--heading-level line)
  (let ((caps (regexp-captures "^(\\*+)([ \t]|$)" line)))
    (if (null? caps) #f (string-length (nth caps 1)))))

(define (org--block-begin? line) (>= (string-match "^[ \t]*#\\+[bB][eE][gG][iI][nN]_" line) 0))
(define (org--block-end? line)   (>= (string-match "^[ \t]*#\\+[eE][nN][dD]_" line) 0))
(define (org--keyword-line? line) (>= (string-match "^[ \t]*#\\+[A-Za-z_]+:" line) 0))
(define (org--comment-line? line) (>= (string-match "^[ \t]*#[ \t]" line) 0))
(define (org--drawer-line? line)  (>= (string-match "^[ \t]*:[A-Za-z]+:[ \t]*$" line) 0))
(define (org--table-line? line)   (>= (string-match "^[ \t]*\\|" line) 0))
(define (org--ulist-line? line)   (>= (string-match "^[ \t]*[-+][ \t]+" line) 0))
(define (org--olist-line? line)   (>= (string-match "^[ \t]*[0-9]+[.)][ \t]+" line) 0))
(define (org--hr-line? line)      (>= (string-match "^[ \t]*-----+[ \t]*$" line) 0))

;; A separator row in a table (|---+---|): a table line of only |-+:space.
(define (org--table-sep? line)
  (and (org--table-line? line)
       (>= (string-match "^[ \t]*\\|[-+:| \t]*$" line) 0)
       (>= (string-match "-" line) 0)))

;; The language token of a `#+begin_src LANG …` line, or "".
(define (org--block-lang line)
  (let ((c (regexp-captures "[bB][eE][gG][iI][nN]_[sS][rR][cC][ \t]+([A-Za-z0-9_+-]+)" line)))
    (if (null? c) "" (nth c 1))))

;; ===========================================================================
;; Buffer line / offset helpers (operate on the active buffer)
;; ===========================================================================
(define (org--lines) (buffer-lines))

;; Lines-list variants: loops should split the buffer ONCE via (org--lines) and
;; thread the list through these, not call the one-shot helpers per iteration
;; (each of those re-splits the whole buffer — O(lines^2) in a loop).
(define (org--line-text-in lines i)
  (if (< i (length lines)) (nth lines i) ""))

;; One-shot helpers for single lookups.
(define (org--line-text i) (org--line-text-in (org--lines) i))

;; Byte offset of the start of line `i` (core's lines-byte-offset over a fresh
;; split — single lookups only).
(define (org--line-start i) (lines-byte-offset (org--lines) i))

;; Replace the whole buffer — `buffer-size`, not `point-max`, so a narrowed
;; buffer is rewritten whole too (the editor keeps the narrowing around the
;; part that really changed).
(define (org--replace-all text) (replace-region 0 (buffer-size) text))

;; Byte length of the longest common prefix of strings `a` and `b` (compared by
;; character, so the count never splits a multibyte character).
(define (org--common-prefix-bytes a b)
  (let loop ((x (string->list a)) (y (string->list b)) (n 0))
    (if (and (pair? x) (pair? y) (equal? (car x) (car y)))
        (loop (cdr x) (cdr y) (+ n (char-bytes (car x))))
        n)))
(define (org--common-suffix-bytes a b)
  (org--common-prefix-bytes (list->string (reverse (string->list a)))
                            (list->string (reverse (string->list b)))))

;; Where point `p` belongs after bytes [start, end) holding `old` become `new`.
;; Before the edit (or at a pure insertion's spot) it holds still; after it, it
;; shifts by the size change. Inside a one-line edit it stays on the same
;; character — the common prefix holds still, the common suffix moves with the
;; end (C-c C-t on `** Child` keeps point on its letter). Inside a multi-line
;; rewrite (table align) it keeps its line and column.
(define (org--point-after-edit p start end old new)
  (let ((oldlen (- end start)) (newlen (utf8-length new)))
    (cond
      ((or (< p start) (and (= start end) (= p start))) p)
      ((>= p end) (+ p (- newlen oldlen)))
      ((< (string-match "\n" old) 0)
       (let* ((pre (org--common-prefix-bytes old new))
              (suf (min (org--common-suffix-bytes old new)
                        (- (min oldlen newlen) pre)))
              (rel (- p start)))
         (cond ((<= rel pre) p)
               ((>= rel (- oldlen suf)) (+ start (- newlen (- oldlen rel))))
               (else (+ start (- newlen suf))))))
      (else
       (let* ((before (split-many (byte-take old (- p start)) "\n"))
              (row (- (length before) 1))
              (col (utf8-length (last before)))
              (nls (split-many new "\n"))
              (r (min row (- (length nls) 1))))
         (+ start (lines-byte-offset nls r) (min col (utf8-length (nth nls r)))))))))

;; Replace bytes [start, end) (currently `old`) with `new`, keeping point where
;; it was relative to the text. A bare replace-region leaves the cursor at the
;; END of the replacement, so every line edit (C-c C-t, tags, a checkbox, a
;; planning line) would otherwise yank the cursor to the end of the line.
(define (org--edit-keeping-point start end old new)
  (let ((p (point)))
    (replace-region start end new)
    (goto-char (org--point-after-edit p start end old new))))

(define (org--replace-line i newtext)
  (let* ((line (org--line-text i))
         (start (org--line-start i)))
    (org--edit-keeping-point start (+ start (utf8-length line)) line newtext)))

;; Replace the byte range covering lines [start..end] (inclusive) with the
;; joined `new-lines`.
(define (org--replace-line-range start end new-lines)
  (let* ((ls (org--lines))
         (sb (org--line-start start))
         (eb (+ (org--line-start end) (utf8-length (nth ls end)))))
    (org--edit-keeping-point sb eb (join-lines (org--slice ls start end)) (join-lines new-lines))))

;; ===========================================================================
;; Heading-component parsing
;; ===========================================================================
;; The headline tag/priority syntax, shared by every parser and by the
;; fontifier (which needs the regex itself for byte offsets).
(define org--tag-chars "[A-Za-z0-9_@#%:]+")
(define org--tag-re (string-append ":(" org--tag-chars "):[ \t]*$"))          ; capture w/o colons
(define org--tags-capture-re (string-append "(:" org--tag-chars ":)[ \t]*$")) ; capture with colons
(define org--tag-strip-re (string-append "[ \t]*:" org--tag-chars ":[ \t]*$"))
(define org--priority-re "\\[#[A-Z]\\]")
(define org--priority-capture-re "\\[#([A-Z])\\]")

;; The first whitespace-delimited token of `s` if it is a known TODO keyword.
;; A keyword may carry digits, `_` and `-` after its first letter, so custom
;; labels like IN-PROGRESS or P1 are recognized too.
(define (org--leading-keyword s)
  (let ((caps (regexp-captures "^([A-Z][A-Z0-9_-]*)([ \t]|$)" s)))
    (if (and (not (null? caps)) (org--known-keyword? (nth caps 1)))
        (nth caps 1)
        #f)))

(define (org--done-keyword? kw) (and kw (member kw *org-done-keywords*) #t))

;; The headline title with keyword, priority cookie and trailing tags removed.
(define (org--heading-title rest kw)
  (let* ((s (if kw (replace-regexp-in-string (string-append "^" (regexp-quote kw) "(?:[ \t]+|$)") "" rest) rest))
         (s (replace-regexp-in-string "^\\[#[A-Z]\\][ \t]*" "" s))
         (s (replace-regexp-in-string org--tag-strip-re "" s)))
    (string-trim s)))

;; (stars-string rest) where rest is everything after `* `.
(define (org--heading-split line)
  (let ((caps (regexp-captures "^(\\*+)[ \t]*(.*)$" line)))
    (if (null? caps) (list "*" "") (list (nth caps 1) (nth caps 2)))))

;; Decompose a headline into its five components, the ONE parser every consumer
;; shares: (stars kw prio title tags) — stars = the "*"s string; kw = keyword or
;; #f; prio = the bare priority letter or ""; title = the cleaned title; tags =
;; the inner tag string ("a:b") or "".
(define (org--parse-headline line)
  (let* ((sr (org--heading-split line))
         (stars (nth sr 0)) (rest (nth sr 1))
         (kw (org--leading-keyword rest))
         ;; Org only recognizes the cookie right after the keyword (or stars).
         (prio (let ((c (regexp-captures (if kw
                                             (string-append "^" (regexp-quote kw) "[ \t]+\\[#([A-Z])\\]")
                                             "^\\[#([A-Z])\\]")
                                         rest)))
                 (if (null? c) "" (nth c 1))))
         (tags (let ((c (regexp-captures org--tag-re rest)))
                 (if (null? c) "" (nth c 1)))))
    (list stars kw prio (org--heading-title rest kw) tags)))

;; Reassemble a headline from org--parse-headline components (#f / "" parts are
;; omitted). The inverse of org--parse-headline for structure edits.
(define (org--render-headline stars kw prio title tags)
  (string-append stars " "
    (if (or (not kw) (equal? kw "")) "" (string-append kw " "))
    (if (equal? prio "") "" (string-append "[#" prio "] "))
    title
    (if (equal? tags "") "" (string-append " :" tags ":"))))

;; ===========================================================================
;; Layer 1a — fontification
;; ===========================================================================
;; The visited file's path, or the buffer name for a fileless buffer. Name-based
;; checks must use this: a uniquified buffer is named `notes.org<dir>`, which
;; no longer ends in ".org".
(define (org--file-or-name) (buffer-path-or-name))

;; Is the active buffer an Org buffer? (.org file, or explicitly activated.)
(define (org--active?)
  (or (ends-with? (org--file-or-name) ".org")
      (equal? (buffer-local-get "org-active") "t")))

(define (org--level-face n)
  (string-append "org-level-"
                 (number->string (+ 1 (modulo (- n 1) (length *org-level-colors*))))))

;; Fontify span batching: face spans accumulate here and flush as ONE
;; overlay-set-faces call per repaint — a large .org repaint carries thousands
;; of spans, and one FFI call per span dominated it. Emission order is
;; preserved (later spans win where they overlap, e.g. the keyword face on top
;; of the heading's whole-line level face). overlay-hide stays per-call: hides
;; are few (one per heading) and a different primitive.
(define *org--spans* '())
(define (org--span start end face)
  (set! *org--spans* (cons (list start end face) *org--spans*)))
(define (org--flush-spans)
  (overlay-set-faces (reverse *org--spans*))
  (set! *org--spans* '()))

;; Overlay `face` on every match of `pattern` within one line. `lstart` is the
;; line's start byte; `string-match` gives the byte offset of a match inside the
;; line and `regexp-captures` its text (→ byte length), so offsets stay exact
;; across multi-byte content.
(define (org--paint lstart line pattern face)
  (let loop ((base 0) (s line))
    (let ((m (string-match pattern s)))
      (when (>= m 0)
        (let* ((caps (regexp-captures pattern s))
               (whole (if (null? caps) "" (car caps)))
               (wlen (utf8-length whole)))
          (when (> wlen 0)
            (org--span (+ lstart base m) (+ lstart base m wlen) face)
            (let ((consumed (+ m wlen)))
              (loop (+ base consumed) (byte-drop s consumed)))))))))

(define (org--fontify-heading lstart line level)
  (let ((lend (+ lstart (utf8-length line))))
    (org--span lstart lend (org--level-face level))
    ;; Collapse all the leading stars AND the single separator space after them, so
    ;; the title sits exactly at the heading's virtual indent — a level-N heading
    ;; then aligns with the body under a level-(N-1) heading, like org-indent.
    ;; Stars + the space are ASCII (1 byte each); overlay-hide removes them from
    ;; display without touching the buffer. A bare heading (`**` with no title, so
    ;; no separator) hides only the stars, never the newline.
    (when *org-hide-leading-stars*
      (let ((hide (if (> (utf8-length line) level) (+ level 1) level)))
        (overlay-hide lstart (+ lstart hide))))
    ;; Keyword (stars + one space are ASCII, so byte = char here). A custom
    ;; keyword paints in its own color only if the user defined one — otherwise
    ;; it inherits the heading title color (no overlay). DONE-words are green and
    ;; the remaining TODO words share the org-todo red.
    (let* ((rest (substring line (min (+ level 1) (string-length line)) (string-length line)))
           (kw (org--leading-keyword rest)))
      (when kw
        (let ((ks (+ lstart level 1)))
          (cond
            ((org--custom-keyword? kw)
             (when (org--keyword-color kw)
               (org--span ks (+ ks (string-length kw)) (org--keyword-face-name kw))))
            ((org--done-keyword? kw)
             (org--span ks (+ ks (string-length kw)) "org-done"))
            (else
             (org--span ks (+ ks (string-length kw)) "org-todo"))))))
    ;; priority cookie [#A]
    (let ((b (string-match org--priority-re line)))
      (when (>= b 0) (org--span (+ lstart b) (+ lstart b 4) "org-priority")))
    ;; trailing tags
    (let ((c (regexp-captures org--tags-capture-re line)))
      (when (not (null? c))
        (let ((tags (nth c 1)))
          ;; group 0 includes trailing blanks after the tags; measure from it
          (let ((ts (- lend (utf8-length (car c)))))
            (org--span ts (+ ts (utf8-length tags)) "org-tag")))))))

(define (org--fontify-plain lstart line)
  ;; checkbox state
  (let ((b (string-match "\\[[ xX-]\\]" line)))
    (when (and (>= b 0) (>= (string-match "^[ \t]*[-+*][ \t]+\\[[ xX-]\\]" line) 0))
      (let ((mark (substring line (+ b 1) (+ b 2))))
        (org--span (+ lstart b) (+ lstart b 3)
                          (if (equal? (string-upcase mark) "X") "org-checkbox-done" "org-checkbox")))))
  ;; inline emphasis (delimiters cannot span their own delimiter or a newline)
  (org--paint lstart line "\\*[^ \t\n*][^*\n]*\\*" "org-bold")
  (org--paint lstart line "/[^ \t\n/][^/\n]*/" "org-italic")
  (org--paint lstart line "=[^ \t\n=][^=\n]*=" "org-verbatim")
  (org--paint lstart line "~[^ \t\n~][^~\n]*~" "org-code")
  (org--paint lstart line "\\+[^ \t\n+][^+\n]*\\+" "org-strike")
  (org--paint lstart line "_[^ \t\n_][^_\n]*_" "org-underline")
  ;; footnote references [fn:label]
  (org--paint lstart line "\\[fn:[^]\n]*\\]" "org-footnote")
  ;; links [[target]] / [[target][desc]]
  (org--paint lstart line "\\[\\[[^]\n]*\\](\\[[^]\n]*\\])?\\]" "org-link")
  ;; active <…> and inactive [...] timestamps
  (org--paint lstart line "[<\\[][0-9]{4}-[0-9]{2}-[0-9]{2}[^]>\n]*[>\\]]" "org-date"))

;; Fontification, indent, and fold-marker passes each walk every line of the
;; buffer (re-splitting `org--lines` as they go) in interpreted Scheme on the
;; engine thread. On a very large .org file that blocks the engine long enough
;; that M-x and every other Scheme command stop responding, even though cursor
;; movement (handled in Rust) still works. Past a line threshold we skip the
;; auto passes — the Emacs `so-long` move. The mode, fold-style, and keymap are
;; still set, so editing works; only the org face/indent/chevron decoration is
;; dropped. Mirrors folding.scm's *fold-auto-limit* and markdown's guard.
(define *org-fontify-line-limit* 20000)
(define (org--fontify-ok?)
  (and (org--active?) (<= (line-count) *org-fontify-line-limit*)))

;; Repaint the whole buffer's faces. One pass; carries a flag for src-block
;; interiors (which are painted as a literal block, not parsed for emphasis).
;; The guard is captured BEFORE the text is read: this repaint runs debounced on
;; the engine thread while the user may still be typing, and a batch computed
;; against text that has since changed would paint faces/hides/indents at stale
;; byte offsets (every heading below the edit visibly blinks — hidden stars
;; swallow the previous newline, indents jump). If that happens, Rust drops the
;; whole batch and `org--fontify-schedule` re-runs us once typing pauses.
(define (org--fontify)
  (when (org--fontify-ok?)
    (guard-buffer-unchanged "org--fontify-schedule")
    (clear-overlay-faces)
    (clear-overlay-hides)   ; clear-overlay-faces no longer clears hides
    (set! *org--spans* '()) ; fresh batch (a prior aborted walk can't leak)
    (let ((all-lines (org--lines)))   ; ONE split, shared with the indent pass
      (let loop ((lines all-lines) (off 0) (insrc #f))
        (when (not (null? lines))
          (let* ((line (car lines))
                 (lend (+ off (utf8-length line)))
                 (lvl (org--heading-level line)))
            ;; Paint this line; each branch yields whether the NEXT line is
            ;; inside a src/example block.
            (loop (cdr lines) (+ lend 1)
                  (cond
                    ((and insrc (org--block-end? line))
                     (org--span off lend "org-meta") #f)
                    (insrc
                     (org--span off lend "org-block") #t)
                    ((org--block-begin? line)
                     (org--span off lend "org-meta")
                     (>= (string-match "_[sS][rR][cC]|_[eE][xX]" line) 0))
                    ((org--block-end? line)
                     (org--span off lend "org-meta") #f)
                    (lvl
                     (org--fontify-heading off line lvl) #f)
                    ((or (org--keyword-line? line) (org--comment-line? line))
                     (org--span off lend "org-meta") #f)
                    ((org--drawer-line? line)
                     (org--span off lend "org-drawer") #f)
                    ((org--table-line? line)
                     (org--span off lend "org-table") #f)
                    (else
                     (let ((cl (org--chart-line line)))
                       (if cl
                           (org--fontify-chart-line off lend cl)
                           (org--fontify-plain off line)))
                     #f))))))
      (org--flush-spans)
      (org--apply-indent all-lines))))

;; Virtual `org-indent-mode` indentation. Walk the lines tracking the enclosing
;; heading level: a heading of level L is indented (L-1)*per-level columns, and
;; every body line beneath it L*per-level (one level deeper, so it sits under the
;; title). Display-only via `set-line-indent`; cleared first so toggling off and
;; edits both refresh cleanly. Takes the caller's already-split lines so the face
;; and indent passes always describe the SAME text (a second `(org--lines)` here
;; could observe a newer snapshot mid-eval while the user types).
(define (org--apply-indent lines)
  (clear-line-indents)
  (when *org-indent*
    (let loop ((lines lines) (i 0) (curlvl 0))
      (when (not (null? lines))
        (let ((lvl (org--heading-level (car lines))))
          (if lvl
              (begin
                (when (> lvl 1) (set-line-indent i (* (- lvl 1) *org-indent-per-level*)))
                (loop (cdr lines) (+ i 1) lvl))
              (begin
                (when (> curlvl 0) (set-line-indent i (* curlvl *org-indent-per-level*)))
                (loop (cdr lines) (+ i 1) curlvl))))))))

;; Debounced live refresh (core's make-debounce): each edit schedules a timer;
;; only the last to fire repaints.
(define org--fontify-debounce
  (make-debounce "org-fontify" 200
                 (lambda () (when (org--active?) (org--fontify)))))
(define (org--fontify-schedule)
  (when (org--fontify-ok?) (org--fontify-debounce)))

;; ===========================================================================
;; Layer 1b — visibility cycling (folding)
;; ===========================================================================
;; All headlines in the active buffer as (line-index level), in document order.
(define (org--headings)
  (let loop ((lines (org--lines)) (i 0) (acc '()))
    (if (null? lines)
        (reverse acc)
        (let ((lvl (org--heading-level (car lines))))
          (loop (cdr lines) (+ i 1) (if lvl (cons (list i lvl) acc) acc))))))

(define (org--total-lines) (length (org--lines)))

;; A drawer is `:NAME:` … `:end:` (Emacs `org-drawer`); its body folds away,
;; leaving the `:NAME:` header — the heading fold model, but the range is the
;; lines from the header to its `:end:`. The opener and `:end:` share the
;; `org--drawer-line?` shape, so `:end:` (case-insensitive, like `:END:`) is
;; matched explicitly.
(define (org--drawer-end-line? line)
  (>= (string-match "(?i)^[ \t]*:end:[ \t]*$" line) 0))
(define (org--drawer-opener? line)
  (and (org--drawer-line? line) (not (org--drawer-end-line? line))))

;; Pair each opener with its `:end:` → (open-line . end-line) list, document
;; order. Pure (takes the line list) so the pairing is unit-testable. An opener
;; with no `:end:` yet — a tool drawer still streaming — is dropped, so a fold
;; never swallows the live tail.
(define (org--pair-drawers lines)
  (let loop ((ls lines) (i 0) (open #f) (acc '()))
    (cond
      ((null? ls) (reverse acc))
      ((and open (org--drawer-end-line? (car ls)))
       (loop (cdr ls) (+ i 1) #f (cons (cons open i) acc)))
      ((and (not open) (org--drawer-opener? (car ls)))
       (loop (cdr ls) (+ i 1) i acc))
      (else (loop (cdr ls) (+ i 1) open acc)))))
(define (org--drawers) (org--pair-drawers (org--lines)))
(define (org--drawer-end-of line drawers)
  (let loop ((ds drawers))
    (cond ((null? ds) #f)
          ((= (car (car ds)) line) (cdr (car ds)))
          (else (loop (cdr ds))))))

(define (org--find-heading headings line)
  (cond ((null? headings) #f)
        ((= (car (car headings)) line) (car headings))
        (else (org--find-heading (cdr headings) line))))

(define (org--next-heading-from headings line)
  (cond ((null? headings) #f)
        ((>= (car (car headings)) line) (car headings))
        (else (org--next-heading-from (cdr headings) line))))

;; Last line of the subtree rooted at line `start` (level `lvl`) in a raw
;; `lines` list — the on-disk twin of org--subtree-end below, shared by refile
;; and capture, which walk files that aren't the active buffer.
(define (org--subtree-end-in lines start lvl)
  (let ((n (length lines)))
    (let loop ((i (+ start 1)))
      (cond ((>= i n) (- n 1))
            ((let ((l (org--heading-level (nth lines i)))) (and l (<= l lvl))) (- i 1))
            (else (loop (+ i 1)))))))

;; Last body line of the subtree rooted at headline `H` (level `L`): one line
;; before the next headline of level ≤ L, else the last line.
(define (org--subtree-end H L headings total)
  (let loop ((hs headings))
    (cond ((null? hs) (- total 1))
          ((and (> (car (car hs)) H) (<= (nth (car hs) 1) L)) (- (car (car hs)) 1))
          (else (loop (cdr hs))))))

;; Per-buffer local fold state: per buffer, a list of (heading-line . state),
;; state ∈ 'folded | 'children. A heading absent from the list is 'subtree (fully
;; shown). This drives the three-state TAB cycle (subtree→folded→children→…).
;; Each folded line's text anchor is kept too, so the line numbers can be
;; relocated when edits above them shift the buffer (make-fold-store, folding.scm).
(define *org-fold-store* (make-fold-store car (lambda (p n) (cons n (cdr p)))))
(define (org--fold-state-get) (fold-store-get *org-fold-store*))
(define (org--fold-state-set v) (fold-store-set *org-fold-store* v))
;; Relocate every recorded fold to its heading/drawer's current line (dropping
;; ones whose line is gone), so a toggle never re-folds a stale line number.
(define (org--fold-state-resync) (fold-store-resync *org-fold-store*))
(define (org--heading-state H)
  (let loop ((ps (org--fold-state-get)))
    (cond ((null? ps) 'subtree)
          ((= (car (car ps)) H) (cdr (car ps)))
          (else (loop (cdr ps))))))
(define (org--set-heading-state H st)
  (let ((rest (filter (lambda (p) (not (= (car p) H))) (org--fold-state-get))))
    (org--fold-state-set (if (eq? st 'subtree) rest (cons (cons H st) rest)))))

;; Next local-cycle state for a heading, given its current state and whether it
;; has sub-headings. A childless heading renders 'children identically to 'folded
;; (see org--fold-ranges-for), so it skips that no-op step and toggles
;; folded↔subtree — a collapsed leaf expands in one TAB, not two.
(define (org--cycle-next state has-kids)
  (cond ((eq? state 'subtree) 'folded)
        ((eq? state 'folded) (if has-kids 'children 'subtree))
        (else 'subtree)))

;; Smallest heading level among the descendants in subtree (H, E], or #f.
(define (org--min-child-level headings H E)
  (let loop ((hs headings) (m #f))
    (cond ((null? hs) m)
          ((and (> (car (car hs)) H) (<= (car (car hs)) E))
           (loop (cdr hs) (if m (min m (nth (car hs) 1)) (nth (car hs) 1))))
          (else (loop (cdr hs) m)))))

;; The direct child heading lines of H (the shallowest descendant level).
(define (org--direct-children headings H E)
  (let ((ml (org--min-child-level headings H E)))
    (if (not ml) '()
        (let loop ((hs headings) (acc '()))
          (cond ((null? hs) (reverse acc))
                ((and (> (car (car hs)) H) (<= (car (car hs)) E) (= (nth (car hs) 1) ml))
                 (loop (cdr hs) (cons (car (car hs)) acc)))
                (else (loop (cdr hs) acc)))))))

;; Inclusive (start . end) line ranges to hide for heading H in `state`. Pure, so
;; the visibility logic is unit-testable independently of the fold primitives.
(define (org--fold-ranges-for headings total H state)
  (let* ((entry (org--find-heading headings H))
         (L (nth entry 1))
         (E (org--subtree-end H L headings total)))
    (cond
      ((<= E H) '())                                   ; nothing under the heading
      ((eq? state 'folded) (list (cons (+ H 1) E)))    ; hide the whole subtree
      ((eq? state 'children)
       (let ((kids (org--direct-children headings H E)))
         (if (null? kids)
             (list (cons (+ H 1) E))                   ; no children ⇒ same as folded
             (let* ((first (car kids))
                    (body (if (> first (+ H 1)) (list (cons (+ H 1) (- first 1))) '())))
               (append body
                 (map (lambda (C)
                        (cons (+ C 1)
                              (org--subtree-end C (nth (org--find-heading headings C) 1)
                                                headings total)))
                      kids))))))
      (else '()))))

;; Re-derive folds from the per-heading state (the only fold primitives are
;; fold-lines / unfold-all, so we clear and re-apply).
(define (org--reapply-folds)
  (unfold-all)
  (let ((headings (org--headings)) (drawers (org--drawers)) (total (org--total-lines)))
    (for-each
     (lambda (pair)
       (let ((line (car pair)) (st (cdr pair)))
         (cond
           ((org--find-heading headings line)
            (for-each (lambda (r)
                        (when (>= (cdr r) (car r)) (fold-lines (car r) (+ (cdr r) 1))))
                      (org--fold-ranges-for headings total line st)))
           ;; A folded drawer: hide its body and `:end:` line, keep the header.
           (else
            (let ((end (and (eq? st 'folded) (org--drawer-end-of line drawers))))
              (when end (fold-lines (+ line 1) (+ end 1))))))))
     (org--fold-state-get))))

;; Paint a gutter chevron on every headline (down = expanded, right = folded),
;; so the fold can be toggled by clicking the chevron as well as with TAB.
(define (org--refresh-fold-markers)
  (org--fold-state-resync)
  (clear-fold-markers)
  (for-each (lambda (h)
              (set-fold-marker (car h)
                               (not (eq? (org--heading-state (car h)) 'folded))))
            (org--headings))
  ;; Drawer openers fold too (binary: open or folded), so chevron them as well.
  (for-each (lambda (d)
              (set-fold-marker (car d)
                               (not (eq? (org--heading-state (car d)) 'folded))))
            (org--drawers)))

;; Toggle the fold of the heading on `line` — the chevron click entry point (a
;; click can't move point first, so this takes an explicit line rather than
;; reading point like `org-cycle`). Unlike TAB's three-state cycle, a chevron is
;; a binary affordance: collapse a shown subtree, or fully expand a folded one —
;; so the arrow always matches what's actually visible (no "open arrow but body
;; still hidden" `children` state).
(define (org-fold-toggle-at-line line)
  (org--fold-state-resync)
  (when (or (org--heading-level (org--line-text line))
            (org--drawer-opener? (org--line-text line)))
    (let ((next (if (eq? (org--heading-state line) 'folded) 'subtree 'folded)))
      (org--set-heading-state line next)
      (org--reapply-folds)
      (org--refresh-fold-markers))))

;; Route chevron clicks in Org buffers (fold-style "org") to the headline toggle.
(fold-register-style "org" org-fold-toggle-at-line)

;; Fold every drawer in the current buffer (collapse each to its `:NAME:` header)
;; and repaint the chevrons. Used by agent-shell to tuck a finished turn's tool
;; activity away. A drawer the user later expands (TAB) is removed from the fold
;; state, so it re-folds only if this runs again.
(define (org-fold-all-drawers)
  (org--fold-state-resync)
  (for-each (lambda (d) (org--set-heading-state (car d) 'folded)) (org--drawers))
  (org--reapply-folds)
  (org--refresh-fold-markers))

;; TAB — on a headline, cycle local visibility subtree→folded→children→subtree
;; (like Emacs); inside a src block, indent the code; in plain body do nothing
;; (Emacs never inserts a tab at point there — it would just shove the line right).
(define-command (org-cycle)
  (org--fold-state-resync)
  (let* ((L (buffer-line)) (line (org--line-text L)))
    (cond
      ((org--heading-level line)
       (let* ((headings (org--headings))
              (total (org--total-lines))
              (lvl (nth (org--find-heading headings L) 1))
              (E (org--subtree-end L lvl headings total))
              (has-kids (not (null? (org--direct-children headings L E))))
              (next (org--cycle-next (org--heading-state L) has-kids)))
         (org--set-heading-state L next)
         (org--reapply-folds)
         (org--refresh-fold-markers)
         (message (cond ((eq? next 'folded) "Org: folded")
                        ((eq? next 'children) "Org: children")
                        (else "Org: subtree")))))
      ((org--drawer-opener? line)
       (let ((next (if (eq? (org--heading-state L) 'folded) 'subtree 'folded)))
         (org--set-heading-state L next)
         (org--reapply-folds)
         (org--refresh-fold-markers)
         (message (if (eq? next 'folded) "Org: drawer folded" "Org: drawer open"))))
      ((org--src-block-bounds L) (indent-for-tab-command))
      (else #f))))

(define *org-global-state* '())
(define (org--global-state) (buffer-state-get *org-global-state* "showall"))
(define (org--set-global-state s)
  (set! *org-global-state* (buffer-state-set *org-global-state* s)))

;; Show only top-level headlines (fold every level-1 subtree). Routed through the
;; per-heading state so a following local TAB expands one heading and leaves the
;; rest folded, like Emacs.
(define (org--show-overview)
  (org--fold-state-set
   (map (lambda (h) (cons (car h) 'folded))
        (filter (lambda (h) (= (nth h 1) 1)) (org--headings))))
  (org--reapply-folds))

;; Show all headlines, hide all body text (fold the gap between each headline
;; and the next).
(define (org--show-contents)
  (org--fold-state-set '()) (unfold-all)
  (let ((headings (org--headings)) (total (org--total-lines)))
    (let loop ((hs headings))
      (when (not (null? hs))
        (let* ((h (car hs))
               (next-line (if (null? (cdr hs)) total (car (car (cdr hs)))))
               (body-start (+ (car h) 1)))
          (when (> next-line body-start) (fold-lines body-start next-line))
          (loop (cdr hs)))))))

;; S-TAB — rotate the whole buffer: overview → contents → show-all.
(define-command (org-global-cycle)
  (let ((next (cond ((equal? (org--global-state) "showall") "overview")
                    ((equal? (org--global-state) "overview") "contents")
                    (else "showall"))))
    (org--set-global-state next)
    (cond ((equal? next "overview") (org--show-overview))
          ((equal? next "contents") (org--show-contents))
          (else (org--fold-state-set '()) (unfold-all)))
    (org--refresh-fold-markers)
    (message (string-append "Org global: " next))))

;; ===========================================================================
;; Layer 2 — structure editing
;; ===========================================================================
;; The headline line at point, or the nearest one above, or #f.
(define (org--heading-line-at-point)
  (let ((lines (org--lines)) (L (buffer-line)))
    (if (org--heading-level (org--line-text-in lines L))
        L
        (let loop ((i (- L 1)))
          (cond ((< i 0) #f)
                ((org--heading-level (org--line-text-in lines i)) i)
                (else (loop (- i 1))))))))

;; The keyword on the heading at point, or #f (a public accessor, so configs
;; need not reach into the org--* internals to read it).
(define (org-keyword-at-point)
  (let ((H (org--heading-line-at-point)))
    (if H (nth (org--parse-headline (org--line-text H)) 1) #f)))

;; The full ordered cycle C-c C-t rotates through: the TODO vocabulary followed
;; by the user's custom keywords. Like Emacs `org-todo-keywords`, every keyword
;; that is recognized is part of the rotation.
(define (org--cycle-keywords) (append *org-todo-keywords* *org-custom-keywords*))

;; The keyword following `cur` in `seq`, Emacs-style: #f (none) → first entry;
;; the last entry → "" (clear); otherwise the next entry. A `cur` not in `seq`
;; restarts the cycle at the first entry.
(define (org--next-keyword cur seq)
  (cond ((null? seq) "")
        ((not cur) (car seq))
        (else
         (let loop ((ls seq))
           (cond ((null? ls) (car seq))
                 ((equal? (car ls) cur) (if (null? (cdr ls)) "" (car (cdr ls))))
                 (else (loop (cdr ls))))))))

;; Rewrite heading H to carry keyword `kw` ("" clears it), preserving the
;; priority cookie and trailing tags. Shared by org-todo and org-set-keyword.
(define (org--apply-keyword H kw)
  (let ((h (org--parse-headline (org--line-text H))))
    (org--replace-line H
      (org--render-headline (nth h 0) kw (nth h 2) (nth h 3) (nth h 4)))))

;; C-c C-t — rotate the headline at point through the full keyword cycle
;; (nothing → each TODO/custom keyword → nothing).
(define-command (org-todo)
  (let ((H (org--heading-line-at-point)))
    (if (not H)
        (message "Not on a heading")
        (let ((next (org--next-keyword (org-keyword-at-point) (org--cycle-keywords))))
          ;; Marking a repeating task DONE shifts its planning stamps to the next
          ;; occurrence and resets the keyword instead (Emacs `org-todo-repeat`).
          (if (and (org--done-keyword? next) (org--has-repeater? H))
              (org--repeat-task H)
              (org--apply-keyword H next))))))

;; Set a specific keyword on the headline at point, or clear it if that same
;; keyword is already there (so a binding toggles it). Unlike org-todo's rotate,
;; this jumps straight to one keyword — handy for direct per-label bindings.
(define (org-set-keyword kw)
  (let ((H (org--heading-line-at-point)))
    (if H
        (org--apply-keyword H (if (equal? (org-keyword-at-point) kw) "" kw))
        (message "Not on a heading"))))

;; M-right — demote (one deeper): add a star.
(define-command (org-demote)
  (let ((H (org--heading-line-at-point)))
    (when H (org--replace-line H (string-append "*" (org--line-text H))))))

;; M-left — promote (one shallower): remove a star.
(define-command (org-promote)
  (let ((H (org--heading-line-at-point)))
    (when H
      (let ((line (org--line-text H)))
        (if (>= (string-match "^\\*\\*" line) 0)
            (org--replace-line H (substring line 1 (string-length line)))
            (message "Already at top level"))))))

;; M-RET — insert a sibling headline just after the current subtree.
(define-command (org-insert-heading)
  (let* ((H (org--heading-line-at-point))
         (headings (org--headings)) (total (org--total-lines))
         (lvl (if H (nth (org--find-heading headings H) 1) 1))
         (end (if H (org--subtree-end H lvl headings total) (buffer-line)))
         (pos (+ (org--line-start end) (utf8-length (org--line-text end))))
         (stars (string-repeat "*" lvl)))
    (insert-at pos (string-append "\n" stars " "))
    (goto-char (+ pos 1 (string-length stars) 1))))

;; C-x n s — narrow the buffer to the subtree at point, from its headline
;; to the next headline of the same or a higher level (Emacs
;; `org-narrow-to-subtree`). C-x n w widens again.
(define-command (org-narrow-to-subtree)
  (let ((H (org--heading-line-at-point)))
    (if (not H)
        (message "Before first headline")
        (let* ((lines (org--lines)) (headings (org--headings)) (total (length lines))
               (lvl (nth (org--find-heading headings H) 1))
               (end (org--subtree-end H lvl headings total)))
          (narrow-to-region (lines-byte-offset lines H)
                            (if (< (+ end 1) total)
                                (lines-byte-offset lines (+ end 1))
                                (buffer-size)))))))

;; M-down — move the subtree at point below its next sibling.
(define-command (org-move-subtree-down)
  (let ((H (org--heading-line-at-point)))
    (when H
      (let* ((lines (org--lines)) (headings (org--headings)) (total (length lines))
             (lvl (nth (org--find-heading headings H) 1))
             (end (org--subtree-end H lvl headings total))
             (next (org--next-heading-from headings (+ end 1))))
        ;; Only a true sibling (same level) — a shallower heading after the
        ;; subtree is the parent's next sibling, not ours (mirrors
        ;; org--prev-sibling).
        (if (and next (= (nth next 1) lvl))
            (let* ((H2 (car next))
                   (end2 (org--subtree-end H2 (nth next 1) headings total))
                   (blockA (org--slice lines H end))
                   (blockB (org--slice lines (+ end 1) end2))
                   (before (org--slice lines 0 (- H 1)))
                   (after  (org--slice lines (+ end2 1) (- total 1))))
              (org--replace-all (join-lines (append before blockB blockA after)))
              (goto-line (+ H (length blockB))))
            (message "No sibling below"))))))

;; M-up — move the subtree at point above its previous sibling.
(define (org--prev-sibling headings H lvl)
  (let loop ((hs headings) (cand #f))
    (cond ((null? hs) (if (and cand (= (nth cand 1) lvl)) cand #f))
          ((>= (car (car hs)) H) (if (and cand (= (nth cand 1) lvl)) cand #f))
          ((<= (nth (car hs) 1) lvl) (loop (cdr hs) (car hs)))
          (else (loop (cdr hs) cand)))))
;; M-up — move the subtree at point above its previous sibling. Done as a direct
;; block swap (not goto + move-down): under the snapshot model a queued goto isn't
;; visible to reads in the same command, so re-reading point would move the wrong
;; subtree the wrong way.
(define-command (org-move-subtree-up)
  (let ((H (org--heading-line-at-point)))
    (when H
      (let* ((lines (org--lines)) (headings (org--headings)) (total (length lines))
             (lvl (nth (org--find-heading headings H) 1))
             (prev (org--prev-sibling headings H lvl)))
        (if prev
            (let* ((P (car prev))
                   (endH (org--subtree-end H lvl headings total))
                   (blockP (org--slice lines P (- H 1)))   ; prev sibling's subtree
                   (blockH (org--slice lines H endH))       ; this subtree
                   (before (org--slice lines 0 (- P 1)))
                   (after  (org--slice lines (+ endH 1) (- total 1))))
              (org--replace-all (join-lines (append before blockH blockP after)))
              (goto-line P))
            (message "No sibling above"))))))

;; C-c C-q — set the headline's tags (colon- or space-separated input). The
;; heading line is captured in the callback closure (read-string accepts one).
(define (org--current-tags H) (nth (org--parse-headline (org--line-text H)) 4))
(define-command (org-set-tags)
  (let ((H (org--heading-line-at-point)))
    (if H
        (read-string "Tags: " (lambda (input) (org--set-tags-apply H input))
                     (org--current-tags H))
        (message "Not on a heading"))))
(define (org--normalize-tags s)
  (string-join (filter (lambda (x) (not (equal? x "")))
                       (split-many (replace-regexp-in-string "[ \t]+" ":" (string-trim s)) ":"))
               ":"))
(define (org--set-tags-apply H input)
  (let* ((line (org--line-text H))
         (base (replace-regexp-in-string org--tag-strip-re "" line))
         (clean (string-trim input)))
    (org--replace-line H
      (if (equal? clean "")
          base
          (string-append base " :" (org--normalize-tags clean) ":")))))

;; C-c , — set or clear the headline's priority cookie.
(define-command (org-set-priority)
  (let ((H (org--heading-line-at-point)))
    (if H
        (read-string "Priority (A-C, blank clears): "
                     (lambda (input) (org--set-priority-apply H input)))
        (message "Not on a heading"))))
(define (org--set-priority-apply H input)
  (let* ((h (org--parse-headline (org--line-text H)))
         (p (string-upcase (string-trim input))))
    (org--replace-line H
      (org--render-headline (nth h 0) (nth h 1)
                            (if (>= (string-match "^[A-C]$" p) 0) p "")
                            (nth h 3) (nth h 4)))))

;; Toggle the checkbox on a `- [ ]` list item at point; #t if it did.
(define (org-toggle-checkbox)
  (let* ((L (buffer-line)) (line (org--line-text L))
         (caps (regexp-captures "^([ \t]*[-+*][ \t]+)\\[([ xX-])\\]" line)))
    (if (null? caps)
        #f
        (let* ((prefix (nth caps 1)) (state (nth caps 2))
               (new (if (equal? (string-upcase state) "X") " " "X"))
               (rest (substring line (+ (string-length prefix) 3) (string-length line))))
          (org--replace-line L (string-append prefix "[" new "]" rest))
          #t))))

;; ===========================================================================
;; Layer 3 — the Org AST parser
;; ===========================================================================
;; `(org-parse TEXT)` → a flat list of block elements, each a list whose head is
;; a type string:
;;   ("heading" level todo priority title tags)
;;   ("para" text) ("src" lang code) ("example" text) ("quote" text)
;;   ("ulist" items) ("olist" items)   items = list of (checkbox . text)
;;   ("table" rows)  rows = ("sep") | ("row" cell …)
;;   ("keyword" key value) ("hr")
;; Both the HTML exporter and the agenda scanner consume this.
(define (org--parse-heading line)
  (let ((h (org--parse-headline line)))
    (list "heading" (string-length (nth h 0)) (or (nth h 1) "")
          (nth h 2) (nth h 3) (nth h 4))))

(define (org--parse-keyword line)
  (let ((c (regexp-captures "^[ \t]*#\\+([A-Za-z_]+):[ \t]*(.*)$" line)))
    (list "keyword" (if (null? c) "" (nth c 1)) (if (null? c) "" (nth c 2)))))

;; Gather lines up to the matching `#+end_…`; → (body-lines . rest-after-end).
(define (org--collect-block ls)
  (let loop ((ls ls) (acc '()))
    (cond ((null? ls) (cons (reverse acc) '()))
          ((org--block-end? (car ls)) (cons (reverse acc) (cdr ls)))
          (else (loop (cdr ls) (cons (car ls) acc))))))

(define (org--parse-item line kind)
  (let* ((stripped (if (equal? kind "u")
                       (replace-regexp-in-string "^[ \t]*[-+][ \t]+" "" line)
                       (replace-regexp-in-string "^[ \t]*[0-9]+[.)][ \t]+" "" line)))
         (cb (regexp-captures "^\\[([ xX-])\\][ \t]*(.*)$" stripped)))
    (if (null? cb)
        (cons "" stripped)
        (cons (if (equal? (string-upcase (nth cb 1)) "X") "X" (nth cb 1)) (nth cb 2)))))

(define (org--collect-list ls kind)
  (let loop ((ls ls) (acc '()))
    (cond ((null? ls) (cons (reverse acc) '()))
          ((and (equal? kind "u") (org--ulist-line? (car ls)))
           (loop (cdr ls) (cons (org--parse-item (car ls) "u") acc)))
          ((and (equal? kind "o") (org--olist-line? (car ls)))
           (loop (cdr ls) (cons (org--parse-item (car ls) "o") acc)))
          (else (cons (reverse acc) ls)))))

;; Split a table line into trimmed cells.
(define (org--table-cells line)
  (let* ((t (string-trim line))
         (t2 (replace-regexp-in-string "\\|[ \t]*$" ""
               (replace-regexp-in-string "^[ \t]*\\|" "" t))))
    (map string-trim (split-many t2 "|"))))

(define (org--collect-table ls)
  (let loop ((ls ls) (acc '()))
    (cond ((or (null? ls) (not (org--table-line? (car ls)))) (cons (reverse acc) ls))
          ((org--table-sep? (car ls)) (loop (cdr ls) (cons (list "sep") acc)))
          (else (loop (cdr ls) (cons (cons "row" (org--table-cells (car ls))) acc))))))

(define (org--collect-para ls)
  (let loop ((ls ls) (acc '()))
    (cond ((null? ls) (cons (reverse acc) '()))
          ((let ((l (car ls)))
             (or (equal? (string-trim l) "")
                 (org--heading-level l) (org--keyword-line? l) (org--block-begin? l)
                 (org--table-line? l) (org--ulist-line? l) (org--olist-line? l) (org--hr-line? l)
                 (org--planning-line? l) (org--drawer-opener? l) (org--fixed-width-line? l)))
           (cons (reverse acc) ls))
          (else (loop (cdr ls) (cons (car ls) acc))))))

;; A fixed-width line (`: text`, or a bare `:`) — how babel writes #+RESULTS.
(define (org--fixed-width-line? line) (>= (string-match "^[ \t]*:( |$)" line) 0))
(define (org--collect-fixed ls)
  (let loop ((ls ls) (acc '()))
    (if (and (pair? ls) (org--fixed-width-line? (car ls)))
        (loop (cdr ls) (cons (replace-regexp-in-string "^[ \t]*:( |$)" "" (car ls)) acc))
        (cons (reverse acc) ls))))

;; The lines after the drawer opened at the head of `ls` (past its `:END:`), or
;; #f when it is never closed (then it is just text).
(define (org--skip-drawer ls)
  (let loop ((rest (cdr ls)))
    (cond ((null? rest) #f)
          ((org--drawer-end-line? (car rest)) (cdr rest))
          (else (loop (cdr rest))))))

(define (org-parse text)
  (let loop ((ls (split-many text "\n")) (acc '()))
    (if (null? ls)
        (reverse acc)
        (let ((line (car ls)))
          (cond
            ((equal? (string-trim line) "") (loop (cdr ls) acc))
            ((org--heading-level line) (loop (cdr ls) (cons (org--parse-heading line) acc)))
            ;; Planning lines and drawers (PROPERTIES, LOGBOOK, …) are metadata,
            ;; not content: Emacs's exporters leave them out by default.
            ((org--planning-line? line) (loop (cdr ls) acc))
            ((and (org--drawer-opener? line) (org--skip-drawer ls)) (loop (org--skip-drawer ls) acc))
            ((org--fixed-width-line? line)
             (let ((cd (org--collect-fixed ls)))
               (loop (cdr cd) (cons (list "example" (join-lines (car cd))) acc))))
            ((org--keyword-line? line) (loop (cdr ls) (cons (org--parse-keyword line) acc)))
            ((and (org--block-begin? line) (>= (string-match "[bB][eE][gG][iI][nN]_[sS][rR][cC]" line) 0))
             (let ((cd (org--collect-block (cdr ls))))
               (loop (cdr cd) (cons (list "src" (org--block-lang line) (join-lines (car cd))) acc))))
            ((and (org--block-begin? line) (>= (string-match "[bB][eE][gG][iI][nN]_[eE][xX]" line) 0))
             (let ((cd (org--collect-block (cdr ls))))
               (loop (cdr cd) (cons (list "example" (join-lines (car cd))) acc))))
            ((org--block-begin? line)   ; quote / other block
             (let ((cd (org--collect-block (cdr ls))))
               (loop (cdr cd) (cons (list "quote" (join-lines (car cd))) acc))))
            ((org--hr-line? line) (loop (cdr ls) (cons (list "hr") acc)))
            ((org--ulist-line? line)
             (let ((cd (org--collect-list ls "u")))
               (loop (cdr cd) (cons (list "ulist" (car cd)) acc))))
            ((org--olist-line? line)
             (let ((cd (org--collect-list ls "o")))
               (loop (cdr cd) (cons (list "olist" (car cd)) acc))))
            ((org--table-line? line)
             (let ((cd (org--collect-table ls)))
               (loop (cdr cd) (cons (list "table" (car cd)) acc))))
            (else
             (let ((cd (org--collect-para ls)))
               (loop (cdr cd) (cons (list "para" (join-lines (car cd))) acc)))))))))

;; ===========================================================================
;; Layer 3b — HTML export (walks the AST)
;; ===========================================================================
(define (org--escape-html s)
  (replace-regexp-in-string ">" "&gt;"
    (replace-regexp-in-string "<" "&lt;"
      (replace-regexp-in-string "&" "&amp;" s))))

;; Split `s` around Org links ([[url]] / [[url][desc]]): link segments go
;; through `link-fn`, the text between them through `text-fn`, and the results
;; are concatenated. Emphasis regexes must never see a converted link — the
;; `/.../` italic rule would otherwise mangle every URL path.
(define org--link-split-re "(?s)^(.*?)(\\[\\[[^]]+\\](?:\\[[^]]+\\])?\\])(.*)$")
(define (org--inline-split s link-fn text-fn)
  (let loop ((s s) (acc '()))
    (let ((c (regexp-captures org--link-split-re s)))
      (if (null? c)
          (string-concat (reverse (cons (text-fn s) acc)))
          (loop (nth c 3)
                (cons (link-fn (nth c 2)) (cons (text-fn (nth c 1)) acc)))))))

;; Inline markup → HTML. Escape first, then split around links so emphasis only
;; touches the plain text between them.
(define (org--inline->html s)
  (org--inline-split (org--escape-html s) org--link->html org--emphasis->html))
(define (org--link->html s)
  (let* ((s (replace-regexp-in-string "\\[\\[([^]]+)\\]\\[([^]]+)\\]\\]" "<a href=\"$1\">$2</a>" s))
         (s (replace-regexp-in-string "\\[\\[([^]]+)\\]\\]" "<a href=\"$1\">$1</a>" s)))
    s))
;; Italic runs FIRST: every tag the other rules emit has a `/` in its closing
;; half (`</strong>`), which the `/.../` rule would otherwise pair up.
(define (org--emphasis->html s)
  (let* ((s (replace-regexp-in-string "/([^/\n]+)/" "<em>$1</em>" s))
         (s (replace-regexp-in-string "\\*([^*\n]+)\\*" "<strong>$1</strong>" s))
         (s (replace-regexp-in-string "=([^=\n]+)=" "<code>$1</code>" s))
         (s (replace-regexp-in-string "~([^~\n]+)~" "<code>$1</code>" s))
         (s (replace-regexp-in-string "\\+([^+\n]+)\\+" "<del>$1</del>" s))
         (s (replace-regexp-in-string "_([^_\n]+)_" "<u>$1</u>" s))
         (s (replace-regexp-in-string "\\[fn:([^]\n]*)\\]" "<sup>$1</sup>" s)))
    s))

(define (org--list->html e tag)
  (string-append "<" tag ">\n"
    (string-concat (map (lambda (it)
                        (string-append "<li>"
                          (if (equal? (car it) "") "" (string-append "[" (car it) "] "))
                          (org--inline->html (cdr it)) "</li>\n"))
                      (nth e 1)))
    "</" tag ">\n"))

(define (org--table->html e)
  (string-append "<table>\n"
    (string-concat (map (lambda (row)
                        (if (equal? (car row) "sep")
                            ""
                            (string-append "<tr>"
                              (string-concat (map (lambda (c)
                                                  (string-append "<td>" (org--inline->html c) "</td>"))
                                                (cdr row)))
                              "</tr>\n")))
                      (nth e 1)))
    "</table>\n"))

(define (org--block->html e)
  (let ((type (car e)))
    (cond
      ((equal? type "heading")
       (let ((lvl (number->string (min 6 (nth e 1)))) (todo (nth e 2))
             (title (nth e 4)) (tags (nth e 5)))
         (string-append "<h" lvl ">"
           (if (equal? todo "") "" (string-append "<span class=\"todo\">" (org--escape-html todo) "</span> "))
           (org--inline->html title)
           (if (equal? tags "") "" (string-append " <span class=\"tags\">" (org--escape-html tags) "</span>"))
           "</h" lvl ">\n")))
      ((equal? type "para") (string-append "<p>" (org--inline->html (nth e 1)) "</p>\n"))
      ((equal? type "src") (string-append "<pre><code>" (org--escape-html (nth e 2)) "</code></pre>\n"))
      ((equal? type "example") (string-append "<pre>" (org--escape-html (nth e 1)) "</pre>\n"))
      ((equal? type "quote") (string-append "<blockquote>" (org--inline->html (nth e 1)) "</blockquote>\n"))
      ((equal? type "hr") "<hr>\n")
      ((equal? type "ulist") (org--list->html e "ul"))
      ((equal? type "olist") (org--list->html e "ol"))
      ((equal? type "table") (org--table->html e))
      (else ""))))

(define (org--doc-title ast)
  (let loop ((es ast))
    (cond ((null? es) "Untitled")
          ((and (equal? (car (car es)) "keyword")
                (equal? (string-upcase (nth (car es) 1)) "TITLE"))
           (nth (car es) 2))
          (else (loop (cdr es))))))

(define (org--ast->html ast)
  (string-append
    "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>"
    (org--escape-html (org--doc-title ast))
    "</title>\n<style>body{font-family:sans-serif;max-width:48rem;margin:2rem auto;padding:0 1rem;line-height:1.5}pre{background:#f4f4f4;padding:.6rem;overflow:auto}code{background:#f4f4f4}table{border-collapse:collapse}td,th{border:1px solid #ccc;padding:.2rem .5rem}.todo{color:#c00}.tags{color:#888;font-size:.8em}</style>\n</head>\n<body>\n"
    (string-concat (map org--block->html ast))
    "</body>\n</html>\n"))

(define (org--html-output-path) (org--output-path ".html"))

;; C-c C-e h — export the current buffer to a sibling .html file.
(define-command (org-export-html)
  (let ((path (org--html-output-path)))
    (write-file path (org--ast->html (org-parse (buffer-string))))
    (message (string-append "Exported to " path))))

;; ===========================================================================
;; Layer 4 — agenda + capture
;; ===========================================================================
;; The set of files the agenda scans. If unset, every .org file under the
;; current project (or buffer directory).
(define *org-agenda-files* '())
(define (org-set-agenda-files lst) (set! *org-agenda-files* lst))
(define (org--agenda-file-list)
  (if (not (null? *org-agenda-files*))
      *org-agenda-files*
      (let ((root (or (project-root (buffer-directory)) (buffer-directory))))
        (filter (lambda (p) (ends-with? p ".org")) (walk-files root)))))

;; Every (date . kind) on a planning line, for SCHEDULED then DEADLINE — one
;; line may carry both (`DEADLINE: <…> SCHEDULED: <…>`), and the agenda lists
;; each date.
(define (org--planning-infos line)
  (let loop ((kinds (list "SCHEDULED" "DEADLINE")) (acc '()))
    (if (null? kinds)
        (reverse acc)
        (let ((c (regexp-captures (string-append (car kinds) ":[ \t]*[<\\[]([0-9]{4}-[0-9]{2}-[0-9]{2})")
                                  line)))
          (loop (cdr kinds) (if (null? c) acc (cons (cons (nth c 1) (car kinds)) acc)))))))

;; The lines of the agenda file `path`: the live buffer when it is the one being
;; edited (unsaved changes count, and line numbers match what RET / refile will
;; act on), else the file on disk.
(define (org--file-lines path)
  (if (and (org--active?) (same-file? path (org--current-file-path)))
      (org--lines)
      (split-many (read-file path) "\n")))

;; ":a:b:" / "a:b" → ("a" "b").
(define (org--split-tag-string s)
  (remove-equal "" (split-many s ":")))
;; a ∪ b, preserving a's order then b's new tags.
(define (org--union a b) (append a (filter (lambda (x) (not (member x a))) b)))

;; Scan one file → entries (path line date kind todo title tags) for headlines
;; that carry a TODO keyword and/or a planning date — one entry per planning
;; date. With `all?`, every headline yields exactly one entry (the tags view
;; matches plain tagged notes too). `tags` includes tags inherited from ancestor
;; headings (Emacs `org-use-tag-inheritance`). A `stack` of
;; (level . inherited-tags) tracks the heading ancestry as we descend.
(define (org--scan-file path . all)
  (let ((all? (and (not (null? all)) (car all))))
    (let loop ((ls (org--file-lines path)) (i 0) (stack '()) (acc '()))
      (if (null? ls)
          (reverse acc)
          (let ((line (car ls)))
            (if (org--heading-level line)
                (let* ((h (org--parse-headline line))
                       (lvl (string-length (nth h 0)))
                       (kw (nth h 1))
                       (title (nth h 3))
                       (own (org--split-tag-string (nth h 4)))
                       (stack2 (filter (lambda (e) (< (car e) lvl)) stack))
                       (tags (org--union (if (null? stack2) '() (cdr (car stack2))) own))
                       (plans (if (null? (cdr ls)) '() (org--planning-infos (nth ls 1))))
                       (plans (cond ((and all? (not (null? plans))) (list (car plans)))
                                    ((or all? (and kw (null? plans))) (list (cons "" "")))
                                    (else plans)))
                       (new (map (lambda (p) (list path i (car p) (cdr p) (or kw "") title tags))
                                 plans)))
                  (loop (cdr ls) (+ i 1) (cons (cons lvl tags) stack2)
                        (append (reverse new) acc)))
                (loop (cdr ls) (+ i 1) stack acc)))))))

(define (org--all-entries . all)
  (let loop ((files (org--agenda-file-list)) (acc '()))
    (if (null? files) acc
        (loop (cdr files) (append acc (org--scan-file (car files) (and (not (null? all)) (car all))))))))

;; Sort by date (undated entries last), lexically — ISO dates order correctly.
(define (org--entry-key e) (let ((d (nth e 2))) (if (equal? d "") "9999-99-99" d)))
(define (org--sort-entries es) (sort-by org--entry-key string<? es))

;; line-in-agenda-buffer → (path . src-line), for RET.
(define *org-agenda-targets* '())
(define (org--render-agenda entries)
  (set! *org-agenda-targets* '())
  (let loop ((es entries) (i 0) (lines '()) (faces '()))
    (if (null? es)
        (render-buffer "*Org Agenda*" "org-agenda-mode" (join-lines (reverse lines)) (reverse faces))
        (let* ((e (car es))
               (date (nth e 2)) (kind (nth e 3)) (todo (nth e 4)) (title (nth e 5))
               (tags (if (> (length e) 6) (nth e 6) '()))
               (text (string-append
                       (if (equal? date "") "             " (string-append date "  "))
                       (if (equal? kind "") "" (string-append (substring kind 0 (min 4 (string-length kind))) ": "))
                       (if (equal? todo "") "" (string-append todo " "))
                       title
                       (if (null? tags) "" (string-append "  :" (string-join tags ":") ":"))
                       "   " (basename (nth e 0)))))
          (set! *org-agenda-targets* (cons (cons i (cons (nth e 0) (nth e 1))) *org-agenda-targets*))
          (loop (cdr es) (+ i 1) (cons text lines)
                (cons (list i (if (org--done-keyword? todo) "org-done" "org-todo")) faces))))))

;; The view the *Org Agenda* buffer shows, as a thunk → entries, so `g`
;; recomputes THAT view (the TODO list, a tags match) rather than always
;; falling back to the full agenda.
(define *org--agenda-view* #f)
(define (org--agenda-show thunk)
  (set! *org--agenda-view* thunk)
  (org--render-agenda (thunk)))
;; g in *Org Agenda* — rebuild the current view from the files.
(define-command (org-agenda-redo)
  (if *org--agenda-view* (org--agenda-show *org--agenda-view*) (org-agenda)))

;; C-c a — agenda: every dated/TODO headline across the agenda files.
(define-command (org-agenda)
  (org--agenda-show (lambda () (org--sort-entries (org--all-entries)))))
;; C-c t — just the open TODO items.
(define-command (org-todo-list)
  (org--agenda-show
   (lambda ()
     (org--sort-entries (filter (lambda (e) (and (not (equal? (nth e 4) ""))
                                                 (not (org--done-keyword? (nth e 4)))))
                                (org--all-entries))))))

(define (org-agenda-visit)
  (let ((t (alist-ref *org-agenda-targets* (buffer-line))))
    ;; open-file is async: jump once the file's buffer is current.
    (when t (open-file (car t)) (goto-when-visiting "" (car t) (cdr t) 0))))

;; C-c c — capture a TODO headline, appended to the capture file on disk.
(define *org-capture-target* #f)
(define (org-set-capture-file path) (set! *org-capture-target* path))
(define (org--capture-file)
  (or *org-capture-target*
      (let ((files (org--agenda-file-list)))
        (if (null? files) (path-join (buffer-directory) "inbox.org") (car files)))))
;; Capture templates (see Layer 7f for the engine). Defined here so org-capture,
;; below, can test it; populate from init.scm.
(define *org-capture-templates* '())
;; C-c c — capture. With no templates configured this is the simple "* TODO"
;; prompt; configure *org-capture-templates* (see Layer 7f) for the full flow.
(define-command (org-capture)
  (if (null? *org-capture-templates*)
      (read-string "Capture (TODO headline): " 'org--capture-finish)
      (org--capture-pick)))
(define (org--capture-finish text)
  (when (not (equal? (string-trim text) ""))
    (let ((path (org--capture-file))
          (entry (string-append "* TODO " (string-trim text))))
      (org--edit-file path
        (lambda (existing)
          (org--bookmark-last-stored path existing (org--append-sep existing) entry)
          (message (string-append "Captured to " (basename path)))
          (org--capture-appended existing entry))))))

;; Rewrite the Org file `path` with `edit`, a function from its text to the new
;; text. When a buffer visits `path` the change goes into that buffer (undoable,
;; left for the user to save): written to disk underneath it, the next save of
;; the buffer would silently drop it. Otherwise the file is rewritten
;; atomically. `edit` can run more than once — again on fresh text if the
;; buffer changes before the result lands — so its side effects (bookmark,
;; message) must be safe to repeat.
(define *org--file-edits* '())
(define (org--edit-file path edit)
  (set! *org--file-edits* (cons (cons path edit) (alist-remove *org--file-edits* path)))
  (edit-file-text path "org--file-edit-run"))
(define (org--file-edit-run path text)
  (let ((edit (alist-ref *org--file-edits* path)))
    (when edit (set-file-text path (edit text)))))

;; The newline that joins appended text to `existing`: none when it is empty or
;; already ends in one.
(define (org--append-sep existing)
  (if (or (equal? existing "") (ends-with? existing "\n")) "" "\n"))
(define (org--capture-appended existing text)
  (string-append existing (org--append-sep existing) text "\n"))

;; org-capture-last-stored: like Emacs's `org-capture` bookmarking the last
;; stored item — a bookmark named "org-capture-last-stored" lands on the
;; headline just inserted, so C-x r b jumps straight back to it.
(define (org--bookmark-last-stored path existing sep text)
  ;; The new text was appended after `existing` + `sep`; its line index is the
  ;; number of newlines BEFORE it (split-many of a file ending in \n yields a
  ;; trailing "", so len-1 is the newline count — and `sep` is part of the
  ;; prefix when `existing` doesn't already end in a newline).
  (bookmark-store "org-capture-last-stored" path
                  (+ (utf8-length existing) (utf8-length sep))
                  (- (length (split-many (string-append existing sep) "\n")) 1) 0
                  (bookmark-clip (string-trim text)) ""))

;; ===========================================================================
;; Layer 5a — table alignment
;; ===========================================================================
(define (org--table-bounds L)
  (let ((lines (org--lines)))
    (let ((n (length lines)))
      (if (or (>= L n) (not (org--table-line? (nth lines L))))
          #f
          (let ((s (let up ((i L)) (if (and (>= i 0) (org--table-line? (nth lines i))) (up (- i 1)) (+ i 1))))
                (e (let dn ((i L)) (if (and (< i n) (org--table-line? (nth lines i))) (dn (+ i 1)) (- i 1)))))
            (list s e))))))

(define (org--cell cells c) (if (< c (length cells)) (nth cells c) ""))
(define (org--max-cols parsed)
  (let loop ((ps parsed) (m 0))
    (cond ((null? ps) m)
          ((car (car ps)) (loop (cdr ps) m))
          (else (loop (cdr ps) (max m (length (cdr (car ps)))))))))
(define (org--col-width parsed c)
  (let loop ((ps parsed) (m 1))
    (cond ((null? ps) m)
          ((car (car ps)) (loop (cdr ps) m))
          (else (loop (cdr ps) (max m (string-length (org--cell (cdr (car ps)) c))))))))
(define (org--col-widths parsed ncols)
  (let loop ((c 0) (acc '()))
    (if (>= c ncols) (reverse acc) (loop (+ c 1) (cons (org--col-width parsed c) acc)))))
(define (org--pad-cells cells widths)
  (let loop ((ws widths) (c 0) (acc '()))
    (if (null? ws)
        (reverse acc)
        (loop (cdr ws) (+ c 1) (cons (org--pad (org--cell cells c) (car ws)) acc)))))
(define (org--data-row cells widths)
  (string-append "| " (string-join (org--pad-cells cells widths) " | ") " |"))
(define (org--sep-row widths)
  (string-append "|" (string-join (map (lambda (w) (string-repeat "-" (+ w 2))) widths) "+") "|"))
(define (org--format-table rows)
  (let* ((parsed (map (lambda (l) (cons (org--table-sep? l) (org--table-cells l))) rows))
         (ncols (org--max-cols parsed))
         (widths (org--col-widths parsed ncols)))
    (map (lambda (p) (if (car p) (org--sep-row widths) (org--data-row (cdr p) widths))) parsed)))

;; Re-align the table at point (also bound on C-c C-c inside a table).
(define-command (org-table-align)
  (let ((b (org--table-bounds (buffer-line))))
    (if (not b)
        (message "Not in a table")
        (org--replace-line-range (nth b 0) (nth b 1)
          (org--format-table (org--slice (org--lines) (nth b 0) (nth b 1)))))))

;; --- interactive table editing (cell nav, row/column ops) -------------------
;; Byte positions of every '|' (ASCII 124) in a row.
(define (org--pipe-positions line)
  (let loop ((chars (string->list line)) (byte 0) (acc '()))
    (cond ((null? chars) (reverse acc))
          ((= (char->integer (car chars)) 124) (loop (cdr chars) (+ byte 1) (cons byte acc)))
          (else (loop (cdr chars) (+ byte (char-bytes (car chars))) acc)))))

;; Which cell (0-based) the byte-column `col` falls in, on a table row.
(define (org--cell-index-at line col)
  (let ((m (length (filter (lambda (p) (< p col)) (org--pipe-positions line)))))
    (max 0 (- m 1))))

;; A string of n '|' — i.e. an empty row with n-1 cells (org--format-table pads it).
(define (org--pipes n) (string-repeat "|" n))

;; Column count of a table slice (its widest data row).
(define (org--table-ncols slice)
  (org--max-cols (map (lambda (l) (cons (org--table-sep? l) (org--table-cells l))) slice)))

;; Total bytes (incl. newlines) of rows[0..idx).
(define (org--rows-byte-prefix rows idx)
  (let loop ((rs rows) (i 0) (acc 0))
    (if (or (null? rs) (>= i idx)) acc
        (loop (cdr rs) (+ i 1) (+ acc (utf8-length (car rs)) 1)))))

;; Point column (byte offset within its line).
(define (org--point-col)
  (- (point) (org--line-start (buffer-line))))

;; Replace the table [start..end] with `formatted`, then put point at the start of
;; cell `col` on formatted row index `row` (cell content begins 2 bytes past the
;; row's `col`-th pipe: the "| " prefix). Lines before `start` are untouched, so
;; the start byte is stable across the replace.
(define (org--retab-goto start end formatted row col)
  (let* ((base (org--line-start start))
         (rowline (nth formatted row))
         (pipes (org--pipe-positions rowline))
         (cellpos (if (< col (length pipes)) (+ (nth pipes col) 2) (utf8-length rowline)))
         (target (+ base (org--rows-byte-prefix formatted row) cellpos)))
    (org--replace-line-range start end formatted)
    (goto-char target)))

;; Index of the next / previous non-separator row in `slice` from index `from`
;; (exclusive), or #f.
(define (org--next-data-row slice from)
  (let ((n (length slice)))
    (let loop ((i (+ from 1)))
      (cond ((>= i n) #f)
            ((org--table-sep? (nth slice i)) (loop (+ i 1)))
            (else i)))))
(define (org--prev-data-row slice from)
  (let loop ((i (- from 1)))
    (cond ((< i 0) #f)
          ((org--table-sep? (nth slice i)) (loop (- i 1)))
          (else i))))

;; Cells of `line` padded to exactly `ncols` (missing trailing cells become "").
(define (org--cells-pad line ncols)
  (let ((cells (org--table-cells line)))
    (let loop ((i 0) (acc '()))
      (if (>= i ncols) (reverse acc) (loop (+ i 1) (cons (org--cell cells i) acc))))))
(define (org--list-set lst i v)
  (let loop ((l lst) (k 0) (acc '()))
    (cond ((null? l) (reverse acc))
          ((= k i) (loop (cdr l) (+ k 1) (cons v acc)))
          (else (loop (cdr l) (+ k 1) (cons (car l) acc))))))
(define (org--list-swap lst i j)
  (org--list-set (org--list-set lst i (nth lst j)) j (nth lst i)))
(define (org--row-from-cells cells)
  (string-append "| " (string-join cells " | ") " |"))

;; TAB — re-align and move to the next cell (creating a new row past the last).
(define-command (org-table-next-cell)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (line (org--line-text L)) (sep? (org--table-sep? line))
             (col (if sep? -1 (org--cell-index-at line (org--point-col)))))
        (if (and (not sep?) (< col (- ncols 1)))
            (org--retab-goto start end (org--format-table slice) ridx (+ col 1))
            (let ((nxt (org--next-data-row slice ridx)))
              (if nxt
                  (org--retab-goto start end (org--format-table slice) nxt 0)
                  (let* ((slice2 (append slice (list (org--pipes (+ ncols 1)))))
                         (formatted2 (org--format-table slice2)))
                    (org--retab-goto start end formatted2 (- (length formatted2) 1) 0)))))))))

;; S-TAB — re-align and move to the previous cell.
(define-command (org-table-prev-cell)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (line (org--line-text L)) (sep? (org--table-sep? line))
             (col (if sep? ncols (org--cell-index-at line (org--point-col)))))
        (if (and (not sep?) (> col 0))
            (org--retab-goto start end (org--format-table slice) ridx (- col 1))
            (let ((prv (org--prev-data-row slice ridx)))
              (when prv
                (org--retab-goto start end (org--format-table slice) prv (- ncols 1)))))))))

;; RET — re-align and move to the first cell of the next row (creating one).
(define-command (org-table-next-row)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (nxt (org--next-data-row slice ridx)))
        (if nxt
            (org--retab-goto start end (org--format-table slice) nxt 0)
            (let* ((slice2 (append slice (list (org--pipes (+ ncols 1)))))
                   (formatted2 (org--format-table slice2)))
              (org--retab-goto start end formatted2 (- (length formatted2) 1) 0)))))))

;; Swap the current column with its neighbour (data rows only); keep point in it.
(define (org--table-move-column delta)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (col (org--cell-index-at (org--line-text L) (org--point-col)))
             (other (+ col delta)))
        (when (and (>= other 0) (< other ncols))
          (let ((swapped (map (lambda (l)
                                (if (org--table-sep? l) l
                                    (org--row-from-cells
                                     (org--list-swap (org--cells-pad l ncols) col other))))
                              slice)))
            (org--retab-goto start end (org--format-table swapped) ridx other)))))))
(define-command (org-table-move-column-right) (org--table-move-column 1))
(define-command (org-table-move-column-left)  (org--table-move-column -1))

;; Swap the current data row with the next / previous data row; keep point in it.
(define (org--table-move-row finder)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start)))
        (when (not (org--table-sep? (org--line-text L)))
          (let ((other (finder slice ridx)))
            (when other
              (let ((col (org--cell-index-at (org--line-text L) (org--point-col))))
                (org--retab-goto start end
                  (org--format-table (org--list-swap slice ridx other)) other col)))))))))
(define-command (org-table-move-row-down) (org--table-move-row org--next-data-row))
(define-command (org-table-move-row-up)   (org--table-move-row org--prev-data-row))

;; Insert / delete a column at point.
(define-command (org-table-insert-column)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (col (org--cell-index-at (org--line-text L) (org--point-col))))
        (let ((grown (map (lambda (l)
                            (if (org--table-sep? l) l
                                (let* ((cells (org--cells-pad l ncols))
                                       (head (org--slice cells 0 (- col 1)))
                                       (tail (org--slice cells col (- ncols 1))))
                                  (org--row-from-cells (append head (list "") tail)))))
                          slice)))
          (org--retab-goto start end (org--format-table grown) ridx col))))))
(define-command (org-table-delete-column)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (col (org--cell-index-at (org--line-text L) (org--point-col))))
        (when (> ncols 1)
          (let ((shrunk (map (lambda (l)
                               (if (org--table-sep? l) l
                                   (org--row-from-cells
                                    (filter (lambda (x) (not (eq? x 'drop)))
                                      (let loop ((cs (org--cells-pad l ncols)) (i 0) (acc '()))
                                        (if (null? cs) (reverse acc)
                                            (loop (cdr cs) (+ i 1)
                                                  (cons (if (= i col) 'drop (car cs)) acc))))))))
                             slice)))
            (org--retab-goto start end (org--format-table shrunk) ridx
                             (min col (- ncols 2)))))))))

;; Insert a new empty row below / delete the current row.
(define-command (org-table-insert-row)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (ncols (org--table-ncols slice))
             (before (org--slice slice 0 ridx))
             (after  (org--slice slice (+ ridx 1) (- (length slice) 1)))
             (rows (append before (list (org--pipes (+ ncols 1))) after)))
        (org--retab-goto start end (org--format-table rows) (+ ridx 1) 0)))))
(define-command (org-table-delete-row)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (when b
      (let* ((start (nth b 0)) (end (nth b 1))
             (slice (org--slice (org--lines) start end))
             (ridx (- L start))
             (before (org--slice slice 0 (- ridx 1)))
             (after  (org--slice slice (+ ridx 1) (- (length slice) 1)))
             (rows (append before after)))
        (if (null? rows)
            (org--replace-line-range start end (list ""))
            (org--retab-goto start end (org--format-table rows)
                             (min ridx (- (length rows) 1)) 0))))))

;; Context-sensitive dispatchers: table action when in a table, else the
;; heading / default action (mirrors Org's overloaded TAB / RET / M-arrows).
(define (org--in-table?) (org--table-line? (org--line-text (buffer-line))))
(define (org--on-heading?) (and (org--heading-level (org--line-text (buffer-line))) #t))
(define (org-tab)        (if (org--in-table?) (org-table-next-cell) (org-cycle)))
(define (org-shift-tab)  (if (org--in-table?) (org-table-prev-cell) (org-global-cycle)))

;; Whether RET at byte column `col` of table line `line` should insert a plain
;; newline instead of jumping to the next row: point at end of line (nothing
;; but whitespace after it) or still in the leading indentation. This is
;; org.el's `org-return` escape hatch — it's how you add a fresh line below a
;; row (type the `|` and go) or break out of a table, instead of being
;; trapped cycling through rows.
(define (org--return-escapes? line col)
  (or (>= (string-match "^[ \t]*$" (byte-drop line col)) 0)
      (>= (string-match "^[ \t]*$" (byte-take line col)) 0)))

(define (org-return)
  (if (and (org--in-table?)
           (not (org--return-escapes? (org--line-text (buffer-line)) (org--point-col))))
      (org-table-next-row)
      (newline)))
;; M-arrows act on the table column/row, else on the heading *at point* — never on
;; an enclosing heading from body text (Emacs leaves plain body alone).
(define (org-meta-right)
  (cond ((org--in-table?) (org-table-move-column-right))
        ((org--list-line-prefix (org--line-text (buffer-line))) (org-indent-item))
        ((org--on-heading?) (org-demote)) (else #f)))
(define (org-meta-left)
  (cond ((org--in-table?) (org-table-move-column-left))
        ((org--list-line-prefix (org--line-text (buffer-line))) (org-outdent-item))
        ((org--on-heading?) (org-promote)) (else #f)))
(define (org-meta-up)
  (cond ((org--in-table?) (org-table-move-row-up)) ((org--on-heading?) (org-move-subtree-up)) (else #f)))
(define (org-meta-down)
  (cond ((org--in-table?) (org-table-move-row-down)) ((org--on-heading?) (org-move-subtree-down)) (else #f)))

;; C-k — like Emacs `org-kill-line`. On a collapsed heading the visible "line"
;; spans the whole folded subtree, so kill from point through the subtree's end
;; (Emacs leaves the empty heading line, like plain kill-line). Everywhere else
;; this is the ordinary kill-line (kill to end of line).
(define-command (org-kill-line)
  (let* ((L (buffer-line)) (line (org--line-text L)) (lvl (org--heading-level line)))
    (if (and lvl (not (eq? (org--heading-state L) 'subtree)))
        (let* ((headings (org--headings)) (total (org--total-lines))
               (E (org--subtree-end L lvl headings total))
               (s (point))
               (e (+ (org--line-start E) (utf8-length (org--line-text E)))))
          (if (> e s) (kill-region-between s e) (kill-line)))
        (kill-line))))

;; ===========================================================================
;; Layer 5b — babel (execute source blocks)
;; ===========================================================================
;; (begin-line end-line lang) for the src block containing line L, or #f.
(define (org--src-block-bounds L)
  (let* ((lines (org--lines)) (n (length lines)))
    (if (>= L n)
        #f
        (let up ((i L))
          (cond ((< i 0) #f)
                ((and (< i L) (org--block-end? (nth lines i))) #f)
                ((>= (string-match "^[ \t]*#\\+[bB][eE][gG][iI][nN]_[sS][rR][cC]" (nth lines i)) 0)
                 (let dn ((j (+ i 1)))
                   (cond ((>= j n) #f)
                         ((org--block-end? (nth lines j))
                          (if (>= j L) (list i j (org--block-lang (nth lines i))) #f))
                         (else (dn (+ j 1))))))
                (else (up (- i 1))))))))

(define (org--run-src lang body)
  (let ((cmd (cond ((or (equal? lang "sh") (equal? lang "bash") (equal? lang "shell")) (list "bash"))
                   ((or (equal? lang "python") (equal? lang "python3")) (list "python3"))
                   ((equal? lang "ruby") (list "ruby"))
                   ((or (equal? lang "js") (equal? lang "javascript") (equal? lang "node")) (list "node"))
                   (else #f))))
    (cond ((or (equal? lang "scheme") (equal? lang "steel")) (org--run-scheme body))
          ((not cmd) (string-append "Unsupported src language: " lang))
          (else
           (let* ((res (call-process-full (car cmd) (cdr cmd) (string-append body "\n")))
                  (out (trim-end (nth res 1))) (err (trim-end (nth res 2))))
             (if (equal? out "") err out))))))

;; --- scheme blocks ----------------------------------------------------------
;; A `scheme` block runs in the editor's own Steel engine, like an elisp block
;; in Emacs: its defines persist and it can call any editor function. The value
;; of the last form is the result. A string is written as-is; a chart (a plot
;; from plot.scm — rows of (text spans)) is written as its text and keeps its
;; colors; anything else is printed.
(define (org--styled-rows? v)
  (and (list? v) (not (null? v))
       (all? (lambda (r) (and (list? r) (= (length r) 2)
                              (string? (car r)) (list? (cadr r))))
             v)))

(define (org--run-scheme body)
  (with-handler
    (lambda (err)
      (string-append "Error: " (if (error-object? err) (error-object-message err) (to-string err))))
    (let ((v (eval-string (string-append "(begin\n" body "\n)"))))
      (cond ((string? v) v)
            ((void? v) "")
            ((org--styled-rows? v) v)
            (else (to-string v))))))

;; Chart colors. Org repaints every face on each edit (`org--fontify` clears
;; them all), so a chart's colors can't just be overlaid once. Instead each
;; written chart row's text maps to its (start end face) spans here, and the
;; fontifier paints any `: ` results line whose text it knows. Keyed by text
;; with trailing blanks trimmed, so a save that strips trailing whitespace
;; keeps the colors. Lives for the session: reopening the file shows the chart
;; uncolored until its block is run again.
(define *org--chart-rows* (hash))
(define (org--remember-chart-rows rows)
  (for-each (lambda (r)
              (set! *org--chart-rows*
                    (hash-insert *org--chart-rows* (trim-end (car r)) (cadr r))))
            rows))

;; (prefix-bytes spans) when `line` is a results line holding a known chart
;; row, else #f.
(define (org--chart-line line)
  (let ((c (regexp-captures "^([ \t]*: )" line)))
    (and (not (null? c))
         (let* ((pre (utf8-length (nth c 1)))
                (spans (hash-try-get *org--chart-rows* (trim-end (byte-drop line pre)))))
           (and spans (list pre spans))))))

(define (org--fontify-chart-line lstart lend cl)
  (let ((pre (car cl)))
    (for-each (lambda (sp)
                (let ((s (+ lstart pre (car sp))) (e (min lend (+ lstart pre (cadr sp)))))
                  (when (< s e) (org--span s e (nth sp 2)))))
              (cadr cl))))

;; Existing #+RESULTS block right after `start` (skipping blanks): (s e) or #f.
(define (org--existing-results lines start n)
  (let skip ((i start))
    (cond ((>= i n) #f)
          ((equal? (string-trim (nth lines i)) "") (skip (+ i 1)))
          ((>= (string-match "^[ \t]*#\\+[rR][eE][sS][uU][lL][tT][sS]:" (nth lines i)) 0)
           (let body ((j (+ i 1)))
             (cond ((>= j n) (list i (- j 1)))
                   ((or (>= (string-match "^[ \t]*:" (nth lines j)) 0)
                        (>= (string-match "^[ \t]*\\|" (nth lines j)) 0))
                    (body (+ j 1)))
                   (else (list i (- j 1))))))
          (else #f))))

;; Write `out` as a #+RESULTS: block after line `el`, replacing the one
;; already there. `out` is a string or chart rows (see org--styled-rows?); each
;; result line is prefixed `: ` so Org keeps it verbatim.
(define (org--write-results el out)
  (let* ((lines (org--lines)) (n (length lines))
         (texts (if (string? out) (split-many out "\n") (map car out)))
         (res-lines (cons "#+RESULTS:" (map (lambda (l) (string-append ": " l)) texts)))
         (ex (org--existing-results lines (+ el 1) n))
         (rest-start (if ex (+ (nth ex 1) 1) (+ el 1)))
         (before (org--slice lines 0 el))
         (rest (org--slice lines rest-start (- n 1))))
    (unless (string? out) (org--remember-chart-rows out))
    ;; One blank line separates the results from what follows — but only
    ;; add it when `rest` doesn't already start with one, or every re-run
    ;; would stack another blank line (the old one survives in `rest`).
    (org--replace-all
     (join-lines (append before res-lines
                         (if (and (not (null? rest))
                                  (equal? (string-trim (car rest)) ""))
                             '()
                             (list ""))
                         rest)))))

;; C-c C-c inside a src block — run it, write a #+RESULTS block after it.
(define-command (org-babel-execute)
  (let ((L0 (buffer-line))
        (b (org--src-block-bounds (buffer-line))))
    (if (not b)
        (message "Not in a src block")
        (let* ((el (nth b 1)) (lang (nth b 2))
               (lines (org--lines))
               (body (org--expand-noweb (join-lines (org--slice lines (+ (nth b 0) 1) (- el 1))))))
          (org--write-results el (org--run-src lang body))
          (goto-line L0)
          (org--fontify-schedule)
          (message "Block executed")))))

;; ===========================================================================
;; Layer 6 — inline images + LaTeX preview
;; ===========================================================================
;; These display decoded bitmaps on rows reserved below their line, via the
;; `set-line-image` / `clear-line-images` primitives. The image and LaTeX
;; toggles share that one display layer, so turning one on clears the other.

;; --- inline images ----------------------------------------------------------
;; Absolute path of the local image a `[[file:…]]` / `[[…]]` link points at, or
;; #f. Like Org, only undescribed links to a recognised image type count.
;; Raster types the `image` crate decodes. (SVG is vector — not handled here, so
;; an .svg link stays a normal clickable link rather than silently blank.)
(define (org--image-path? p)
  (>= (string-match "(?i)\\.(png|jpe?g|gif|bmp|webp)$" p) 0))
(define (org--resolve-path p)
  (cond ((>= (string-match "^[/~]" p) 0) p)            ; absolute / home
        (else (path-join (buffer-directory) p))))
(define (org--image-link-path line)
  (let ((c (regexp-captures "\\[\\[(file:)?([^]]+)\\]\\]" line)))
    (if (and (not (null? c)) (org--image-path? (nth c 2)))
        (org--resolve-path (nth c 2))
        #f)))

;; Per-buffer "shown?" flags for the two displays (defined up front so either
;; show-fn can clear the other — they share the one line-image layer).
(define *org-images-shown* '())
(define *org-latex-shown* '())
(define (org--images-shown?) (if (buffer-state-get *org-images-shown* #f) #t #f))
(define (org--set-images-shown v)
  (set! *org-images-shown* (buffer-state-set *org-images-shown* v)))
(define (org--latex-shown?) (if (buffer-state-get *org-latex-shown* #f) #t #f))
(define (org--set-latex-shown v)
  (set! *org-latex-shown* (buffer-state-set *org-latex-shown* v)))

(define-command (org-display-inline-images)
  (clear-line-images)
  (org--set-latex-shown #f)
  (let loop ((lines (org--lines)) (i 0) (n 0))
    (if (null? lines)
        (begin (org--set-images-shown #t)
               (message (string-append "Displaying " (int->string n) " inline image(s)")))
        (let ((p (org--image-link-path (car lines))))
          (if p
              (begin (set-line-image i p) (loop (cdr lines) (+ i 1) (+ n 1)))
              (loop (cdr lines) (+ i 1) n))))))

(define-command (org-remove-inline-images)
  (clear-line-images) (org--set-images-shown #f) (message "Inline images hidden"))

;; C-c C-x C-v — toggle inline image display.
(define-command (org-toggle-inline-images)
  (if (org--images-shown?) (org-remove-inline-images) (org-display-inline-images)))

;; --- LaTeX preview ----------------------------------------------------------
;; Render `$…$` fragments to PNG with the system TeX toolchain (latex + dvipng)
;; and display them inline — the org-preview-latex model. Tunable from init.scm.
(define *org-latex-density* "150")               ; dvipng -D (dots per inch)
(define *org-latex-fg* "rgb 0.85 0.85 0.85")     ; light, for dark themes
(define (org--latex-tmpdir)
  (let ((t (getenv "TMPDIR"))) (path-join (if (equal? t "") "/tmp" t) "macros-org-latex")))

;; Inner math of the first `$…$` on a line, or #f.
(define (org--latex-fragment line)
  (let ((c (regexp-captures "\\$([^$\n]+)\\$" line))) (if (null? c) #f (nth c 1))))

;; Compile one fragment → png path (or #f). Writes a standalone doc, runs latex
;; then dvipng in a temp dir; both shell out via call-process(-full).
(define (org--latex->png inner idx)
  (let* ((dir (org--latex-tmpdir))
         (base (string-append "frag-" (int->string idx)))
         (tex (path-join dir (string-append base ".tex")))
         (dvi (path-join dir (string-append base ".dvi")))
         (png (path-join dir (string-append base ".png")))
         (doc (string-append
                "\\documentclass[preview,border=2pt]{standalone}\n"
                "\\usepackage{amsmath,amssymb,amsfonts}\n"
                "\\begin{document}\n\\( " inner " \\)\n\\end{document}\n")))
    ;; A previous run's output must not stand in for this one: when latex fails
    ;; on an edited fragment, the stale .dvi/.png would show the old formula.
    (delete-file dvi)
    (delete-file png)
    (write-file tex doc)
    (call-process-full "latex"
      (list "-interaction=nonstopmode" "-halt-on-error" "-output-directory" dir tex) "")
    (if (not (file-exists? dvi))
        #f
        (begin
          (call-process-full "dvipng"
            (list "-D" *org-latex-density* "-T" "tight" "-bg" "Transparent"
                  "-fg" *org-latex-fg* "-o" png dvi) "")
          (if (file-exists? png) png #f)))))

(define (org-show-latex-previews)
  (clear-line-images)
  (org--set-images-shown #f)
  (call-process "mkdir" (list "-p" (org--latex-tmpdir)))
  (let loop ((lines (org--lines)) (i 0) (idx 0) (n 0))
    (if (null? lines)
        (begin (org--set-latex-shown #t)
               (message (if (= n 0)
                            "No $…$ fragments (or latex/dvipng not found)"
                            (string-append "Rendered " (int->string n) " LaTeX fragment(s)"))))
        (let ((frag (org--latex-fragment (car lines))))
          (if frag
              (let ((png (org--latex->png frag idx)))
                (when png (set-line-image i png))
                (loop (cdr lines) (+ i 1) (+ idx 1) (if png (+ n 1) n)))
              (loop (cdr lines) (+ i 1) idx n))))))

(define (org-remove-latex-previews)
  (clear-line-images) (org--set-latex-shown #f) (message "LaTeX previews hidden"))

;; C-c C-x C-l — toggle LaTeX fragment previews.
(define-command (org-toggle-latex-preview)
  (if (org--latex-shown?) (org-remove-latex-previews) (org-show-latex-previews)))

;; ===========================================================================
;; C-c C-c dispatcher (context-sensitive, like Org's)
;; ===========================================================================
(define (org-ctrl-c-ctrl-c)
  (let* ((L (buffer-line)) (line (org--line-text L)))
    (cond ((org--src-block-bounds L) (org-babel-execute))
          ((org--table-line? line) (org-table-recalculate))
          ((>= (string-match "src_[A-Za-z0-9]+(\\[[^]]*\\])?\\{" line) 0) (org-babel-execute-inline))
          ((org--heading-level line) (org-set-tags))
          ((org-toggle-checkbox) #t)
          (else (message "C-c C-c: nothing to do here")))))

;; Flip leading-star hiding for the session and repaint.
(define-command (org-toggle-leading-stars)
  (set! *org-hide-leading-stars* (not *org-hide-leading-stars*))
  (org--fontify)
  (message (if *org-hide-leading-stars*
               "Leading stars hidden"
               "Leading stars shown")))

;; Flip virtual indentation for the session and repaint.
(define-command (org-toggle-indent)
  (set! *org-indent* (not *org-indent*))
  (org--fontify)
  (message (if *org-indent* "Org indent on" "Org indent off")))

;; ===========================================================================
;; ===========================================================================
;; Layer 7 — calendar / date arithmetic (pure)
;; ===========================================================================
;; The Gregorian day-count substrate (ymd->days / days->ymd / pad2) lives in
;; core.scm; this layer adds the Org-facing date parsing, formatting, and
;; relative-offset entry for scheduling, repeaters, and clocking.
(define *org-dow-names* (list "Sun" "Mon" "Tue" "Wed" "Thu" "Fri" "Sat"))
;; 1970-01-01 (day 0) is a Thursday (index 4), so (days+4) mod 7 is the weekday.
(define (org--dow-name days) (nth *org-dow-names* (modulo (+ days 4) 7)))

;; "YYYY-MM-DD" (anywhere in `s`) → (y m d), or #f.
;; (The ISO parse/format itself — parse-iso-date, iso-date->days,
;; days->iso-date — is in core.scm, shared with org-roam.)
(define org--parse-date parse-iso-date)
(define org--date-str->days iso-date->days)
;; "YYYY-MM-DD Dow", the Org timestamp body.
(define (org--days->date-str days)
  (string-append (days->iso-date days) " " (org--dow-name days)))
(define (org--today-days) (org--date-str->days (current-time "%Y-%m-%d")))

(define (org--leap? y) (and (= 0 (modulo y 4)) (or (not (= 0 (modulo y 100))) (= 0 (modulo y 400)))))
(define (org--month-days y m)
  (cond ((or (= m 1) (= m 3) (= m 5) (= m 7) (= m 8) (= m 10) (= m 12)) 31)
        ((= m 2) (if (org--leap? y) 29 28))
        (else 30)))
;; Add `n` calendar months to (y m d), clamping the day to the target month.
(define (org--add-months y m d n)
  (let* ((total (+ (* y 12) (- m 1) n))
         (ny (quotient total 12))
         (nm (+ 1 (modulo total 12))))
    (list ny nm (min d (org--month-days ny nm)))))

;; Apply a "+N", "+Nd/w/m/y" (or negative) offset to a base day count.
(define (org--apply-offset s base)
  (let* ((c (regexp-captures "^([+-])([0-9]+)([dwmy]?)$" s)))
    (if (null? c) base
        (let* ((sign (if (equal? (nth c 1) "-") -1 1))
               (num (* sign (string->number (nth c 2))))
               (unit (nth c 3)))
          (cond ((or (equal? unit "") (equal? unit "d")) (+ base num))
                ((equal? unit "w") (+ base (* 7 num)))
                ((equal? unit "m")
                 (let* ((ymd (days->ymd base))
                        (r (org--add-months (nth ymd 0) (nth ymd 1) (nth ymd 2) num)))
                   (ymd->days (nth r 0) (nth r 1) (nth r 2))))
                ((equal? unit "y")
                 (let* ((ymd (days->ymd base))
                        (r (org--add-months (nth ymd 0) (nth ymd 1) (nth ymd 2) (* 12 num))))
                   (ymd->days (nth r 0) (nth r 1) (nth r 2))))
                (else base))))))

;; Interpret a user date string against `base` (day count): "", ".", "today",
;; "tomorrow"/"yesterday", an absolute YYYY-MM-DD, or a relative ±N[dwmy].
(define (org--read-date input base)
  (let ((s (string-trim input)))
    (cond ((or (equal? s "") (equal? s ".") (equal? (string-downcase s) "today")) base)
          ((equal? (string-downcase s) "tomorrow") (+ base 1))
          ((equal? (string-downcase s) "yesterday") (- base 1))
          ((org--parse-date s) (org--date-str->days s))
          ((>= (string-match "^[+-][0-9]+[dwmy]?$" s) 0) (org--apply-offset s base))
          (else base))))

;; ===========================================================================
;; Layer 7b — properties drawers + the entry property API
;; ===========================================================================
(define (org--planning-line? line)
  (>= (string-match "^[ \t]*(SCHEDULED|DEADLINE|CLOSED):" line) 0))

;; (start . end) line indices of the :PROPERTIES: … :END: drawer belonging to
;; heading H (it may follow a planning line), or #f.
(define (org--property-drawer-bounds H)
  (let* ((lines (org--lines)) (n (length lines)))
    (let loop ((i (+ H 1)))
      (cond ((>= i n) #f)
            ((org--heading-level (nth lines i)) #f)
            ((>= (string-match "^[ \t]*:PROPERTIES:[ \t]*$" (nth lines i)) 0)
             (let end ((j (+ i 1)))
               (cond ((>= j n) #f)
                     ((>= (string-match "^[ \t]*:END:[ \t]*$" (nth lines j)) 0) (cons i j))
                     ((org--heading-level (nth lines j)) #f)
                     (else (end (+ j 1))))))
            ((org--planning-line? (nth lines i)) (loop (+ i 1)))
            (else #f)))))

;; Parse the property lines between drawer bounds [s,e] → alist (KEY . VALUE).
(define (org--drawer-props lines s e)
  (let loop ((i (+ s 1)) (acc '()))
    (if (>= i e) (reverse acc)
        (let ((c (regexp-captures "^[ \t]*:([A-Za-z0-9_@%-]+):[ \t]*(.*)$" (nth lines i))))
          (if (null? c) (loop (+ i 1) acc)
              (loop (+ i 1) (cons (cons (nth c 1) (string-trim (nth c 2))) acc)))))))

;; Case-insensitive alist lookup → the (key . value) pair, or #f.
(define (org--assoc-ci alist key)
  (let ((k (string-upcase key)))
    (let loop ((a alist))
      (cond ((null? a) #f)
            ((equal? (string-upcase (car (car a))) k) (car a))
            (else (loop (cdr a)))))))

;; The nearest ancestor heading line of H (level strictly shallower), or #f.
(define (org--parent-heading H)
  (let ((entry (org--find-heading (org--headings) H)))
    (if (not entry) #f
        (let ((lvl (nth entry 1)))
          (let loop ((hs (org--headings)) (parent #f))
            (cond ((null? hs) parent)
                  ((>= (car (car hs)) H) parent)
                  ((< (nth (car hs) 1) lvl) (loop (cdr hs) (car (car hs))))
                  (else (loop (cdr hs) parent))))))))

;; org-entry-get: the value of property KEY on heading H, or #f.
(define (org--entry-get-local H key)
  (let ((b (org--property-drawer-bounds H)))
    (if (not b) #f
        (let ((p (org--assoc-ci (org--drawer-props (org--lines) (car b) (cdr b)) key)))
          (if p (cdr p) #f)))))
(define (org-entry-get H key) (org--entry-get-local H key))

;; Like org-entry-get with inheritance: search H and its ancestors.
(define (org-entry-get-inherited H key)
  (let loop ((h H))
    (if (not h) #f
        (let ((v (org--entry-get-local h key)))
          (if v v (loop (org--parent-heading h)))))))

(define (org--render-drawer props)
  (append (list ":PROPERTIES:")
          (map (lambda (p) (string-append ":" (car p) ": " (cdr p))) props)
          (list ":END:")))

;; Line after which a new drawer is inserted: the heading, or its planning line.
(define (org--planning-end H)
  (let ((lines (org--lines)))
    (if (and (< (+ H 1) (length lines)) (org--planning-line? (nth lines (+ H 1)))) (+ H 1) H)))

(define (org--insert-lines-after idx new-lines)
  (let ((pos (+ (org--line-start idx) (utf8-length (org--line-text idx)))))
    (org--edit-keeping-point pos pos "" (string-append "\n" (join-lines new-lines)))))

;; org-entry-put: set property KEY=VALUE on heading H (creating the drawer).
(define (org-entry-put H key value)
  (let* ((lines (org--lines)) (b (org--property-drawer-bounds H)))
    (if b
        (let* ((s (car b)) (e (cdr b))
               (props (org--drawer-props lines s e))
               (existing (org--assoc-ci props key))
               (newprops (if existing
                             (map (lambda (p)
                                    (if (equal? (string-upcase (car p)) (string-upcase key))
                                        (cons (car p) value) p))
                                  props)
                             (append props (list (cons key value))))))
          (org--replace-line-range s e (org--render-drawer newprops)))
        (org--insert-lines-after (org--planning-end H)
                                 (org--render-drawer (list (cons key value)))))))

;; C-c C-x p — set a property interactively: prompt for the key, then the value
;; (prefilled with the current one). Both prompts capture their operands in the
;; callback closures.
(define-command (org-set-property)
  (let ((H (org--heading-line-at-point)))
    (if (not H) (message "Not on a heading")
        (read-string "Property: "
          (lambda (key)
            (when (not (equal? (string-trim key) ""))
              (let ((k (string-trim key)))
                (read-string (string-append k ": ")
                             (lambda (val)
                               (org-entry-put H k (string-trim val))
                               (message (string-append "Set " k)))
                             (or (org--entry-get-local H k) "")))))))))

;; org-id-get-create — return the heading's :ID:, minting and storing one if absent.
(define-command (org-id-get-create)
  (let ((H (org--heading-line-at-point)))
    (if (not H) (begin (message "Not on a heading") #f)
        (let ((existing (org--entry-get-local H "ID")))
          (if existing existing
              (let ((id (org-id-new))) (org-entry-put H "ID" id) id))))))

;; ===========================================================================
;; Layer 7c — scheduling / deadlines / timestamps (+ repeaters)
;; ===========================================================================
;; The planning line right below heading H (SCHEDULED/DEADLINE/CLOSED), or #f.
(define (org--planning-line-index H)
  (let ((lines (org--lines)))
    (if (and (< (+ H 1) (length lines)) (org--planning-line? (nth lines (+ H 1)))) (+ H 1) #f)))

;; The "<…>"/"[…]" stamp for KIND on a planning line, or #f.
(define (org--planning-get line kind)
  (let ((c (regexp-captures (string-append kind ":[ \t]*([<\\[][^]>\n]*[>\\]])") line)))
    (if (null? c) #f (nth c 1))))

(define (org--render-planning sched dead closed)
  (string-trim (string-append
              (if closed (string-append "CLOSED: " closed " ") "")
              (if dead (string-append "DEADLINE: " dead " ") "")
              (if sched (string-append "SCHEDULED: " sched " ") ""))))

(define-command (org-schedule) (org--plan-read "SCHEDULED"))
(define-command (org-deadline) (org--plan-read "DEADLINE"))
(define (org--plan-read kind)
  (let ((H (org--heading-line-at-point)))
    (if (not H) (message "Not on a heading")
        (read-string (string-append kind " (date / +Nd / today): ")
                     (lambda (input) (org--plan-apply H kind input))))))
(define (org--plan-apply H kind input)
  (let* ((stamp (string-append "<" (org--days->date-str (org--read-date input (org--today-days))) ">"))
         (pidx (org--planning-line-index H))
         (old (if pidx (org--line-text pidx) ""))
         (sched (if (equal? kind "SCHEDULED") stamp (org--planning-get old "SCHEDULED")))
         (dead (if (equal? kind "DEADLINE") stamp (org--planning-get old "DEADLINE")))
         (closed (org--planning-get old "CLOSED"))
         (newline (string-append "  " (org--render-planning sched dead closed))))
    (if pidx (org--replace-line pidx newline) (org--insert-lines-after H (list newline)))
    (message (string-append kind " set"))))

;; Insert an active/inactive timestamp at point.
(define-command (org-time-stamp)
  (insert (string-append "<" (org--days->date-str (org--today-days)) ">")))
(define-command (org-time-stamp-inactive)
  (insert (string-append "[" (org--days->date-str (org--today-days)) "]")))

;; --- repeaters --------------------------------------------------------------
;; A planning line carries a repeater (+N / ++N / .+N with a unit)?
(define (org--has-repeater? H)
  (let ((p (org--planning-line-index H)))
    (and p (>= (string-match "(\\.\\+|\\+\\+|\\+)[0-9]+[dwmy]" (org--line-text p)) 0))))

;; Shift one timestamp string forward per its repeater, preserving brackets and
;; the repeater cookie. `+N`/`++N` advance from the stamp's date (`++` until past
;; today); `.+N` advances from today.
(define (org--shift-stamp stamp)
  (let ((rc (regexp-captures "(\\.\\+|\\+\\+|\\+)([0-9]+)([dwmy])" stamp))
        (days (org--date-str->days stamp)))
    (if (or (null? rc) (not days)) stamp
        (let* ((mark (nth rc 1)) (num (string->number (nth rc 2))) (unit (nth rc 3))
               (offset (string-append "+" (number->string num) unit))
               (today (org--today-days))
               (newdays (cond ((equal? mark ".+") (org--apply-offset offset today))
                              ((equal? mark "++")
                               (let lp ((d (org--apply-offset offset days)))
                                 (if (> d today) d (lp (org--apply-offset offset d)))))
                              (else (org--apply-offset offset days)))))
          ;; Replace only the date and weekday: the time of day (`10:00`), the
          ;; repeater and any warning period (`-2d`) after it are kept as-is.
          (let* ((head (regexp-captures "^.[0-9]{4}-[0-9]{2}-[0-9]{2}( [^ >\\]0-9+.-]+)?" stamp))
                 (rest (if (null? head)
                           (string-append " " mark (number->string num) unit
                                          (substring stamp (- (string-length stamp) 1)
                                                     (string-length stamp)))
                           (substring stamp (string-length (car head)) (string-length stamp)))))
            (string-append (substring stamp 0 1) (org--days->date-str newdays) rest))))))

;; Shift every repeating active timestamp on a planning line.
(define (org--shift-planning line)
  (let loop ((s line) (acc ""))
    (let ((m (string-match "<[^>\n]*(\\.\\+|\\+\\+|\\+)[0-9]+[dwmy][^>\n]*>" s)))
      (if (< m 0) (string-append acc s)
          (let* ((c (regexp-captures "<[^>\n]*(\\.\\+|\\+\\+|\\+)[0-9]+[dwmy][^>\n]*>" s))
                 (whole (car c))
                 (before (byte-take s m))
                 (after (byte-drop s (+ m (utf8-length whole)))))
            (loop after (string-append acc before (org--shift-stamp whole))))))))

(define (org--repeat-task H)
  (let* ((p (org--planning-line-index H)) (line (org--line-text p)))
    (org--replace-line p (org--shift-planning line))
    (org--apply-keyword H (if (null? *org-todo-keywords*) "" (car *org-todo-keywords*)))
    (message "Repeating task shifted to next occurrence")))

;; ===========================================================================
;; Layer 7d — clocking (CLOCK lines in a LOGBOOK drawer) + clock table
;; ===========================================================================
;; Minutes since the epoch for an Org stamp "[YYYY-MM-DD … HH:MM]" (no time → 0).
(define (org--stamp-minutes stamp)
  (let ((days (org--date-str->days stamp))
        (tc (regexp-captures "([0-9]{2}):([0-9]{2})" stamp)))
    (if (not days) #f
        (+ (* days 1440)
           (if (null? tc) 0 (+ (* 60 (string->number (nth tc 1))) (string->number (nth tc 2))))))))
(define (org--fmt-duration mins)
  (string-append (number->string (quotient mins 60)) ":" (pad2 (modulo mins 60))))
(define (org--now-stamp) (current-time "%Y-%m-%d %a %H:%M"))

;; Last content line of H's own section (before its first child / next heading).
(define (org--entry-content-end H)
  (let ((lines (org--lines)) (n (org--total-lines)))
    (let loop ((i (+ H 1)))
      (cond ((>= i n) (- n 1))
            ((org--heading-level (nth lines i)) (- i 1))
            (else (loop (+ i 1)))))))

;; (start . end) line indices of the :NAME: … :END: drawer in H's section, or #f.
(define (org--named-drawer-bounds H name)
  (let ((lines (org--lines)) (e (org--entry-content-end H))
        (pat (string-append "^[ \t]*:" name ":[ \t]*$")))
    (let loop ((i (+ H 1)))
      (cond ((> i e) #f)
            ((>= (string-match pat (nth lines i)) 0)
             (let end ((j (+ i 1)))
               (cond ((> j e) #f)
                     ((>= (string-match "^[ \t]*:END:[ \t]*$" (nth lines j)) 0) (cons i j))
                     (else (end (+ j 1))))))
            (else (loop (+ i 1)))))))

;; Prepend a LOGBOOK entry to H (creating the drawer below any PROPERTIES/planning).
(define (org--logbook-add H entry)
  (let ((b (org--named-drawer-bounds H "LOGBOOK")))
    (if b
        (org--insert-lines-after (car b) (list (string-append "  " entry)))
        (let* ((pb (org--property-drawer-bounds H))
               (after (cond (pb (cdr pb))
                            ((org--planning-line-index H) (org--planning-line-index H))
                            (else H))))
          (org--insert-lines-after after (list "  :LOGBOOK:" (string-append "  " entry) "  :END:"))))))

;; The line index of an open clock (CLOCK: [start] with no end) anywhere, or #f.
(define (org--open-clock-line)
  (let ((lines (org--lines)) (n (org--total-lines)))
    (let loop ((i 0))
      (cond ((>= i n) #f)
            ((>= (string-match "^[ \t]*CLOCK:[ \t]*\\[[^]]*\\][ \t]*$" (nth lines i)) 0) i)
            (else (loop (+ i 1)))))))

;; C-c C-x C-i — start a clock on the heading at point.
(define-command (org-clock-in)
  (let ((H (org--heading-line-at-point)))
    (if (not H) (message "Not on a heading")
        (if (org--open-clock-line) (message "A clock is already running")
            (begin (org--logbook-add H (string-append "CLOCK: [" (org--now-stamp) "]"))
                   (message "Clock started"))))))

;; C-c C-x C-o — stop the running clock, recording the elapsed time.
(define-command (org-clock-out)
  (let ((i (org--open-clock-line)))
    (if (not i) (message "No running clock")
        (let* ((line (org--line-text i))
               (start (nth (regexp-captures "\\[([^]]*)\\]" line) 1))
               (now (org--now-stamp))
               (mins (max 0 (- (org--stamp-minutes (string-append "[" now "]"))
                               (org--stamp-minutes (string-append "[" start "]"))))))
          (org--replace-line i (string-append "  CLOCK: [" start "]--[" now "] =>  "
                                              (org--fmt-duration mins)))
          (message (string-append "Clocked " (org--fmt-duration mins)))))))

(define (org--delete-lines a b)
  (let* ((n (org--total-lines))
         (sb (org--line-start a))
         (be (+ (org--line-start b) (utf8-length (org--line-text b)))))
    (cond ((< (+ b 1) n) (replace-region sb (+ be 1) ""))
          ((> a 0) (replace-region (- sb 1) be ""))
          (else (replace-region sb be "")))))

;; C-c C-x C-x — discard the running clock.
(define-command (org-clock-cancel)
  (let ((i (org--open-clock-line)))
    (if (not i) (message "No running clock")
        (begin (org--delete-lines i i) (message "Clock canceled")))))

;; --- clock table ------------------------------------------------------------
(define (org--parse-clock-mins line)
  (let ((c (regexp-captures "=>[ \t]*([0-9]+):([0-9]{2})" line)))
    (if (null? c) #f (+ (* 60 (string->number (nth c 1))) (string->number (nth c 2))))))
(define (org--acc-add acc key m)
  (if (alist-ref acc key)
      (map (lambda (e) (if (equal? (car e) key) (cons (car e) (+ (cdr e) m)) e)) acc)
      (append acc (list (cons key m)))))

;; (total . ((title . minutes) …)) of all clocked time, attributed to the nearest
;; enclosing heading.
(define (org--clock-table-rows)
  (let loop ((ls (org--lines)) (cur "(before first heading)") (acc '()) (total 0))
    (if (null? ls)
        (cons total acc)
        (let* ((line (car ls)) (lvl (org--heading-level line)))
          (if lvl
              (loop (cdr ls) (nth (org--parse-headline line) 3) acc total)
              (let ((m (org--parse-clock-mins line)))
                (if m (loop (cdr ls) cur (org--acc-add acc cur m) (+ total m))
                    (loop (cdr ls) cur acc total))))))))

(define (org--clocktable-lines)
  (let* ((data (org--clock-table-rows)) (total (car data)) (rows (cdr data)))
    (org--format-table
     (append
      (list "| Headline | Time |" "|-+-|"
            (string-append "| *Total time* | *" (org--fmt-duration total) "* |"))
      (map (lambda (e) (string-append "| " (car e) " | " (org--fmt-duration (cdr e)) " |")) rows)))))

;; Bounds of a #+BEGIN: clocktable … #+END: block enclosing line L, or #f.
(define (org--clocktable-bounds L)
  (let ((lines (org--lines)) (n (org--total-lines)))
    (let up ((i (min L (- n 1))))
      (cond ((< i 0) #f)
            ((>= (string-match "^[ \t]*#\\+BEGIN:[ \t]+clocktable" (nth lines i)) 0)
             (let dn ((j (+ i 1)))
               (cond ((>= j n) #f)
                     ((>= (string-match "^[ \t]*#\\+END:" (nth lines j)) 0) (cons i j))
                     (else (dn (+ j 1))))))
            ((and (< i L) (>= (string-match "^[ \t]*#\\+END:" (nth lines i)) 0)) #f)
            (else (up (- i 1)))))))

;; C-c C-x C-r — insert or refresh a clock report (dynamic clocktable block).
(define-command (org-clock-report)
  (let* ((L (buffer-line)) (b (org--clocktable-bounds L))
         (block (append (list "#+BEGIN: clocktable :maxlevel 3")
                        (org--clocktable-lines) (list "#+END:"))))
    (if b (org--replace-line-range (car b) (cdr b) block)
        (org--insert-lines-after L block))
    (message "Clock report updated")))

;; ===========================================================================
;; Layer 7e — refile + archive
;; ===========================================================================
(define (org--current-file-path)
  (let ((f (buffer-file-name)))
    (if (or (not (string? f)) (equal? f ""))
        (path-join (buffer-directory) (current-buffer))
        f)))

;; (root-level . subtree-lines) for the subtree rooted at heading H.
(define (org--subtree-text H)
  (let* ((headings (org--headings)) (total (org--total-lines))
         (lvl (nth (org--find-heading headings H) 1))
         (E (org--subtree-end H lvl headings total)))
    (cons lvl (org--slice (org--lines) H E))))

;; Shift the depth of every heading line in `lines` by `delta` (clamped ≥ 1 star).
(define (org--shift-levels lines delta)
  (map (lambda (l)
         (let ((lvl (org--heading-level l)))
           (cond ((not lvl) l)
                 ((> delta 0) (string-append (string-repeat "*" delta) l))
                 ((< delta 0) (substring l (min (- 0 delta) (- lvl 1)) (string-length l)))
                 (else l))))
       lines))

(define (org--delete-subtree H)
  (let* ((headings (org--headings)) (total (org--total-lines))
         (lvl (nth (org--find-heading headings H) 1))
         (E (org--subtree-end H lvl headings total)))
    (org--delete-lines H E)))

;; Insert `block` as the last child of the heading at `tline` (level `tlvl`) in
;; the on-disk file `path`. #t once written. Reads strictly: a file that exists
;; but can't be read as text raises rather than being rewritten from "".
(define (org--insert-into-file path tline tlvl block)
  (let* ((lines (split-many (read-file-strict path) "\n")) (n (length lines))
         (E (org--subtree-end-in lines tline tlvl))
         (before (org--slice lines 0 E))
         (after (org--slice lines (+ E 1) (- n 1))))
    (write-file path (join-lines (append before block after)))))

;; Reorder within the current buffer: move the subtree at H under heading tline.
(define (org--inbuffer-refile H tline tlvl)
  (let* ((lines (org--lines)) (total (length lines)) (headings (org--headings))
         (slvl (nth (org--find-heading headings H) 1))
         (E (org--subtree-end H slvl headings total))
         (block (org--shift-levels (org--slice lines H E) (- (+ tlvl 1) slvl)))
         (Et (org--subtree-end tline tlvl headings total))
         (without (append (org--slice lines 0 (- H 1)) (org--slice lines (+ E 1) (- total 1))))
         (blocklen (+ (- E H) 1))
         ;; The target's end sits after the moved block (a later heading, or an
         ;; ANCESTOR whose subtree contains it): removing the block shifts it up.
         (insert-idx (if (>= Et E) (- (+ Et 1) blocklen) (+ Et 1)))
         (newlines (append (org--slice without 0 (- insert-idx 1)) block
                           (org--slice without insert-idx (- (length without) 1)))))
    (org--replace-all (join-lines newlines))))

;; Candidate "Title  (file)" → (path line level), rebuilt on each refile.
(define *org-refile-map* '())
(define *org--refile-source* -1)
(define (org--refile-file-targets path)
  ;; Live lines for the current file: an unsaved edit above the target would
  ;; otherwise point the refile at the wrong heading.
  (let ((lines (org--file-lines path)) (bn (basename path)))
    (let loop ((ls lines) (i 0) (acc '()))
      (if (null? ls) (reverse acc)
          (let ((lvl (org--heading-level (car ls))))
            (if lvl
                (let ((cand (string-append (nth (org--parse-headline (car ls)) 3)
                                           "  (" bn ")")))
                  (set! *org-refile-map* (cons (cons cand (list path i lvl)) *org-refile-map*))
                  (loop (cdr ls) (+ i 1) (cons cand acc)))
                (loop (cdr ls) (+ i 1) acc)))))))
(define (org--collect-refile-candidates)
  (set! *org-refile-map* '())
  (let loop ((files (org--agenda-file-list)) (acc '()))
    (if (null? files) acc (loop (cdr files) (append acc (org--refile-file-targets (car files)))))))

;; C-c C-w — refile the subtree at point under a chosen heading.
(define-command (org-refile)
  (let ((H (org--heading-line-at-point)))
    (if (not H) (message "Not on a heading")
        (begin (set! *org--refile-source* H)
               (completing-read "Refile to: " (org--collect-refile-candidates)
                                'org--refile-finish)))))
(define (org--refile-finish cand)
  (let ((tgt (alist-ref *org-refile-map* cand)))
    (if (not tgt) (message "No such refile target")
        (let* ((H *org--refile-source*)
               (tpath (nth tgt 0)) (tline (nth tgt 1)) (tlvl (nth tgt 2)))
          (cond
            ;; Same file however it's spelled (`~`, relative, symlinked).
            ((same-file? tpath (org--current-file-path))
             (let ((E (org--subtree-end H (nth (org--find-heading (org--headings) H) 1)
                                        (org--headings) (length (org--lines)))))
               (if (and (>= tline H) (<= tline E))
                   (message "Can't refile a subtree under itself")
                   (begin (org--inbuffer-refile H tline tlvl)
                          (message (string-append "Refiled to " (basename tpath)))))))
            ;; Writing to disk under a buffer that visits the file would be lost
            ;; (or clobber it) when that buffer is saved.
            ((find-buffer-visiting tpath)
             (message (string-append (basename tpath)
                                     " is open in a buffer: save and kill it, then refile")))
            (else
             (let* ((st (org--subtree-text H))
                    (block (org--shift-levels (cdr st) (- (+ tlvl 1) (car st)))))
               ;; Only drop the subtree once it is safely in the target file.
               (if (org--insert-into-file tpath tline tlvl block)
                   (begin (org--delete-subtree H)
                          (message (string-append "Refiled to " (basename tpath))))
                   (message (string-append "Refile failed: could not write " tpath))))))))))

;; Add the property line `prop` (":KEY: value") to the heading that heads
;; `block` (a subtree's lines): into its :PROPERTIES: drawer when it has one,
;; else in a new drawer. Either way the drawer sits after the planning line —
;; SCHEDULED/DEADLINE must stay directly under the heading to count.
(define (org--block-add-property block prop)
  (let* ((head (car block)) (rest (cdr block))
         (plan (if (and (pair? rest) (org--planning-line? (car rest))) (list (car rest)) '()))
         (body (if (null? plan) rest (cdr rest))))
    (if (and (pair? body) (>= (string-match "^[ \t]*:PROPERTIES:[ \t]*$" (car body)) 0))
        (let loop ((ls (cdr body)) (acc (list (car body))))
          (cond ((null? ls) (append (list head) plan (reverse acc) (list (string-append "  " prop))))
                ((>= (string-match "^[ \t]*:END:[ \t]*$" (car ls)) 0)
                 (append (list head) plan (reverse acc) (list (string-append "  " prop)) ls))
                (else (loop (cdr ls) (cons (car ls) acc)))))
        (append (list head) plan (list "  :PROPERTIES:" (string-append "  " prop) "  :END:") body))))

;; C-c C-x C-a — archive the subtree to <file>_archive.
(define (org--archive-file-path) (string-append (org--current-file-path) "_archive"))
(define-command (org-archive-subtree)
  (let ((H (org--heading-line-at-point)))
    (cond
      ((not H) (message "Not on a heading"))
      ;; Writing to disk under a buffer that visits the archive would be lost
      ;; (or clobber it) when that buffer is saved — as for refile.
      ((find-buffer-visiting (org--archive-file-path))
       (message (string-append (basename (org--archive-file-path))
                               " is open in a buffer: save and kill it, then archive")))
      (else
        (let* ((block (cdr (org--subtree-text H)))
               (with-time (org--block-add-property
                           block (string-append ":ARCHIVE_TIME: [" (org--now-stamp) "]")))
               (apath (org--archive-file-path))
               (existing (read-file-strict apath))
               (sep (if (or (equal? existing "") (ends-with? existing "\n")) "" "\n")))
          ;; Only drop the subtree once it is safely in the archive file.
          (if (write-file apath (string-append existing sep (join-lines with-time) "\n"))
              (begin
                (org--delete-subtree H)
                (message (string-append "Archived to " (basename apath))))
              (message (string-append "Archive failed: could not write " apath))))))))

;; ===========================================================================
;; Layer 7f — capture templates
;; ===========================================================================
;; Each template is (KEY DESCRIPTION TARGET TEMPLATE-STRING) where TARGET is
;;   (list "file" PATH)               → append to the file, or
;;   (list "file+headline" PATH HEAD) → append under heading HEAD in PATH.
;; The template string may contain %? (point / prompt), %t %T (active date /
;; datetime), %u %U (inactive date / datetime). Example, from init.scm:
;;   (set! *org-capture-templates*
;;     (list (list "t" "Todo" (list "file" "~/org/inbox.org") "* TODO %?\n  %U")
;;           (list "n" "Note" (list "file+headline" "~/org/notes.org" "Notes")
;;                 "* %?\n  %U")))
;; (*org-capture-templates* itself is defined back in Layer 4 so org-capture can
;; see it.)
(define (org--capture-template-by-key key)
  (let loop ((es *org-capture-templates*))
    (cond ((null? es) #f) ((equal? (nth (car es) 0) key) (car es)) (else (loop (cdr es))))))

;; Non-interactive placeholder expansion (everything but %?).
(define (org--expand-template s)
  (let* ((today (org--days->date-str (org--today-days)))
         (s (replace-regexp-in-string "%T" (string-append "<" (org--now-stamp) ">") s))
         (s (replace-regexp-in-string "%t" (string-append "<" today ">") s))
         (s (replace-regexp-in-string "%U" (string-append "[" (org--now-stamp) "]") s))
         (s (replace-regexp-in-string "%u" (string-append "[" today "]") s)))
    s))
;; `$` in a replace-regexp-in-string replacement is a group reference; double it
;; so user text (a captured "$5 lunch") is inserted literally.
(define (org--expand-template-final s input)
  (replace-regexp-in-string "%\\?" (replacement-literal input) s))

(define (org--capture-pick)
  (completing-read "Template: "
    (map (lambda (e) (string-append (nth e 0) " — " (nth e 1))) *org-capture-templates*)
    'org--capture-chosen))
(define (org--capture-chosen cand)
  (let* ((key (car (split-many cand " ")))
         (tmpl (org--capture-template-by-key key)))
    (if (not tmpl) (message "No such template")
        (let ((expanded (org--expand-template (nth tmpl 3))))
          (if (>= (string-match "%\\?" (nth tmpl 3)) 0)
              (read-string "Capture: "
                           (lambda (input)
                             (org--capture-commit tmpl (org--expand-template-final expanded input))))
              (org--capture-commit tmpl (org--expand-template-final expanded "")))))))

(define (org--capture-under-headline headline text lines)
  ;; File the captured `text` under HEADLINE in a file whose content is LINES
  ;; (already split; org--capture-commit also uses them for the last-stored
  ;; offset). Inserted after the headline's subtree end, as a child of it.
  ;; Returns (new-text . index of the FIRST inserted line in it), or #f when
  ;; the headline is missing (the caller then appends instead).
  (let* ((n (length lines))
         (hidx (let loop ((i 0))
                 (cond ((>= i n) #f)
                       ((let ((l (nth lines i)))
                          (and (org--heading-level l)
                               (equal? (nth (org--parse-headline l) 3) headline))) i)
                       (else (loop (+ i 1)))))))
    (if (not hidx)
        #f
        (let* ((hlvl (org--heading-level (nth lines hidx)))
               (E (org--subtree-end-in lines hidx hlvl))
               (before (org--slice lines 0 E))
               (after (org--slice lines (+ E 1) (- n 1))))
          ;; before is lines[0..E] INCLUSIVE, so the captured text starts one
          ;; line BELOW the subtree's last line — at E+1, not at the
          ;; headline's own index.
          (cons (join-lines (append before (org--as-child-lines text hlvl) after))
                (+ E 1))))))

;; The lines of captured `text` re-leveled so its top heading becomes a child of
;; a level-`plvl` heading (a "* %?" template filed under "** Inbox" lands as
;; "*** …", as in Emacs). Text without headings is left as is.
(define (org--as-child-lines text plvl)
  (let* ((lines (split-many text "\n"))
         (top (let loop ((ls lines) (m #f))
                (if (null? ls) m
                    (let ((l (org--heading-level (car ls))))
                      (loop (cdr ls) (if (and l (or (not m) (< l m))) l m)))))))
    (if top (org--shift-levels lines (- (+ plvl 1) top)) lines)))

(define (org--capture-commit tmpl text)
  (let* ((target (nth tmpl 2))
         (path (org--resolve-path (nth target 1)))
         (entry (string-trim text))
         (append-it
          (lambda (existing)
            (org--bookmark-last-stored path existing (org--append-sep existing) entry)
            (org--capture-appended existing text))))
    (org--edit-file path
      (if (equal? (car target) "file+headline")
          (lambda (existing)
            (let* ((lines (if (equal? existing "") '() (split-many existing "\n")))
                   (filed (org--capture-under-headline (nth target 2) text lines)))
              (message "Captured")
              (if filed
                  ;; The captured text was inserted after the headline's
                  ;; subtree: its first line is the line at index (cdr filed)
                  ;; in the new text (org--rows-byte-prefix gives that line's
                  ;; byte offset, counting the unmodified lines before it).
                  (begin
                    (bookmark-store "org-capture-last-stored" path
                                    (org--rows-byte-prefix lines (cdr filed)) (cdr filed) 0
                                    (bookmark-clip entry) "")
                    (car filed))
                  (append-it existing))))
          (lambda (existing)
            (message "Captured")
            (append-it existing))))))

;; ===========================================================================
;; Layer 7g — tag search + custom agenda views
;; ===========================================================================
;; Match a tag expression against a tag list. "a+b" = AND, "a|b" = OR, and a
;; "-" before a term negates it — as a separator too, like Emacs: "work-done"
;; is work AND NOT done. Combine with care: "|" splits first.
(define (org--tag-match expr tags)
  (cond
    ((>= (string-match "\\|" expr) 0)
     (let any ((as (split-many expr "|")))
       (cond ((null? as) #f) ((org--tag-match (string-trim (car as)) tags) #t) (else (any (cdr as))))))
    (else
     ;; Tag names never contain "-", so "a-b" → "a+-b" splits into ("a" "-b").
     (let all ((ts (remove-equal "" (map string-trim
                                         (split-many (replace-regexp-in-string "-" "+-" expr) "+")))))
       (cond ((null? ts) #t)
             (else (let* ((t (car ts)) (neg (>= (string-match "^-" t) 0))
                          (name (if neg (substring t 1 (string-length t)) t))
                          (present (and (member name tags) #t)))
                     (if (if neg (not present) present) (all (cdr ts)) #f))))))))

;; C-c \ — show every entry whose (inherited) tags match an expression.
(define-command (org-tags-view) (read-string "Match tags (work+urgent, a|b, -done): " 'org--tags-view-finish))
(define (org--tags-view-finish expr)
  (org--agenda-show
   (lambda ()
     (org--sort-entries
      (filter (lambda (e) (and (> (length e) 6) (org--tag-match (string-trim expr) (nth e 6))))
              (org--all-entries #t))))))

;; Custom agenda commands: each is (KEY DESCRIPTION TYPE ARG), TYPE one of
;; "agenda", "todo" (ARG = keyword), "tags" (ARG = tag expression). e.g.
;;   (set! *org-agenda-custom-commands*
;;     (list (list "w" "Waiting" "todo" "WAITING")
;;           (list "u" "Urgent"  "tags" "urgent")))
(define *org-agenda-custom-commands* '())
(define-command (org-agenda-dispatch)
  (if (null? *org-agenda-custom-commands*)
      (org-agenda)
      (completing-read "Agenda command: "
        (map (lambda (c) (string-append (nth c 0) " — " (nth c 1))) *org-agenda-custom-commands*)
        'org--agenda-dispatch-run)))
(define (org--agenda-dispatch-run cand)
  (let* ((key (car (split-many cand " ")))
         (c (let loop ((cs *org-agenda-custom-commands*))
              (cond ((null? cs) #f) ((equal? (nth (car cs) 0) key) (car cs)) (else (loop (cdr cs)))))))
    (if (not c) (message "No such command")
        (let ((type (nth c 2)) (arg (if (> (length c) 3) (nth c 3) "")))
          (cond ((equal? type "tags") (org--tags-view-finish arg))
                ((equal? type "todo")
                 (org--agenda-show
                  (lambda ()
                    (org--sort-entries (filter (lambda (e) (equal? (nth e 4) arg)) (org--all-entries))))))
                (else (org-agenda)))))))

;; ===========================================================================
;; Layer 7h — link store/insert + footnotes
;; ===========================================================================
;; A ring of (TARGET . DESCRIPTION) links, pushed by org-store-link.
(define *org-stored-links* '())
;; C-c l — store a link to the current location (an id: link if the heading has
;; an :ID:, else a file: link).
(define-command (org-store-link)
  (let* ((H (org--heading-line-at-point))
         (link (if H
                   (let* ((title (nth (org--parse-headline (org--line-text H)) 3))
                          (id (org--entry-get-local H "ID")))
                     (if id (cons (string-append "id:" id) title)
                         (cons (string-append "file:" (org--current-file-path)) title)))
                   (cons (string-append "file:" (org--current-file-path)) (current-buffer)))))
    (set! *org-stored-links* (cons link *org-stored-links*))
    (message (string-append "Stored link: " (car link)))))

;; C-c C-l — insert a link, choosing from the stored ring or typing a target.
(define-command (org-insert-link)
  (if (null? *org-stored-links*)
      (read-string "Link target: " 'org--insert-link-target)
      (completing-read "Link: " (map car *org-stored-links*) 'org--insert-link-chosen)))
(define (org--insert-link-target target)
  (when (not (equal? (string-trim target) ""))
    (insert (string-append "[[" (string-trim target) "]]"))))
(define (org--insert-link-chosen target)
  (let ((p (let loop ((ls *org-stored-links*))
             (cond ((null? ls) #f) ((equal? (car (car ls)) target) (car ls)) (else (loop (cdr ls)))))))
    (if p (insert (string-append "[[" (car p) "][" (cdr p) "]]"))
        (insert (string-append "[[" target "]]")))))

;; The largest numeric footnote label in the buffer (0 if none).
(define (org--max-fn-number)
  (let loop ((s (buffer-string)) (mx 0))
    (let ((m (string-match "\\[fn:([0-9]+)\\]" s)))
      (if (< m 0) mx
          (let* ((c (regexp-captures "\\[fn:([0-9]+)\\]" s))
                 (consumed (+ m (utf8-length (car c)))))
            (loop (byte-drop s consumed) (max mx (string->number (nth c 1)))))))))
;; C-c C-x f — insert a numbered footnote reference and a definition stub at EOF.
;; The reference goes in FIRST: an edit moves the cursor to its end, so inserting
;; the definition first would carry the reference to the end of the file too.
;; Point finishes after the new definition's label, ready to type it (Emacs).
(define-command (org-footnote-new)
  (let* ((label (string-append "fn:" (number->string (+ 1 (org--max-fn-number)))))
         (ref (string-append "[" label "]")))
    (insert ref)
    (insert-at (+ (point-max) (utf8-length ref)) (string-append "\n" ref " "))
    (message (string-append "Footnote [" label "] — definition added at end of file"))))

;; ===========================================================================
;; Layer 7i — table formulas (#+TBLFM:)
;; ===========================================================================
;; A small arithmetic evaluator (+ - * / parens, unary minus) over a tokenized
;; expression, driven by shunting-yard → RPN. Field references ($C, @R$C) are
;; substituted with their numeric values (wrapped in parens) before evaluation.
(define (org--char->str ch) (list->string (list ch)))
(define (org--digit-char? ch) (let ((cp (char->integer ch))) (or (and (>= cp 48) (<= cp 57)) (= cp 46))))
(define (org--op-char? ch)
  (let ((cp (char->integer ch))) (or (= cp 43) (= cp 45) (= cp 42) (= cp 47) (= cp 40) (= cp 41))))
(define (org--ws-char? ch) (let ((cp (char->integer ch))) (or (= cp 32) (= cp 9))))

(define (org--tokenize s)
  (let loop ((chars (string->list s)) (acc '()) (cur ""))
    (cond
      ((null? chars) (reverse (if (equal? cur "") acc (cons cur acc))))
      (else
       (let ((ch (car chars)) (rest (cdr chars)) (flushed (if (equal? cur "") acc (cons cur acc))))
         (cond
           ((org--digit-char? ch) (loop rest acc (string-append cur (org--char->str ch))))
           ((org--op-char? ch) (loop rest (cons (org--char->str ch) flushed) ""))
           ((org--ws-char? ch) (loop rest flushed ""))
           (else (loop rest acc (string-append cur (org--char->str ch))))))))))

(define (org--num-tok? t) (and (string->number t) #t))
(define (org--op-tok? t)
  (or (equal? t "+") (equal? t "-") (equal? t "*") (equal? t "/") (equal? t "u-")))
(define (org--prec op)
  (cond ((or (equal? op "+") (equal? op "-")) 1)
        ((or (equal? op "*") (equal? op "/")) 2)
        ((equal? op "u-") 3) (else 0)))

(define (org--to-rpn tokens)
  (let loop ((ts tokens) (prev #f) (out '()) (ops '()))
    (cond
      ((null? ts) (append (reverse out) ops))
      (else
       (let ((t (car ts)))
         (cond
           ((org--num-tok? t) (loop (cdr ts) t (cons t out) ops))
           ((equal? t "(") (loop (cdr ts) t out (cons t ops)))
           ((equal? t ")")
            (let pop ((o ops) (acc out))
              (cond ((null? o) (loop (cdr ts) t acc '()))
                    ((equal? (car o) "(") (loop (cdr ts) t acc (cdr o)))
                    (else (pop (cdr o) (cons (car o) acc))))))
           ((or (equal? t "+") (equal? t "-") (equal? t "*") (equal? t "/"))
            (let* ((unary (and (or (equal? t "-") (equal? t "+"))
                               (or (not prev) (equal? prev "(") (org--op-tok? prev))))
                   (tok (cond ((and unary (equal? t "-")) "u-")
                              ((and unary (equal? t "+")) "u+")
                              (else t))))
              (if (equal? tok "u+")
                  (loop (cdr ts) t out ops)
                  (let pop ((o ops) (acc out))
                    (cond ((and (not (null? o)) (not (equal? (car o) "("))
                                (not (equal? tok "u-"))
                                (>= (org--prec (car o)) (org--prec tok)))
                           (pop (cdr o) (cons (car o) acc)))
                          (else (loop (cdr ts) t acc (cons tok o))))))))
           (else (loop (cdr ts) t out ops))))))))

(define (org--apply-op op a b)
  (cond ((equal? op "+") (+ a b)) ((equal? op "-") (- a b)) ((equal? op "*") (* a b))
        ((equal? op "/") (if (= b 0) 0 (/ (* 1.0 a) (* 1.0 b)))) (else 0)))
(define (org--eval-rpn rpn)
  (let loop ((ts rpn) (stack '()))
    (cond
      ((null? ts) (if (null? stack) 0 (car stack)))
      (else
       (let ((t (car ts)))
         (cond
           ((equal? t "u-") (loop (cdr ts) (cons (- 0 (car stack)) (cdr stack))))
           ((org--op-tok? t)
            (let ((b (car stack)) (a (car (cdr stack))) (rest (cdr (cdr stack))))
              (loop (cdr ts) (cons (org--apply-op t a b) rest))))
           (else (loop (cdr ts) (cons (string->number t) stack)))))))))
(define (org--eval-arith expr) (org--eval-rpn (org--to-rpn (org--tokenize expr))))

;; Format a number for a cell: integers bare, others rounded to 6 dp, trailing
;; ".0" stripped.
(define (org--fmt-num n)
  (let* ((n2 (if (integer? n) n (/ (round (* n 1000000.0)) 1000000.0)))
         (s (number->string n2)))
    (if (ends-with? s ".0") (substring s 0 (- (string-length s) 2)) s)))

;; --- grid evaluation --------------------------------------------------------
(define (org--cell-num grid r c)
  (let* ((row (if (and (>= r 1) (<= r (length grid))) (nth grid (- r 1)) '()))
         (v (if (and (>= c 1) (<= c (length row))) (nth row (- c 1)) ""))
         (n (string->number (string-trim v))))
    (if n n 0)))
(define (org--subst-at grid expr)
  (let loop ((s expr) (acc ""))
    (let ((m (string-match "@([0-9]+)\\$([0-9]+)" s)))
      (if (< m 0) (string-append acc s)
          (let* ((c (regexp-captures "@([0-9]+)\\$([0-9]+)" s))
                 (whole (car c)) (r (string->number (nth c 1))) (col (string->number (nth c 2)))
                 (before (byte-take s m)) (after (byte-drop s (+ m (utf8-length whole)))))
            (loop after (string-append acc before "(" (org--fmt-num (org--cell-num grid r col)) ")")))))))
(define (org--subst-dollar grid curr expr)
  (let loop ((s expr) (acc ""))
    (let ((m (string-match "\\$([0-9]+)" s)))
      (if (< m 0) (string-append acc s)
          (let* ((c (regexp-captures "\\$([0-9]+)" s))
                 (whole (car c)) (col (string->number (nth c 1)))
                 (before (byte-take s m)) (after (byte-drop s (+ m (utf8-length whole)))))
            (loop after (string-append acc before "(" (org--fmt-num (org--cell-num grid curr col)) ")")))))))
(define (org--subst-refs expr grid curr) (org--subst-dollar grid curr (org--subst-at grid expr)))

(define (org--row-set cells col val)
  (let ((n (max col (length cells))))
    (let loop ((i 1) (acc '()))
      (if (> i n) (reverse acc) (loop (+ i 1) (cons (if (= i col) val (org--cell cells (- i 1))) acc))))))
(define (org--grid-set grid r col val)
  (let loop ((g grid) (i 1) (acc '()))
    (cond ((null? g) (reverse acc))
          ((= i r) (loop (cdr g) (+ i 1) (cons (org--row-set (car g) col val) acc)))
          (else (loop (cdr g) (+ i 1) (cons (car g) acc))))))

(define (org--apply-formula grid fstr) (org--apply-formula-from grid fstr 1))
;; Like org--apply-formula, but a column formula ($C=...) only fills rows
;; `first`.. — the rows above a table's first hline are its header, which Org
;; never overwrites. Field formulas (@R$C=...) still address any row.
(define (org--apply-formula-from grid fstr first)
  (let ((c (regexp-captures "^[ \t]*(@?[0-9]*\\$[0-9]+)[ \t]*=[ \t]*(.*)$" fstr)))
    (if (null? c) grid
        (let* ((lhs (nth c 1)) (rhs (nth c 2)) (atc (regexp-captures "@([0-9]+)\\$([0-9]+)" lhs)))
          (if (not (null? atc))
              (org--grid-set grid (string->number (nth atc 1)) (string->number (nth atc 2))
                             (org--fmt-num (org--eval-arith (org--subst-refs rhs grid (string->number (nth atc 1))))))
              (let ((col (string->number (nth (regexp-captures "\\$([0-9]+)" lhs) 1))))
                (let loop ((r first) (g grid))
                  (if (> r (length grid)) g
                      (loop (+ r 1)
                            (org--grid-set g r col
                                           (org--fmt-num (org--eval-arith (org--subst-refs rhs g r)))))))))))))

;; (slice-index . cells) for each non-separator row of a table slice.
(define (org--table-grid slice)
  (let loop ((ls slice) (i 0) (acc '()))
    (cond ((null? ls) (reverse acc))
          ((org--table-sep? (car ls)) (loop (cdr ls) (+ i 1) acc))
          (else (loop (cdr ls) (+ i 1) (cons (cons i (org--table-cells (car ls))) acc))))))
;; 1-based grid row of the first data row: the row after the header block when
;; the table has an hline below at least one row, else 1 (no header).
(define (org--table-first-data-row slice)
  (let loop ((ls slice) (rows 0))
    (cond ((null? ls) 1)
          ((org--table-sep? (car ls)) (if (> rows 0) (+ rows 1) (loop (cdr ls) rows)))
          (else (loop (cdr ls) (+ rows 1))))))
(define (org--grid->slice slice rows grid2)
  (let loop ((ls slice) (i 0) (rs rows) (gs grid2) (acc '()))
    (cond ((null? ls) (reverse acc))
          ((and (not (null? rs)) (= (car (car rs)) i))
           (loop (cdr ls) (+ i 1) (cdr rs) (cdr gs) (cons (org--row-from-cells (car gs)) acc)))
          (else (loop (cdr ls) (+ i 1) rs gs (cons (car ls) acc))))))

(define (org--tblfm-after end)
  (let ((lines (org--lines)) (n (org--total-lines)))
    (if (and (< (+ end 1) n) (>= (string-match "^[ \t]*#\\+TBLFM:" (nth lines (+ end 1))) 0)) (+ end 1) #f)))
(define (org--tblfm-formulas line)
  (let ((c (regexp-captures "#\\+TBLFM:[ \t]*(.*)$" line)))
    (if (null? c) '() (filter (lambda (x) (not (equal? (string-trim x) ""))) (split-many (nth c 1) "::")))))

;; Recompute the table at point from its #+TBLFM: line (or just re-align it).
(define-command (org-table-recalculate)
  (let* ((L (buffer-line)) (b (org--table-bounds L)))
    (if (not b) (message "Not in a table")
        (let* ((start (nth b 0)) (end (nth b 1)) (fl (org--tblfm-after end)))
          (if (not fl) (org-table-align)
              (let* ((slice (org--slice (org--lines) start end))
                     (rows (org--table-grid slice))
                     (grid (map cdr rows))
                     (formulas (org--tblfm-formulas (org--line-text fl)))
                     (first (org--table-first-data-row slice))
                     (grid2 (let loop ((fs formulas) (g grid))
                              (if (null? fs) g
                                  (loop (cdr fs) (org--apply-formula-from g (car fs) first))))))
                (org--replace-line-range start end (org--format-table (org--grid->slice slice rows grid2)))
                (message "Table recalculated")))))))

;; ===========================================================================
;; Layer 7j — export backends: Markdown, LaTeX, ASCII (walk the org-parse AST)
;; ===========================================================================
(define (org--range n) (let loop ((i 0) (acc '())) (if (>= i n) (reverse acc) (loop (+ i 1) (cons i acc)))))
(define (org--indent-lines text prefix)
  (join-lines (map (lambda (l) (string-append prefix l)) (split-many text "\n"))))
(define (org--ast-table->rows e)
  (map (lambda (r) (if (equal? (car r) "sep") "|-|" (org--row-from-cells (cdr r)))) (nth e 1)))
(define (org--output-path ext)
  ;; Prefer the visited file's real path: a uniquified buffer is named
  ;; `notes.org<dir>`, so the buffer name can't be trusted for the stem.
  (let ((f (buffer-file-name)))
    (if (and (string? f) (not (equal? f "")))
        (string-append (if (ends-with? f ".org") (substring f 0 (- (string-length f) 4)) f) ext)
        (let* ((b (current-buffer))
               (stem (if (ends-with? b ".org") (substring b 0 (- (string-length b) 4)) b)))
          (path-join (buffer-directory) (string-append stem ext))))))

;; --- Markdown (GitHub-flavored) ---------------------------------------------
(define (org--inline->md s)
  (org--inline-split s org--link->md org--emphasis->md))
(define (org--link->md s)
  (let* ((s (replace-regexp-in-string "\\[\\[([^]]+)\\]\\[([^]]+)\\]\\]" "[$2]($1)" s))
         (s (replace-regexp-in-string "\\[\\[([^]]+)\\]\\]" "[$1]($1)" s)))
    s))
(define (org--emphasis->md s)
  (let* ((s (replace-regexp-in-string "\\*([^*\n]+)\\*" "**$1**" s))
         (s (replace-regexp-in-string "/([^/\n]+)/" "_${1}_" s))
         (s (replace-regexp-in-string "=([^=\n]+)=" "`$1`" s))
         (s (replace-regexp-in-string "~([^~\n]+)~" "`$1`" s))
         (s (replace-regexp-in-string "\\+([^+\n]+)\\+" "~~$1~~" s))
         (s (replace-regexp-in-string "\\[fn:([^]\n]*)\\]" "[^$1]" s)))
    s))
(define (org--list->md e ordered)
  (let loop ((its (nth e 1)) (i 1) (acc ""))
    (if (null? its) acc
        (let ((it (car its)))
          (loop (cdr its) (+ i 1)
                (string-append acc (if ordered (string-append (number->string i) ". ") "- ")
                               (if (equal? (car it) "") "" (string-append "[" (car it) "] "))
                               (org--inline->md (cdr it)) "\n"))))))
(define (org--md-row cells ncols)
  (string-append "|"
    (string-concat (map (lambda (i) (string-append " " (org--inline->md (org--cell cells i)) " |")) (org--range ncols)))
    "\n"))
(define (org--table->md e)
  (let* ((rows (filter (lambda (r) (not (equal? (car r) "sep"))) (nth e 1)))
         (ncols (let loop ((rs rows) (m 1)) (if (null? rs) m (loop (cdr rs) (max m (length (cdr (car rs)))))))))
    (if (null? rows) ""
        (string-append (org--md-row (cdr (car rows)) ncols)
                       "|" (string-concat (map (lambda (i) " --- |") (org--range ncols))) "\n"
                       (string-concat (map (lambda (r) (org--md-row (cdr r) ncols)) (cdr rows)))))))
(define (org--block->md e)
  (let ((type (car e)))
    (cond
      ((equal? type "heading")
       (string-append (string-repeat "#" (min 6 (nth e 1))) " "
                      (if (equal? (nth e 2) "") "" (string-append (nth e 2) " "))
                      (org--inline->md (nth e 4))
                      (if (equal? (nth e 5) "") "" (string-append "  :" (nth e 5) ":")) "\n\n"))
      ((equal? type "para") (string-append (org--inline->md (nth e 1)) "\n\n"))
      ((equal? type "src") (string-append "```" (nth e 1) "\n" (nth e 2) "\n```\n\n"))
      ((equal? type "example") (string-append "```\n" (nth e 1) "\n```\n\n"))
      ((equal? type "quote") (string-append "> " (replace-regexp-in-string "\n" "\n> " (org--inline->md (nth e 1))) "\n\n"))
      ((equal? type "hr") "---\n\n")
      ((equal? type "ulist") (string-append (org--list->md e #f) "\n"))
      ((equal? type "olist") (string-append (org--list->md e #t) "\n"))
      ((equal? type "table") (string-append (org--table->md e) "\n"))
      (else ""))))
(define (org--ast->md ast)
  (let ((title (org--doc-title ast)))
    (string-append (if (equal? title "Untitled") "" (string-append "# " title "\n\n"))
                   (string-concat (map org--block->md ast)))))

;; --- LaTeX ------------------------------------------------------------------
(define (org--escape-latex s)
  (let* ((s (replace-regexp-in-string "%" "\\%" s))
         (s (replace-regexp-in-string "&" "\\&" s))
         (s (replace-regexp-in-string "#" "\\#" s))
         (s (replace-regexp-in-string "_" "\\_" s)))
    s))
;; Escape only the text between links: a URL keeps its `_` (hyperref reads
;; `\_` literally, breaking the link) — only `%` and `#` need escaping there.
(define (org--inline->latex s)
  (org--inline-split s org--link->latex (lambda (t) (org--emphasis->latex (org--escape-latex t)))))
(define (org--latex-url u)
  (replace-regexp-in-string "#" "\\#" (replace-regexp-in-string "%" "\\%" u)))
(define (org--link->latex s)
  (let ((c (regexp-captures "^\\[\\[([^]]+)\\]\\[([^]]+)\\]\\]$" s)))
    (if (null? c)
        (let ((u (regexp-captures "^\\[\\[([^]]+)\\]\\]$" s)))
          (if (null? u) (org--escape-latex s)
              (string-append "\\url{" (org--latex-url (nth u 1)) "}")))
        (string-append "\\href{" (org--latex-url (nth c 1)) "}{"
                       (org--emphasis->latex (org--escape-latex (nth c 2))) "}"))))
(define (org--emphasis->latex s)
  (let* ((s (replace-regexp-in-string "\\*([^*\n]+)\\*" "\\textbf{$1}" s))
         (s (replace-regexp-in-string "/([^/\n]+)/" "\\emph{$1}" s))
         (s (replace-regexp-in-string "=([^=\n]+)=" "\\texttt{$1}" s))
         (s (replace-regexp-in-string "~([^~\n]+)~" "\\texttt{$1}" s)))
    s))
(define (org--latex-section-cmd lvl)
  (cond ((= lvl 1) "\\section") ((= lvl 2) "\\subsection") ((= lvl 3) "\\subsubsection")
        ((= lvl 4) "\\paragraph") (else "\\subparagraph")))
(define (org--list->latex e ordered)
  (let ((env (if ordered "enumerate" "itemize")))
    (string-append "\\begin{" env "}\n"
      (string-concat (map (lambda (it) (string-append "\\item " (org--inline->latex (cdr it)) "\n")) (nth e 1)))
      "\\end{" env "}\n\n")))
(define (org--table->latex e)
  (let* ((rows (filter (lambda (r) (not (equal? (car r) "sep"))) (nth e 1)))
         (ncols (let loop ((rs rows) (m 1)) (if (null? rs) m (loop (cdr rs) (max m (length (cdr (car rs)))))))))
    (string-append "\\begin{tabular}{" (string-concat (map (lambda (i) "l") (org--range ncols))) "}\n"
      (string-concat (map (lambda (r)
        (string-append (string-join (map (lambda (i) (org--inline->latex (org--cell (cdr r) i))) (org--range ncols)) " & ")
                       " \\\\\n")) rows))
      "\\end{tabular}\n\n")))
(define (org--block->latex e)
  (let ((type (car e)))
    (cond
      ((equal? type "heading")
       (string-append (org--latex-section-cmd (nth e 1)) "{" (org--inline->latex (nth e 4)) "}\n\n"))
      ((equal? type "para") (string-append (org--inline->latex (nth e 1)) "\n\n"))
      ((equal? type "src") (string-append "\\begin{verbatim}\n" (nth e 2) "\n\\end{verbatim}\n\n"))
      ((equal? type "example") (string-append "\\begin{verbatim}\n" (nth e 1) "\n\\end{verbatim}\n\n"))
      ((equal? type "quote") (string-append "\\begin{quote}\n" (org--inline->latex (nth e 1)) "\n\\end{quote}\n\n"))
      ((equal? type "hr") "\\hrulefill\n\n")
      ((equal? type "ulist") (org--list->latex e #f))
      ((equal? type "olist") (org--list->latex e #t))
      ((equal? type "table") (org--table->latex e))
      (else ""))))
(define (org--ast->latex ast)
  (string-append
   "\\documentclass{article}\n\\usepackage[utf8]{inputenc}\n\\usepackage{hyperref}\n"
   "\\title{" (org--escape-latex (org--doc-title ast)) "}\n\\begin{document}\n\\maketitle\n\n"
   (string-concat (map org--block->latex ast))
   "\\end{document}\n"))

;; --- ASCII ------------------------------------------------------------------
(define (org--inline->ascii s)
  (org--inline-split s org--link->ascii org--emphasis->ascii))
(define (org--link->ascii s)
  (let* ((s (replace-regexp-in-string "\\[\\[([^]]+)\\]\\[([^]]+)\\]\\]" "$2 ($1)" s))
         (s (replace-regexp-in-string "\\[\\[([^]]+)\\]\\]" "$1" s)))
    s))
(define (org--emphasis->ascii s)
  (let* ((s (replace-regexp-in-string "\\*([^*\n]+)\\*" "$1" s))
         (s (replace-regexp-in-string "/([^/\n]+)/" "$1" s))
         (s (replace-regexp-in-string "=([^=\n]+)=" "$1" s))
         (s (replace-regexp-in-string "~([^~\n]+)~" "$1" s))
         (s (replace-regexp-in-string "\\+([^+\n]+)\\+" "$1" s)))
    s))
(define (org--list->ascii e ordered)
  (let loop ((its (nth e 1)) (i 1) (acc ""))
    (if (null? its) acc
        (let ((it (car its)))
          (loop (cdr its) (+ i 1)
                (string-append acc (if ordered (string-append (number->string i) ". ") "- ")
                               (if (equal? (car it) "") "" (string-append "[" (car it) "] "))
                               (org--inline->ascii (cdr it)) "\n"))))))
(define (org--block->ascii e)
  (let ((type (car e)))
    (cond
      ((equal? type "heading")
       (let* ((title (string-append (if (equal? (nth e 2) "") "" (string-append (nth e 2) " "))
                                    (org--inline->ascii (nth e 4))))
              (ch (cond ((= (nth e 1) 1) "=") ((= (nth e 1) 2) "-") (else "~"))))
         (string-append title "\n" (string-repeat ch (string-length title)) "\n\n")))
      ((equal? type "para") (string-append (org--inline->ascii (nth e 1)) "\n\n"))
      ((equal? type "src") (string-append (org--indent-lines (nth e 2) "    ") "\n\n"))
      ((equal? type "example") (string-append (org--indent-lines (nth e 1) "    ") "\n\n"))
      ((equal? type "quote") (string-append (org--indent-lines (org--inline->ascii (nth e 1)) "  ") "\n\n"))
      ((equal? type "hr") "----------------------------------------\n\n")
      ((equal? type "ulist") (string-append (org--list->ascii e #f) "\n"))
      ((equal? type "olist") (string-append (org--list->ascii e #t) "\n"))
      ((equal? type "table") (string-append (join-lines (org--format-table (org--ast-table->rows e))) "\n\n"))
      (else ""))))
(define (org--ast->ascii ast) (string-concat (map org--block->ascii ast)))

;; Export commands (C-c C-e m / l / a).
(define-command (org-export-markdown)
  (let ((p (org--output-path ".md")))
    (write-file p (org--ast->md (org-parse (buffer-string)))) (message (string-append "Exported to " p))))
(define-command (org-export-latex)
  (let ((p (org--output-path ".tex")))
    (write-file p (org--ast->latex (org-parse (buffer-string)))) (message (string-append "Exported to " p))))
(define-command (org-export-ascii)
  (let ((p (org--output-path ".txt")))
    (write-file p (org--ast->ascii (org-parse (buffer-string)))) (message (string-append "Exported to " p))))

;; ===========================================================================
;; Layer 7k — column view (read-only, driven by #+COLUMNS:)
;; ===========================================================================
(define *org-columns-default* "%40ITEM %TODO %PRIORITY %TAGS")
(define (org--columns-spec)
  (let loop ((ls (org--lines)))
    (cond ((null? ls) *org-columns-default*)
          (else (let ((c (regexp-captures "^[ \t]*#\\+COLUMNS:[ \t]*(.*)$" (car ls))))
                  (if (null? c) (loop (cdr ls)) (nth c 1)))))))
;; "%[width]NAME" tokens → list of (width . name).
(define (org--parse-columns spec)
  (let loop ((toks (remove-equal "" (split-many spec " "))) (acc '()))
    (if (null? toks) (reverse acc)
        (let ((c (regexp-captures "^%([0-9]*)([A-Za-z_]+)" (car toks))))
          (if (null? c) (loop (cdr toks) acc)
              (loop (cdr toks)
                    (cons (cons (if (equal? (nth c 1) "") 0 (string->number (nth c 1))) (nth c 2)) acc)))))))

;; Total clocked minutes in heading H's own section.
(define (org--heading-clocksum H)
  (let ((lines (org--lines)) (e (org--entry-content-end H)))
    (let loop ((i (+ H 1)) (sum 0))
      (if (> i e) sum
          (let ((m (org--parse-clock-mins (org--line-text-in lines i))))
            (loop (+ i 1) (if m (+ sum m) sum)))))))

(define (org--column-value H name)
  (let ((h (org--parse-headline (org--line-text H))))
    (cond
      ((equal? name "ITEM") (nth h 3))
      ((equal? name "TODO") (or (nth h 1) ""))
      ((equal? name "PRIORITY") (nth h 2))
      ((equal? name "TAGS") (nth h 4))
      ((equal? name "CLOCKSUM") (org--fmt-duration (org--heading-clocksum H)))
      (else (or (org-entry-get-inherited H name) "")))))

;; M-x org-columns — tabulate every heading's column values in a side buffer.
(define-command (org-columns)
  (let* ((names (map cdr (org--parse-columns (org--columns-spec))))
         (header (string-append "| " (string-join names " | ") " |"))
         (rows (map (lambda (h)
                      (let ((H (car h)) (lvl (nth h 1)))
                        (string-append "| "
                          (string-join
                           (map (lambda (nm)
                                  (let ((v (org--column-value H nm)))
                                    (if (equal? nm "ITEM") (string-append (string-repeat " " (* 2 (- lvl 1))) v) v)))
                                names)
                           " | ")
                          " |")))
                    (org--headings))))
    (render-buffer "*Org Columns*" "org-columns-mode"
                   (join-lines (org--format-table (append (list header "|-|") rows))) '())
    (message "Org columns")))

;; ===========================================================================
;; Layer 7l — babel noweb / named blocks / inline + list-item editing
;; ===========================================================================
;; Body of the src block named NAME (via a #+name: line), or #f.
(define (org--named-block-body name)
  (let ((lines (org--lines)) (n (org--total-lines)))
    (let loop ((i 0))
      (cond ((>= i n) #f)
            ((>= (string-match (string-append "(?i)^[ \t]*#\\+name:[ \t]*" (regexp-quote name) "[ \t]*$")
                               (nth lines i)) 0)
             (let skip ((j (+ i 1)))
               (cond ((>= j n) #f)
                     ((org--block-begin? (nth lines j))
                      (let end ((k (+ j 1)) (acc '()))
                        (cond ((>= k n) (join-lines (reverse acc)))
                              ((org--block-end? (nth lines k)) (join-lines (reverse acc)))
                              (else (end (+ k 1) (cons (nth lines k) acc))))))
                     ((equal? (string-trim (nth lines j)) "") (skip (+ j 1)))
                     (else #f))))
            (else (loop (+ i 1)))))))
;; Expand <<name>> noweb references in a block body.
(define (org--expand-noweb body)
  (let loop ((s body) (acc ""))
    (let ((m (string-match "<<([A-Za-z0-9_-]+)>>" s)))
      (if (< m 0) (string-append acc s)
          (let* ((c (regexp-captures "<<([A-Za-z0-9_-]+)>>" s))
                 (whole (car c)) (name (nth c 1))
                 (before (byte-take s m)) (after (byte-drop s (+ m (utf8-length whole)))))
            (loop after (string-append acc before (or (org--named-block-body name) ""))))))))

;; C-c C-c on an inline `src_lang{code}` runs it and appends {{{results(…)}}}.
(define-command (org-babel-execute-inline)
  (let* ((L (buffer-line)) (line (org--line-text L))
         (m (string-match "src_([A-Za-z0-9]+)(\\[[^]]*\\])?\\{([^}]*)\\}" line)))
    (if (< m 0) (message "No inline src at point")
        (let* ((c (regexp-captures "src_([A-Za-z0-9]+)(\\[[^]]*\\])?\\{([^}]*)\\}" line))
               (whole (car c))
               (res (org--run-src (nth c 1) (nth c 3)))
               (out (trim-end (if (string? res) res (join-lines (map car res)))))
               (pos (+ (org--line-start L) m (utf8-length whole)))
               ;; A previous run's result right after the call is replaced, not
               ;; stacked (Org rewrites its {{{results(…)}}} macro in place).
               (prev (regexp-captures "^[ \t]*\\{\\{\\{results\\(.*?\\)\\}\\}\\}"
                                      (byte-drop line (+ m (utf8-length whole)))))
               (old-end (+ pos (if (null? prev) 0 (utf8-length (car prev))))))
          (replace-region pos old-end (string-append " {{{results(" out ")}}}"))
          (message "Inline src executed")))))

;; --- list-item editing ------------------------------------------------------
;; (whole indent bullet) for a plain-list line, or #f.
(define (org--list-line-prefix line)
  (let ((c (regexp-captures "^([ \t]*)([-+*]|[0-9]+[.)])[ \t]+" line)))
    ;; A `*` bullet at column 0 is a heading, not a list item (as in Org): M-RET,
    ;; M-left/right on `* Foo` must insert / promote / demote headings.
    (if (or (null? c)
            (and (equal? (nth c 2) "*") (equal? (nth c 1) "")))
        #f
        c)))
;; M-RET on a list item — insert a sibling item below (incrementing an ordinal).
(define-command (org-insert-item)
  (let* ((L (buffer-line)) (line (org--line-text L)) (c (org--list-line-prefix line)))
    (if (not c) #f
        (let* ((indent (nth c 1)) (bullet (nth c 2))
               (n (regexp-captures "^([0-9]+)([.)])$" bullet))
               (newbullet (if (null? n) bullet
                              (string-append (number->string (+ 1 (string->number (nth n 1)))) (nth n 2))))
               ;; A checkbox item begets an unchecked one (Emacs org-insert-item).
               (cb (if (>= (string-match "^[ \t]*([-+*]|[0-9]+[.)])[ \t]+\\[[ xX-]\\]" line) 0) "[ ] " ""))
               (new (string-append indent newbullet " " cb))
               (pos (+ (org--line-start L) (utf8-length line))))
          (insert-at pos (string-append "\n" new))
          (goto-char (+ pos 1 (utf8-length new)))
          #t))))
(define (org-indent-item)
  (let* ((L (buffer-line)) (line (org--line-text L)))
    (when (org--list-line-prefix line) (org--replace-line L (string-append "  " line)))))
(define (org-outdent-item)
  (let* ((L (buffer-line)) (line (org--line-text L)))
    (when (and (org--list-line-prefix line) (>= (string-match "^  " line) 0))
      (org--replace-line L (substring line 2 (string-length line))))))
;; M-RET dispatcher: a list item on a list line, else a heading.
(define (org-meta-return)
  (if (org--list-line-prefix (org--line-text (buffer-line))) (org-insert-item) (org-insert-heading)))

;; Activation
;; ===========================================================================
;; Turn the active buffer into an Org buffer (M-x org-mode, or auto for .org).
(define (org--activate)
  (set-major-mode "org-mode")
  (buffer-local-set "org-active" "t")
  ;; Use Org's outline folding (headlines) for this buffer's chevrons rather
  ;; than the generic indentation folder.
  (buffer-local-set "fold-style" "org")
  (org--fontify)
  ;; Fold chevrons walk every heading (another whole-buffer pass); skip on
  ;; oversized files, same threshold as fontify so neither hangs the engine.
  (when (org--fontify-ok?) (org--refresh-fold-markers)))

(define-command (org-mode)
  (org--activate)
  (message "Org mode"))

;; .org files: activate on open. (set-major-mode installs the org-mode keymap.)
(define (org--maybe-activate)
  (when (and (ends-with? (org--file-or-name) ".org")
             (not (equal? (buffer-local-get "org-active") "t")))
    (org--activate)))
(add-hook "find-file-hook" "org--maybe-activate")
;; Buffers restored by desktop-save are visited in the background without
;; find-file-hook, so also activate when one first becomes current (a no-op
;; for a buffer that is already active).
(add-hook "buffer-switch-hook" "org--maybe-activate")
(add-hook "after-change-functions" "org--fontify-schedule")

;; ===========================================================================
;; Keymap + M-x
;; ===========================================================================
;; TAB / S-TAB / RET / M-arrows are context-sensitive: in a table they edit it,
;; otherwise they fall back to heading folding / structure editing (like Org).
(define-key "org-mode" "tab" 'org-tab)
(define-key "org-mode" "shift-tab" 'org-shift-tab)
(define-key "org-mode" "enter" 'org-return)
(define-key "org-mode" "ctrl-k" 'org-kill-line)
(define-key "org-mode" "ctrl-c ctrl-t" 'org-todo)
(define-key "org-mode" "ctrl-c ctrl-c" 'org-ctrl-c-ctrl-c)
(define-key "org-mode" "ctrl-c ctrl-q" 'org-set-tags)
(define-key "org-mode" "ctrl-c ," 'org-set-priority)
(define-key "org-mode" "alt-enter" 'org-meta-return)
(define-key "org-mode" "alt-left" 'org-meta-left)
(define-key "org-mode" "alt-right" 'org-meta-right)
(define-key "org-mode" "alt-up" 'org-meta-up)
(define-key "org-mode" "alt-down" 'org-meta-down)
;; Table structure: insert/delete column (M-S-←/→) and row (M-S-↓/↑).
(define-key "org-mode" "alt-shift-right" 'org-table-insert-column)
(define-key "org-mode" "alt-shift-left" 'org-table-delete-column)
(define-key "org-mode" "alt-shift-down" 'org-table-insert-row)
(define-key "org-mode" "alt-shift-up" 'org-table-delete-row)
;; Export dispatcher prefix C-c C-e: h html, m markdown, l latex, a ascii.
(define-key "org-mode" "ctrl-c ctrl-e h" 'org-export-html)
(define-key "org-mode" "ctrl-c ctrl-e m" 'org-export-markdown)
(define-key "org-mode" "ctrl-c ctrl-e l" 'org-export-latex)
(define-key "org-mode" "ctrl-c ctrl-e a" 'org-export-ascii)
(define-key "org-mode" "ctrl-c a" 'org-agenda)
(define-key "org-mode" "ctrl-c shift-a" 'org-agenda-dispatch)
(define-key "org-mode" "ctrl-c t" 'org-todo-list)
(define-key "org-mode" "ctrl-c c" 'org-capture)
;; Scheduling / properties / links / footnotes.
(define-key "org-mode" "ctrl-c ctrl-s" 'org-schedule)
(define-key "org-mode" "ctrl-c ctrl-d" 'org-deadline)
(define-key "org-mode" "ctrl-c ." 'org-time-stamp)
(define-key "org-mode" "ctrl-c ctrl-w" 'org-refile)
(define-key "org-mode" "ctrl-c \\" 'org-tags-view)
(define-key "org-mode" "ctrl-c ctrl-l" 'org-insert-link)
(define-key "org-mode" "ctrl-c l" 'org-store-link)
;; Clocking / properties / archive / columns / footnote / inline images / latex
;; live under the C-c C-x prefix.
(define-key "org-mode" "ctrl-x n s" 'org-narrow-to-subtree)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-i" 'org-clock-in)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-o" 'org-clock-out)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-x" 'org-clock-cancel)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-r" 'org-clock-report)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-a" 'org-archive-subtree)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-c" 'org-columns)
(define-key "org-mode" "ctrl-c ctrl-x p" 'org-set-property)
(define-key "org-mode" "ctrl-c ctrl-x f" 'org-footnote-new)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-v" 'org-toggle-inline-images)
(define-key "org-mode" "ctrl-c ctrl-x ctrl-l" 'org-toggle-latex-preview)

;; Agenda / capture / store-link are useful from any buffer too.
(define-key "fundamental" "ctrl-c a" 'org-agenda)
(define-key "fundamental" "ctrl-c c" 'org-capture)
(define-key "fundamental" "ctrl-c l" 'org-store-link)

;; *Org Agenda* buffer.
(define-list-mode-keys "org-agenda-mode" 'org-agenda-visit)
(define-key "org-agenda-mode" "g" 'org-agenda-redo)


;; *Org Columns* buffer.
(define-key "org-columns-mode" "q" 'kill-buffer)
(set-option "line-numbers:org-columns-mode" "false")

;; *scratch* defaults to Org mode (Emacs `initial-major-mode`). At startup the
;; active buffer is *scratch* (this file evals before welcome.scm), so switch it
;; to org-mode here — the same steps org--maybe-activate runs for .org files,
;; minus the find-file message. The mode sticks to the buffer, so dismissing the
;; splash with `q` lands you in an Org *scratch* buffer.
(when (equal? (current-buffer) "*scratch*")
  (set-major-mode "org-mode")
  (buffer-local-set "org-active" "t")
  (buffer-local-set "fold-style" "org")
  (org--fontify))
