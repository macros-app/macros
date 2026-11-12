;; requires: core

;; Editing commands written in Scheme on the new primitives — comment-dwim.
;; Each command's edits coalesce into one undo group (the editor wraps a
;; command batch), so they undo as a unit.

;; --- comment-dwim -----------------------------------------------------------
;; The comment leader table, `comment-string` (buffer-local "comment-string"
;; override, else by language), `comment-closer` and the `comment-lines` toggle
;; live in core.scm, shared with evil's `gc` operator.

;; M-; — toggle a line comment on the current line (at its indentation). A
;; blank line gets a fresh comment leader.
(define (comment-dwim)
  (let* ((b (line-bounds))
         (s (car b))
         (e (cadr b))
         (line (buffer-substring s e)))
    (if (equal? (string-trim line) "")
        (let ((cs (comment-string)))
          (replace-region s e (string-append line cs (comment-closer cs))))
        (comment-lines s e))))

;; replace-string / query-replace live in replace.scm; occur (and its
;; editable occur-edit mode) in occur.scm.

;; --- keymap -----------------------------------------------------------------
(define-key "fundamental" "alt-;" 'comment-dwim)

;; steel-mode uses ";; " line comments.
(add-hook "steel-mode-hook" "edit-steel-comment-string")
(define (edit-steel-comment-string) (buffer-local-set "comment-string" ";; "))

;; Make the editing entry points M-x discoverable.
(register-command 'comment-dwim)
