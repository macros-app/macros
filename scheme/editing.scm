;; requires: core

;; editing.scm — core Emacs editing commands, on the range-edit / regexp / read
;; primitives. Region-aware where Emacs is.

;; The active region as (start end), or the fallback range [a b).
(define (region-or a b)
  (if (region-active?) (list (region-beginning) (region-end)) (list a b)))

;; Byte offset of the start of the line containing `pos`.
(define (line-start-before pos)
  (let loop ((i pos))
    (if (or (= i 0) (equal? (buffer-substring (- i 1) i) "\n")) i (loop (- i 1)))))

;; --- sort-lines (region, or whole buffer) -----------------------------------
;; A text ending in a newline splits to a trailing "" — sorting that would move
;; the final newline to the top, so sort only the real lines and keep it last.
(define (sort-lines-text text)
  (if (ends-with? text "\n")
      (string-append (sort-lines-text (substring text 0 (- (string-length text) 1))) "\n")
      (join-lines (sort-strings (split-many text "\n")))))
(define (sort-lines)
  (let* ((r (region-or (point-min) (point-max))) (s (car r)) (e (cadr r)))
    (replace-region s e (sort-lines-text (buffer-substring s e)))
    (message "Sorted lines")))

;; --- case commands (region) -------------------------------------------------
(define (upcase-region)
  (let* ((r (region-or (point) (point))) (s (car r)) (e (cadr r)))
    (when (< s e)
      (replace-region s e (string-upcase (buffer-substring s e)))
      (message "Upcased region"))))
(define (downcase-region)
  (let* ((r (region-or (point) (point))) (s (car r)) (e (cadr r)))
    (when (< s e)
      (replace-region s e (string-downcase (buffer-substring s e)))
      (message "Downcased region"))))
(define (capitalize-token tok)
  (if (equal? tok "")
      tok
      (string-append (string-upcase (substring tok 0 1))
                     (string-downcase (substring tok 1 (string-length tok))))))
