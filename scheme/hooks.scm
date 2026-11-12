;; requires: core

;; hooks.scm — friendlier `add-hook` / `remove-hook`.
;;
;; The raw builtins take a string (the editor calls hooks by name across the
;; engine-thread boundary). These wrappers add the same ergonomics `bind-key`
;; has: a 'symbol defers to its name, and an anonymous procedure is stashed in
;; the thunk table behind a generated, callable global stub. So all three forms
;; work — handy for per-major-mode setup. The PUBLIC hook spellings are the
;; Emacs ones — `<mode>-hook` (`rust-mode-hook`) or `<mode>-mode-hook`; the
;; editor fires all three (NAME, NAME-hook, NAME-mode-hook), so the bare mode
;; name ("rust") still fires as a back-compat alias, but it is NOT the
;; documented API:
;;
;;   (add-hook "rust-mode-hook" "my-setup")                  ; string  (raw form)
;;   (add-hook "rust-mode-hook" 'my-setup)                   ; 'symbol (resolved at run time)
;;   (add-hook "rust-mode-hook" (lambda () (message "hi")))  ; anonymous procedure
;;
;; Must be a separate file from core.scm (where `raw-add-hook`/`hook->name` live):
;; Steel compiles each file with letrec* scope, so redefining `add-hook` in the
;; same file as `(define raw-add-hook add-hook)` would shadow the builtin and turn
;; that capture into a forward reference. core.scm sorts first, so its bindings
;; are in scope here.

(define (add-hook hook proc)
  (raw-add-hook hook (hook->name proc)))

;; A 'symbol or string removes the matching registration; an anonymous procedure
;; can't be matched (each `add-hook` of a lambda gets a fresh stub name — the
;; same limitation Emacs has), so name it or use a 'symbol if you need removal.
(define (remove-hook hook proc)
  (cond
    ((string? proc) (raw-remove-hook hook proc))
    ((symbol? proc) (raw-remove-hook hook (symbol->string proc)))
    (else
     (message "remove-hook: can't remove an anonymous procedure; add it by name or 'symbol"))))

;; (run-hooks h1 h2 …) — run each hook in order, like Emacs `run-hooks`.
;; Each argument is a hook name (a 'symbol or "string"); every function
;; registered on it fires with no arguments. A trigger that needs to fire
;; several hooks (a save-like path, say) uses this instead of repeating
;; `run-hook`; `run-hook-with-args` and `run-hook-wrapped` are NOT provided —
;; hooks here fire asynchronously by name (no return values reach the caller),
;; which neither variant fits; add the args one when a Scheme caller exists.
(define (run-hooks . hooks)
  (for-each (lambda (h) (run-hook (hook->name h))) hooks))
