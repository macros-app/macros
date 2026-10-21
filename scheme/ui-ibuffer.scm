;; requires: core fundamental tabulated

;; ibuffer — a tabulated buffer manager, the Macros port of Emacs `ibuffer`.
;; Lists every open buffer with its type; RET switches to it, d/x kills it, g
;; refreshes. Built entirely on tabulated-list (scheme/tabulated.scm) — no new
;; Rust. Bound to C-x C-b (the traditional list-buffers key).

;; Buffer names are (possibly uniquified `name<dir>`) basenames, so classify by
;; the buffer's visited path; fileless buffers have "".
(define (ibuffer-type name)
  (let ((path (buffer-file-name-of name)))
    (cond ((equal? path "") "special")
          ((file-directory? path) "dir")
          ((file-exists? path) "file")
          (else "special"))))

;; Buffers killed from within this view. `kill-buffer-named` is queued, so the
;; `buffer-names` snapshot in the same command still lists the killed buffer —
;; we filter it out here so it disappears immediately. Cleared on (re)open and
;; on `g`: a modified file buffer's kill waits on a yes/no prompt, and if that's
;; declined the buffer must come back on the next refresh.
(define *ibuffer-killed* '())

(define (ibuffer-rows)
  (map (lambda (n) (list n (list n (ibuffer-type n))))
       (filter (lambda (n) (not (member n *ibuffer-killed*))) (buffer-names))))

;; (Re)draw the *Ibuffer* list and (idempotently) install its keys.
(define (ibuffer-refresh)
  (tabulated-render
    "*Ibuffer*" "ibuffer-mode"
    (list (list "Buffer" 56) (list "Type" #f))
    (ibuffer-rows)
    (lambda (id) (switch-to-buffer id)))
  (tabulated-setup-keys "ibuffer-mode")
  (define-key "ibuffer-mode" "d" 'ibuffer-kill)
  (define-key "ibuffer-mode" "x" 'ibuffer-kill)
  (define-key "ibuffer-mode" "g" 'ibuffer-revert))

;; g : forget optimistic kills (the snapshot has caught up by now) and redraw.
(define (ibuffer-revert)
  (set! *ibuffer-killed* '())
  (ibuffer-refresh))

;; d / x : kill the buffer on the current line, then refresh in place.
(define (ibuffer-kill)
  (let ((row (tabulated-row-at-point)))
    (when row
      (kill-buffer-named (row-id row))
      (set! *ibuffer-killed* (cons (row-id row) *ibuffer-killed*))
      (ibuffer-refresh))))

;; Open the buffer manager (fresh — forget any prior kills).
(define (ibuffer)
  (set! *ibuffer-killed* '())
  (ibuffer-refresh))

(define-key "fundamental" "ctrl-x ctrl-b" 'ibuffer)
(register-command 'ibuffer)
