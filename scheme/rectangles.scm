;; requires: core

;; rectangles.scm — Emacs rectangles (`rect.el`). The rectangle's corners are
;; the mark and point; its edges are DISPLAY columns, so a tab counts to its
;; tab stop and a wide character counts two. Where an edge cuts through a tab
;; or wide char, that char is split into spaces (Emacs's `move-to-column` with
;; FORCE). Every command needs an active region (C-SPC or C-x SPC, then move).
;;
;;   C-x r k   kill-rectangle           C-x r d   delete-rectangle
;;   C-x r y   yank-rectangle           C-x r o   open-rectangle
;;   C-x r c   clear-rectangle          C-x r t   string-rectangle
;;   C-x r N   rectangle-number-lines   C-x r M-w copy-rectangle-as-kill
;;   C-x SPC   rectangle-mark-mode: show the region as a column block; while
;;             it is on, C-w kills, M-w copies and C-t string-replaces the
;;             rectangle (the editor redirects them; see src/editor/rectangle.rs).
;;
;; The last killed rectangle lives in `*killed-rectangle*` (Emacs
;; `killed-rectangle`) — a list of lines — and is also put on the clipboard as
;; newline-joined text. registers.scm stores rectangles with
;; `rectangle-extract` and puts them back with `insert-rectangle`.
;;
;; Every edit is computed from one snapshot of the buffer and applied bottom-up
;; (so earlier rows' offsets stay valid) inside one undo step.

;; The last killed or copied rectangle: a list of lines, top first.
(define *killed-rectangle* '())

;; The tab width columns are measured with (the `tab-width` option, default 4).
(define (rect--tab)
  (let ((t (get-option "tab-width")))
    (if (and (integer? t) (> t 0)) t 4)))

(define (rect--spaces n) (if (> n 0) (string-repeat " " n) ""))

;; How (0-based) `line` meets columns [left, right): see `rectangle-row`.
(define (rect--row line left right) (rectangle-row line left right (rect--tab)))
(define (rect--start r) (list-ref r 0))
(define (rect--end r) (list-ref r 1))
(define (rect--before r) (list-ref r 2))
(define (rect--after r) (list-ref r 3))
(define (rect--pad r) (list-ref r 4))
(define (rect--inside r) (list-ref r 5))

;; The region's rectangle as (top bottom left right) — rows inclusive, display
;; columns [left, right) — or #f without an active region.
(define (rectangle-bounds)
  (if (not (region-active?))
      #f
      (let* ((sels (selections)) (mark (car sels)) (pt (cadr sels)) (tab (rect--tab))
             (lm (line-of-pos mark)) (lp (line-of-pos pt))
             (cm (display-column mark tab)) (cp (display-column pt tab)))
        (list (min lm lp) (max lm lp) (min cm cp) (max cm cp)))))

;; Call (f top bottom left right) on the region's rectangle, or complain.
(define (rect--with-bounds f)
  (let ((b (rectangle-bounds)))
    (if b
        (apply f b)
        (message "The mark is not set now, so there is no region"))))

;; Rows top..bottom mapped through (f row), top first.
(define (rect--rows top bottom f)
  (let loop ((row bottom) (acc '()))
    (if (< row top) acc (loop (- row 1) (cons (f row) acc)))))

;; Apply EDITS — (start end text) triples or #f, top row first — bottom-up as
;; one undo step. In a narrowed buffer (C-x n n) a row whose span reaches
;; outside the accessible part is left alone.
(define (rect--apply edits)
  (let ((lo (point-min)) (hi (point-max)))
    (undo-amalgamate-begin)
    (for-each (lambda (e)
                (when (and e (>= (car e) lo) (<= (cadr e) hi))
                  (replace-region (car e) (cadr e) (caddr e))))
              (reverse edits))
    (undo-amalgamate-end)))

;; Where pre-edit offset `pos` ends up once EDITS are applied: shifted by every
;; edit lying wholly before it.
(define (rect--shift edits pos)
  (let loop ((es (rect--accessible edits)) (p pos))
    (cond ((null? es) p)
          ((and (car es) (< (car (car es)) pos) (<= (cadr (car es)) pos))
           (let ((e (car es)))
             (loop (cdr es) (+ p (- (utf8-length (caddr e)) (- (cadr e) (car e)))))))
          (else (loop (cdr es) p)))))

;; EDITS without those `rect--apply` skips (outside the narrowing).
(define (rect--accessible edits)
  (let ((lo (point-min)) (hi (point-max)))
    (map (lambda (e) (and e (>= (car e) lo) (<= (cadr e) hi) e)) edits)))

;; The total length change EDITS make.
(define (rect--delta edits)
  (apply + (map (lambda (e) (- (utf8-length (caddr e)) (- (cadr e) (car e))))
                (filter (lambda (e) e) (rect--accessible edits)))))

;; The pre-edit offset of display column `col` on `row` (or the row's end
;; when it is shorter).
(define (rect--col-pos row col)
  (let ((r (rect--row row col col)))
    (+ (rect--start r) (if (> (rect--pad r) 0) 0 (rect--before r)))))

;; The rectangle's text: one string per row, top first, each exactly
;; `right - left` columns wide (short lines are space-padded).
(define (rectangle-extract top bottom left right)
  (rect--rows top bottom (lambda (row) (rect--inside (rect--row row left right)))))

;; Edits that delete the rectangle (cut tabs / wide chars leave spaces).
(define (rect--delete-edits top bottom left right)
  (rect--rows top bottom
              (lambda (row)
                (let ((r (rect--row row left right)))
                  (and (< (rect--start r) (rect--end r))
                       (list (rect--start r) (rect--end r)
                             (rect--spaces (+ (rect--before r) (rect--after r)))))))))

;; Delete the rectangle and leave point at its top-left corner.
(define (rect--delete top bottom left right)
  (let* ((corner (rect--col-pos top left))
         (edits (rect--delete-edits top bottom left right)))
    (rect--apply edits)
    (goto-char (rect--shift edits corner))))

;; Remember LINES as the killed rectangle (and put them on the clipboard).
(define (rect--save-kill lines)
  (set! *killed-rectangle* lines)
  (clipboard-set (join-lines lines)))

;; C-x r k — delete the rectangle, saving it as the last killed rectangle.
(define-command (kill-rectangle)
  (rect--with-bounds
   (lambda (top bottom left right)
     (rect--save-kill (rectangle-extract top bottom left right))
     (rect--delete top bottom left right))))

;; C-x r d — delete the rectangle without saving it.
(define-command (delete-rectangle)
  (rect--with-bounds rect--delete))

;; C-x r M-w — save the rectangle as the last killed one, leaving the text.
(define-command (copy-rectangle-as-kill)
  (rect--with-bounds
   (lambda (top bottom left right)
     (rect--save-kill (rectangle-extract top bottom left right))
     (deactivate-mark)
     (message "Rectangle copied"))))

;; Insert LINES as a rectangle whose top-left corner is point: line i goes on
;; the i-th row down at point's column (short rows are space-padded; rows past
;; the end of the buffer are added). Point ends after the last line.
(define (insert-rectangle lines)
  (unless (null? lines)
    (let* ((p (point)) (tab (rect--tab))
           (row0 (line-of-pos p)) (col (display-column p tab))
           ;; Rows past the narrowing's end count as missing and are added at
           ;; its end. When it ends at a line start, that line is outside too
           ;; (unless it is the buffer's last, empty line).
           (pm (point-max))
           (open-end (= (line-end-pos pm) pm))
           (nrows (+ (line-of-pos pm) (if open-end 1 0))) (n (length lines))
           ;; Lines that land on existing rows, and those that need new rows.
           (have (max 0 (min n (- nrows row0))))
           (edits (let loop ((i 0) (ls lines) (acc '()))
                    (if (or (>= i have) (null? ls))
                        (reverse acc)
                        (let ((r (rect--row (+ row0 i) col col)))
                          (loop (+ i 1) (cdr ls)
                                (cons (list (rect--start r) (rect--end r)
                                            (string-append
                                             (rect--spaces (+ (rect--pad r) (rect--before r)))
                                             (car ls)
                                             (rect--spaces (rect--after r))))
                                      acc))))))
           (extra (list-tail lines have))
           (tail (apply string-append
                        (map (lambda (l)
                               (if open-end
                                   (string-append "\n" (rect--spaces col) l)
                                   (string-append (rect--spaces col) l "\n")))
                             extra))))
      (undo-amalgamate-begin)
      (unless (null? extra) (insert-at pm tail))
      (rect--apply edits)
      (undo-amalgamate-end)
      (if (null? extra)
          (let ((last (list-ref edits (- (length edits) 1))))
            (goto-char (+ (rect--shift edits (car last)) (utf8-length (caddr last)))))
          ;; Every edit lies before the added rows: point lands at the very end.
          (goto-char (+ pm (rect--delta edits) (utf8-length tail) (if open-end 0 -1)))))))

;; C-x r y — insert the last killed rectangle at point.
(define-command (yank-rectangle)
  (if (null? *killed-rectangle*)
      (message "No rectangle to yank")
      (insert-rectangle *killed-rectangle*)))

;; C-x r o — insert blank space filling the rectangle, shifting text right.
;; Lines that end before the left edge are left alone.
(define-command (open-rectangle)
  (rect--with-bounds
   (lambda (top bottom left right)
     (let* ((corner (rect--col-pos top left))
            (edits (rect--rows top bottom
                               (lambda (row)
                                 (let ((r (rect--row row left left)))
                                   (and (< (rect--start r) (line-end-pos (rect--start r)))
                                        (list (rect--start r) (rect--end r)
                                              (rect--spaces (+ (rect--before r) (- right left)
                                                               (rect--after r))))))))))
       (rect--apply edits)
       (goto-char (rect--shift edits corner))))))

;; C-x r c — blank out the rectangle with spaces. A line ending inside it is
;; just cut at the left edge (no trailing whitespace), as in Emacs.
(define-command (clear-rectangle)
  (rect--with-bounds
   (lambda (top bottom left right)
     (let* ((corner (rect--col-pos top left))
            (edits (rect--rows top bottom
                               (lambda (row)
                                 (let* ((r (rect--row row left right))
                                        (s (rect--start r)) (e (rect--end r)))
                                   (and (< s e)
                                        (list s e
                                              (if (and (= (rect--after r) 0) (= e (line-end-pos s)))
                                                  (rect--spaces (rect--before r))
                                                  (rect--spaces (+ (rect--before r) (- right left)
                                                                   (rect--after r)))))))))))
       (rect--apply edits)
       (goto-char (rect--shift edits corner))))))

;; Replace every row of the rectangle with the result of (text-for i), i
;; counting rows from 0; short rows are padded out to the left edge first.
;; Point ends after the last row's text.
(define (rect--replace-rows top bottom left right text-for)
  (let* ((edits (let loop ((row top) (acc '()))
                  (if (> row bottom)
                      (reverse acc)
                      (let ((r (rect--row row left right)))
                        (loop (+ row 1)
                              (cons (list (rect--start r) (rect--end r)
                                          (string-append
                                           (rect--spaces (+ (rect--pad r) (rect--before r)))
                                           (text-for (- row top))
                                           (rect--spaces (rect--after r))))
                                    acc))))))
         (last (list-ref edits (- (length edits) 1))))
    (rect--apply edits)
    (goto-char (+ (rect--shift edits (car last))
                  (- (utf8-length (caddr last))
                     (rect--after (rect--row bottom left right)))))))

;; The rectangle `string-rectangle` is about to replace (bounds are taken
;; before the minibuffer opens).
(define *rect-pending* #f)

;; C-x r t — replace each row of the rectangle with a string read from the
;; minibuffer (a zero-width rectangle inserts it on every row).
(define-command (string-rectangle)
  (let ((b (rectangle-bounds)))
    (if (not b)
        (message "The mark is not set now, so there is no region")
        (begin
          (set! *rect-pending* b)
          (read-string "String rectangle: " 'string-rectangle-finish)))))

(define (string-rectangle-finish s)
  (let ((b *rect-pending*))
    (set! *rect-pending* #f)
    (when b
      (apply rect--replace-rows (append b (list (lambda (i) s)))))))

;; C-x r N — number the rectangle's lines from 1, inserting "N " at its left
;; edge (numbers right-aligned to the widest).
(define-command (rectangle-number-lines)
  (rect--with-bounds
   (lambda (top bottom left right)
     (let ((width (string-length (number->string (+ 1 (- bottom top))))))
       (rect--replace-rows top bottom left left
                           (lambda (i)
                             (let ((n (number->string (+ i 1))))
                               (string-append (rect--spaces (- width (string-length n))) n " "))))))))

(define-key "fundamental" "ctrl-x r k" 'kill-rectangle)
(define-key "fundamental" "ctrl-x r d" 'delete-rectangle)
(define-key "fundamental" "ctrl-x r y" 'yank-rectangle)
(define-key "fundamental" "ctrl-x r o" 'open-rectangle)
(define-key "fundamental" "ctrl-x r c" 'clear-rectangle)
(define-key "fundamental" "ctrl-x r t" 'string-rectangle)
(define-key "fundamental" "ctrl-x r shift-n" 'rectangle-number-lines)
(define-key "fundamental" "ctrl-x r alt-w" 'copy-rectangle-as-kill)
(define-key "fundamental" "ctrl-x space" 'rectangle-mark-mode)
