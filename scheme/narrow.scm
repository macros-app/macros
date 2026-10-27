;; requires: core

;; Narrowing (Emacs C-x n): restrict a buffer to part of its text. While
;; narrowed only that part shows, motion and search stop at its edges, edits
;; can't reach past them, and `point-min` / `point-max` are its bounds; saving
;; still writes the whole file. The bounds live in Rust (src/editor/narrow.rs);
;; `narrow-to-defun` and `widen` are Rust commands, and org-mode adds
;; `org-narrow-to-subtree` (C-x n s). A grid buffer keeps its own C-x n map
;; (grid-narrow-to-selection and friends in grid.scm), which wins there.

;; (narrow-to-region [start end]) — restrict the buffer to [start, end). With
;; no arguments (M-x, C-x n n), narrow to the active region.
(define (narrow-to-region . args)
  (cond ((= (length args) 2) (buffer-narrow (car args) (cadr args)))
        ((region-active?) (buffer-narrow (region-beginning) (region-end)))
        (else (message "The mark is not set now, so there is no region"))))
(register-command 'narrow-to-region)

(define-key "fundamental" "ctrl-x n n" 'narrow-to-region)
(define-key "fundamental" "ctrl-x n d" 'narrow-to-defun)
(define-key "fundamental" "ctrl-x n w" 'widen)
