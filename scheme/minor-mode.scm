;; requires: core

;; minor-mode.scm — a `define-minor-mode` framework for Macros, with minor
;; modes as real entries on the editor's mode stack (Rust state).
;; Doc: web/content/docs/minor-mode.md
;;
;; Emacs parity subset: :lighter, :init-value, :keymap, :global, and a toggle
;; body, plus `define-globalized-minor-mode`. `:variable`, `:interactive`,
;; `:set`/`:initialize`/`:type`, `:after-hook`, and the pre-Emacs-21
;; positional triplet are NOT supported — see the doc page for why.
;;
;; The model (M3 of the mode framework):
;;  - Enabling a mode pushes it onto the buffer's mode stack ABOVE the major
;;    (enable-minor-mode / disable-minor-mode primitives); it survives a
;;    major-mode switch. Per-buffer state lives in Rust, not a Scheme alist.
;;  - KEYS are real per-mode maps: the mode's own map (plain Emacs dispatch
;;    resolves it over the stack while the mode is on it) plus
;;    "<state>:<mode>" maps for modal users. Two minor modes can bind the
;;    same chord and each keeps its own binding; nothing is a gated thunk in
;;    the shared global Normal maps.
;;  - The mode line renders the mode's LIGHTER from the stack entry (no
;;    Scheme slot per mode).
;;  - HOOKS: the editor fires NAME-hook (the public spelling; NAME-hook /
;;    NAME-mode-hook) on enable AND disable, from the same path as major-mode
;;    hooks. The bare NAME spelling also fires — a back-compat alias, not the
;;    documented API.
;;  - `:init-value` modes are pushed at a buffer's first application of init
;;    (Rust side, per-mode marks in init_minors_done) — retroactive like
;;    Emacs: a mode registered LATE still reaches buffers that never applied
;;    its init, and a later user disable sticks.
;;  - `:global` modes are NOT stack entries: the editor keeps one on/off
;;    state in a global layer that key lookup consults between every
;;    buffer's local minors and its major chain (local minors shadow global
;;    ones). Same enable/disable primitives; the Rust side routes by the
;;    registered definition. Hooks and body run once per toggle.
;;
;; Probe outcomes that still govern the macro mechanics (see
;; src/script/tests.rs :: minor_mode_probes): syntax-case identifier synthesis
;; via whole-datum return, keywords-are-symbols, shallow syntax->datum, no
;; dotted patterns.

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

;; --- per-buffer state (Rust: entries on the mode stack) -----------------------
;; Active minor modes are stack entries above the major. The snapshot carries
;; them space-joined; membership is all Scheme needs (mode names are
;; identifiers — no spaces). `(active-minor-modes)` also applies the
;; enable/disable-minor-mode calls this script has queued so far (Rust side,
;; reset per script), so a read after a toggle sees it. Toggles queued after
;; a buffer or window switch target the new buffer and are not reflected.
(define (minor-mode-active? name)
  (let ((n (if (symbol? name) (symbol->string name) name)))
    (string-contains? (string-append " " (active-minor-modes) " ")
                      (string-append " " n " "))))

(define (minor-mode--on? name) (minor-mode-active? name))

;; --- helpers used by the macro expansion --------------------------------------
(define (minor-mode--kwval kws key default)
  (cond ((null? kws) default)
        ((eq? (caar kws) key) (cadar kws))
        (else (minor-mode--kwval (cdr kws) key default))))

;; The user-visible hook name for a mode (the editor fires it on enable and
;; disable; keys NAME, "NAME-hook" and "NAME-mode-hook" all run).
(define (minor-mode-hook-name name)
  (string-append (symbol->string name) "-hook"))

;; Mode-SCOPED key installer, shared by minor modes and derived majors: the
;; mode's own map (plain Emacs dispatch resolves it over the mode stack — the
;; mode IS on the stack while active) plus the per-state maps, so evil/helix
;; users get the key wherever the mode is on. Nothing lands in the shared
;; global Normal maps.
(define (minor-mode--install-scoped-keys name keymap)
  (unless (null? keymap)
    (let ((n (symbol->string name)))
      (define-keys-in (list n
                            (string-append "evil-normal:" n)
                            (string-append "helix-normal:" n))
        (map (lambda (b) (list (car b) (cadr b))) keymap)))))

