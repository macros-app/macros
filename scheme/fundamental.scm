;; requires:

;; fundamental-mode keymap — the global map: motion, editing, region,
;; kill/yank, undo, files, and buffers. Every binding here is overridable:
;; call (define-key "fundamental" "<chord>" "<command>") to rebind.

;; Motion
(define-key "fundamental" "ctrl-f" 'forward-char)
(define-key "fundamental" "ctrl-b" 'backward-char)
(define-key "fundamental" "ctrl-n" 'next-line)
(define-key "fundamental" "ctrl-p" 'previous-line)
(define-key "fundamental" "ctrl-a" 'move-beginning-of-line)
(define-key "fundamental" "ctrl-e" 'move-end-of-line)
(define-key "fundamental" "alt-f" 'forward-word)
(define-key "fundamental" "alt-b" 'backward-word)
;; macOS reports shifted symbols as the produced char with shift cleared, so
;; M-< / M-> are alt-< / alt-> (not alt-shift-, / alt-shift-.).
(define-key "fundamental" "alt-<" 'beginning-of-buffer)
(define-key "fundamental" "alt->" 'end-of-buffer)

;; Deletion / killing
(define-key "fundamental" "ctrl-d" 'delete-char)
(define-key "fundamental" "ctrl-k" 'kill-line)
(define-key "fundamental" "alt-d" 'kill-word)
(define-key "fundamental" "alt-backspace" 'backward-kill-word)

;; Region / kill ring
(define-key "fundamental" "ctrl-space" 'set-mark)
(define-key "fundamental" "ctrl-w" 'kill-region)
(define-key "fundamental" "alt-w" 'kill-ring-save)
(define-key "fundamental" "ctrl-y" 'yank)
(define-key "fundamental" "alt-y" 'yank-pop)
;; System clipboard (macOS cmd-c/x/v) — work everywhere, incl. evil modes.
(define-key "fundamental" "cmd-c" 'clipboard-copy)
(define-key "fundamental" "cmd-x" 'clipboard-cut)
(define-key "fundamental" "cmd-v" 'clipboard-paste)
(define-key "fundamental" "cmd-a" 'mark-whole-buffer)

;; Editing / view
(define-key "fundamental" "ctrl-g" 'keyboard-quit)
(define-key "fundamental" "ctrl-/" 'undo)
;; C-_ is the same key as C-/ on a US layout, but backends disagree on how a
;; shifted symbol is reported (macOS: the produced char, "ctrl-_"; some
;; Windows paths: the base key with shift, "ctrl-shift--") — bind both.
(define-key "fundamental" "ctrl-_" 'undo)
(define-key "fundamental" "ctrl-shift--" 'undo)
(define-key "fundamental" "ctrl-t" 'transpose-chars)
(define-key "fundamental" "ctrl-l" 'recenter)
(define-key "fundamental" "ctrl-o" 'open-line)
(define-key "fundamental" "ctrl-v" 'scroll-up)
(define-key "fundamental" "alt-v" 'scroll-down)
(define-key "fundamental" "alt-x" 'execute-extended-command)
;; Incremental search (Emacs C-s / C-r). Live matching, C-s/C-r repeat,
;; <Return> finishes on the match, C-g aborts back to where it started.
(define-key "fundamental" "ctrl-s" 'isearch-forward)
(define-key "fundamental" "ctrl-r" 'isearch-backward)
;; M-: evaluate a Steel expression in the minibuffer (echoes => result).
(define-key "fundamental" "alt-:" 'eval-expression)
(define-key "fundamental" "alt-i" 'completion-at-point)
;; C-M-\ reindent the region (no region: the whole buffer) from the language's
;; structure — Emacs `indent-region`. (Our `indent-region` is vim's `>` shift.)
(define-key "fundamental" "ctrl-alt-\\" 'reindent-region)

;; Prefix (C-x) commands
(define-key "fundamental" "ctrl-x ctrl-x" 'exchange-point-and-mark)
(define-key "fundamental" "ctrl-x ctrl-h" 'mark-whole-buffer)
(define-key "fundamental" "ctrl-x ctrl-u" 'undo)
(define-key "fundamental" "ctrl-x ctrl-f" 'find-file)
(define-key "fundamental" "ctrl-x b" 'switch-buffer)
(define-key "fundamental" "ctrl-x ctrl-b" 'list-buffers)
(define-key "fundamental" "ctrl-x ctrl-right" 'next-buffer)
(define-key "fundamental" "ctrl-x ctrl-left" 'previous-buffer)
(define-key "fundamental" "ctrl-x ctrl-s" 'save-buffer)
(define-key "fundamental" "ctrl-x k" 'kill-buffer)
(define-key "fundamental" "ctrl-x ctrl-c" 'quit)

;; Windows (split panes), Emacs C-x bindings.
(define-key "fundamental" "ctrl-x 2" 'split-window-below)
(define-key "fundamental" "ctrl-x 3" 'split-window-right)
(define-key "fundamental" "ctrl-x o" 'other-window)
(define-key "fundamental" "ctrl-x +" 'balance-windows)   ; equalize window sizes
(define-key "fundamental" "ctrl-x 0" 'delete-window)
(define-key "fundamental" "ctrl-x 1" 'delete-other-windows)
;; Resize (Emacs C-x ^ taller, C-x } / C-x { wider/narrower).
(define-key "fundamental" "ctrl-x ^" 'enlarge-window)
(define-key "fundamental" "ctrl-x }" 'enlarge-window-horizontally)
(define-key "fundamental" "ctrl-x {" 'shrink-window-horizontally)

;; Keyboard macros (Emacs C-x ( record, C-x ) end, C-x e replay).
(define-key "fundamental" "ctrl-x (" 'start-kbd-macro)
(define-key "fundamental" "ctrl-x )" 'end-kbd-macro)
(define-key "fundamental" "ctrl-x e" 'call-last-kbd-macro)

;; Start the configured language server when a buffer is opened (any language;
;; runs once per buffer via run-mode-hooks). `maybe_start_lsp` is idempotent and
;; a no-op for file types with no `set-lsp` server, so this is safe everywhere.
;; Override in ~/.config/macros/init.scm: (remove-hook "fundamental-mode-hook" "enable-lsp").
(add-hook "fundamental-mode-hook" "enable-lsp")
