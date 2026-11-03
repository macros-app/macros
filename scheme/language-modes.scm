;; requires: core

;; language-modes.scm — an M-x major-mode command per bundled tree-sitter
;; language. Emacs exposes `rust-mode`, `python-mode`, … as interactive commands
;; you can invoke on any buffer; this generates the same set from the grammars
;; Rust actually ships (see `bundled-languages`), so `M-x scala-mode`,
;; `M-x clojure-mode`, etc. switch the current buffer's major mode without any
;; hand-written wiring per language.
;;
;; `set-major-mode` is highlighting-aware in Rust: switching to a mode whose name
;; matches a bundled language attaches that language's grammar (and, via the
;; `fundamental` `enable-lsp` hook, its language server) and re-runs the mode's
;; hooks. So `(add-hook "rust" my-setup)` fires whenever a buffer enters
;; rust-mode — by file detection OR by `M-x rust-mode` — which is the seam for
;; user per-mode configuration.
;;
;; Scheme is intentionally excluded: it has the richer, editor-aware `steel-mode`
;; / `macros-steel-mode` commands (see steel-mode.scm, macros-steel-mode.scm).

;; The `<lang>-mode` command: a closure over LANG, so the language id is a
;; value rather than text spliced into source.
(define (language-mode-command lang)
  (lambda ()
    (set-major-mode lang)
    (message (string-append lang " mode"))))

;; Bind `<lang>-mode` globally and register it for M-x and with the Rust mode
;; registry. The command NAME comes from data (`bundled-languages`), and M-x
;; runs commands by name, so it must be a real global binding: `eval` of a
;; constructed `define` datum binds it (a macro can't do this — registered
;; primitives such as `bundled-languages` don't exist at expansion time). The
;; language id travels as a datum, never spliced into source text.
;;
;; register-mode: the `<lang>-mode-hook` alias works for all of them
;; (run_mode_hooks_once fires `<name>`, `<name>-hook` AND `<name>-mode-hook`),
;; and set-major-mode builds the stack from the registry (fundamental → lang,
;; grammar via the language field).
;;
;; Not `define-derived-mode`: it would add a keymap variable, a
;; `<lang>-mode-hook` variable, scoped-key wiring and a setup chain per
;; language, and stop these commands echoing "<lang> mode". It also takes a
;; literal name, and would record the commands as its own, so a user's
;; `(define-derived-mode rust …)` would silently replace the bundled mode
;; instead of erroring as it does now.
(define (define-language-mode lang)
  (let ((cmd (string->symbol (string-append lang "-mode"))))
    (eval (list 'define cmd (list 'language-mode-command lang)))
    (register-command cmd)
    (register-mode lang "" lang)))

(for-each define-language-mode
          (filter (lambda (lang) (not (equal? lang "scheme")))
                  (bundled-languages)))
