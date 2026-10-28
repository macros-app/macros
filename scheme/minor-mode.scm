;; requires: core hooks modeline

;; minor-mode.scm — a `define-minor-mode` framework for Macros.
;; Doc: web/content/docs/minor-mode.md
;;
;; Emacs parity subset: :lighter, :init-value, :keymap, and a toggle body.
;; Modes are buffer-local. `:global`, `:variable`, `:interactive`,
;; `:set`/`:initialize`/`:type`, `:after-hook`, and the pre-Emacs-21
;; positional triplet are NOT supported — see the doc page for why.
;;
;; Task 0 probe outcomes (see src/script/tests.rs :: minor_mode_probes):
;;  - syntax-case works in the embedded engine; identifier SYNTHESIS goes
;;    through with-syntax binding a `string->symbol` VALUE (Steel's
;;    `datum->syntax` is a compile-time identifier-rewrite pass requiring
;;    literal identifier arguments — unusable for computed names).
;;  - Keyword literals read as colon-prefixed symbols (`:lighter` eq
;;    `':lighter`).
;;  - bind-key accepts arbitrary new map names, but a private "<mode>-map" is
;;    only reachable while that buffer's mode stack names it — modal minor-mode
;;    keys therefore install into "evil-normal" + "helix-normal" (spell.scm
;;    pattern), and `<mode>-map` is defined as an introspection alist.

