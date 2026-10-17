;; requires: core magit persistence titlebar-menu

;; Perspectives — one workspace context per project, switched from the title
;; bar. Visiting a file inside a project (find-file-hook) creates that
;; project's perspective on first touch and records the file as its resume
;; point; the title bar's "projects" menu (a titlebar-menu.scm menu) lists
;; every perspective and reopens the picked project at its last visited file.
;; It's on by default; turn it off with (titlebar-menu-hide "projects") — the
;; perspectives keep tracking, and `perspective-switch` still works.
;; The list survives restarts via ~/.config/macros/perspectives.
;;
;; All the logic lives here — titlebar-menu.scm publishes the menu and hands
;; the clicked label back to `perspective-select`. This file doesn't
;; require projectile.scm and Steel resolves free identifiers at definition
;; time, so it must not call projectile functions; the root
;; resolver (`project-root`) lives in core.scm, which is fine.

(define *perspectives* '())        ;; alist: project root -> last visited file, creation order
(define *perspective-current* #f)  ;; root of the perspective shown in the title bar

(define (perspectives-file) (string-append (persistence-dir) "/perspectives"))

;; --- display names -----------------------------------------------------------

(define (perspective-parts root)
  (remove-equal "" (split-many root "/")))

;; The trailing `n` path segments of `root`, joined ("b/proj" for n = 2); the
;; whole root when it has fewer than `n` segments.
(define (perspective-suffix root n)
  (string-join (take-last (perspective-parts root) n) "/"))

;; Whether no OTHER perspective root shares `root`'s n-segment suffix.
(define (perspective-suffix-unique? root n)
  (null? (filter (lambda (kv)
                   (and (not (equal? (car kv) root))
                        (equal? (perspective-suffix (car kv) n)
                                (perspective-suffix root n))))
                 *perspectives*)))

;; A root's dropdown label: the shortest trailing-path suffix ("proj", then
;; "b/proj", then "a/b/proj", …) that no other perspective shares. One
;; qualification level isn't always enough (/x/a/proj and /y/a/proj would both
;; label "a/proj"), and duplicate labels break the label→root reverse lookup —
;; a click on either row would open whichever project registered first.
(define (perspective-display root)
  (define (shortest n)
    (if (or (perspective-suffix-unique? root n)
            (>= n (length (perspective-parts root))))
        (perspective-suffix root n)
        (shortest (+ n 1))))
  (shortest 1))

