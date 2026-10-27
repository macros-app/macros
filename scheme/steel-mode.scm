;; requires: core

;; steel-mode keymap — Steel (Scheme) editing, layered on fundamental.
;;
;; Plain `steel` buffers hold arbitrary Steel code, so their eval keys target
;; the isolated ANVIL image (steel-anvil.scm), never the live editor: running
;; a file that happens to `(define (insert …))` or `(define (message …))` must
;; not silently replace the editor's own functions. Code that is *meant* to
;; change the editor lives in `macros-steel` buffers, whose keys evaluate into
;; the live runtime (macros-steel-mode.scm). `M-x eval-last-sexp` /
;; `M-x eval-buffer` still evaluate live from anywhere.
(define-key "steel" "ctrl-c ctrl-e" 'anvil-eval-last-sexp)
(define-key "steel" "ctrl-x ctrl-e" 'anvil-eval-last-sexp)  ; Emacs muscle memory
(define-key "steel" "ctrl-c ctrl-b" 'anvil-eval-buffer)

;; LSP autostart lives on `fundamental` (see fundamental.scm) so it fires for
;; every language, not just Steel.

;; Activation: drop the active buffer into Steel mode (M-x steel-mode, or auto
;; for .scm by language detection). `set-major-mode` installs the keymap above;
;; for the editor-aware variant (live runtime eval, editor-API completion) use
;; `macros-steel-mode` instead. See macros-steel-mode.scm.
(define (steel-mode)
  (set-major-mode "steel")
  (message "Steel mode"))
(register-command 'steel-mode)

;; --- structural editing ------------------------------------------------------
;; The core paredit edits, on reader-aware boundaries: parens inside strings,
;; comments and `#\(` char literals never count. Sexp motion (C-M-f/b/u/d/n/p,
;; C-M-k, C-M-a/e …) is global — see editing-extras.scm — and reads these
;; buffers as Lisp.
;;   C-)  / C-}     slurp the next sexp in / barf the last one out
;;   M-(            wrap the sexp at point in parens
;;   M-r            raise the sexp at point over its enclosing list
;;   M-s s          splice: remove the enclosing list's parens
(define (lisp-structural-keys mode)
  (define-key mode "ctrl-)" 'paredit-forward-slurp-sexp)
  (define-key mode "ctrl-}" 'paredit-forward-barf-sexp)
  (define-key mode "alt-(" 'paredit-wrap-round)
  (define-key mode "alt-r" 'paredit-raise-sexp)
  (define-key mode "alt-s s" 'paredit-splice-sexp))
(for-each lisp-structural-keys
          '("steel" "macros-steel" "racket" "clojure" "lisp" "commonlisp" "elisp" "fennel" "janet"))
