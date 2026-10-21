;; requires: core dired

;; dired-sidebar.scm — the bundled file-tree, à la Emacs `dired-sidebar` +
;; `dired-subtree`. A real dired buffer pinned to a side window: you navigate the
;; tree AND operate on files (mark, delete, rename, copy, mkdir), and directories
;; expand/collapse inline (the dired-subtree behavior). M-x dired-sidebar / C-c t.
;;
;; Two layers in one file:
;;   1. the dired-subtree ENGINE — `dired-subtree-build` / `-paint` and friends:
;;      a pure, reusable renderer that turns a root directory, a set of OPEN
;;      subdirectories, and a set of MARKED paths into an indented, icon-decorated,
;;      expandable listing. It owns no buffer and no keymap.
;;   2. the `dired-sidebar` consumer: state, the side-window lifecycle, the
;;      dired-style file operations, and the keymap.
;;
;; They live together because Macros evaluates bundled scheme/ files in sorted
;; order and a file cannot forward-reference a later one (see also the dired /
;; dired-subtree split note in steel-load-order): a consumer must be able to see
;; the engine at eval time, so the engine and its consumer share a file.
;;
;; The flat directory editor in dired.scm is untouched — this is an additive tree
;; layer, not a change to dired-mode. The file operations reuse dired.scm's
;; (dired-move-file / -copy-file / -make-directory / -run: error reporting,
;; overwrite confirmation, "~/" and absolute targets), addressed at the entry
;; under point in the tree rather than a single flat directory.
;;
;; Built on general primitives (list-directory, render-buffer /
;; render-buffer-quiet, add-line-icon, overlays, window splitting, call-process,
;; the per-mode cursor-visible / hl-line options). Live refresh rides the same "notify" path dired uses —
;; watch-directories declares the root + every expanded subdir, and
;; directory-changed-hook re-renders when one changes on disk, in place (never
;; stealing focus from the editing pane).
;;
;; Colors come from faces the active theme already defines, so the tree tracks the
;; current theme (and any `load-theme` switch) instead of hardcoding hex:
;;   folder icons          -> "keyword" (the theme's primary accent; names stay
;;                            the default foreground)
;;   dotfiles, gitignored  -> "comment" (the theme's muted color, whole line)
;;   chevrons              -> "comment"
;;   indent rails          -> "dim"     (the theme's faint UI color)
;; An icon added with face "" takes its line's face; the renderer falls back to
;; the foreground if a theme omits one of these names.

;; ===========================================================================
;; dired-subtree engine — pure builder + painter, no state, no keymap.
;; ===========================================================================

;; Drop a trailing "/" (list-directory marks dirs with one; the editor reports
;; changed directories without one — normalize before comparing/keying).

;; Vertical indent guides: one "│ " per ancestor level below the top. Each is 2
;; columns wide — the width of the chevron column — so a rail sits under its
;; parent directory's chevron. Top-level entries (depth 0) get none.
(define (dired-subtree-rails depth) (string-repeat "│ " depth))

;; Whole-line face for an entry, or #f to leave it the default foreground.
;; Dotfiles and gitignored paths (IGNORED?) dim; everything else — directories
;; included — keeps the foreground (directories are told apart by their
;; chevron and accent-tinted icon, not a colored name).
(define (dired-subtree-face name ignored?)
  (if (or ignored? (starts-with? name ".")) "comment" #f))

;; Is the directory DIR (no trailing slash) a symlink? Its real path then differs
;; from its parent's real path plus its own name.
(define (dired-subtree-symlink-dir? dir)
  (not (equal? (file-truename dir)
               (path-join (file-truename (parent-directory dir)) (basename dir)))))

;; PATHS as `git check-ignore` can take them. git aborts the WHOLE call ("beyond
;; a symbolic link") on a symlinked directory spelled with a trailing "/" or on
;; anything under one, so such a directory goes in as a plain link (no slash)
;; and its contents are left out.
(define (dired-subtree-git-paths paths)
  (let loop ((ps paths) (links '()) (acc '()))
    (if (null? ps)
        (reverse acc)
        (let* ((p (car ps)) (clean (strip-trailing-slash p)))
          (cond ((not (null? (filter (lambda (l) (starts-with? p (string-append l "/")))
                                     links)))
                 (loop (cdr ps) links acc))
                ((and (ends-with? p "/") (dired-subtree-symlink-dir? clean))
                 (loop (cdr ps) (cons clean links) (cons clean acc)))
                (else (loop (cdr ps) links (cons p acc))))))))

;; The gitignored subset of PATHS (full paths under ROOT, dirs with a trailing
;; "/"), as a hash keyed by slash-stripped full path. One `git check-ignore
;; --stdin` call per render; outside a git repo (or without git) it reports
;; nothing, so nothing dims. Paths are fed relative to ROOT and mapped back.
(define (dired-subtree-ignored root paths)
  (let* ((prefix (string-append root "/"))
         (plen (string-length prefix))
         (rel (lambda (p) (if (starts-with? p prefix)
                              (substring p plen (string-length p))
                              p)))
         (paths (dired-subtree-git-paths paths)))
    (if (null? paths)
        (hash)
        (let ((out (nth (call-process-full
                          "git"
                          (list "-C" root "-c" "core.quotePath=false"
                                "check-ignore" "--stdin")
                          (string-append (join-lines (map rel paths)) "\n"))
                        1)))
          (let loop ((ls (non-empty-lines out)) (acc (hash)))
            (if (null? ls)
                acc
                (loop (cdr ls)
                      (hash-insert acc
                                   (string-append prefix
                                                  (strip-trailing-slash (car ls)))
                                   #t))))))))

;; Extension → Lucide icon (assets/icons/<name>.svg). Customize freely.
(define (dired-subtree-icon name)
  (cond
    ((or (ends-with? name ".rs") (ends-with? name ".c") (ends-with? name ".h")
         (ends-with? name ".cpp") (ends-with? name ".go") (ends-with? name ".py")
         (ends-with? name ".js") (ends-with? name ".ts") (ends-with? name ".java")
         (ends-with? name ".rb") (ends-with? name ".lua") (ends-with? name ".scm")) "file-code")
    ((ends-with? name ".json") "braces")
    ((or (ends-with? name ".md") (ends-with? name ".txt") (ends-with? name ".org")) "file-text")
    ((or (ends-with? name ".toml") (ends-with? name ".yaml") (ends-with? name ".yml")
         (ends-with? name ".ini") (ends-with? name ".conf")) "settings-2")
    ((or (ends-with? name ".png") (ends-with? name ".jpg") (ends-with? name ".jpeg")
         (ends-with? name ".gif") (ends-with? name ".svg")) "image")
    ((ends-with? name ".lock") "lock")
    (else "file")))

;; Directories first, then files — each group keeps list-directory's
;; alphabetical order (filter is stable).
(define (dired-subtree-sort entries)
  (append (filter (lambda (e) (ends-with? e "/")) entries)
          (filter (lambda (e) (not (ends-with? e "/"))) entries)))

;; Build the tree for ROOT with EXPANDED (a list of directory paths, no trailing
;; slash, currently open) and MARKS (a list of full paths, as they appear in the
;; entry map, flagged for deletion). ROOT's own row is not shown — its children
;; are the top level (the mode line already names the project). Returns a list
;; of five lists, top-to-bottom:
;;   0 lines   — (string …)                      rendered text rows
;;   1 entries — ((line path is-dir) …)          hit-test map for the caller's keys
;;   2 icons   — ((line byte-col icon face) …)   per-row icon placements; face ""
;;                                               takes the line's face
;;   3 faces   — ((line face) …)                 whole-line color overrides
;;   4 guides  — ((start-byte end-byte face) …)  indent-rail overlay accents
;; No buffer side effects — the caller paints the result with dired-subtree-paint
;; (it reads the filesystem and asks git which paths are ignored). Each row is:
;;   [mark gutter] [rails] [chevron: 2 cols] [icon: 3 cols] name
;; The 2-column mark gutter ("D " / "  ") only exists while some path is marked,
;; so an unmarked tree starts flush left. Icons/guides are addressed by BYTE
;; offset (a "│" is 3 bytes), so the rail prefix is measured with utf8-length.
(define dired-subtree-mark-width 2)   ; bytes in the "D "/"  " gutter (ASCII)
(define dired-subtree-chevron-width 2) ; reserved columns for the chevron
(define (dired-subtree-build root expanded marks)
  ;; Pass 1: walk the visible tree into (name full is-dir open depth) rows.
  (define (walk dir depth acc)
    (foldl (lambda (full acc)
             (let* ((is-dir (ends-with? full "/"))
                    (clean (strip-trailing-slash full))
                    (open (and is-dir (member clean expanded)))
                    (acc (cons (list (basename clean) full is-dir open depth) acc)))
               (if open (walk clean (+ depth 1) acc) acc)))
           acc
           (dired-subtree-sort (list-directory dir))))
  (let* ((rows (reverse (walk root 0 '())))
         (ignored (dired-subtree-ignored root (map (lambda (r) (nth r 1)) rows)))
         (mw (if (null? marks) 0 dired-subtree-mark-width))
         (lines '()) (entries '()) (icons '()) (faces '()) (guides '())
         (byte 0) (n 0))
    ;; Pass 2: emit each row's text, icons, face and rail overlay.
    (for-each
      (lambda (row)
        (let* ((name (nth row 0)) (path (nth row 1)) (is-dir (nth row 2))
               (open (nth row 3)) (depth (nth row 4))
               (rails (dired-subtree-rails depth))
               (gb (utf8-length rails))                 ; byte width of the rail region
               (mark (cond ((= mw 0) "")
                           ((member path marks) "D ")
                           (else "  ")))
               ;; Chevron + icon in their reserved columns, then the name.
               (text (string-append mark rails "     " name))
               (chev-col (+ mw gb))
               (icon-col (+ chev-col dired-subtree-chevron-width))
               (face (dired-subtree-face
                       name (hash-contains? ignored (strip-trailing-slash path)))))
          (set! lines (cons text lines))
          (set! entries (cons (list n path is-dir) entries))
          (if is-dir
              (begin
                (set! icons (cons (list n chev-col
                                        (if open "chevron-down" "chevron-right")
                                        "comment")
                                  icons))
                ;; A dimmed folder keeps its dim icon; others take the accent.
                (set! icons (cons (list n icon-col (if open "folder-open" "folder")
                                        (if face "" "keyword"))
                                  icons)))
              (set! icons (cons (list n icon-col (dired-subtree-icon name) "") icons)))
          (when face (set! faces (cons (list n face) faces)))
          (when (> gb 0)
            (set! guides (cons (list (+ byte mw) (+ byte mw gb) "dim") guides)))
          (set! n (+ n 1))
          (set! byte (+ byte (utf8-length text) 1))))   ; +1 = newline
      rows)
    (list (reverse lines) (reverse entries) (reverse icons)
          (reverse faces) (reverse guides))))

;; Paint a tree BUILT by dired-subtree-build into BUFFER (major mode MODE). When
;; QUIET? is true, render in place without stealing focus from the editing pane
;; (an FS auto-refresh); otherwise switch to the buffer. clear-overlays first so a
;; refresh doesn't stack stale indent rails (the whole-line face would otherwise
;; tint the rails too, hence painting them via overlays with the "dim" face).
(define (dired-subtree-paint built buffer mode quiet?)
  (let ((lines  (nth built 0))
        (icons  (nth built 2))
        (faces  (nth built 3))
        (guides (nth built 4)))
    (if quiet?
        (render-buffer-quiet buffer mode (join-lines lines) faces)
        (render-buffer buffer mode (join-lines lines) faces))
    (for-each (lambda (ic) (add-line-icon (nth ic 0) (nth ic 1) (nth ic 2) (nth ic 3)))
              icons)
    (clear-overlays)
    (overlay-set-faces guides)))  ; already (start end face) triples — one call

;; The (path is-dir) of the ENTRIES row at LINE, or #f. ENTRIES is element 1 of a
;; dired-subtree-build result; consumers keep it from the last render to resolve
;; clicks / point.
(define (dired-subtree-entry-at entries line)
  (let loop ((es entries))
    (cond ((null? es) #f)
          ((= (car (car es)) line) (cdr (car es)))
          (else (loop (cdr es))))))

;; Build → paint → re-declare the watch set, in one call. Returns the entries
;; list (element 1) for the consumer to stash for hit-testing. Replace-per-owner:
;; this single watch-directories call re-declares root + expanded each render, so
;; collapsed dirs drop off the watch list.
(define (dired-subtree-show root expanded marks buffer mode owner quiet?)
  (let ((built (dired-subtree-build root expanded marks)))
    (dired-subtree-paint built buffer mode quiet?)
    (watch-directories owner (cons root expanded))
    (nth built 1)))

;; The root to open a tree at: the project root of the current buffer's directory,
;; else that directory; slash stripped.
(define (dired-subtree-default-root)
  (strip-trailing-slash
    (or (project-root (buffer-directory)) (buffer-directory))))

;; Directories to open so PATH (a file) becomes visible under ROOT: every ancestor
;; of PATH from just inside ROOT down to PATH's own parent. Returns '() when PATH
;; isn't under ROOT (nothing to reveal). Walks up parent-by-parent, so it stops at
;; the filesystem root even when the prefix never matches.
(define (dired-subtree-ancestors root path)
  (let ((root (strip-trailing-slash root)))
    (let loop ((dir (parent-directory (strip-trailing-slash path))) (acc '()))
      (cond ((equal? dir root) acc)
            ((or (equal? dir "")
                 (not (starts-with? (string-append dir "/")
                                    (string-append root "/")))) '())
            (else (loop (parent-directory dir) (cons dir acc)))))))

;; Toggle PATH in EXPANDED (a list of dir paths), returning the new list.
(define (dired-subtree-toggled expanded path)
  (if (member path expanded)
      (remove-equal path expanded)
      (cons path expanded)))

;; ===========================================================================
;; dired-sidebar — a dired tree pinned to a side window (C-c t).
;; ===========================================================================

(define *dired-sidebar-root* "")         ; tree root (no trailing /)
(define *dired-sidebar-expanded* '())    ; dir paths (no trailing /) currently open
(define *dired-sidebar-marks* '())       ; full paths (as in entries) flagged "D"
(define *dired-sidebar-entries* '())     ; (line path is-dir) from the last render
(define *dired-sidebar-window* #f)       ; the pane id hosting the sidebar
(define *dired-sidebar-quiet* #f)        ; #t during an FS auto-refresh

(define (dired-sidebar-render)
  (set! *dired-sidebar-entries*
        (dired-subtree-show *dired-sidebar-root* *dired-sidebar-expanded*
                            *dired-sidebar-marks*
                            "*dired-sidebar*" "dired-sidebar-mode" "dired-sidebar"
                            *dired-sidebar-quiet*)))

;; Is the sidebar already on screen? Its buffer must still exist and its pane must
;; still be live (the user may have killed either).
(define (dired-sidebar-live?)
  (and (buffer-live? "*dired-sidebar*")
       *dired-sidebar-window*
       (member *dired-sidebar-window* (window-list))))

;; Open the tree in a 20%-wide sidebar on the left, pushing the current buffer into
;; an 80% pane on the right. Re-invoking just refocuses the sidebar, preserving the
;; expanded directories and any pending marks; only a fresh open resets them.
;; Move point onto the row showing full path PATH, if it's currently visible in
;; the tree. Entries are (line path is-dir); dirs carry a trailing slash, so both
;; sides are stripped before comparing.
(define (dired-sidebar-goto-path path)
  (let ((target (strip-trailing-slash path)))
    (let loop ((es *dired-sidebar-entries*))
      (cond ((null? es) #f)
            ((equal? (strip-trailing-slash (nth (car es) 1)) target)
             (goto-line (nth (car es) 0)))
            (else (loop (cdr es)))))))

(define (dired-sidebar)
  (if (dired-sidebar-live?)
      (begin (select-window *dired-sidebar-window*) (dired-sidebar-render))
      ;; Read the visited file BEFORE the split — afterward the tree buffer is
      ;; active and buffer-file-name no longer points at the file we came from.
      (let ((file (buffer-file-name)))
        (set! *dired-sidebar-root* (dired-subtree-default-root))
        ;; Reveal the current file: open its ancestor dirs so it shows in the tree
        ;; ('() when there's no file, or it's outside the root).
        (set! *dired-sidebar-expanded*
              (if (equal? file "")
                  '()
                  (dired-subtree-ancestors *dired-sidebar-root* file)))
        (set! *dired-sidebar-marks* '())
        ;; Pin the tree to the frame's left edge: split the first (leftmost)
        ;; window, not whichever pane happens to be selected. The split keeps
        ;; that pane selected, so it (now the left 20%) is where the tree
        ;; renders; the right 80% keeps its buffer.
        (select-window (car (window-list)))
        (set! *dired-sidebar-window* (selected-window))
        (split-window-right-ratio 0.2)
        (dired-sidebar-render)
        ;; Land point on the revealed file (no-op if it isn't under the root).
        (when (not (equal? file ""))
          (dired-sidebar-goto-path file)))))
(define (dired-sidebar-refresh) (dired-sidebar-render))

;; directory-changed-hook handler: re-render only when the sidebar buffer is alive
;; and the change is in a directory the tree shows (root or an expanded subdir).
(define (dired-sidebar-on-fs-change dir)
  (when (and (buffer-live? "*dired-sidebar*")
             (member (strip-trailing-slash dir)
                     (cons *dired-sidebar-root* *dired-sidebar-expanded*)))
    (set! *dired-sidebar-quiet* #t)
    (dired-sidebar-render)
    (set! *dired-sidebar-quiet* #f)))
(add-hook "directory-changed-hook" "dired-sidebar-on-fs-change")

;; Close the sidebar: kill the tree buffer and collapse its window.
(define (dired-sidebar-quit)
  (set! *dired-sidebar-window* #f)
  (watch-directories "dired-sidebar" '())   ; stop watching; nothing to refresh
  (kill-buffer-and-window))

;; --- point helpers ----------------------------------------------------------
;; The (path is-dir) under point, or #f.
(define (dired-sidebar-entry) (dired-subtree-entry-at *dired-sidebar-entries* (buffer-line)))
;; The full path under point, or #f.
(define (dired-sidebar-path)
  (let ((e (dired-sidebar-entry))) (if e (car e) #f)))
;; The directory to operate IN for the entry under point: the entry itself if it's
;; a directory, else its parent; the root when point is off any entry.
(define (dired-sidebar-context-dir)
  (let ((e (dired-sidebar-entry)))
    (if e
        (let ((path (strip-trailing-slash (car e))) (is-dir (car (cdr e))))
          (if is-dir path (parent-directory path)))
        *dired-sidebar-root*)))

;; --- navigation -------------------------------------------------------------
;; RET/TAB/click: toggle a directory, or open a file in the main editor window
;; (the tree stays put).
(define (dired-sidebar-visit)
  (let ((e (dired-sidebar-entry)))
    (when e
      (let ((path (car e)) (is-dir (car (cdr e))))
        (if is-dir
            (let ((dir (strip-trailing-slash path)))
              ;; Collapsing hides the subtree: drop its marks too, so `x` never
              ;; deletes something the tree no longer shows.
              (when (member dir *dired-sidebar-expanded*)
                (set! *dired-sidebar-marks*
                      (filter (lambda (m) (not (starts-with? m (string-append dir "/"))))
                              *dired-sidebar-marks*)))
              (set! *dired-sidebar-expanded*
                    (dired-subtree-toggled *dired-sidebar-expanded* dir))
              (dired-sidebar-render))
            (open-file-other-window path))))))

;; --- marks + delete ---------------------------------------------------------
(define (dired-sidebar-mark)
  (let ((p (dired-sidebar-path)))
    ;; Never flag the root row — `x` would `rm -rf` the whole tree.
    (when (and p
               (not (equal? (strip-trailing-slash p) *dired-sidebar-root*))
               (not (member p *dired-sidebar-marks*)))
      (set! *dired-sidebar-marks* (cons p *dired-sidebar-marks*))
      (dired-sidebar-render)
      (next-line))))
(define (dired-sidebar-unmark)
  (let ((p (dired-sidebar-path)))
    (when p
      (set! *dired-sidebar-marks* (remove-equal p *dired-sidebar-marks*))
      (dired-sidebar-render)
      (next-line))))

(define (dired-sidebar-do-delete)
  (if (null? *dired-sidebar-marks*)
      (message "No files marked (mark with d)")
      (y-or-n-p (string-append "Delete " (int->string (length *dired-sidebar-marks*)) " marked file(s)?")
                'dired-sidebar-delete-confirmed)))
(define (dired-sidebar-delete-confirmed ans)
  (when (equal? ans "yes")
    ;; dired-run echoes each failure; only a clean run says "Deleted".
    (let ((failed (filter (lambda (p)
                            (not (dired-run "rm" (list "-rf" (strip-trailing-slash p)))))
                          *dired-sidebar-marks*)))
      (set! *dired-sidebar-marks* failed)
      (dired-sidebar-render)
      (when (null? failed) (message "Deleted")))))

;; --- rename / copy / mkdir --------------------------------------------------
;; SRC was renamed to DEST: marks and open subdirectories follow it.
(define (dired-sidebar-moved src dest)
  (set! *dired-sidebar-marks*
        (map (lambda (m) (dired-retarget m src dest)) *dired-sidebar-marks*))
  (set! *dired-sidebar-expanded*
        (map (lambda (d) (dired-retarget d src dest)) *dired-sidebar-expanded*))
  (dired-sidebar-render))

;; The source is captured in the closure — the callback runs with the minibuffer
;; current, so the entry under point can't be re-read there. Targets resolve
;; against the entry's own directory.
(define (dired-sidebar-rename)
  (let ((p (dired-sidebar-path)))
    (when p
      (let ((src (strip-trailing-slash p)))
        (read-string (string-append "Rename " (basename src) " to: ")
          (lambda (name)
            (dired-move-file src (parent-directory src) name
              (lambda (dest) (dired-sidebar-moved src dest)))))))))

(define (dired-sidebar-copy)
  (let ((p (dired-sidebar-path)))
    (when p
      (let ((src (strip-trailing-slash p)))
        (read-string (string-append "Copy " (basename src) " to: ")
          (lambda (name)
            (dired-copy-file src (parent-directory src) name
              (lambda (dest) (dired-sidebar-render)))))))))

(define (dired-sidebar-mkdir)
  (let ((dir (dired-sidebar-context-dir)))
    (read-string "Create directory: "
      (lambda (name)
        (dired-make-directory dir name (lambda (dest) (dired-sidebar-render)))))))

;; --- keymap -----------------------------------------------------------------
;; Single keys resolve against this mode keymap under both Emacs and evil: as a
;; special buffer, dired-sidebar-mode's only evil overlay is j/k (evil-collection),
;; so d/u/x/g/q/R/C/+ fall through here just like dired-mode.
(define-key "dired-sidebar-mode" "enter" 'dired-sidebar-visit)
(define-key "dired-sidebar-mode" "tab" 'dired-sidebar-visit)
;; Left click acts like RET: expand/collapse a dir, or open a file at point.
(define-key "dired-sidebar-mode" "mouse-1" 'dired-sidebar-visit)
(define-key "dired-sidebar-mode" "n" 'next-line)
(define-key "dired-sidebar-mode" "p" 'previous-line)
(define-key "dired-sidebar-mode" "g" 'dired-sidebar-refresh)
(define-key "dired-sidebar-mode" "d" 'dired-sidebar-mark)
(define-key "dired-sidebar-mode" "u" 'dired-sidebar-unmark)
(define-key "dired-sidebar-mode" "x" 'dired-sidebar-do-delete)
(define-key "dired-sidebar-mode" "shift-r" 'dired-sidebar-rename)
(define-key "dired-sidebar-mode" "shift-c" 'dired-sidebar-copy)
(define-key "dired-sidebar-mode" "+" 'dired-sidebar-mkdir)
;; q closes the sidebar outright — kills the tree buffer AND collapses its pane.
(define-key "dired-sidebar-mode" "q" 'dired-sidebar-quit)

;; Evil + Helix overlays (each consulted only under its own modal mode, before
;; the keymap above — same pattern as magit-status). The collection layer only
;; gives j/k, so add the top/bottom motions: `g g` and `G` (evil), plus helix's
;; `g e`. `g g` makes `g` a prefix, shadowing the single-`g` refresh, so
;; re-expose refresh on `g r`. The motion helpers are evil.scm's (plain point
;; moves, modal-agnostic; helix-goto-top/-bottom rewrite selections, which a
;; read-only tree doesn't have). Emacs M-< / M-> need nothing here — alt chords
;; fall through the special-buffer dispatch to fundamental's bindings.
(for-each
 (lambda (prefix)
   (let ((km (string-append prefix "dired-sidebar-mode")))
     (define-key km "g g" 'evil-goto-first-line)
     (define-key km "shift-g" 'evil-goto-last-line)
     (define-key km "g r" 'dired-sidebar-refresh)))
 (list "evil-normal:" "helix-normal:"))
(define-key "helix-normal:dired-sidebar-mode" "g e" 'evil-goto-last-line)

;; The tree is a listing, not code — no line-number gutter (on by default for
;; every other major mode); highlight the current row instead of drawing a
;; cursor; truncate long names at the
;; pane edge instead of wrapping (ordinary buffers still soft-wrap).
(set-option "line-numbers:dired-sidebar-mode" "false")
(set-option "hl-line:dired-sidebar-mode" "true")
;; The hl-line band marks the row, so don't also paint a cursor block in the
;; (empty) leading column.
(set-option "cursor-visible:dired-sidebar-mode" "false")
(set-option "word-wrap:dired-sidebar-mode" "false")

(define-key "fundamental" "ctrl-c t" 'dired-sidebar)
(register-command 'dired-sidebar)
