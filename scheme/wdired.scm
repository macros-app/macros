;; requires: core dired

;; wdired.scm — Emacs wdired: rename files by editing the dired listing.
;;
;;   C-x C-q / C-c C-e   (i under evil/helix)   make the file names editable.
;;                Only the names can change: the header and each line's mark
;;                column carry a `read-only` text property, so the editor
;;                refuses edits that touch them (or the newline between two
;;                entries). Anything that edits text works on the names —
;;                query-replace, multiple cursors, rectangles, macros.
;;   C-c C-c / C-x C-s / C-x C-q   rename every changed file. A name with "/"
;;                moves the file (missing directories are created). Swaps and
;;                cycles (a→b, b→a) go through temporary names. Nothing is
;;                renamed when the plan has a problem (an empty name, two files
;;                to one name, a name that already exists); a rename that fails
;;                partway undoes the ones already done, so no file is lost.
;;   C-c C-k / C-c ESC   abort: throw the edits away and restore the listing.
;;
;; The pieces are pure where they can be (`wdired-parse`, `wdired-plan`,
;; `wdired-execute` take the file-system calls as arguments) so the plan, swaps
;; and failures are testable without touching the disk.

(define *wdired-spec* '())  ; ((line prefix display fullpath) …) of the listing

;; --- entering -----------------------------------------------------------------
;; The listing's entry lines as `(line prefix display fullpath)`, in line order.
;; Each rendered entry line is a 2-char mark column ("D " / "  ") then the name.
(define (wdired-entries lines)
  (let ((entries (dired-current-entries)))
    (let loop ((ls lines) (i 0) (acc '()))
      (if (null? ls)
          (reverse acc)
          (let ((full (alist-ref entries i)) (line (car ls)))
            (loop (cdr ls) (+ i 1)
                  (if (and full (>= (string-length line) 2))
                      (cons (list i (substring line 0 2)
                                  (substring line 2 (string-length line)) full)
                            acc)
                      acc)))))))

;; Read-only BYTE ranges protecting everything but the names: from the start of
;; the buffer to the first name, then each later line's mark column. A range
;; starting at a line start also guards the newline before it (no joining).
(define (wdired-read-only-ranges lines spec)
  (let loop ((ls lines) (i 0) (off 0) (first? #t) (sp spec) (acc '()))
    (cond
      ((or (null? ls) (null? sp)) (reverse acc))
      ((= i (car (car sp)))
       (let ((name-start (+ off (utf8-length (cadr (car sp))))))
         (loop (cdr ls) (+ i 1) (+ off (utf8-length (car ls)) 1) #f (cdr sp)
               (cons (list (if first? 0 off) name-start) acc))))
      (else (loop (cdr ls) (+ i 1) (+ off (utf8-length (car ls)) 1) first? sp acc)))))

(define-command (wdired-change-to-wdired-mode)
  (if (not (equal? (current-buffer) "*dired*"))
      (message "Not a dired buffer")
      (let* ((lines (split-many (buffer-string) "\n"))
             (spec (wdired-entries lines)))
        (if (null? spec)
            (message "No files to rename")
            (begin
              (set! *wdired-spec* spec)
              ;; An FS refresh would redraw the listing over the edits.
              (dired-freeze! #t)
              (set-special-mode "")
              (set-major-mode "wdired-mode")
              (for-each (lambda (r) (put-text-property (car r) (cadr r) "read-only" "t"))
                        (wdired-read-only-ranges lines spec))
              (message "Edit the file names, then C-c C-c to rename (C-c C-k aborts)"))))))

;; Back to the read-only listing. Like occur-edit, the edited buffer is killed
;; and drawn fresh, so its undo history and read-only properties go with it.
(define (wdired-leave line)
  (cond ((helix-enabled?) (helix-normal-state))
        ((evil-enabled?) (evil-normal-state)))
  (set! *wdired-spec* '())
  (dired-freeze! #f)
  (kill-buffer-named "*dired*")
  (dired-refresh)
  (goto-line line))

(define-command (wdired-abort-changes)
  (when (equal? (current-buffer) "*dired*")
    (wdired-leave (buffer-line))
    (message "Changes aborted")))

;; --- parsing ------------------------------------------------------------------
;; The edited TEXT against SPEC: `((fullpath . new-name) …)` for every changed
;; name (a directory's trailing "/" is optional), or an error string.
(define (wdired-parse spec text)
  (let ((lines (list->vector (split-many text "\n"))))
    (let loop ((sp spec) (acc '()))
      (if (null? sp)
          (reverse acc)
          (let* ((e (car sp)) (i (car e)) (prefix (cadr e))
                 (old (strip-trailing-slash (caddr e)))
                 (line (and (< i (vector-length lines)) (vector-ref lines i))))
            (cond
              ((or (not line) (not (starts-with? line prefix)))
               "The listing's lines changed (line breaks can't be added or removed) — fix it or C-c C-k")
              (else
               (let ((new (strip-trailing-slash
                           (substring line (string-length prefix) (string-length line)))))
                 (loop (cdr sp)
                       (if (equal? new old) acc (cons (cons (list-ref e 3) new) acc)))))))))))

;; --- the plan (pure) ----------------------------------------------------------
;; Ops are `(mkdir dir)` and `(mv from to)`, in the order to run them.

(define (wdired-op-from op) (cadr op))
(define (wdired-op-to op) (caddr op))

;; The missing directories above PATH, outermost first.
(define (wdired-missing-parents path exists?)
  (let loop ((dir (parent-directory path)) (acc '()))
    (if (or (equal? dir "") (equal? dir "/") (exists? dir))
        acc
        (loop (parent-directory dir) (cons dir acc)))))

;; Order `((src . dest) …)` so no move lands on a file another move has yet to
;; vacate: a move whose dest is still some pending source waits; when every
;; pending move waits (a swap or cycle) one source steps aside to a temp name.
;; Dests compare case-insensitively, so a case-only rename on a case-folding
;; file system (foo → Foo) also goes through a temp name.
(define (wdired-order moves temp-for)
  (define (blocked? m pending)
    (let ((d (string-downcase (cdr m))))
      (wdired-any? (lambda (p) (equal? (string-downcase (car p)) d)) pending)))
  (let loop ((pending moves) (ops '()))
    (if (null? pending)
        (reverse ops)
        (let ((ready (filter (lambda (m) (not (blocked? m pending))) pending)))
          (if (null? ready)
              (let* ((m (car pending)) (tmp (temp-for (car m))))
                (loop (cons (cons tmp (cdr m)) (cdr pending))
                      (cons (list 'mv (car m) tmp) ops)))
              (let ((m (car ready)))
                (loop (remove-equal m pending) (cons (list 'mv (car m) (cdr m)) ops))))))))

(define (wdired-any? pred lst)
  (cond ((null? lst) #f) ((pred (car lst)) #t) (else (wdired-any? pred (cdr lst)))))

;; Plan RENAMES (`((fullpath . new-name) …)` from `wdired-parse`) in DIR.
;; `exists?` is the file-system test. ⇒ ops, or an error string.
(define (wdired-plan dir renames exists?)
  (let* ((moves (map (lambda (r)
                       (cons (strip-trailing-slash (car r))
                             (strip-trailing-slash (dired-resolve dir (cdr r)))))
                     renames))
         (srcs (map car moves))
         (empty (filter (lambda (r) (equal? (trim-end (cdr r)) "")) renames))
         (dests (map cdr moves))
         ;; A dest that exists and isn't vacated by this batch. A case-only
         ;; rename (foo → Foo) "exists" on a case-folding file system.
         (taken (filter (lambda (m)
                          (and (exists? (cdr m))
                               (not (member (cdr m) srcs))
                               (not (equal? (string-downcase (cdr m))
                                            (string-downcase (car m))))))
                        moves)))
    (cond
      ((not (null? empty))
       (string-append "Empty file name for " (basename (car (car empty)))))
      ((wdired-duplicate dests)
       (string-append "Two files would be renamed to " (wdired-duplicate dests)))
      ((not (null? taken))
       (string-append (cdr (car taken)) " already exists"))
      (else
       (let* ((used '())
              (temp-for
               (lambda (src)
                 (let loop ((n 0))
                   (let ((t (path-join (parent-directory src)
                                       (string-append ".wdired-" (int->string n) "-"
                                                      (basename src)))))
                     (if (or (exists? t) (member t used))
                         (loop (+ n 1))
                         (begin (set! used (cons t used)) t))))))
              (dirs (wdired-dedupe
                     (let loop ((ds dests) (acc '()))
                       (if (null? ds)
                           acc
                           (loop (cdr ds)
                                 (append acc (wdired-missing-parents (car ds) exists?))))))))
         (append (map (lambda (d) (list 'mkdir d)) dirs)
                 (wdired-order moves temp-for)))))))

(define (wdired-duplicate xs)
  (cond ((null? xs) #f)
        ((member (car xs) (cdr xs)) (car xs))
        (else (wdired-duplicate (cdr xs)))))

(define (wdired-dedupe xs)
  (let loop ((xs xs) (acc '()))
    (cond ((null? xs) (reverse acc))
          ((member (car xs) acc) (loop (cdr xs) acc))
          (else (loop (cdr xs) (cons (car xs) acc))))))

;; --- running it -----------------------------------------------------------------
;; Run OPS with `(mv from to)` / `(mkdir dir)` / `(rmdir dir)` (each ⇒ #t or an
;; error string) and `exists?`. A move never overwrites: its target must be
;; absent when it runs. On the first failure every op already done is undone,
;; newest first. ⇒ `(ok)`, or `(failed error stranded)` where `stranded` lists
;; the `(from to)` moves the rollback itself couldn't undo (the file is at `to`).
(define (wdired-execute ops mv mkdir rmdir exists?)
  (define (run op)
    (cond ((equal? (car op) 'mkdir) (mkdir (cadr op)))
          ((exists? (wdired-op-to op)) (string-append (wdired-op-to op) " already exists"))
          (else (mv (wdired-op-from op) (wdired-op-to op)))))
  (define (undo op)
    (if (equal? (car op) 'mkdir)
        (rmdir (cadr op))
        (mv (wdired-op-to op) (wdired-op-from op))))
  (let loop ((ops ops) (done '()))
    (if (null? ops)
        (list 'ok)
        (let ((r (run (car ops))))
          (if (equal? r #t)
              (loop (cdr ops) (cons (car ops) done))
              (list 'failed r
                    (filter (lambda (x) x)
                            (map (lambda (op)
                                   (and (not (equal? (undo op) #t))
                                        (equal? (car op) 'mv)
                                        (list (wdired-op-from op) (wdired-op-to op))))
                                 done))))))))

;; The real file-system calls.
(define (wdired-call program args)
  (let ((r (call-process-full program args "")))
    (if (equal? (car r) "0")
        #t
        (let ((err (trim-end (nth r 2))))
          (if (equal? err "") (string-append program " failed") err)))))
(define (wdired-fs-mv from to) (wdired-call "mv" (list from to)))
(define (wdired-fs-mkdir dir) (wdired-call "mkdir" (list dir)))
(define (wdired-fs-rmdir dir) (wdired-call "rmdir" (list dir)))

;; "a → b, c → d" for the echo area (names relative to DIR where possible).
(define (wdired-describe dir renames)
  (string-join (map (lambda (r) (string-append (basename (car r)) " → " (cdr r))) renames)
               ", "))

;; --- committing -------------------------------------------------------------------
(define-command (wdired-finish-edit)
  (when (equal? (current-buffer) "*dired*")
    (let ((parsed (wdired-parse *wdired-spec* (buffer-string)))
          (dir (dired-current-dir))
          (line (buffer-line)))
      (cond
        ((string? parsed) (message parsed))
        ((null? parsed) (wdired-leave line) (message "(No changes need to be saved)"))
        (else
         (let ((plan (wdired-plan dir parsed file-exists?)))
           (if (string? plan)
               (message (string-append "Nothing renamed: " plan " — fix it or C-c C-k"))
               (let ((result (wdired-execute plan wdired-fs-mv wdired-fs-mkdir
                                             wdired-fs-rmdir file-exists?)))
                 (if (equal? (car result) 'ok)
                     (begin
                       (for-each (lambda (r)
                                   (let ((dest (dired-resolve dir (cdr r))))
                                     (retarget-file-buffers (strip-trailing-slash (car r)) dest)
                                     (dired-retarget-marks! (car r) dest)))
                                 parsed)
                       (wdired-leave line)
                       (let ((n (length parsed)))
                         (message (string-append "Renamed " (int->string n) " file"
                                                 (if (= n 1) "" "s") ": "
                                                 (truncate-string (wdired-describe dir parsed) 200)))))
                     (let ((stranded (caddr result)))
                       (message
                        (string-append
                         "Rename failed: " (cadr result)
                         (if (null? stranded)
                             " — nothing was renamed (fix it or C-c C-k)"
                             (string-append
                              " — and the rollback failed too; these files are at their"
                              " new names: "
                              (string-join (map (lambda (s) (string-append (car s) " → " (cadr s)))
                                                stranded)
                                           ", ")))))))))))))))

;; --- keymaps ----------------------------------------------------------------------
;; Mode-local, so they win over a global C-x C-q (read-only-mode).
(define-key "dired-mode" "ctrl-x ctrl-q" 'wdired-change-to-wdired-mode)
(define-key "dired-mode" "ctrl-c ctrl-e" 'wdired-change-to-wdired-mode)
;; Under evil/helix the special-buffer overlays sit over dired; `i` (insert)
;; enters editing there, like evil-collection's wgrep.
(define-key "evil-normal:dired-mode" "i" 'wdired-change-to-wdired-mode)
(define-key "helix-normal:dired-mode" "i" 'wdired-change-to-wdired-mode)

(define-key "wdired-mode" "ctrl-c ctrl-c" 'wdired-finish-edit)
(define-key "wdired-mode" "ctrl-x ctrl-s" 'wdired-finish-edit)
(define-key "wdired-mode" "ctrl-x ctrl-q" 'wdired-finish-edit)
(define-key "wdired-mode" "ctrl-c ctrl-k" 'wdired-abort-changes)
(define-key "wdired-mode" "ctrl-c escape" 'wdired-abort-changes)
