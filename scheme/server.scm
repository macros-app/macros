;; requires: core

;; server.scm — the edit server (src/server.rs, src/editor/server.rs), Emacs
;; `server-start` / `emacsclient` style.
;;
;; A running Macros listens on a socket only you can reach, and
;;   macros --client [-n] [+LINE[:COLUMN]] FILE...
;; opens the files here instead of starting a second editor. Without `-n` the
;; client waits until you finish each file with C-x # (save and release), so
;;   export EDITOR="macros --client"
;; works for `git commit`. On by default; turn it off in init.scm with
;;   (set-option "server" #f)

;; Start the edit server (sets the `server` option for this session).
(define-command (server-start) (set-option "server" #t))

;; Stop the edit server; `macros --client` then starts a new editor instead.
(define-command (server-stop) (set-option "server" #f))

(define-key "fundamental" "ctrl-x #" 'server-edit)
