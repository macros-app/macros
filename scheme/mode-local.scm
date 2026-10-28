;; requires: core
;;
;; mode-local.scm — mode-scoped variables (Emacs `mode-local.el` subset).
;; Doc: web/content/docs/mode-local.md
;;
;; A mode-local variable holds ONE value per major mode, shared across every
;; buffer running that mode (Emacs semantics — NOT per-buffer; pair with
;; `buffer-state-get`/`buffer-state-set` in scheme/core.scm when a buffer needs
;; its own override). The intended use is exactly the setup-function pattern:
;; set once from user config (load-order safe — the table exists whether or
;; not the mode has been defined yet), read while the mode is active.
;;
;; Inheritance: `define-derived-mode` (scheme/derived-mode.scm) registers the
;; child→parent edge via `mode-local--set-parent`, so `mode-local-value` walks
;; the chain and a child inherits every value it has not set itself. The walk
;; carries a visited list, so a hand-registered cycle just stops the lookup
;; instead of looping forever.
;;
;; Values are stored WRAPPED in a list ((var . (value))) so an explicitly-set
;; #f stays distinguishable from "not set" — same tri-state gotcha as
;; minor-mode.scm's state. Mode names are strings or symbols — the
;; setters/getters normalize.
;;
;; Deviation from Emacs: there is no `defvar-mode-local` variable-splicing (a
;; variable's value is read through `mode-local-value`, not as a bare
;; identifier), and no mode-hierarchy inference beyond the parent edges the
;; derived-mode framework records.

;; --- registry -----------------------------------------------------------------
;; mode-name(string) -> ((var . value) ...); parents: mode -> parent mode name.
(define *mode-local-table* '())
(define *mode-local-parents* '())

(define (mode-local--name m)
  (cond ((symbol? m) (symbol->string m))
        ((string? m) m)
        (else (error "mode-local: mode name must be a string or symbol:" m))))

;; Functional rebuild — no in-place mutation of shared cells (same rule as
;; minor-mode.scm's state handling).
(define (mode-local--set! table var val)
  (let ((rest (filter (lambda (e) (not (eq? (car e) var))) table)))
    (cons (cons var (list val)) rest)))

(define (mode-local-set mode var val)
  (let ((name (mode-local--name mode)))
    (set! *mode-local-table*
          (cons (cons name (mode-local--set!
                            (mode-local--own-table name) var val))
                (filter (lambda (e) (not (string=? (car e) name)))
                        *mode-local-table*)))))

(define (mode-local--own-table name)
  (let loop ((entries *mode-local-table*))
    (cond ((null? entries) '())
          ((string=? (caar entries) name) (cdar entries))
          (else (loop (cdr entries))))))

;; Registered by define-derived-mode's expansion. Public-ish so the macro can
;; call it across the module boundary; not meant for hand use.
(define (mode-local--set-parent child parent)
  (let ((child (mode-local--name child)) (parent (mode-local--name parent)))
    (set! *mode-local-parents*
          (cons (cons child parent)
                (filter (lambda (e) (not (string=? (car e) child)))
                        *mode-local-parents*)))))

(define (mode-local-parent mode)
  (let ((name (mode-local--name mode)))
    (let loop ((entries *mode-local-parents*))
      (cond ((null? entries) #f)
            ((string=? (caar entries) name) (cdar entries))
            (else (loop (cdr entries)))))))

;; The value THIS mode set itself, without walking the parent chain.
;; Returns 'unset when the mode has no binding for var (an explicitly-set #f
;; is a real hit — values are stored wrapped, see the header).
(define (mode-local--lookup1 name var)
  (let ((cell (assq var (mode-local--own-table name))))
    (cond ((not cell) 'unset)
          (else (car (cdr cell))))))

;; DEFAULT is REQUIRED (a rest-arg default can come back #<void> when omitted
;; — engine quirk); pass 'unset if you want "was it set at all" instead.
;; Walks child → parent → ... with a visited guard against hand-made cycles.
;; The visited list is compared with string=? (a plain memq on strings only
;; matches interned literals — names built at runtime would slip past it).
(define (mode-local-value mode var default)
  (let walk ((name (mode-local--name mode)) (seen '()))
    (let ((hit (mode-local--lookup1 name var)))
      (cond ((not (eq? hit 'unset)) hit)
            ((let seen? ((xs seen))
               (cond ((null? xs) #f)
                     ((string=? name (car xs)) #t)
                     (else (seen? (cdr xs)))))
             default)
            (else
             (let ((parent (mode-local-parent name)))
               (if parent
                   (walk parent (cons name seen))
                   default)))))))

;; --- setq-mode-local ------------------------------------------------------------
;; Emacs-compatible shape: (setq-mode-local MODE (VAR VALUE) ...) — vars are
;; UNEVALUATED symbols, values are evaluated. Multiple bindings per form.
(define-syntax setq-mode-local
  (lambda (stx)
    (syntax-case stx ()
      [(_ mode (var val) rest ...)
       #'(begin (mode-local-set mode 'var val)
                (setq-mode-local mode rest ...))]
      [(_ mode) #'(begin)]
      [(_ mode bad ...)
       (error "setq-mode-local: bindings must be (VAR VALUE) pairs:"
              (syntax->datum #'(bad ...)))])))
