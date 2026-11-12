;; requires: core hooks minor-mode mode-local

;; derived-mode.scm — `define-derived-mode` for Macros, built on the editor's
;; mode registry (src/mode.rs ModeRegistry). Doc: web/content/docs/derived-mode.md
;;
;; NAMING (one convention, matching the bundled modes): the mode's INTERNAL
;; name is the bare name — "mylang" — and the command is `mylang-mode`, the
;; same rust-mode → "rust" convention. The macro accepts either spelling for
;; CHILD and PARENT (`mylang` or `mylang-mode`); a trailing "-mode" is
;; stripped. The bare name lines up with grammar ids, abbrev tables and
;; "evil-normal:mylang" keymaps, and `<name>-mode-hook` works for hooks
;; (run_mode_hooks_once fires `<name>`, `<name>-hook` AND
;; `<name>-mode-hook`).
;;
;; The registry carries the parent edge; set-major-mode builds the buffer's
;; stack from the ancestry (fundamental → … → mode), which gives — with no
;; Scheme glue — keymap inheritance (child overrides parent, unbound keys
;; fall through), hooks in Emacs order (furthest ancestor first, each fired
;; exactly once, fundamental once) and grammar inheritance (nearest ancestor
;; with a language).
;;
;; BODIES stay in Scheme as a chained setup: the command runs the child's
;; setup — which runs each ancestor's first — then set-major-mode; the editor
;; fires the hooks on apply, so every body runs before every hook (Emacs's
;; delay-mode-hooks / run-mode-hooks order).
;;
;; Refuses to define a mode whose command already exists (e.g. the bundled
;; rust-mode) — replacing it would silently lose its grammar and wiring —
;; except re-defining a mode this framework itself made (a config reload).
;;
;; Macro-mechanics constraints (probe-verified, see scheme/minor-mode.scm):
;; no dotted patterns, shallow syntax->datum with a deep conversion pass,
;; primitives only inside the transformer, whole-datum expansion.