(define (perspective-root-for-name name)
  (let ((hits (filter (lambda (kv) (equal? (perspective-display (car kv)) name))
                      *perspectives*)))
    (if (null? hits) #f (car (car hits)))))

;; --- title bar ----------------------------------------------------------------

(define perspective-menu "projects")

;; Registered at load, before the user's init.scm, so it sits leftmost.
(titlebar-menu-configure! perspective-menu
                          'section "Recent Projects"
                          'on-select 'perspective-select)

;; Rows carry no command of their own: a click hands the label to the
;; 'on-select function, `perspective-select`.
(define (perspective-sync-titlebar)
  (titlebar-menu-set!
    perspective-menu
    (map (lambda (kv) (cons (perspective-display (car kv)) "")) *perspectives*)
    (if *perspective-current* (perspective-display *perspective-current*) "")))

;; --- tracking -----------------------------------------------------------------

;; Record `file` as `root`'s resume point, creating the perspective on first
;; touch. Updates happen in place so the dropdown keeps creation order.
(define (perspective-update root file)
  (if (alist-ref *perspectives* root)
      (set! *perspectives*
            (map (lambda (kv) (if (equal? (car kv) root) (cons root file) kv))
                 *perspectives*))
      (set! *perspectives* (append *perspectives* (list (cons root file))))))

;; find-file-hook: visiting a file inside a project creates/refreshes that
;; project's perspective, makes it current, and writes the list through to disk
;; (so a perspective survives a restart as soon as it exists — no clean quit
;; required, matching projectile's recent-ring behaviour).
;;
;; The current perspective is STICKY: only visiting a file inside some project
;; moves it. Everything else — dired / the sidebar / terminal / magit /
;; *scratch* (fileless buffers whose buffer-directory can't attribute them to
;; a project), or a stray file outside every root — leaves the checkmark (and
;; the project commands that follow it) on the project you were working in
;; rather than clearing it on every sidebar or terminal visit.
(define (perspective-track)
  (let ((root (project-root (buffer-directory)))
        (file (buffer-file-name)))
    (when (and root (not (equal? file "")))
      (perspective-update root file)
      (set! *perspective-current* root)
      (perspective-sync-titlebar)
      (perspective-save))))

;; --- switching ----------------------------------------------------------------

;; Drop `root`'s perspective (its project vanished from disk): remove it from
;; the list, clear it from the title bar if it was current, and write through.
(define (perspective-remove root)
  (set! *perspectives*
        (alist-remove *perspectives* root))
  (when (equal? *perspective-current* root)
    (set! *perspective-current* #f))
  (perspective-sync-titlebar)
  (perspective-save))

;; Reopen `root`'s perspective at its last visited file. If that file vanished,
;; fall back to a find-file picker in the root (a copy of projectile-find-file-in,
;; which this file can't reference — see the load-order note up top). If the
;; ROOT itself vanished (project deleted/unmounted mid-session — startup only
;; prunes at load), drop the perspective instead of switching the title bar to
;; a dead project over an empty picker.
;;
;; Mark the perspective current HERE, not via find-file-hook: `open-file` only
;; fires that hook on a fresh visit — switching back to a project whose resume
;; file is already buffered just activates the buffer, so relying on the hook
;; left the title bar showing the previous project.
(define (perspective-open root)
  (if (not (file-exists? root))
      (begin
        (perspective-remove root)
        (message (string-append "Project " root " no longer exists — perspective removed")))
      (begin
        (set! *perspective-current* root)
        (perspective-sync-titlebar)
        (let ((file (alist-ref *perspectives* root)))
          (if (and file (file-exists? file))
              (open-file file)
              (helm (list (list (string-append "Find file in " (basename root))
                                (walk-files root)
                                'open-file))))))))

;; The dropdown's click callback (and perspective-switch's Helm action): the
;; picked display label, mapped back to its root.
(define (perspective-select name)
  (let ((root (perspective-root-for-name name)))
    (if root
        (perspective-open root)
        (begin
          (set! *perspectives*
                (filter (lambda (pair) (file-exists? (car pair)))
                        *perspectives*))
          (perspective-save)
          (perspective-sync-titlebar)
          (message (string-append "Removed stale project: " name))))))

;; Keyboard access to the same list.
(define-command (perspective-switch)
  (if (null? *perspectives*)
      (message "No perspectives yet — open a file in a project first")
      (helm (list (list "Switch perspective"
                        (map (lambda (kv) (perspective-display (car kv))) *perspectives*)
                        'perspective-select)))))

;; --- persistence --------------------------------------------------------------
;; One "root<TAB>last-file" line per perspective, in dropdown order.

(define (perspective-save)
  (write-file (perspectives-file)
              (join-lines (map (lambda (kv) (string-append (car kv) "\t" (cdr kv)))
                               *perspectives*))))

;; Restore at startup, dropping projects whose roots no longer exist. The menu
;; starts with no active perspective (nothing checkmarked) until the first file
;; visit picks one.
(define (perspective-load)
  (when (file-exists? (perspectives-file))
    (for-each
      (lambda (line)
        (let ((parts (split-many line "\t")))
          (when (and (>= (length parts) 2) (file-exists? (nth parts 0)))
            (perspective-update (nth parts 0) (nth parts 1)))))
      (non-empty-lines (read-file (perspectives-file))))
    (perspective-sync-titlebar)))

;; Magit follows the title bar: scope it to the current perspective's git work
;; tree so `C-x g` opens the project shown in the dropdown, not whatever buffer
;; is current (a *magit* buffer, terminal, sidebar, …). Installed here because
;; perspectives.scm requires magit.scm (not the reverse), so magit needn't reference perspective
;; state at definition time. Returns #f when no perspective is active or its root
;; isn't a git repo, letting magit fall back to the current buffer's work-tree.
;; MUST go through the owners' setters, not a raw `set!` — a top-level set! from
;; this dependent file would mutate a binding the owners' functions never read.
(set-magit-scope-hook!
  (lambda ()
    (and *perspective-current*
         (let ((top (gix-toplevel *perspective-current*)))
           (and (not (equal? top "")) top)))))

;; Same idea for project-aware commands (compile, term, …): they default to the
;; current perspective's root (core.scm's `project-directory`) rather than the
;; current buffer, so switching projects from the title bar repoints them too.
(set-project-dir-hook! (lambda () *perspective-current*))

;; buffer-switch-hook (re-activating an already-open buffer: C-x b, cycling,
;; the survivor after a kill, pane focus) does BOOKKEEPING ONLY: refresh the
;; buffer's project's resume file so switching back later lands right. It must
;; NOT re-point the CURRENT perspective — those activations are often
;; incidental (killing *compilation* activates whatever the ring surfaces;
;; compile's window dance re-selects panes), and since project commands
;; (compile, term, magit) follow the current perspective, a silent flip sends
;; the next build to the wrong project. The current perspective changes only
;; deliberately: the title-bar dropdown / perspective-switch, or visiting a
;; project file fresh (find-file-hook above).
(define (perspective-note)
  (let ((root (project-root (buffer-directory)))
        (file (buffer-file-name)))
    (when (and root (not (equal? file "")))
      (perspective-update root file)
      (perspective-sync-titlebar)   ; labels may re-qualify on first touch
      (perspective-save))))

;; buffer-select-hook (DELIBERATE buffer switches only: C-x b, C-x <arrow>
;; cycling, revisiting an already-open file, an LSP jump) re-points the current
;; perspective the way a fresh find-file does — so switching to a file in
;; another project moves the title bar (and the project commands that follow it:
;; compile / term / magit) to that project. Incidental activations (pane-focus
;; dances, process-output surfacing) fire buffer-switch-hook but NOT this hook,
;; so they still leave the title bar on the project you were working in.
;;
;; Guard on a real file inside a project: switching to *scratch*, a terminal, or
;; magit (fileless buffers) leaves the current perspective alone, matching
;; perspective-track's stickiness. No-op — and no re-render — when the target's
;; project is already current.
(define (perspective-follow)
  (let ((root (project-root (buffer-directory)))
        (file (buffer-file-name)))
    (when (and root (not (equal? file "")) (not (equal? root *perspective-current*)))
      (perspective-update root file)
      (set! *perspective-current* root)
      (perspective-sync-titlebar)
      (perspective-save))))

;; --- session (desktop) accessors ------------------------------------------------
;; session.scm (loads later) saves/restores the CURRENT perspective with the
;; desktop. It must go through these functions: a cross-file read or `set!` of
;; `*perspective-current*` would hit a different binding than the one this
;; file's functions use (see the setter note above).
(define (perspective-current) *perspective-current*)

;; Re-select `root` as the current perspective without visiting anything (the
;; desktop restores the files itself). Ignored unless `root` is a known, still
;; existing perspective.
(define (perspective-restore-current! root)
  (when (and root (alist-ref *perspectives* root) (file-exists? root))
    (set! *perspective-current* root)
    (perspective-sync-titlebar)))

(add-hook "find-file-hook" "perspective-track")
(add-hook "buffer-switch-hook" "perspective-note")
(add-hook "buffer-select-hook" "perspective-follow")
(perspective-load)

(define-key "fundamental" "ctrl-c p s" 'perspective-switch)
