;; requires: core lib-results persistence

;; Projectile — project-aware navigation, the projectile.el pattern. The root
;; resolver (`project-root`, walk-up-to-marker) lives in core.scm (Emacs's
;; project.el concept); this file adds the interactive surface — find-file,
;; switch-project, grep — plus a mode-line indicator, built on Helm,
;; call-process, and set-mode-line-slot.

(set-face "projectile-modeline" "#66d9ef")

(define *projectile-current* #f)

;; --- file listing -----------------------------------------------------------

;; Project files as absolute paths, via ripgrep's gitignore-aware walker
;; (`walk-files`). Works in non-git dirs too, and is tuned by the search-*
;; options (see init.scm). The result is a STATIC Helm source — walked once when
;; the picker opens, then filtered in memory as you type.
(define (project-files root)
  (walk-files root))

;; --- commands ---------------------------------------------------------------

(define (projectile-find-file-in root)
  (helm (list (list (string-append "Find file in " (basename root))
                    (project-files root)
                    'open-file))))

(define (projectile-find-file)
  (let ((root (project-root (buffer-directory))))
    (if root (projectile-find-file-in root) (message "Not in a project"))))

;; Open another known project: pick its root, set it current, find a file there.
(define (projectile-switch-to root)
  (set! *projectile-current* root)
  (set-mode-line-slot "project" (basename root))
  (projectile-recent-add root)
  (projectile-find-file-in root))

(define (projectile-switch-project)
  (if (null? *project-known*)
      (message "No known projects yet — open a file in one first")
      (helm (list (list "Switch project" *project-known* 'projectile-switch-to)))))

;; Grep across the project with ripgrep's content searcher (`grep-files`,
;; gitignore-aware and honouring the search-* options). Two surfaces:
;;
;;   projectile-grep        (C-c p g)  LIVE Helm picker — type a regexp and the
;;                                     matches narrow in the panel as you go;
;;                                     RET jumps to the file at that line. This
;;                                     is the helm-ag / counsel-rg experience.
;;   projectile-grep-buffer (C-c p G)  one-shot dump into a read-only *grep*
;;                                     buffer, walkable with next-error /
;;                                     previous-error (`g` re-runs it).
;;
;; The pattern is a regexp in both.

;; Live: a RAW (self-filtered) Helm source re-runs the searcher on every
;; keystroke — Rust does no extra filtering, Scheme alone decides the list. We
;; guard to >=2 chars so the first keystroke doesn't grep the whole tree for a
;; single letter. Candidates come back as `path:line:text`; we split off the
;; path and line and jump. (Absolute paths from the searcher carry no colon —
;; the same assumption shell.scm's compile-match makes for grep output.)
(define (projectile-grep-candidates pattern)
  (let ((root (project-root (buffer-directory))))
    (if (or (not root) (< (string-length pattern) 2))
        '()
        (grep-files root pattern))))

(define (projectile-grep-goto sel)
  (let* ((parts (split-many sel ":"))
         (ln (and (>= (length parts) 2) (string->number (nth parts 1)))))
    (when ln
      ;; grep-files reports 1-based line numbers; the jump is 0-based.
      (find-file-goto-line (car parts) (max 0 (- ln 1))))))

(define (projectile-grep)
  (if (project-root (buffer-directory))
      (helm (list (list "Grep project"
                        (list 'raw "projectile-grep-candidates")
                        'projectile-grep-goto)))
      (message "Not in a project")))

;; One-shot buffer grep: a *grep* results buffer (occur.scm's results engine)
;; with one `relative/path:line:text` line per hit. RET / mouse visits a hit,
;; n / p step, `g` greps again, and `e` / C-c C-p (`i` under evil/helix) makes
;; it editable wgrep-style — C-c C-c writes the edited lines back to the files.
(define (projectile-grep-buffer)
  (read-string "Grep project (regexp): " 'projectile-grep-buffer-run))

;; `path:line:text` (as grep-files reports it) -> a results record, with the
;; path shown relative to `root`, or #f for a line that isn't a hit.
(define (projectile-grep-record root hit)
  (let ((parts (grep-hit-parts hit)))
    (and parts
         (let* ((path (car parts))
                (line (string->number (cadr parts)))
                (text (nth parts 2))
                (rel (if (starts-with? path (path-join root ""))
                         (substring path (string-length (path-join root "")) (string-length path))
                         path)))
           (results-record "" path (- line 1) (string-append rel ":" (cadr parts) ":") text)))))

(define (projectile-grep-buffer-run pattern)
  (let ((root (project-root (buffer-directory))))
    (if (not root)
        (message "Not in a project")
        (let ((matches (grep-files root pattern)))
          (if (null? matches)
              (message (string-append "No matches for " pattern))
              (begin
                (results-set! "*grep*"
                              (string-append (int->string (length matches)) " match"
                                             (if (= (length matches) 1) "" "es")
                                             " for \"" pattern "\" in " root)
                              "grep-mode" pattern
                              (filter (lambda (r) r)
                                      (map (lambda (hit) (projectile-grep-record root hit))
                                           matches)))
                (results-render "*grep*")
                (goto-line 1)))))))

;; --- recent projects (persisted) --------------------------------------------
;; A ring of project roots, newest-first, written through to
;; ~/.config/macros/projects (one root per line) so the welcome/home screen can
;; list recently-opened projects on the NEXT launch — unlike *project-known*,
;; which is session-only. Reuses persistence.scm's config dir (required above)
;; and core.scm's mru-push.
(define *projectile-recent* '())
(define *projectile-recent-max* 20)
(define (projectile-recent-file) (string-append (persistence-dir) "/projects"))

;; Promote `root` to the front of the ring; persist immediately if it changed,
;; so a project survives a restart as soon as you open a file in it (no save or
;; clean quit required).
(define (projectile-recent-add root)
  (when (and root (not (equal? root "")))
    (let ((new (mru-push root *projectile-recent* *projectile-recent-max*)))
      (when (not (equal? new *projectile-recent*))
        (set! *projectile-recent* new)
        (projectile-recent-save)))))

(define (projectile-recent-save)
  (write-file (projectile-recent-file) (join-lines *projectile-recent*)))

;; #t when `child` lives inside `parent` (a strict sub-directory), so a
;; workspace member that was recorded before VCS-preference resolution can be
;; dropped in favor of the enclosing repo root.
(define (projectile-subpath? child parent)
  (and (not (equal? child parent))
       (starts-with? child (path-join parent ""))))

(define (projectile-nested-in? p roots)
  (cond ((null? roots) #f)
        ((projectile-subpath? p (car roots)) #t)
        (else (projectile-nested-in? p (cdr roots)))))

;; Drop any root that is a sub-directory of another root in the same list.
(define (projectile-prune-nested roots)
  (filter (lambda (p) (not (projectile-nested-in? p roots))) roots))

;; Load the ring at startup, dropping roots that no longer exist or that nest
;; inside another root, and seed *project-known* so switch-project lists them
;; before any file is opened. Re-persist if pruning changed the list.
(define (projectile-recent-load)
  (when (file-exists? (projectile-recent-file))
    (let ((loaded (take (filter (lambda (p) (file-exists? p))
                                     (non-empty-lines (read-file (projectile-recent-file))))
                             *projectile-recent-max*)))
      (set! *projectile-recent* (projectile-prune-nested loaded))
      (when (not (equal? *projectile-recent* loaded))
        (projectile-recent-save)))
    (for-each (lambda (root)
                (when (not (member root *project-known*))
                  (set! *project-known* (cons root *project-known*))))
              *projectile-recent*)))

;; find-file-hook: record the visited file's project as recently opened.
(define (projectile-track)
  (let ((root (project-root (buffer-directory))))
    (when root (projectile-recent-add root))))

;; --- mode-line indicator ----------------------------------------------------
;; Recompute the project on every file visit (find-file-hook) and show its name,
;; plus the git branch ("git:main") when the file is inside a git work tree.
(define (projectile-update-modeline)
  (let ((root (project-root (buffer-directory))))
    (set-mode-line-slot "project" (if root (basename root) ""))
    (set-mode-line-slot "git" (gix-branch (buffer-directory)))))

(add-hook "find-file-hook" "projectile-update-modeline")
(add-hook "find-file-hook" "projectile-track")
;; Re-activating an already-open buffer skips find-file-hook, so the mode-line
;; project/branch would show the previous buffer's project after C-x b or
;; cycling. buffer-switch-hook fires on every activation — keep the indicator
;; pointed at whatever buffer is now current.
(add-hook "buffer-switch-hook" "projectile-update-modeline")

;; Restore the persisted recent-project list at startup.
(projectile-recent-load)

;; --- keymap (projectile's C-c p prefix) -------------------------------------
(define-key "fundamental" "ctrl-c p f" 'projectile-find-file)
(define-key "fundamental" "ctrl-c p p" 'projectile-switch-project)
(define-key "fundamental" "ctrl-c p g" 'projectile-grep)
(define-key "fundamental" "ctrl-c p G" 'projectile-grep-buffer)

;; grep results buffer: occur's view keys (RET/mouse visit, n/p, e / C-c C-p
;; to edit, q to close). Its `g` (grep again) is grep.scm's `grep-again`,
;; which reruns M-x grep's search or falls back to projectile-grep-buffer.
(results-bind-view-keys "grep-mode")

;; M-x discoverability for the projectile entry points.
(register-command 'projectile-find-file)
(register-command 'projectile-grep)
(register-command 'projectile-grep-buffer)
(register-command 'projectile-switch-project)
