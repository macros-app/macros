;; requires: core

;; dired.scm — a directory editor (file manager), on list-directory + the
;; special-buffer model + call-process for fs operations. C-x d opens it.

(define *dired-dir* "")
(define *dired-lines* '())    ; rendered lines (reversed while building)
(define *dired-entries* '())  ; (line . fullpath)
(define *dired-marks* '())    ; fullpaths marked for deletion
(define *dired-n* 0)
(define *dired-quiet* #f)     ; #t during an FS auto-refresh: render in place
(define *dired-frozen* #f)    ; #t while wdired edits the listing: no auto-refresh

(define (dired-emit text path)
  (set! *dired-lines* (cons text *dired-lines*))
  (when path (set! *dired-entries* (cons (cons *dired-n* path) *dired-entries*)))
  (set! *dired-n* (+ *dired-n* 1)))

;; basename, keeping a trailing "/" for directories.
(define (dired-display full)
  (if (ends-with? full "/") (string-append (basename full) "/") (basename full)))

(define (dired-render)
  (set! *dired-lines* '())
  (set! *dired-entries* '())
  (set! *dired-n* 0)
  (dired-emit (string-append *dired-dir* ":") #f)
  (dired-emit "" #f)
  (for-each (lambda (full)
              (let ((mark (if (member full *dired-marks*) "D " "  ")))
                (dired-emit (string-append mark (dired-display full)) full)))
            (list-directory *dired-dir*))
  (if *dired-quiet*
      (render-buffer-quiet "*dired*" "dired-mode" (join-lines (reverse *dired-lines*)) '())
      (render-buffer "*dired*" "dired-mode" (join-lines (reverse *dired-lines*)) '()))
  ;; Watch the listed directory so external create/rename/delete refresh it live.
  ;; Replace-per-owner: navigating (dired-in) re-points the watch to the new dir.
  (watch-directories "dired" (list *dired-dir*)))

(define (dired)
  (set! *dired-dir* (buffer-directory))
  (set! *dired-marks* '())
  (dired-render))
(define (dired-in dir)
  (set! *dired-dir* dir)
  (set! *dired-marks* '())
  (dired-render))

;; The fullpath on the current line, or #f.
(define (dired-at-point)
  (let loop ((es *dired-entries*) (line (buffer-line)))
    (cond ((null? es) #f)
          ((= (car (car es)) line) (cdr (car es)))
          (else (loop (cdr es) line)))))

;; --- file operations (shared with dired-sidebar) ----------------------------
;; Where NAME, typed at a rename/copy/mkdir prompt in DIR, points: a relative
;; name lands in DIR; an absolute path, "~/…" or "//…" shadows DIR (the same
;; expand-file-name rules as the file prompt).
(define (dired-resolve dir name)
  (expand-file-name (path-join dir name)))

;; Where SRC ends up when moved/copied to TARGET: inside TARGET when it's an
;; existing directory (as mv / cp do), else TARGET itself.
(define (dired-destination src target)
  (if (file-directory? target)
      (path-join target (basename src))
      target))

;; Run PROGRAM with ARGS. #t on success; otherwise echo its error and return #f.
(define (dired-run program args)
  (let ((r (call-process-full program args "")))
    (if (equal? (car r) "0")
        #t
        (let ((err (trim-end (nth r 2))))
          (message (if (equal? err "") (string-append program " failed") err))
          #f))))

;; Call THUNK, first asking before it would overwrite an existing DEST.
(define (dired-confirm-overwrite dest thunk)
  (if (file-exists? dest)
      (y-or-n-p (string-append (basename dest) " already exists; overwrite?")
                (callback->name (lambda (ans) (when (equal? ans "yes") (thunk)))))
      (thunk)))

;; PATH with a moved OLD prefix (the file itself, or anything under it when it's
;; a directory) replaced by NEW; PATH unchanged otherwise. A trailing "/" is kept.
(define (dired-retarget path old new)
  (let ((o (strip-trailing-slash old)) (n (strip-trailing-slash new)))
    (cond ((equal? (strip-trailing-slash path) o)
           (if (ends-with? path "/") (string-append n "/") n))
          ((starts-with? path (string-append o "/"))
           (string-append n (substring path (string-length o) (string-length path))))
          (else path))))

;; Move SRC to TARGET (relative to DIR) after any overwrite confirmation; on
;; success re-point buffers visiting it and call (DONE dest).
(define (dired-move-file src dir name done)
  (when (not (equal? (trim-end name) ""))
    (let* ((src (strip-trailing-slash src))
           (dest (dired-destination src (dired-resolve dir name))))
      (dired-confirm-overwrite dest
        (lambda ()
          (when (dired-run "mv" (list src dest))
            (retarget-file-buffers src dest)
            (done dest)
            (message (string-append "Renamed to " dest))))))))

;; Copy SRC to TARGET (relative to DIR), likewise; then call (DONE dest).
(define (dired-copy-file src dir name done)
  (when (not (equal? (trim-end name) ""))
    (let* ((src (strip-trailing-slash src))
           (dest (dired-destination src (dired-resolve dir name))))
      (dired-confirm-overwrite dest
        (lambda ()
          (when (dired-run "cp" (list "-R" src dest))
            (done dest)
            (message (string-append "Copied to " dest))))))))

;; Create directory NAME (relative to DIR, parents included); then (DONE dest).
(define (dired-make-directory dir name done)
  (when (not (equal? (trim-end name) ""))
    (let ((dest (dired-resolve dir name)))
      (when (dired-run "mkdir" (list "-p" dest))
        (done dest)
        (message "Created directory")))))

;; --- commands ---------------------------------------------------------------
(define (dired-visit)
  (let ((p (dired-at-point)))
    (when p (if (ends-with? p "/") (dired-in p) (open-file p)))))
;; At "/" there is no parent (parent-directory returns ""): stay put.
(define (dired-up)
  (let ((parent (parent-directory *dired-dir*)))
    (dired-in (if (equal? parent "") "/" parent))))
(define (dired-refresh) (dired-render))

;; basename/parent helpers keep a trailing "/", but the editor reports changed
;; directories without one — strip both sides before comparing.

;; directory-changed-hook handler: re-render when the directory dired is showing
;; changes on disk, and only while the dired buffer is still alive.
(define (dired-on-fs-change dir)
  (when (and (buffer-live? "*dired*")
             (not *dired-frozen*)
             (equal? (strip-trailing-slash dir) (strip-trailing-slash *dired-dir*)))
    (set! *dired-quiet* #t)
    (dired-render)
    (set! *dired-quiet* #f)))
(add-hook "directory-changed-hook" "dired-on-fs-change")

;; Close dired and stop watching its directory.
(define (dired-quit)
  (watch-directories "dired" '())
  (kill-buffer))

(define (dired-mark)
  (let ((p (dired-at-point)))
    (when (and p (not (member p *dired-marks*)))
      (set! *dired-marks* (cons p *dired-marks*))
      (dired-render)
      (next-line))))
(define (dired-unmark)
  (let ((p (dired-at-point)))
    (when p
      (set! *dired-marks* (remove-equal p *dired-marks*))
      (dired-render)
      (next-line))))

(define (dired-do-delete)
  (if (null? *dired-marks*)
      (message "No files marked (mark with d)")
      (y-or-n-p (string-append "Delete " (int->string (length *dired-marks*)) " marked file(s)?")
                'dired-delete-confirmed)))
(define (dired-delete-confirmed ans)
  (when (equal? ans "yes")
    ;; dired-run echoes each failure; only a clean run says "Deleted".
    (let ((failed (filter (lambda (p) (not (dired-run "rm" (list "-rf" p))))
                          *dired-marks*)))
      (set! *dired-marks* failed)
      (dired-render)
      (when (null? failed) (message "Deleted")))))

;; P was renamed to DEST: a mark on it (or on anything under it) follows it.
(define (dired-moved p dest)
  (set! *dired-marks* (map (lambda (m) (dired-retarget m p dest)) *dired-marks*))
  (dired-render))

;; Rename / copy: the source path is captured in the prompt's closure (point is
;; in the minibuffer by the time the callback runs, so it can't be re-read).
(define (dired-rename)
  (let ((p (dired-at-point)))
    (when p
      (read-string (string-append "Rename " (basename p) " to: ")
        (lambda (name)
          (dired-move-file p *dired-dir* name
            (lambda (dest) (dired-moved p dest))))))))
(define (dired-copy)
  (let ((p (dired-at-point)))
    (when p
      (read-string (string-append "Copy " (basename p) " to: ")
        (lambda (name)
          (dired-copy-file p *dired-dir* name (lambda (dest) (dired-render))))))))
(define (dired-mkdir) (read-string "Create directory: " 'dired-mkdir-finish))
(define (dired-mkdir-finish name)
  (dired-make-directory *dired-dir* name (lambda (dest) (dired-render))))

;; --- for wdired.scm -----------------------------------------------------------
;; Accessors: a cross-file read or set! of the globals above would hit another
;; binding than the one these functions use.
(define (dired-current-dir) *dired-dir*)
(define (dired-current-entries) *dired-entries*)
(define (dired-freeze! on) (set! *dired-frozen* on))
;; A mark on P (or on anything under it) follows it to DEST.
(define (dired-retarget-marks! p dest)
  (set! *dired-marks* (map (lambda (m) (dired-retarget m p dest)) *dired-marks*)))

;; --- keymap -----------------------------------------------------------------
;; The standard list keys (RET/n/p + no line numbers), then dired's own set.
(define-list-mode-keys "dired-mode" 'dired-visit)
(define-key "dired-mode" "^" 'dired-up)
(define-key "dired-mode" "g" 'dired-refresh)
(define-key "dired-mode" "d" 'dired-mark)
(define-key "dired-mode" "u" 'dired-unmark)
(define-key "dired-mode" "x" 'dired-do-delete)
(define-key "dired-mode" "shift-r" 'dired-rename)
(define-key "dired-mode" "shift-c" 'dired-copy)
(define-key "dired-mode" "+" 'dired-mkdir)
(define-key "dired-mode" "q" 'dired-quit)  ; dired q ≠ kill-buffer

(define-key "fundamental" "ctrl-x d" 'dired)
(register-command 'dired)
