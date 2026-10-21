;; requires: core fundamental

;; Helm — a generic selection framework (the behaviour is in Rust; sources,
;; actions, and applications are written in Scheme). These keys are active only
;; during a Helm session; arrows / RET / C-g are handled as well.
(define-key "helm" "ctrl-n" 'helm-next)
(define-key "helm" "ctrl-p" 'helm-previous)
;; C-SPC marks the candidate for multi-select; RET then acts on every marked one.
(define-key "helm" "ctrl-space" 'helm-mark)
;; TAB completes/descends (handled in Rust); M-a opens the action menu for
;; sources that offer more than one action.
(define-key "helm" "alt-a" 'helm-actions)

;; Make M-x go through Helm (built-in source over all commands).
(define-key "fundamental" "alt-x" 'helm-M-x)

;; Candidate annotations (marginalia): a candidate may be a bare string, or a
;; `(candidate annotation)` pair. The annotation renders dimmed and right-aligned
;; in the Helm panel; only the candidate string is matched on and passed to the
;; action. A dynamic source's function may return the same pair form. Example:
;;   (list (list "ada" "engineer") "alan" (list "grace" "admiral"))

;; A buffer picker — a Helm application written entirely in Scheme on top of two
;; Rust primitives: `(buffer-names)` queries the open buffers, and
;; `switch-to-buffer` is the action. A second action (M-a → Kill) lets you close
;; buffers from the list, like Emacs `helm-buffers-list`. Special buffers (names
;; like *scratch*) carry a dimmed "special" annotation — a marginalia-style hint.
(define (helm-buffer-candidate b)
  (if (starts-with? b "*") (list b "special") b))
(define (helm-buffers)
  (helm (list (list "Buffers" (map helm-buffer-candidate (buffer-names))
                    (list (list "Switch to buffer" 'switch-to-buffer)
                          (list "Kill buffer"      'kill-buffer-named))))))

;; Familiar aliases (Emacs names) for the same picker.
(define (helm-buffers-list) (helm-buffers))
(define (helm-mini)         (helm-buffers))
(define (list-buffers)      (helm-buffers))

;; Make C-x b the Helm buffer picker (overrides the minibuffer switch-buffer).
(define-key "fundamental" "ctrl-x b" 'helm-buffers)

;; Typed paths go through `expand-file-name` (core.scm), so you can jump
;; elsewhere without first deleting the pre-filled prefix: a "~" or "//"
;; SHADOWS everything before it (Emacs `file-name-shadow`).

;; A DYNAMIC directory source: `find-file-candidates` lists `(directory-of
;; pattern)` and re-runs as you type. `list-directory` marks directories with a
;; trailing "/", which is how TAB / RET know to descend into a folder. The
;; pattern is first run through `expand-file-name` so "~/" / "//" shortcuts work.
(define (find-file-candidates pattern)
  (list-directory
    (directory-of
      (expand-file-name
        (if (equal? pattern "") (path-join (buffer-directory) "") pattern)))))

;; C-x C-f: the file navigator, pre-filled with the current directory (so fuzzy
;; matching works immediately and there are folders to descend). Type to fuzzy-
;; match entries; TAB completes to the selection — a folder ends with "/" so TAB
;; (or RET) descends into it; RET on a file opens it.
(define (helm-find-files)
  (helm-from (path-join (buffer-directory) "")
             (list (list "Files" "find-file-candidates" "open-file"))))
(define-key "fundamental" "ctrl-x ctrl-f" 'helm-find-files)

;; C-x f: open/create a file by an arbitrary path (minibuffer, TAB-completed).
;; C-c p f: recursive fuzzy finder over ALL project files (projectile).
(define-key "fundamental" "ctrl-x f" 'find-file)

;; --- A fully Scheme-driven Helm: filtering in Scheme ------------------------
;; A normal dynamic source returns a POOL and Rust filters it by the pattern. A
;; SELF-FILTERED source — written `(list 'raw "fn")` — is shown verbatim: "fn"
;; re-runs per keystroke and Rust does no further filtering, so Scheme alone
;; decides what (and in what order) to show. `(fuzzy-match pattern candidates)`
;; ranks with the same nucleo matcher Helm uses, but in Rust, so per-keystroke
;; filtering stays fast even on large lists. Together they let you build Helm-
;; like pickers entirely in Scheme. Demo — a recentf picker filtered in Scheme:
(define (recentf-fuzzy pattern) (fuzzy-match pattern *recentf*))
(define (helm-recentf-live)
  (helm (list (list "Recent files" (list 'raw "recentf-fuzzy") 'open-file))))

;; A source can also offer MULTIPLE actions — a list of (label fn) pairs. RET
;; runs the first; M-a opens an action menu for the rest (acting on every
;; C-SPC-marked candidate, or the selected one):
;;   (helm (list (list "Files" "find-file-candidates"
;;                 (list (list "open" "open-file")
;;                       (list "insert path" "insert")))))

;; Another example, to show the shape of a Helm app: name + candidates + action.
;; (define (open-greeting who) (message (string-append "hello, " who)))
;; (define (helm-greet)
;;   (helm (list (list "People" (list "ada" "alan" "grace") "open-greeting"))))

;; --- helm-recentf: recently visited files -----------------------------------
;; find-file-hook fires after the new buffer is active, so (buffer-file-name) is
;; the path it visits ((current-buffer) is only the uniquified basename). Keep
;; real files (skip *scratch*, *magit*, …).
(define *recentf* '())
(define *recentf-max* 50)
(define (recentf-track)
  (let ((f (buffer-file-name)))
    (when (and (not (equal? f "")) (file-exists? f))
      (set! *recentf* (mru-push f *recentf* *recentf-max*)))))
(add-hook "find-file-hook" "recentf-track")
(define (helm-recentf)
  (helm (list (list "Recent files" *recentf* 'open-file))))

;; --- helm-occur: live line search of the current buffer ---------------------
;; All lines become "N: text" candidates, N 1-based like the line-number
;; gutter; Helm's fuzzy filter narrows them as you type, and the action jumps
;; to line N. Shared by helm-imenu.
(define (buffer-lines-numbered)
  (let loop ((lines (buffer-lines)) (n 1) (acc '()))
    (if (null? lines)
        (reverse acc)
        (loop (cdr lines) (+ n 1)
              (cons (string-append (int->string n) ": " (car lines)) acc)))))
(define (helm-occur-goto sel)
  (let ((parts (split-many sel ":")))
    (when (>= (string-match "^[0-9]" (car parts)) 0)
      (goto-line (- (string->number (car parts)) 1)))))  ; goto-line is 0-based
(define (helm-occur)
  (helm (list (list "Occur" (buffer-lines-numbered) 'helm-occur-goto))))

;; --- helm-imenu: jump to a definition in the current buffer -----------------
;; A language-agnostic scan for common definition forms; reuses helm-occur-goto.
(define (imenu-name line)
  (let ((m (regexp-captures
             "^\\s*\\(?(?:define|defun|fn|def|function|func|fun|class|struct|type|impl|trait|enum|interface|module|package)\\s+\\(?([A-Za-z0-9_*!?<>=.+-]+)"
             line)))
    (if (null? m) #f (nth m 1))))
(define (imenu-candidates)
  (let loop ((lines (buffer-lines)) (n 1) (acc '()))
    (if (null? lines)
        (reverse acc)
        (let ((nm (imenu-name (car lines))))
          (loop (cdr lines) (+ n 1)
                (if nm (cons (string-append (int->string n) ": " nm) acc) acc))))))
(define (helm-imenu)
  (helm (list (list "Imenu" (imenu-candidates) 'helm-occur-goto))))

;; Bindings: C-x C-r recentf, M-s o occur (Emacs occur prefix). imenu is
;; M-x-only so it doesn't clobber alt-i (completion-at-point).
(define-key "fundamental" "ctrl-x ctrl-r" 'helm-recentf)
(define-key "fundamental" "alt-s o" 'helm-occur)

;; M-x discoverability for the helm entry points.
(register-command 'helm-buffers)
(register-command 'helm-buffers-list)
(register-command 'helm-mini)
(register-command 'list-buffers)
(register-command 'helm-find-files)
(register-command 'helm-recentf)
(register-command 'helm-recentf-live)
(register-command 'helm-occur)
(register-command 'helm-imenu)
