;; requires: core

;; winner.scm — Emacs winner-mode: undo/redo window-layout changes. The editor
;; records the layout (splits, sizes, and which buffer each window shows) after
;; every change; `C-c left` steps back through it (repeat to keep going) and
;; `C-c right` steps forward again. It's on by default:
;;
;;   (winner-mode #f)                     ; in init.scm: stop recording
;;
;; which is the same as (set-option "winner-mode" #f). The history itself and
;; `winner-undo` / `winner-redo` live in Rust (src/editor/winner.rs).

(define (winner-mode on) (set-option "winner-mode" on))

(define-key "fundamental" "ctrl-c left" 'winner-undo)
(define-key "fundamental" "ctrl-c right" 'winner-redo)
;; Evil: Doom's C-w u / C-w U next to the other window commands.
(define-key "evil-normal" "ctrl-w u" 'winner-undo)
(define-key "evil-normal" "ctrl-w shift-u" 'winner-redo)
