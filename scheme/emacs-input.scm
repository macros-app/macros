;; requires: core

;; emacs-input.scm — Emacs core input keys: the mark ring, text scale,
;; overwrite-mode, quoted-insert, scroll-other-window, repeat, and the regexp /
;; symbol isearch entry points. The commands are Rust builtins
;; (src/editor/emacs_input.rs); this file only binds them. See
;; web/content/docs/editing-commands.md.

;; Mark ring. `C-SPC` (set-mark) pushes onto it; `C-u C-SPC` pops back (and
;; further `C-SPC`s keep popping); `C-x C-SPC` pops the global ring.
(define-key "fundamental" "ctrl-x ctrl-space" 'pop-global-mark)

;; Text scale for this buffer. After any of these, plain `+` / `=` / `-` / `0`
;; keep adjusting until another key.
(define-key "fundamental" "ctrl-x ctrl-+" 'text-scale-increase)
(define-key "fundamental" "ctrl-x ctrl-shift-=" 'text-scale-increase)
(define-key "fundamental" "ctrl-x ctrl-=" 'text-scale-increase)
(define-key "fundamental" "ctrl-x ctrl--" 'text-scale-decrease)
(define-key "fundamental" "ctrl-x ctrl-0" 'text-scale-reset)
;; Every buffer and the UI (macOS-style zoom; Emacs C-x C-M-+ / C-M-- / C-M-0).
;; Grid buffers keep their own cmd-= zoom (scheme/grid.scm).
(define-key "fundamental" "cmd-=" 'global-text-scale-increase)
(define-key "fundamental" "cmd-+" 'global-text-scale-increase)
(define-key "fundamental" "cmd--" 'global-text-scale-decrease)
(define-key "fundamental" "cmd-0" 'global-text-scale-reset)
(define-key "fundamental" "ctrl-x ctrl-alt-+" 'global-text-scale-increase)
(define-key "fundamental" "ctrl-x ctrl-alt-=" 'global-text-scale-increase)
(define-key "fundamental" "ctrl-x ctrl-alt--" 'global-text-scale-decrease)
(define-key "fundamental" "ctrl-x ctrl-alt-0" 'global-text-scale-reset)

;; Typing.
(define-key "fundamental" "insert" 'overwrite-mode)
(define-key "fundamental" "ctrl-q" 'quoted-insert)

;; Windows.
(define-key "fundamental" "ctrl-alt-v" 'scroll-other-window)
(define-key "fundamental" "ctrl-alt-shift-v" 'scroll-other-window-down)

;; Repeat the last command; `z` repeats again.
(define-key "fundamental" "ctrl-x z" 'repeat)

;; Isearch variants (C-s / C-r are literal; M-r toggles regexp while searching).
(define-key "fundamental" "ctrl-alt-s" 'isearch-forward-regexp)
(define-key "fundamental" "ctrl-alt-r" 'isearch-backward-regexp)
(define-key "fundamental" "alt-s ." 'isearch-forward-symbol-at-point)
