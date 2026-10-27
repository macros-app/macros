;; requires: core

;; expand-region — grow the selection outward along tree-sitter node boundaries
;; (word → expression → statement → block → …), the Macros port of
;; expand-region.el. Unlike the LSP-backed `lsp-expand-selection`, this works in
;; ANY buffer with a tree-sitter grammar — no language server required.
;;
;;   C-=    er/expand     widen selection to the enclosing syntax node
;;   C--    er/contract   step back to the previous (narrower) selection
;;
;; Built on `tree-sitter-node-at`, which returns the enclosing node chain
;; (innermost → outermost) as "kind\tstart\tend" lines. We pick the smallest
;; node that strictly contains the current selection.

(define *er-history* '())     ; stack of (start end) we've expanded through

;; Current selection as (start end): the active region, else point/point.
(define (er--current)
  (if (region-active?)
      (list (region-beginning) (region-end))
      (list (point) (point))))

;; Byte to query the node chain at: region start when active, else point.
(define (er--query-byte)
  (if (region-active?) (region-beginning) (point)))

(define (er--parse-node rec)
  (let ((parts (split-many rec "\t")))
    (if (>= (length parts) 3)
        (list (string->number (nth parts 1))      ; start byte
              (string->number (nth parts 2)))     ; end byte
        #f)))

;; Callback (run by tree-sitter-node-at): pick the smallest enclosing node
;; strictly larger than the current selection and select it, remembering where
;; we came from so er/contract can step back.
(define (er--apply-expand records)
  (let* ((cur (er--current))
         (cs (nth cur 0))
         (ce (nth cur 1))
         (nodes (filter (lambda (x) x)
                        (map er--parse-node (non-empty-lines records))))
         ;; chain is innermost→outermost, so the first strictly-bigger node is
         ;; the smallest one that grows the selection.
         (bigger (filter (lambda (n)
                           (and (<= (nth n 0) cs) (>= (nth n 1) ce)
                                (or (< (nth n 0) cs) (> (nth n 1) ce))))
                         nodes)))
    (if (null? bigger)
        (message "expand-region: no larger node")
        (let ((target (car bigger)))
          (set! *er-history* (cons (list cs ce) *er-history*))
          (set-region (nth target 0) (nth target 1))))))

;; C-= : expand the region to the enclosing syntax node. Expanding from a fresh
;; (inactive) region resets the history so a stale stack can't replay.
(define (er/expand)
  (when (not (region-active?)) (set! *er-history* '()))
  (tree-sitter-node-at (er--query-byte) 'er--apply-expand))

;; C-- : shrink back to the previous selection on the expansion stack.
(define (er/contract)
  (if (null? *er-history*)
      (message "expand-region: nothing to contract")
      (let ((prev (car *er-history*)))
        (set! *er-history* (cdr *er-history*))
        (if (< (nth prev 0) (nth prev 1))
            (set-region (nth prev 0) (nth prev 1))
            (goto-char (nth prev 0))))))

(define-key "fundamental" "ctrl-=" 'er/expand)
(define-key "fundamental" "ctrl--" 'er/contract)   ; M-= is count-words-region
;; Evil: keep growing with `+` / shrinking with `-` in visual state (harmless
;; if evil-mode is never enabled — it's just an unused keymap entry).
(define-key "evil-visual" "+" 'er/expand)
(define-key "evil-visual" "-" 'er/contract)
;; Helix has no separate visual state — selections live in Normal — so its
;; expand/shrink go on the Normal map, where Helix itself puts them (M-o / M-i).
;; Without this, expand-region was unreachable under helix-mode.
(define-key "helix-normal" "alt-o" 'er/expand)
(define-key "helix-normal" "alt-i" 'er/contract)

(register-command 'er/expand)
(register-command 'er/contract)