;; Emacs arg semantics: no arg = toggle; #t or positive number = on; #f or
;; non-positive number = off. minor-mode-active? reads the pre-command
;; snapshot plus the toggles this script already queued, so `(m) (m)` flips
;; twice. Order per call: queue the enable/disable FIRST (the EDITOR applies
;; it and fires NAME-hook from the same path as major-mode hooks), so the body
;; sees the new state, as in Emacs's `(if my-mode (setup) (teardown))`; then
;; run the body ONLY on an actual flip (DEVIATION from Emacs, which runs the
;; body on every invocation; use MODE-hook for unconditional per-invocation
;; work); then message. The lighter renders from the stack, so there is no
;; slot to update.
(define (minor-mode--toggle name arg)
  (let* ((on? (if (null? arg)
                  (not (minor-mode-active? name))
                  (let ((a (car arg)))
                    (cond ((eq? a #f) #f)
                          ((number? a) (> a 0))
                          (else #t)))))
         (was (minor-mode-active? name)))
    (unless (equal? on? was)
      ((if on? enable-minor-mode disable-minor-mode)
       (symbol->string name))
      (let ((body (minor-mode--prop name 'body)))
        (when body (body))))
    (message (string-append (symbol->string name)
                            (if on? " enabled" " disabled")))
    on?))

;; --- the macro ----------------------------------------------------------------
;; Grammar: (define-minor-mode NAME DOC [:lighter S] [:init-value B]
;;                                [:global B] [:keymap ((CHORD CMD) ...)]
;;                                [BODY ...])
;; Expansion shape:
;;   (begin
;;     (define NAME-map <keymap datum>)             ; introspection alist
;;     (minor-mode--install-scoped-keys 'NAME '<keymap>)
;;     (register-minor-mode-def "NAME" lighter init-value)  ; Rust stack entry
;;                                ; (register-global-minor-mode-def for :global)
;;     (minor-mode--register! 'NAME props-with-body-thunk)
;;     (define NAME (lambda arg (minor-mode--toggle 'NAME arg)))
;;     (register-command 'NAME))
;;
;; Everything inside (define-syntax ...) except the final template runs at
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
                                          '(:lighter :init-value :keymap :global))
                                    (pair? (cdr items)))
                               (peel (cddr items)
                                     (cons (list (car items) (cadr items)) kws)
                                     body))
                              ((memq (car items) '(:lighter :init-value :keymap :global))
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
               (global (if (kwval kws ':global #f) #t #f))
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
                 (list 'minor-mode--install-scoped-keys (list 'quote name-sym)
                       (list 'quote keymap))
                 ;; The Rust-side definition: stack entry (or global layer)
                 ;; + lighter + :init-value handling. "" lighter = none.
                 (list (if global
                           'register-global-minor-mode-def
                           'register-minor-mode-def)
                       mode-name lighter init-value)
                 (list 'minor-mode--register!
                       (list 'quote name-sym)
                       (list 'list
                             (list 'cons ''lighter lighter)
                             (list 'cons ''init-value init-value)
                             (list 'cons ''global global)
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

;; --- globalized minor modes ---------------------------------------------------
;; (define-globalized-minor-mode GLOBAL-MODE MODE TURN-ON [KEYWORD VAL]... [BODY])
;;
;; Defines GLOBAL-MODE as a `:global` minor mode (keywords and body pass
;; through to define-minor-mode) and registers the per-buffer wiring with the
;; editor: while GLOBAL-MODE is on, the editor calls TURN-ON — a NAMED
;; function, typically `(lambda () (MODE 1))` given a name — in the current
;; buffer at enable, in each buffer the first time it is activated after
;; that, and again after every major-mode switch. Switching GLOBAL-MODE off
;; removes MODE from every buffer. MODE stays an ordinary buffer-local mode,
;; so `(MODE 0)` in one buffer still works.
(define-syntax define-globalized-minor-mode
  (syntax-rules ()
    [(_ global mode turn-on rest ...)
     (begin
       (define-minor-mode global
         "Toggle a buffer-local minor mode in every buffer."
         :global #t
         rest ...)
       (register-globalized-minor-mode (symbol->string 'global)
                                       (symbol->string 'mode)
                                       (symbol->string 'turn-on)))]))

;; Public: is NAME on in the current buffer (a `:global` mode counts as on in
;; every buffer)? Other modules gate their hooks and key handlers on this
;; (the pre-command snapshot plus this script's queued toggles).
(define (minor-mode-enabled? name) (minor-mode-active? name))