;; --- setup chain (bodies, ancestors first) --------------------------------
;; name -> (parent-name-or-#f . body-thunk)
(define *derived-mode-setups* '())

(define (derived-mode--register-setup! name parent thunk)
  (set! *derived-mode-setups*
        (cons (cons name (cons parent thunk))
              (filter (lambda (e) (not (string=? (car e) name)))
                      *derived-mode-setups*))))

;; Run NAME's setup chain: ancestors first (nearest parent, then its parent,
;; …), then NAME's own body. An unregistered parent (a bundled mode such as
;; "rust") simply has no setup to run. Explicit string=? loop — assq/memq on
;; runtime-built strings only match interned literals.
(define (derived-mode--run-setup name)
  (let loop ((entries *derived-mode-setups*))
    (cond ((null? entries) #f)
          ((string=? (caar entries) name)
           (let ((e (car entries)))
             (when (cadr e) (derived-mode--run-setup (cadr e)))
             ((cddr e))))
          (else (loop (cdr entries))))))

;; Commands this framework defined — re-defining them (a config reload) is
;; allowed, unlike replacing a bundled command.
(define *derived-mode-commands* '())
(define (derived-mode--register-command! cmd-name)
  (set! *derived-mode-commands*
        (cons cmd-name (filter (lambda (c) (not (string=? c cmd-name)))
                               *derived-mode-commands*))))

;; --- the macro ---------------------------------------------------------------
;; (define-derived-mode CHILD PARENT DOC
;;                        [:keymap ((CHORD CMD) …)] [:LANGUAGE SYM-OR-STRING]
;;                        BODY …)
(define-syntax define-derived-mode
  (lambda (stx)
    (syntax-case stx ()
      [(_ child parent doc rest ...)
       (let* ((child-name (symbol->string (syntax->datum #'child)))
              (parent-name (symbol->string (syntax->datum #'parent)))
              ;; BARE naming: strip a trailing "-mode" from both child and
              ;; parent; the command is <bare>-mode.
              (strip (lambda (n)
                       (if (and (>= (string-length n) 5)
                                (string=? (substring n (- (string-length n) 5)
                                                   (string-length n))
                                          "-mode"))
                           (substring n 0 (- (string-length n) 5))
                           n)))
              (bare (strip child-name))
              (parent-bare (strip parent-name))
              (cmd-name (if (string=? bare child-name)
                            (string-append bare "-mode")
                            child-name))
              (cmd-sym (string->symbol cmd-name))
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
                                    (memq (car items) '(:keymap :language))
                                    (pair? (cdr items)))
                               (peel (cddr items)
                                     (cons (list (car items) (cadr items)) kws)
                                     body))
                              ((memq (car items) '(:keymap :language))
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
               (language-raw (kwval kws ':language #f))
               (bare-sym (string->symbol bare)))
           (unless (or (not language-raw)
                       (symbol? language-raw)
                       (string? language-raw))
             (error "define-derived-mode: :language must be a symbol or string:"
                    language-raw))
           (let ((language (cond ((not language-raw) "")
                                 ((symbol? language-raw)
                                  (symbol->string language-raw))
                                 (else language-raw)))
                 ;; "" = no parent on the wire (the Rust side also normalizes
                 ;; "fundamental", but keep the wire value clean).
                 (parent-wire (if (string=? parent-bare "fundamental")
                                  ""
                                  parent-bare))
                 (parent-for-setup (if (string=? parent-bare "fundamental")
                                       #f
                                       parent-bare)))
             (list 'begin
                   (list 'if
                         (list 'and
                               (list 'member cmd-name '*interactive-commands*)
                               (list 'not (list 'member cmd-name
                                                '*derived-mode-commands*)))
                         (list 'error "define-derived-mode: command already defined:"
                               cmd-name))
                   (list 'derived-mode--register-command! cmd-name)
                   (list 'register-mode bare parent-wire language)
                   (list 'define (string->symbol (string-append cmd-name "-map"))
                         (list 'quote keymap))
                   (list 'minor-mode--install-scoped-keys (list 'quote bare-sym)
                         (list 'quote keymap))
                   (list 'derived-mode--register-setup! bare
                         parent-for-setup
                         (cons 'lambda (cons '() (append body (list #f)))))
                   (list 'define cmd-sym
                         (list 'lambda 'arg
                               (list 'begin
                                     (list 'derived-mode--run-setup bare)
                                     (list 'set-major-mode bare)
                                     #f)))
                   (list 'register-command (list 'quote cmd-sym))))))])))

;; --- auto-mode-alist -----------------------------------------------------------
;; Scheme-level file association (Emacs's `auto-mode-alist`): entries are
;; (REGEX . COMMAND-NAME) — the M-x command (`<bare>-mode` for derived modes),
;; which eval-string dispatches — walked FIRST-match-wins; `add-auto-mode`
;; prepends so later calls win, preserving Emacs add-to-list priority
;; semantics.
;;
;; REGEX DIALECT: Macros' regexp primitives ride the Rust `regex` crate, which
;; does NOT support Emacs's `'` end-of-string anchor — use `$`
;; (`(add-auto-mode "\\.foo$" "foo-mode")`). An invalid pattern is warned
;; about and the entry disabled at registration.
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

;; The live Rust path dispatches to the COMMAND, so the table stores the
;; command name VERBATIM — no `<bare>-mode` → bare translation here. Running
;; the command (not a raw SetMajorMode) is what makes derived-mode bodies run,
;; lets bundled modes keep their grammar wiring, and preserves hand-written
;; mode-command setup.
;; A bad pattern must not silently never-match (worse than Emacs, which
;; signals invalid-regexp at match time): warn LOUDLY and disable the entry
;; — never a fatal error, or one ported Emacs pattern bricks init.
(define (auto-mode--warn pattern)
  (string-append "add-auto-mode: invalid pattern \""
                 pattern
                 "\" — entry disabled (Rust regex dialect: Emacs \\' is $)"))

(define (add-auto-mode pattern mode-name)
  (unless (string? pattern)
    (error "add-auto-mode: pattern must be a string:" pattern))
  ;; A bad pattern WARNS and disables ONLY this entry — never a fatal error,
  ;; or one ported Emacs pattern drops the rest of init.scm after it. The
  ;; invalid-mode-name case stays fatal: it is a code bug, not config data.
  (unless (regex-valid? pattern)
    (message (auto-mode--warn pattern)))
  (unless (auto-mode--valid-name? mode-name)
    (error "add-auto-mode: mode-name must be a command identifier (a-zA-Z0-9-_):"
           mode-name))
  ;; Dedup same-pattern: a reloaded config must not duplicate entries. The
  ;; alist stays as the pure test seam / introspection surface; the LIVE path
  ;; is Rust (add-auto-mode-impl queues an entry consulted by open_file
  ;; BEFORE the built-in extension→language detection). open_file dispatches
  ;; to the command via eval-string, so derived-mode bodies, bundled-mode
  ;; wiring and hand-written mode commands all run.
  (when (regex-valid? pattern)
    (set! *auto-mode-alist*
          (cons (cons pattern mode-name)
                (filter (lambda (e) (not (string=? (car e) pattern)))
                        *auto-mode-alist*)))
    (add-auto-mode-impl pattern mode-name)))

;; Pure matcher — the testable seam / introspection surface (the live path is
;; Rust's auto_modes table, applied by open_file).
(define (auto-mode--match path)
  (let loop ((entries *auto-mode-alist*))
    (cond ((null? entries) #f)
          ((>= (string-match (caar entries) path) 0) (cdar entries))
          (else (loop (cdr entries))))))

;; Dispatch for an explicit path — kept for tests and explicit use.
(define (auto-mode--apply-for path)
  (let ((mode (auto-mode--match path)))
    (when mode
      (eval-string (string-append "(" mode ")")))))
