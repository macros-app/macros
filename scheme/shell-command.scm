;; requires: core lib-async

;; shell-command.scm — Emacs's one-shot shell commands:
;;
;;   M-!  shell-command             run a command in the buffer's directory
;;   M-|  shell-command-on-region   pipe the region to a command
;;   M-&  async-shell-command       stream a command into *Async Shell Command*
;;
;; Commands run through the user's shell (`$SHELL -c`, `cmd /C` on Windows; see
;; the `shell-command-argv` primitive) with stderr merged into the output, the
;; way Emacs does without a separate error buffer. Output of one line goes to
;; the echo area, anything longer to *Shell Command Output*. A `C-u` prefix
;; puts the output in the buffer instead: M-! inserts it at point, M-|
;; REPLACES the region with it (`C-u M-| sort`, `C-u M-| jq .`). A failing
;; command reports its exit status; a command ending in `&` given to M-! runs
;; asynchronously, as in Emacs.

(define *shell-command-output-buffer* "*Shell Command Output*")
(define *async-shell-command-buffer* "*Async Shell Command*")

;; Show `name` (rendered from `text`) in a window below the current one,
;; leaving focus where it was; a window already showing it is refreshed in place.
(define (shell-command--display name mode text)
  (if (>= (get-buffer-window name) 0)
      (render-buffer name mode text '())
      (let ((orig (selected-window)))
        (split-window-below)
        (other-window)
        (render-buffer name mode text '())
        (select-window orig))))

;; `out` without its final newline (echo-area display).
(define (shell-command--chomp out)
  (if (ends-with? out "\n") (substring out 0 (- (string-length out) 1)) out))

;; Run `cmd` in `dir` synchronously, feeding `input` on stdin →
;; `(exit-code output)`, stderr merged into the output. A shell that couldn't
;; be started reports code "-1" with the spawn error as the output.
(define (shell-command--run dir cmd input)
  (let* ((argv (shell-command-argv dir cmd #t))
         (r (call-process-full (car argv) (cdr argv) input)))
    (list (car r) (string-append (cadr r) (caddr r)))))

;; The echo-area summary of a finished command's status.
(define (shell-command--status code)
  (cond ((equal? code "0") "Shell command succeeded")
        ((equal? code "-1") "Shell command could not be started")
        (else (string-append "Shell command exited with code " code))))

;; Show a finished command's output (Emacs `display-message-or-buffer`): none
;; or one line in the echo area, more in *Shell Command Output*, with the exit
;; status in the echo area whenever the command failed.
(define (shell-command--show code out)
  (let ((text (shell-command--chomp out))
        (ok (equal? code "0")))
    (cond
      ((equal? text "")
       (message (if ok
                    "(Shell command succeeded with no output)"
                    (string-append "(" (shell-command--status code) " and no output)"))))
      ((not (string-contains? text "\n"))
       (message (if ok text (string-append text "  [" (shell-command--status code) "]"))))
      (else
       (shell-command--display *shell-command-output-buffer* "shell-output-mode" out)
       (when (not ok) (message (shell-command--status code)))))))

;; Strip a trailing `&` (and the space before it) → the command, or #f when
;; `cmd` doesn't end in one.
(define (shell-command--async-form cmd)
  (let ((t (trim-end cmd)))
    (and (ends-with? t "&")
         (not (ends-with? t "&&"))
         (trim-end (substring t 0 (- (string-length t) 1))))))

;; --- M-! ----------------------------------------------------------------------
(define (shell-command-run dir cmd insert?)
  (let ((async (shell-command--async-form cmd)))
    (cond
      ((equal? (trim cmd) "") (message "No command given"))
      (async (async-shell-command-run dir async))
      (else
       (let* ((r (shell-command--run dir cmd ""))
              (code (car r))
              (out (cadr r)))
         (if insert?
             (begin
               (insert out)
               (when (not (equal? code "0")) (message (shell-command--status code))))
             (shell-command--show code out)))))))

;; The directory and C-u are read when the command starts, before the prompt.
(define-command (shell-command)
  (let ((dir (buffer-directory))
        (insert? (> (prefix-count) 0)))
    (read-string (if insert? "Shell command (insert output): " "Shell command: ")
                 (lambda (cmd) (shell-command-run dir cmd insert?)))))

;; --- M-| ----------------------------------------------------------------------
(define (shell-command-on-region-run dir start end input cmd replace?)
  (if (equal? (trim cmd) "")
      (message "No command given")
      (let* ((r (shell-command--run dir cmd input))
             (code (car r))
             (out (cadr r)))
        (if replace?
            ;; A command that couldn't start leaves the region alone instead of
            ;; replacing it with an error message.
            (if (equal? code "-1")
                (message (string-append (shell-command--status code) ": "
                                        (shell-command--chomp out)))
                (begin
                  (replace-region start end out)
                  (when (not (equal? code "0")) (message (shell-command--status code)))))
            (shell-command--show code out)))))

(define-command (shell-command-on-region)
  (if (not (region-active?))
      (message "The mark is not set now, so there is no region")
      (let ((dir (buffer-directory))
            (start (region-beginning))
            (end (region-end))
            (input (region-text))
            (replace? (> (prefix-count) 0)))
        (read-string (if replace? "Shell command on region (replace): "
                         "Shell command on region: ")
                     (lambda (cmd)
                       (shell-command-on-region-run dir start end input cmd replace?))))))

;; --- M-& ----------------------------------------------------------------------
;; One async command at a time on lib-async's "async-shell-command" lane; C-g
;; in its buffer (or M-x kill-async-shell-command) stops it, and killing the
;; buffer kills the process.
(define *async-shell-command-last* "")

(define (async-shell-command-run dir cmd)
  (if (equal? (trim cmd) "")
      (message "No command given")
      (let* ((argv (shell-command-argv dir cmd #t))
             (id (async-lane-run "async-shell-command" (car argv) (cdr argv)
                                 'async-shell-command-output 'async-shell-command-done)))
        (when id
          (set! *async-shell-command-last* cmd)
          (shell-command--display *async-shell-command-buffer* "async-shell-mode"
                                  (string-append "$ " cmd "\n"))
          (set-process-buffer id *async-shell-command-buffer*)))))

(define (async-shell-command-output line)
  (append-to-buffer *async-shell-command-buffer* (string-append line "\n")))

(define (async-shell-command-done code)
  (let* ((cancelled (async-lane-finish "async-shell-command"))
         (status (cond (cancelled "Shell command killed")
                       ((equal? code "0") "Shell command finished")
                       (else (string-append "Shell command exited abnormally with code "
                                            code)))))
    (append-to-buffer *async-shell-command-buffer* (string-append "\n" status "\n"))
    (message (string-append status ": " *async-shell-command-last*))))

(define-command (async-shell-command)
  (let ((dir (buffer-directory)))
    (read-string "Async shell command: " (lambda (cmd) (async-shell-command-run dir cmd)))))

(define-command (kill-async-shell-command)
  (when (not (async-lane-cancel "async-shell-command"))
    (message "No async shell command running")))
(define-command (async-shell-command-keyboard-quit)
  (async-lane-quit "async-shell-command"))

(define-key "fundamental" "alt-!" 'shell-command)
(define-key "fundamental" "alt-|" 'shell-command-on-region)
(define-key "fundamental" "alt-&" 'async-shell-command)

(define-key "shell-output-mode" "q" 'kill-buffer)
(define-key "async-shell-mode" "q" 'kill-buffer)
(define-key "async-shell-mode" "ctrl-g" 'async-shell-command-keyboard-quit)
(define-key "evil-normal:async-shell-mode" "escape" 'async-shell-command-keyboard-quit)
(define-key "helix-normal:async-shell-mode" "escape" 'async-shell-command-keyboard-quit)