;; Capitalize every whitespace-separated word, keeping the separators as they
;; are: splitting on " " alone left words after a newline or tab untouched.
(define (capitalize-words str)
  (let loop ((cs (string->list str)) (word-start #t) (acc '()))
    (if (null? cs)
        (list->string (reverse acc))
        (let ((c (car cs)))
          (if (char-whitespace? c)
              (loop (cdr cs) #t (cons c acc))
              (loop (cdr cs) #f (cons (if word-start (char-upcase c) (char-downcase c)) acc)))))))
(define (capitalize-region)
  (let* ((r (region-or (point) (point))) (s (car r)) (e (cadr r)))
    (when (< s e)
      (replace-region s e (capitalize-words (buffer-substring s e)))
      (message "Capitalized region"))))

;; --- goto-line (M-g g) ------------------------------------------------------
(define (goto-line-cmd) (read-string "Goto line: " 'goto-line-finish))
(define (goto-line-finish s)
  (let ((n (string->number (trim-end s))))
    (when n (goto-line (max 0 (- n 1))))))   ; user enters 1-based; goto-line is 0-based

;; --- join-line (M-^): pull the next line up, collapsing indentation to a space.
(define (join-line)
  (let* ((b (line-bounds)) (e (cadr b)) (mx (point-max)))
    (when (< e mx)
      (let loop ((i (+ e 1)))
        (if (and (< i mx)
                 (let ((c (buffer-substring i (+ i 1)))) (or (equal? c " ") (equal? c "\t"))))
            (loop (+ i 1))
            (replace-region e i " "))))))

;; --- zap-to-char (M-z): delete from point up to and including a character ----
(define (zap-to-char) (read-string "Zap to char: " 'zap-to-char-finish))
(define (zap-to-char-finish s)
  (when (> (string-length s) 0)
    ;; Through the WHOLE target char: `(+ idx 1)` stopped inside a multibyte
    ;; char (é), which floored back and left it in place.
    (let* ((ch (substring s 0 1)) (idx (search-forward ch)))
      (when (>= idx 0) (delete-region (point) (+ idx (utf8-length ch)))))))

;; --- transpose-lines (C-x C-t): swap this line with the previous -------------
(define (transpose-lines)
  (let* ((b (line-bounds)) (cur-s (car b)) (cur-e (cadr b)))
    (when (> cur-s 0)
      (let* ((prev-e (- cur-s 1))                 ; the newline before this line
             (prev-s (line-start-before prev-e)))
        (replace-region prev-s cur-e
                        (string-append (buffer-substring cur-s cur-e) "\n"
                                       (buffer-substring prev-s prev-e)))))))

;; --- word case (M-u / M-l / M-c) ----------------------------------------------
;; Fold from point to the end of the word and move there (Emacs semantics);
;; with an active region, fold the region instead.
(define (fold-word folder)
  (if (region-active?)
      (let ((s (region-beginning)) (e (region-end)))
        (when (< s e)
          (replace-region s e (folder (buffer-substring s e)))))
      (let* ((p (point)) (e (word-forward p)))
        (when (> e p)
          (replace-region p e (folder (buffer-substring p e)))
          (goto-char e)))))
(define (upcase-word)     (fold-word string-upcase))
(define (downcase-word)   (fold-word string-downcase))
(define (capitalize-word) (fold-word capitalize-token))

;; --- transpose-words (M-t): swap the words around point ----------------------
(define (transpose-words)
  (let* ((p (point))
         (e2 (word-forward p)) (s2 (word-backward e2))
         (s1 (word-backward s2)) (e1 (word-forward s1)))
    (when (and (< s1 e1) (< s2 e2) (<= e1 s2) (< s1 s2))
      (replace-region s1 e2
                      (string-append (buffer-substring s2 e2)
                                     (buffer-substring e1 s2)
                                     (buffer-substring s1 e1)))
      (goto-char e2))))

;; --- back-to-indentation (M-m): point to the line's first non-blank ----------
(define (back-to-indentation)
  (let* ((b (line-bounds)) (s (car b)) (e (cadr b)))
    (goto-char
     (let loop ((i s))
       (if (and (< i e)
                (let ((c (buffer-substring i (+ i 1))))
                  (or (equal? c " ") (equal? c "\t"))))
           (loop (+ i 1))
           i)))))

;; --- paragraph motion (M-{ / M-}) and mark-paragraph (M-h) -------------------
;; The boundary positions live in core.scm (paragraph-forward-pos /
;; paragraph-backward-pos) so the evil `{`/`}` motions share them.
(define (forward-paragraph)  (goto-char (paragraph-forward-pos (point))))
(define (backward-paragraph) (goto-char (paragraph-backward-pos (point))))
(define (mark-paragraph)
  (let ((b (paragraph-bounds (point))))
    (when b (set-region (car b) (min (point-max) (+ (cadr b) 1))))))

;; --- just-one-space (M-SPC): collapse whitespace around point to one space ---
(define (just-one-space)
  (let* ((mx (point-max))
         (blank? (lambda (i)
                   (let ((c (buffer-substring i (+ i 1))))
                     (or (equal? c " ") (equal? c "\t")))))
         (s (let loop ((i (point))) (if (and (> i 0) (blank? (- i 1))) (loop (- i 1)) i)))
         (e (let loop ((i (point))) (if (and (< i mx) (blank? i)) (loop (+ i 1)) i))))
    (replace-region s e " ")))

;; --- delete-blank-lines (C-x C-o) ---------------------------------------------
;; On a blank line: collapse the surrounding run of blank lines to one (delete
;; it entirely if it's already a single blank line). On a non-blank line:
;; delete any blank lines that follow (Emacs semantics).
(define (delete-blank-lines)
  (let* ((mx (point-max))
         ;; end of the blank run whose first blank line starts at `i` — the
         ;; offset just past the last blank line's newline.
         (run-end (lambda (i)
                    (let loop ((ls i))
                      (let ((next (+ (line-end-pos ls) 1)))
                        (cond ((> next mx) mx)
                              ((and (< next mx) (blank-line-at? next)) (loop next))
                              (else next)))))))
    (if (blank-line-at? (point))
        (let* ((s (let loop ((ls (line-start-pos (point))))
                    (if (and (> ls 0) (blank-line-at? (- ls 1)))
                        (loop (line-start-pos (- ls 1)))
                        ls)))
               (e (run-end s)))
          ;; One blank line → remove it; a run → leave a single blank line.
          (replace-region s e (if (> e (min mx (+ (line-end-pos s) 1))) "\n" "")))
        (let ((next (+ (line-end-pos (point)) 1)))
          (when (and (< next mx) (blank-line-at? next))
            (delete-region next (run-end next)))))))

;; --- fill-paragraph (M-q): re-wrap text to *fill-column* ---------------------
;; With an active region every paragraph in it is re-wrapped (fill-region);
;; otherwise the blank-line-delimited paragraph at point is. The wrap engine
;; (fill-string / fill-text, indentation- and comment-leader-aware) and
;; *fill-column* live in core.scm so the evil/helix gq operators share them.
(define (fill-region)
  (let* ((r (region-or (point) (point)))
         (s (line-start-pos (car r)))
         (e (line-end-pos (max (car r) (- (cadr r) 1)))))
    (when (< s e)
      (replace-region s e (fill-text (buffer-substring s e) *fill-column*)))))
(define (fill-paragraph)
  (if (region-active?)
      (fill-region)
      (let ((b (paragraph-bounds (point))))
        (when b
          (replace-region (car b) (cadr b)
                          (fill-string (buffer-substring (car b) (cadr b)) *fill-column*))))))

;; M-x set-fill-column — read a new wrap column. (Emacs's C-x f; that chord is
;; find-file here, so M-x only.)
(define (set-fill-column) (read-string "Fill column: " 'set-fill-column-finish))
(define (set-fill-column-finish s)
  (let ((n (string->number (trim-end s))))
    (when (and n (> n 0))
      (set! *fill-column* n)
      (message (string-append "Fill column set to " (number->string n))))))

;; --- keymap + M-x registration ----------------------------------------------
(define-key "fundamental" "alt-g g" 'goto-line-cmd)
(define-key "fundamental" "alt-^" 'join-line)
(define-key "fundamental" "alt-z" 'zap-to-char)
(define-key "fundamental" "ctrl-x ctrl-t" 'transpose-lines)
(define-key "fundamental" "alt-q" 'fill-paragraph)
(define-key "fundamental" "alt-u" 'upcase-word)
(define-key "fundamental" "alt-l" 'downcase-word)
(define-key "fundamental" "alt-c" 'capitalize-word)
(define-key "fundamental" "alt-t" 'transpose-words)
(define-key "fundamental" "alt-m" 'back-to-indentation)
(define-key "fundamental" "alt-{" 'backward-paragraph)
(define-key "fundamental" "alt-}" 'forward-paragraph)
(define-key "fundamental" "alt-h" 'mark-paragraph)
(define-key "fundamental" "alt-space" 'just-one-space)
(define-key "fundamental" "ctrl-x ctrl-o" 'delete-blank-lines)

(register-command 'sort-lines)
(register-command 'upcase-region)
(register-command 'downcase-region)
(register-command 'capitalize-region)
(register-command 'goto-line-cmd)
(register-command 'join-line)
(register-command 'zap-to-char)
(register-command 'transpose-lines)
(register-command 'fill-paragraph)
(register-command 'fill-region)
(register-command 'set-fill-column)
(register-command 'upcase-word)
(register-command 'downcase-word)
(register-command 'capitalize-word)
(register-command 'transpose-words)
(register-command 'back-to-indentation)
(register-command 'forward-paragraph)
(register-command 'backward-paragraph)
(register-command 'mark-paragraph)
(register-command 'just-one-space)
(register-command 'delete-blank-lines)