;; --- registry ----------------------------------------------------------------
;; name -> props alist: lighter, init-value, keymap, doc, body thunk.
(define *minor-modes* '())

(define (minor-mode--register! name props)
  (set! *minor-modes*
        (cons (cons name props)
              (filter (lambda (e) (not (eq? (car e) name))) *minor-modes*))))

(define (minor-mode--entry name)
  (let loop ((entries *minor-modes*))
    (cond ((null? entries) #f)
          ((eq? (caar entries) name) (car entries))
          (else (loop (cdr entries))))))

(define (minor-mode--registered? name) (if (minor-mode--entry name) #t #f))

(define (minor-mode--prop name key)
  (let ((e (minor-mode--entry name)))
    (if e (cdr (assq key (cdr e))) #f)))

(define (minor-mode-lighter name)    (minor-mode--prop name 'lighter))
(define (minor-mode-init-value name) (minor-mode--prop name 'init-value))

;; --- per-buffer state ----------------------------------------------------------
;; buffer -> ((name (on?))). Values are WRAPPED in a list so an explicit #f is
;; distinguishable from "no entry" (both falsy) — the tri-state gotcha.
(define *minor-mode-state* '())

(define (minor-mode--on? name)
  (let ((cell (assq name (buffer-state-get *minor-mode-state* '()))))
    (if cell (car (cadr cell))
        (if (minor-mode-init-value name) #t #f))))

;; REBUILD the alist functionally — no in-place mutation of cells fetched from
;; buffer-state-get (no aliasing assumption on the stored structure).
(define (minor-mode--set-state! name on?)
  (let* ((alist (buffer-state-get *minor-mode-state* '()))
         (rest  (filter (lambda (c) (not (eq? (car c) name))) alist)))
    (set! *minor-mode-state*
          (buffer-state-set *minor-mode-state*
                            (cons (list name (list on?)) rest)))))

;; --- helpers used by the macro expansion --------------------------------------
(define (minor-mode--kwval kws key default)
  (cond ((null? kws) default)
        ((eq? (caar kws) key) (cadar kws))
        (else (minor-mode--kwval (cdr kws) key default))))

;; The user-visible hook name for a mode (hooks.scm keys on strings).
(define (minor-mode-hook-name name)
  (string-append (symbol->string name) "-hook"))

;; Key bindings: modal minor-mode chords go on BOTH Normal maps (evil and
;; helix have separate keymaps — see spell.scm). KEYS ONLY; the lighter slot
;; is registered separately so major modes can reuse this helper.
(define *minor-mode-key-targets* '("evil-normal" "helix-normal"))
(define (minor-mode--install-keys name keymap)
  (unless (null? keymap)
    (define-keys-in *minor-mode-key-targets*
      (map (lambda (b) (list (car b) (cadr b))) keymap))))

;; Run a :keymap command from the gated wrapper. Command resolution must be
;; KEYpress-time (the symbol may not be defined yet at expansion), so symbols
;; and plain strings go through eval-string — same trust level as bind-key's
;; string form. %c command strings are refused: the substitution happens in the
;; key-dispatch layer BEFORE the bound string is evaluated, so a thunk wrapper
;; would see the literal %c and eval garbage.
(define (minor-mode--invoke cmd)
  (cond ((symbol? cmd)
         (eval-string (string-append "(" (symbol->string cmd) ")")))
        ((and (string? cmd) (not (string-contains? cmd "%c")))
         (eval-string (string-append "(" cmd ")")))
        (else
         (error "minor-mode :keymap: use a 'symbol command (or a %c-free string); wrap %c forms in a command:"
                cmd))))

;; GATED installer for minor-mode chords: each chord binds a thunk that no-ops
;; unless the mode is ON in the current buffer (minor-mode--on? reads per-buffer
;; state). This is what makes "dispatched only while the mode is on" true;
;; `minor-mode--install-keys` above stays UNGATED because derived (major) modes
;; reuse it and a major mode's keys must not consult minor-mode state.
(define (minor-mode--install-gated-keys name keymap)
  (unless (null? keymap)
    (for-each
     (lambda (b)
       (define-keys-in *minor-mode-key-targets*
         (list (list (car b)
                     (lambda ()
                       (when (minor-mode--on? name)
                         (minor-mode--invoke (cadr b))))))))
     keymap)))

;; Lighter slot: minor modes only. Registered once per mode at define time.
(define (minor-mode--register-lighter name)
  (mode-line-add-segment (ml-slot* (symbol->string name) "" "  " "")))

(define (minor-mode--update-lighter name)
  (set-mode-line-slot (symbol->string name)
                      (if (minor-mode--on? name)
                          (minor-mode-lighter name)
                          "")))

;; Keep the lighter honest across buffers (slots are focus-scoped globals).
(define (minor-mode--buffer-switch)
  (for-each
   (lambda (e) (minor-mode--update-lighter (car e)))
   *minor-modes*))
(add-hook "buffer-switch-hook" "minor-mode--buffer-switch")

;; Emacs arg semantics: no arg = toggle; #t or positive number = on; #f or
;; non-positive number = off. Order per call: set state, run body (ONLY on an
;; actual flip — DEVIATION from Emacs, which runs the body on every invocation;
;; use MODE-hook for unconditional per-invocation work), update the lighter,
;; run the mode hook, message.
(define (minor-mode--toggle name arg)
  (let* ((on? (if (null? arg)
                  (not (minor-mode--on? name))
                  (let ((a (car arg)))
                    (cond ((eq? a #f) #f)
                          ((number? a) (> a 0))
                          (else #t)))))
         (was (minor-mode--on? name)))
    (minor-mode--set-state! name on?)
    (unless (equal? on? was)
      (let ((body (minor-mode--prop name 'body)))
        (when body (body))))
    (minor-mode--update-lighter name)
    (run-hooks (minor-mode-hook-name name))
    (message (string-append (symbol->string name)
                            (if on? " enabled" " disabled")))
    on?))

;; --- the macro ----------------------------------------------------------------
;; Grammar: (define-minor-mode NAME DOC [:lighter S] [:init-value B]
;;                                [:keymap ((CHORD . CMD) ...)] [BODY ...])
;; Expansion shape (map-id/hook-id are the ONLY synthesized identifiers):
;;   (begin
;;     (define NAME-map <keymap datum>)             ; introspection alist
;;     (define NAME-hook #f)                        ; parity binding; the LIVE
;;                                                  ; hook key is the STRING
;;     (minor-mode--install-gated-keys 'NAME '<keymap>)
;;     (minor-mode--register-lighter 'NAME)
;;     (minor-mode--register! 'NAME props-with-body-thunk)
;;     (define-command (NAME . arg) (minor-mode--toggle 'NAME arg)))
;;
;; Everything inside (define-syntax ...) except the final #` template runs at
;; EXPANSION time. A bare `:kw` in Steel reads as an ordinary SYMBOL whose name
;; includes the colon, so `:lighter` and `':lighter` are eq, and any
;; colon-prefixed symbol that survives peeling is an UNSUPPORTED keyword —
;; loud error at expansion, not silent body forms.
(define-syntax define-minor-mode
  (lambda (stx)
    (syntax-case stx ()
      [(_ name doc rest ...)
       (let* ((mode-name (symbol->string (syntax->datum #'name)))
              ;; syntax->datum is SHALLOW: nested lists (e.g. the :keymap
              ;; alist) stay Syntax-wrapped, and dotted pairs cannot appear
              ;; in macro arguments at all (the expander refuses improper
              ;; lists) — hence :keymap bindings are 2-lists (CHORD COMMAND).
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
                                    (memq (car items)
                                          '(:lighter :init-value :keymap))
                                    (pair? (cdr items)))
                               (peel (cddr items)
                                     (cons (list (car items) (cadr items)) kws)
                                     body))
                              ((memq (car items) '(:lighter :init-value :keymap))
                               (error "define-minor-mode: keyword needs a value:"
                                      (car items)))
                              ((and (pair? items)
                                    (symbol? (car items))
                                    (starts-with? (symbol->string (car items)) ":"))
                               ;; Keeps scanning THROUGH the body, so a stray
                               ;; keyword after the first body form still errors
                               ;; loudly instead of being absorbed as body.
                               (error "define-minor-mode: unsupported keyword:"
                                      (car items)))
                              (else (peel (cdr items) kws (cons (car items) body))))))
              (body (car parsed))
              (kws (cdr parsed))
              (doc-str (syntax->datum #'doc)))
         ;; Expander-time code can only call PRIMITIVES — module defines
         ;; don't exist yet at macro-expansion (compile) time.
         (define (kwval kws key default)
           (cond ((null? kws) default)
                 ((eq? (caar kws) key) (cadar kws))
                 (else (kwval (cdr kws) key default))))
         (unless (string? doc-str)
           (error "define-minor-mode: DOC must be a string:" doc-str))
         (let ((lighter (kwval kws ':lighter ""))
               (init-value (kwval kws ':init-value #f))
               (keymap (kwval kws ':keymap '()))
               (name-sym (syntax->datum #'name)))
           ;; The whole expansion is a constructed datum returned from the
           ;; transformer — defines land in the caller's scope (probe:
           ;; syntax_probe2 :: probe_whole_datum_splice). A procedural-macro
           ;; template cannot contain a dotted list, so the toggle command is
           ;; (lambda arg …) — rest-args — plus an explicit register-command,
           ;; exactly what define-command expands to.
           (list 'begin
                 (list 'define
                       (string->symbol (string-append mode-name "-map"))
                       (list 'quote keymap))
                 (list 'define
                       (string->symbol (string-append mode-name "-hook"))
                       #f)
                 (list 'minor-mode--install-gated-keys (list 'quote name-sym)
                       (list 'quote keymap))
                 (list 'minor-mode--register-lighter (list 'quote name-sym))
                 (list 'minor-mode--register!
                       (list 'quote name-sym)
                       (list 'list
                             (list 'cons ''lighter lighter)
                             (list 'cons ''init-value init-value)
                             (list 'cons ''keymap (list 'quote keymap))
                             (list 'cons ''doc doc-str)
                             (list 'cons ''body
                                   (cons 'lambda
                                         (cons '()
                                               (append body (list #f)))))))
                 (list 'define name-sym
                       (list 'lambda 'arg
                             (list 'minor-mode--toggle
                                   (list 'quote name-sym) 'arg)))
                 (list 'register-command (list 'quote name-sym)))))])))

;; Public: is NAME on in the current buffer? Other modules gate their hooks
;; and key handlers on this.
(define (minor-mode-enabled? name) (minor-mode--on? name))
