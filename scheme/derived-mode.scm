;; requires: core hooks minor-mode mode-local

;; derived-mode.scm — `define-derived-mode` for Macros (Phase 2 of the mode
;; framework; see web/content/docs/derived-mode.md).
;;
;; Major modes are already primitive-level here: `(set-major-mode "name")`
;; swaps the mode stack (src/editor/commands.rs), keymaps resolve by mode-name
;; string, and the EDITOR fires the new mode's hooks itself when it applies
;; the switch (Editor::run_mode_hooks_once — keys "<mode>" AND "<mode>-hook").
;; This macro is the glue: one form defines the `<child>` command, a
;; `<child>-map` introspection alist, key bindings, and the hook ordering.
;;
;; Deviations from Emacs, all consequences of the model above:
;;  - Keymap inheritance is NOT automatic (keymaps resolve per mode-name over
;;    the stack, and there is no keymap-introspection primitive to copy the
;;    parent's bindings). A child binds only its own `:keymap`; repeat shared
;;    chords or put them on the parent.
;;  - The PARENT hook is fired by the macro (the parent is not on the mode
;;    stack after the switch, so the editor would never run it), BEFORE
;;    `(set-major-mode …)` — so parent setup precedes the switch, matching
;;    Emacs's run-mode-hooks order. The CHILD hook must NOT be fired here:
;;    the editor fires it on apply — running it here too would double-fire.
;;  - `:grammar` is deliberately absent: name the mode after the grammar id
;;    (`set-major-mode` attaches a bundled language by name match).
;;  - Mode-local variables (scheme/mode-local.scm): the expansion registers
;;    the child→parent edge, so the child inherits every parent value it has
;;    not set itself.
;;
;; The macro shape follows scheme/minor-mode.scm and its probe-verified Steel
;; constraints: no dotted patterns/improper lists anywhere (keymap bindings
;; are 2-lists), shallow syntax->datum (deep conversion via syntax? checks),
;; primitives only at expansion time, and the whole expansion is a
;; constructed datum returned from the transformer.

(define-syntax define-derived-mode
  (lambda (stx)
    (syntax-case stx ()
      [(_ child parent doc rest ...)
       (let* ((child-name (symbol->string (syntax->datum #'child)))
              (parent-name (symbol->string (syntax->datum #'parent)))
              (items (syntax->datum #'(rest ...)))
              (items (let deep ((x items))
                       (cond ((syntax? x) (deep (syntax->datum x)))
                             ((null? x) '())
                             ((pair? x) (cons (deep (car x)) (deep (cdr x))))
                             (else x))))
              (parsed (let peel ((items items) (kws '()) (body '()))
                        (cond ((null? items)
                               (cons (reverse body) (reverse kws)))
                              ((and (pair? items)
                                    (memq (car items) '(:keymap))
                                    (pair? (cdr items)))
                               (peel (cddr items)
                                     (cons (list (car items) (cadr items)) kws)
                                     body))
                              ((memq (car items) '(:keymap))
                               (error "define-derived-mode: keyword needs a value:"
                                      (car items)))
                              ((and (pair? items)
                                    (symbol? (car items))
                                    (starts-with? (symbol->string (car items)) ":"))
                               ;; Keeps scanning THROUGH the body, so a stray
                               ;; keyword after the first body form still errors
                               ;; loudly instead of being absorbed as body.
                               (error "define-derived-mode: unsupported keyword:"
                                      (car items)))
                              (else (peel (cdr items) kws (cons (car items) body))))))
              (body (car parsed))
              (kws (cdr parsed))
              (doc-str (syntax->datum #'doc)))
         (unless (string? doc-str)
           (error "define-derived-mode: DOC must be a string:" doc-str))
         (define (kwval kws key default)
           (cond ((null? kws) default)
                 ((eq? (caar kws) key) (cadar kws))
                 (else (kwval (cdr kws) key default))))
         (let ((keymap (kwval kws ':keymap '()))
               (child-sym (syntax->datum #'child)))
           (list 'begin
                 (list 'define
                       (string->symbol (string-append child-name "-map"))
                       (list 'quote keymap))
                 (list 'define
                       (string->symbol (string-append child-name "-hook"))
                       #f)
                 (list 'minor-mode--install-keys (list 'quote child-sym)
                       (list 'quote keymap))
                 (list 'define child-sym
                       (list 'lambda 'arg
                             (list 'begin
                                   ;; Parent hook first (see header): the
                                   ;; editor fires the CHILD's hooks itself
                                   ;; when SetMajorMode is applied.
                                   (list 'run-hooks
                                         (string-append parent-name "-hook"))
                                   (list 'set-major-mode child-name)
                                   (cons 'begin
                                         (append body (list #f))))))
                 (list 'mode-local--set-parent child-name parent-name)
                 (list 'register-command (list 'quote child-sym)))))])))

;; --- auto-mode-alist -----------------------------------------------------------
;; Scheme-level file association (Emacs's `auto-mode-alist`): entries are
;; (REGEX . MODE-NAME), walked FIRST-match-wins; `add-auto-mode` prepends so
;; later calls win, preserving Emacs add-to-list priority semantics.
;;
;; REGEX DIALECT: Macros' regexp primitives ride the Rust `regex` crate, which
;; does NOT support Emacs's `'` end-of-string anchor — use `$`
;; (`(add-auto-mode "\\.foo$" "foo-mode")`).
;;
;; One find-file-hook covers the WHOLE alist; buffers with no matching entry
;; pay one string-match per entry, and there is no per-mode hook churn. The
;; mode command runs via eval-string (the same mechanism execute-command uses)
;; because hooks fire by name across the engine boundary.
(define *auto-mode-alist* '())

;; A mode name must be a plain command identifier: it is spliced into
;; `(MODE-NAME)` and handed to eval-string, so anything with parens, quotes or
;; whitespace would execute as code rather than name a command.
(define (auto-mode--valid-name? s)
  (and (string? s)
       (>= (string-length s) 1)
       ;; string-match returns the match position, or -1 when the pattern
       ;; does not occur — -1 is truthy in Scheme, so compare explicitly.
       (< (string-match "[^a-zA-Z0-9_-]" s) 0)))

(define (add-auto-mode pattern mode-name)
  (unless (string? pattern)
    (error "add-auto-mode: pattern must be a string:" pattern))
  ;; NOTE: an invalid regex fails SILENT at match time (string-match returns
  ;; -1 for both no-match and bad-pattern) — that is engine-wide behavior,
  ;; not something this alist can fix.
  (unless (auto-mode--valid-name? mode-name)
    (error "add-auto-mode: mode-name must be a command identifier (a-zA-Z0-9-_):"
           mode-name))
  (set! *auto-mode-alist*
        (cons (cons pattern mode-name) *auto-mode-alist*)))

;; Pure matcher — the testable seam (the live find-file hook can't be driven
;; headlessly).
(define (auto-mode--match path)
  (let loop ((entries *auto-mode-alist*))
    (cond ((null? entries) #f)
          ((>= (string-match (caar entries) path) 0) (cdar entries))
          (else (loop (cdr entries))))))

;; Dispatch for an explicit path — what the hook calls with the buffer's file.
(define (auto-mode--apply-for path)
  (let ((mode (auto-mode--match path)))
    (when mode
      (eval-string (string-append "(" mode ")")))))

(define (auto-mode--maybe)
  (let ((path (buffer-file-name)))
    (when path (auto-mode--apply-for path))))
(add-hook "find-file-hook" "auto-mode--maybe")
