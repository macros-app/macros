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
;; Inheritance: the parent edges live in the editor's mode registry
;; (src/mode.rs, filled by `register-mode` — `define-derived-mode` and the
;; bundled language modes call it). `mode-local-value` walks them through the
;; `mode-parent` primitive, so a child inherits every value it has not set
;; itself, and every registered major mode ends the chain at "fundamental".
;; The walk carries a visited list, so a hand-registered cycle just stops the
;; lookup instead of looping forever.
;;
;; Values are stored WRAPPED in a list ((var . (value))) so an explicitly-set
;; #f stays distinguishable from "not set" — same tri-state gotcha as
;; minor-mode.scm's state. Mode names are strings or symbols — the
;; setters/getters normalize.
;;
;; Deviation from Emacs: there is no `defvar-mode-local` variable-splicing (a
;; variable's value is read through `mode-local-value`, not as a bare
;; identifier), and no mode-hierarchy inference beyond the parent edges the
;; mode registry records.

;; --- registry -----------------------------------------------------------------
;; mode-name(string) -> ((var . value) ...).
(define *mode-local-table* '())

(define (mode-local--name m)
  (cond ((symbol? m) (symbol->string m))
        ((string? m) m)
        ;; setq-mode-local quotes MODE, so `'mylang` arrives as (quote mylang).
        ((and (pair? m) (eq? (car m) 'quote) (pair? (cdr m)))
         (mode-local--name (cadr m)))
        (else (error "mode-local: mode name must be a string or symbol:" m))))

;; Major modes have two spellings: the command ("mylang-mode", what the docs
;; and Emacs use) and the bare name on the stack and in the registry
;; ("mylang"). Values may be stored under either — setq-mode-local is
;; load-order safe, so it can't consult the registry — and lookups try both.
(define (mode-local--bare name)
  (and (ends-with? name "-mode")
       (substring name 0 (- (string-length name) 5))))

;; The registry name for MODE: the bare name when MODE is a registered major
;; mode's command spelling, else MODE itself (minor modes keep "-mode").
(define (mode-local--canonical name)
  (let ((bare (mode-local--bare name)))
    (if (and bare
             (not (mode-parent name))
             (or (mode-parent bare) (string=? bare "fundamental")))
        bare
        name)))

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

;; The registry's parent for MODE ("fundamental" for a root major mode), or
;; #f for fundamental itself and for modes never registered.
(define (mode-local-parent mode)
  (mode-parent (mode-local--canonical (mode-local--name mode))))

(define (mode-local--lookup-own name var)
  (let ((cell (assq var (mode-local--own-table name))))
    (if cell (car (cdr cell)) 'unset)))

;; The value THIS mode set itself, without walking the parent chain, under
;; either spelling of its name (the bare one wins). Returns 'unset when the
;; mode has no binding for var (an explicitly-set #f is a real hit — values
;; are stored wrapped, see the header).
(define (mode-local--lookup1 name var)
  (let ((hit (mode-local--lookup-own name var)))
    (if (and (eq? hit 'unset) (not (ends-with? name "-mode")))
        (mode-local--lookup-own (string-append name "-mode") var)
        hit)))

;; DEFAULT is REQUIRED (a rest-arg default can come back #<void> when omitted
;; — engine quirk); pass 'unset if you want "was it set at all" instead.
;; The EXPLICIT form walks mode → parent → ... with a visited guard against
;; hand-made cycles. The visited list is compared with string=? (a plain memq
;; on strings only matches interned literals — names built at runtime would
;; slip past it).
(define (mode-local--explicit mode var default)
  (let walk ((name (mode-local--canonical (mode-local--name mode))) (seen '()))
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

;; The CURRENT buffer's mode names, bottom-up (snapshot read, space-joined).
(define (mode-local--stack-names)
  (let ((s (mode-stack)))
    (if (equal? s "") '() (split-many s " "))))

;; (mode-local-value VAR DEFAULT) — no mode: search the buffer's mode stack
;; TOP-DOWN. An active minor mode overrides the major; a child overrides its
;; parent (the ancestors are ON the stack, so inheritance is free — the
;; registry walk in the explicit form is only needed when code names a mode
;; directly).
(define (mode-local--stack-value var default)
  (let loop ((names (reverse (mode-local--stack-names))))
    (if (null? names)
        default
        (let ((hit (mode-local--lookup1 (car names) var)))
          (if (eq? hit 'unset)
              (loop (cdr names))
              hit)))))

;; Arity dispatch: (MODE VAR DEFAULT) names a mode (registry parent walk);
;; (VAR DEFAULT) uses the active buffer's stack.
(define (mode-local-value . args)
  (cond ((and (pair? args) (pair? (cdr args)) (null? (cddr args)))
         (mode-local--stack-value (car args) (cadr args)))
        ((and (pair? args) (pair? (cdr args)) (pair? (cddr args))
              (null? (cdddr args)))
         (mode-local--explicit (car args) (cadr args) (caddr args)))
        (else
         (error "mode-local-value: expected (mode-local-value MODE VAR DEFAULT) or (mode-local-value VAR DEFAULT):"
                args))))

;; --- setq-mode-local ------------------------------------------------------------
;; Emacs-compatible shape: (setq-mode-local MODE (VAR VALUE) ...) — MODE and
;; the vars are UNEVALUATED (a bare `mylang-mode` names the mode rather than
;; evaluating to its command), values are evaluated. MODE may also be a
;; string literal. Multiple bindings per form.
(define-syntax setq-mode-local
  (lambda (stx)
    (syntax-case stx ()
      [(_ mode (var val) rest ...)
       #'(begin (mode-local-set 'mode 'var val)
                (setq-mode-local mode rest ...))]
      [(_ mode) #'(begin)]
      [(_ mode bad ...)
       (error "setq-mode-local: bindings must be (VAR VALUE) pairs:"
              (syntax->datum #'(bad ...)))])))
