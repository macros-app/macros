;; requires: core lib-async lib-section lib-transient

;; Magit — a git porcelain, built entirely in Scheme on the special-buffer and
;; transient primitives (the Rust side knows nothing about git):
;;   * `call-process`  — run git, get stdout
;;   * `render-buffer` — draw a read-only buffer with per-line faces
;;   * `buffer-line` / `goto-line` — find / restore the section at point
;;   * `transient`     — the commit popup
;;
;; This mirrors how `magit-section` + `transient` sit on top of plain Emacs
;; primitives. Folding-by-regeneration: a refresh just rebuilds the buffer text.

;; Faces (fall back to the default fg if a theme doesn't define them). Every
;; "magit-*" face below is user-overridable: redefine any of them in init.scm
;; with (set-face "<name>" "#rrggbb") — init.scm loads after this file, so your
;; color wins (themes can set them too, via load-theme).
(set-face "magit-section-heading" "#a6e22e")
(set-face "magit-file" "#e6db74")
(set-face "magit-key" "#66d9ef")
;; Diff faces: whole-line colors (via render-buffer line faces).
(set-face "magit-added" "#7a9a3a")    ; dim green body for +lines
(set-face "magit-removed" "#b34a5f")  ; dim red body for -lines
(set-face "magit-hunk" "#66d9ef")     ; @@ hunk headers
(set-face "magit-context" "#888888")  ; unchanged context lines
;; Structural-view sub-heading: the enclosing function/scope a run of hunks
;; lives in (git's `@@ … @@ <context>` funcname). Dim teal so it reads as a
;; child of the yellow file name without competing with the green section title.
(set-face "magit-group" "#7fb0a8")
;; Sub-line overlay accents: the leading +/- marker, brighter than its line.
(set-face "magit-add-marker" "#a6e22e")
(set-face "magit-remove-marker" "#f92672")
;; Within-line word diff (delta / diff-highlight style). A MODIFIED line reads in
;; three tiers: a dim base for the unchanged text, the bright marker, and the
;; bright changed span. (A fully added/removed line — no pairing — keeps the
;; medium body face above instead, so brand-new lines still read clearly.)
(set-face "magit-added-dim" "#4e5e34")    ; unchanged text on a modified +line
(set-face "magit-removed-dim" "#6e4651")  ; unchanged text on a modified -line
(set-face "magit-word-added" "#cdee69")   ; the changed span on a +line
(set-face "magit-word-removed" "#fc6a8a") ; the changed span on a -line
;; File-status labels in front of changed files (Emacs magit's "modified",
;; "new file", "deleted", …). Each label is overlay-colored by its status.
(set-face "magit-label-modified" "#e2c08d")
(set-face "magit-label-new"      "#a6e22e")
(set-face "magit-label-deleted"  "#f92672")
(set-face "magit-label-renamed"  "#66d9ef")
(set-face "magit-label-other"    "#e6db74")
;; Commit short hashes in log / unpushed / recent-commits sections.
(set-face "magit-hash" "#b58900")
;; Header lines: the "Head:" / "Merge:" label column vs the branch name vs the
;; trailing subject/tracking info — three tiers so they read apart.
(set-face "magit-header" "#9a9a9a")  ; the label column
(set-face "magit-branch" "#66d9ef")  ; branch / upstream names
;; Current-line band (hl-line, painted by the Rust element as a background).
;; Translucent white reads well on any dark theme.
(set-face "hl-line" "#ffffff14")
;; Git-gutter (diff-hl): per-line change bars in the line-number gutter of
;; ordinary file buffers — added (green bar), modified (amber bar), removed
;; (red mark where lines were deleted). These mark the working tree against
;; HEAD, the inverse of the magit-status diff faces above. Configure with
;;   (set-option "magit-gutter" #f)          ; disable entirely
;;   (set-option "magit-gutter-width" 3)     ; bar width in px (1–8, default 2)
;;   (set-option "magit-gutter-deleted" #f)  ; hide deletion marks
;; or toggle live with M-x magit-gutter-mode; M-x magit-gutter-refresh recomputes.
(set-face "magit-gutter-added" "#6cae3e")
(set-face "magit-gutter-modified" "#c7a23a")
(set-face "magit-gutter-removed" "#c1574b")

;; --- git plumbing -----------------------------------------------------------

;; History depth caps: how many commits the log views fetch, and how many the
;; unpushed/unpulled sections walk before cutting off.
(define *magit-log-limit* 200)
(define *magit-unpushed-limit* 256)

;; The repo root magit is operating on. Resolved from the buffer you launch from
;; (NOT the process cwd) — like Emacs's `magit-toplevel` over `default-directory`.
(define *magit-dir* ".")

;; Run git in the status buffer's repo, returning stdout.
(define (git args)
  (call-process "git" (append (list "-C" *magit-dir*) args)))

;; Walk up from `dir` to the enclosing git work tree, like `magit-toplevel`.
;; Returns `dir` unchanged if it isn't inside a repo (gix returns "").
(define (magit-toplevel dir)
  (let ((root (gix-toplevel dir)))
    (if (equal? root "") dir root)))

;; Porcelain entry helpers: a line is "XY path" (X = index, Y = worktree).
;; A rename/copy entry's path field is "OLD -> NEW"; the helpers below split it
;; so git operations get real pathspecs rather than the literal arrow string.
(define (entry-status e) (substring e 0 2))
(define (entry-path e) (substring e 3 (string-length e)))
(define (magit-char-at s i) (substring s i (+ i 1)))

;; The pathspecs a "OLD -> NEW" (or plain) section path covers — used for `add` /
;; `reset` / `checkout`, which must name every side of a rename.
(define (magit-pathspecs path) (split-many path " -> "))
;; The current on-disk path of a section: NEW for a rename, else the path itself.
;; Used wherever a single real file is needed (visit, blame, log, gitignore, diff).
(define (magit-newpath path)
  (let ((p (magit-pathspecs path))) (list-ref p (- (length p) 1))))

;; Map a porcelain status char (X for staged, Y for unstaged) to the label magit
;; shows in front of the file ("" when the status has no useful label).
(define (magit-label ch)
  (cond ((equal? ch "M") "modified")
        ((equal? ch "A") "new file")
        ((equal? ch "D") "deleted")
        ((equal? ch "R") "renamed")
        ((equal? ch "C") "copied")
        ((equal? ch "T") "typechange")
        ((equal? ch "U") "unmerged")
        (else "")))

;; The overlay face that colors a given label.
(define (magit-label-face label)
  (cond ((equal? label "modified") "magit-label-modified")
        ((equal? label "new file") "magit-label-new")
        ((equal? label "deleted")  "magit-label-deleted")
        ((equal? label "renamed")  "magit-label-renamed")
        (else "magit-label-other")))

;; Pad a label to a fixed column so the file names after it line up.
(define magit-label-width 11)
(define (magit-pad s width)
  (let ((len (utf8-length s)))
    (if (>= len width) (string-append s " ")
        (string-append s (string-repeat " " (- width len))))))

;; ONE `gix-status` call (no subprocess) gives the working tree AND the branch
;; line: porcelain-v1 entries plus a leading "## <branch>...<upstream> [ahead N,
;; behind M]" header, so branch name, upstream, and ahead/behind all come from
;; it. Untracked files are listed individually (like `--untracked-files=all`).
;; Renames read as delete+add (gix doesn't run rename detection) — magit still
;; stages/unstages each side correctly. Being subprocess-free, this is what makes
;; the live repo-watch refresh cheap.
(define (magit-porcelain-full)
  (let* ((lines  (gix-status *magit-dir*))
         (header (if (and (not (null? lines)) (starts-with? (car lines) "##")) (car lines) ""))
         (entries (filter (lambda (l) (not (starts-with? l "##"))) lines)))
    (cons (magit-parse-branch-line header) entries)))

;; Parse the "## …" header into (branch upstream ahead behind), all strings.
;; Handles: "## main", "## main...origin/main", "## main...origin/main [ahead 1,
;; behind 2]", "## No commits yet on main", "## HEAD (no branch)".
(define (magit-parse-branch-line header)
  (if (equal? header "")
      (list "" "" "0" "0")
      (let ((s (substring header 3 (string-length header))))  ; drop "## "
        (cond
          ((starts-with? s "No commits yet on ")
           (list (substring s 18 (string-length s)) "" "0" "0"))
          ((starts-with? s "HEAD (no branch)")
           (list "HEAD (detached)" "" "0" "0"))
          (else
           (let ((dots (split-many s "...")))               ; branch...upstream[ ab]
             (if (= (length dots) 1)
                 (list (car dots) "" "0" "0")               ; no upstream
                 (let ((br (split-many (string-join (cdr dots) "...") " [")))
                   (if (= (length br) 1)
                       (list (car dots) (car br) "0" "0")   ; upstream, in sync
                       (let ((ab (magit-parse-ab (car (split-many (cadr br) "]")))))
                         (list (car dots) (car br) (car ab) (cdr ab))))))))))))

;; "ahead 1, behind 2" / "ahead 1" / "behind 2" → (ahead . behind) strings.
(define (magit-trim-comma s)
  (if (ends-with? s ",") (substring s 0 (- (string-length s) 1)) s))
(define (magit-parse-ab s)
  (let loop ((toks (split-many s " ")) (ahead "0") (behind "0"))
    (cond ((null? toks) (cons ahead behind))
          ((and (equal? (car toks) "ahead") (not (null? (cdr toks))))
           (loop (cdr (cdr toks)) (magit-trim-comma (cadr toks)) behind))
          ((and (equal? (car toks) "behind") (not (null? (cdr toks))))
           (loop (cdr (cdr toks)) ahead (magit-trim-comma (cadr toks))))
          (else (loop (cdr toks) ahead behind)))))

;; Staged / unstaged / untracked entries are derived from ONE porcelain scan
;; (passed in as `entries`) rather than each re-running `git status`.
;; Staged / unstaged carry their status label: (label path).
(define (magit-staged entries)
  (map (lambda (e) (list (magit-label (magit-char-at e 0)) (entry-path e)))
       (filter (lambda (e)
                 (let ((x (magit-char-at e 0)))
                   (and (not (equal? x " ")) (not (equal? x "?")))))
               entries)))

(define (magit-unstaged entries)
  (map (lambda (e) (list (magit-label (magit-char-at e 1)) (entry-path e)))
       (filter (lambda (e)
                 (let ((y (magit-char-at e 1)))
                   (and (not (equal? y " ")) (not (equal? y "?")))))
               entries)))

(define (magit-untracked entries)
  (map entry-path
       (filter (lambda (e) (equal? (entry-status e) "??"))
               entries)))

;; --- branch / upstream introspection (for the header) -----------------------
;; Branch, upstream, and ahead/behind all come from the `--branch` status header
;; (see magit-parse-branch-line) — no dedicated rev-parse / rev-list spawns.

;; The "  (ahead N, behind M)" suffix for a Merge header — "" when in sync, or
;; one-sided when only ahead or only behind (matches magit's tracking display).
(define (magit-ab-string ab)
  (if (not ab)
      ""
      (let ((ahead (car ab)) (behind (cdr ab)))
        (cond ((and (equal? ahead "0") (equal? behind "0")) "")
              ((equal? behind "0") (string-append "  (ahead " ahead ")"))
              ((equal? ahead "0")  (string-append "  (behind " behind ")"))
              (else (string-append "  (ahead " ahead ", behind " behind ")"))))))

;; In-progress merge / rebase, read from the repo's git-dir state by gix.
(define (magit-merging?)  (equal? (gix-state *magit-dir*) "merge"))
(define (magit-rebasing?) (equal? (gix-state *magit-dir*) "rebase"))

;; --- confirmation for destructive operations --------------------------------
;; A one-shot thunk gated behind `y-or-n-p`; the answer arrives asynchronously
;; (minibuffer), so the action is captured as a closure and run only on "yes".
(define *magit-confirm-thunk* #f)
(define (magit-confirm prompt thunk)
  (set! *magit-confirm-thunk* thunk)
  (y-or-n-p prompt 'magit-confirm-yes))
(define (magit-confirm-yes ans)
  (let ((a *magit-confirm-thunk*))
    (set! *magit-confirm-thunk* #f)
    (when (and a (equal? ans "yes")) (a))))

;; --- async git (push / pull / fetch) ----------------------------------------
;; Network ops would freeze the UI synchronously, so stream them through a
;; subprocess. `/bin/sh -c "… 2>&1"` mirrors shell.scm's compile lane (git's
;; progress goes to stderr). Output is accumulated, not shown live — the output
;; primitives steal focus — and only surfaced in *magit-process* on failure.
(define *magit-async-out* "")
(define *magit-async-done* "")
(define (magit-run-async args done-msg)
  ;; lib-async's "magit" lane: one network op at a time (a second is refused),
  ;; C-g-cancellable. `exec` so git replaces the wrapper shell — the cancel
  ;; then signals git itself rather than an sh that would orphan it.
  (when (async-lane-run "magit" "/bin/sh"
                        (list "-c" (string-append "cd " (shell-quote *magit-dir*) " && exec git "
                                                  (string-join (map shell-quote args) " ")
                                                  " 2>&1"))
                        'magit-async-output 'magit-async-exit)
    (set! *magit-async-out* "")
    (set! *magit-async-done* done-msg)
    (message (string-append "git " (string-join args " ") "… (C-g cancels)"))))
(define (magit-async-output line)
  (set! *magit-async-out* (string-append *magit-async-out* line "\n")))
(define (magit-async-exit code)
  (let ((cancelled (async-lane-finish "magit")))
    (magit-refresh)
    (cond (cancelled (message "git cancelled"))
          ((equal? code "0") (message *magit-async-done*))
          (else
            (render-buffer "*magit-process*" "magit-log-mode"
                           (if (equal? (trim-end *magit-async-out*) "") "git failed"
                               (trim-end *magit-async-out*)) '())
            (message "git failed — see *magit-process*")))))
;; C-g in a magit buffer: kill a live push/pull/fetch (its exit callback then
;; reports the cancel), otherwise the normal keyboard-quit.
(define-command (magit-keyboard-quit)
  (async-lane-quit "magit"))

;; --- commit-message buffer helpers ------------------------------------------
(define magit-commit-comment-intro
  "# Please enter the commit message. Lines starting with '#' are ignored.\n#\n# Changes to be committed:\n")
;; Prefix each diff line with "# " so it shows as an ignored comment.
(define (magit-comment-diff raw)
  (string-join (map (lambda (l) (string-append "# " l))
                    (split-many (trim-end raw) "\n")) "\n"))
;; Drop comment lines (and trim) — the inverse, applied before committing.
(define (magit-strip-comments s)
  (trim-end (string-join
              (filter (lambda (l) (not (starts-with? l "#"))) (split-many s "\n"))
              "\n")))

;; --- transient switches (magit-style toggleable flags) ----------------------
;; The Rust transient is modal — any row key closes it. A "switch" row exploits
;; that: it toggles a flag (lib-transient's `transient-toggle-arg`), which then
;; re-opens the SAME popup (re-rendered with the new [x]/[ ] state), so the
;; popup appears to stay open while flags flip. Flags are namespaced
;; "popup:arg", so each popup clears just its own with `transient-clear-args`.
;; Action rows read the flags to assemble their git arguments.
;; A switch row's label: "[x] Text" when on, "[ ] Text" when off.
(define (magit-sw-label text flag)
  (string-append (if (transient-arg-active? flag) "[x] " "[ ] ") text))
;; The git args contributed by the enabled flags among `specs` ((flag . arg) …).
(define (magit-switch-args specs)
  (let loop ((s specs) (acc '()))
    (cond ((null? s) (reverse acc))
          ((transient-arg-active? (car (car s))) (loop (cdr s) (cons (cdr (car s)) acc)))
          (else (loop (cdr s) acc)))))

;; --- buffer construction ----------------------------------------------------
;; Emit accumulates lines + whole-line faces while tracking BOTH the line number
;; (for point→section mapping) and the byte offset (for sub-line overlay accents,
;; which the Rust overlay API addresses by byte). Sections record each file's
;; heading line and its diff body line range, so TAB can fold a section.

;; The line/face/byte/accent accumulation is lib-section's build state
;; (section-begin / section-line / section-accent / section-point-line /
;; section-point-byte); magit layers its own navigation tables on top:
(define *magit-sections* '())  ; (heading-line path type body-start body-end)
(define *magit-expanded* '())  ; (type . path) keys the user has expanded; every
                               ; other file section is collapsed by default
                               ; (persists). Keyed by type AND path so expanding a
                               ; file in Unstaged doesn't also expand its Staged
                               ; counterpart when it moves groups. Bodies render
                               ; LAZILY: a collapsed file emits only its heading,
                               ; so TAB re-renders instead of unfolding in place.
(define *magit-hunks* '())     ; (start-line end-line file-header hunk-text type path)
;; Structural view (see "structural grouping" below): runs of hunks grouped by
;; their enclosing function. (heading-line body-start body-end type path context).
(define *magit-groups* '())
;; Groups the user has collapsed (default is expanded so a freshly-expanded file
;; reads like the line view). Keyed (type path context), persists across refresh.
(define *magit-collapsed-groups* '())
(define *magit-commit-lines* '()) ; (line . ref) — commit/stash lines RET can show
;; (line . type) for each top-level section header ("Untracked files",
;; "Unstaged changes", "Staged changes"), so `s`/`u` on a header act on the
;; whole section (stage/unstage every file in it), like magit.
(define *magit-headings* '())
(define *magit-show-history* #t)  ; stashes/unpushed/unpulled — cheap via gix (no subprocess), so shown by default
;; When set, `magit-render` updates the buffer in place without stealing focus
;; (a background refresh, e.g. from `after-save-hook`). It also skips the sub-line
;; accent overlays, which target the *active* buffer — see magit-maybe-refresh.
(define *magit-quiet* #f)

(define (magit-emit text face) (section-line text face))

;; Section accessors over (heading-line path type body-start body-end).
(define (sec-heading s) (nth s 0))
(define (sec-path s) (nth s 1))
(define (sec-type s) (nth s 2))
(define (sec-body-start s) (nth s 3))
(define (sec-body-end s) (nth s 4))
;; Identity of a file section in *magit-expanded* — type AND path, so the same
;; file in Unstaged vs Staged folds independently.
(define (sec-key s) (cons (sec-type s) (sec-path s)))

;; Emit a file heading line "  <label>  <path>", coloring the label by its
;; status via a sub-line overlay accent (the path keeps the magit-file face).
;; An empty label (untracked files) just emits "  <path>".
(define (magit-emit-file-heading label path)
  (let ((byte (section-point-byte)))
    (if (equal? label "")
        (magit-emit (string-append "  " path) "magit-file")
        (begin
          (magit-emit (string-append "  " (magit-pad label magit-label-width) path)
                      "magit-file")
          ;; Accent just the label bytes (after the 2-space indent).
          (section-accent (+ byte 2) (+ byte 2 (utf8-length label))
                          (magit-label-face label))))))

;; --- diff rendering ---------------------------------------------------------

(define (magit-diff-face l)
  (cond ((starts-with? l "@@") "magit-hunk")
        ((starts-with? l "+") "magit-added")
        ((starts-with? l "-") "magit-removed")
        (else "magit-context")))

(define (magit-diff-marker? l)
  (or (starts-with? l "+") (starts-with? l "-")))  ; (+++/--- already filtered)

(define (magit-diff-marker-face l)
  (if (starts-with? l "+") "magit-add-marker" "magit-remove-marker"))

;; Emit one diff line, recording a 1-byte overlay accent on its +/- marker.
(define (magit-emit-diff-line l)
  (let ((byte (section-point-byte)))
    (magit-emit l (magit-diff-face l))
    (when (magit-diff-marker? l)
      (section-accent byte (+ byte 1) (magit-diff-marker-face l)))))

;; --- within-line word diff --------------------------------------------------
;; A `-`/`+` line pair usually differs in only part of the line. Highlight just
;; that part: trim the shared head and tail, paint the middle on a brighter face.
;; This is git's diff-highlight heuristic — it never changes the hunk TEXT (so
;; staging is unaffected), only adds overlay accents on the changed byte span.
;; Pairing happens only when a run of N removed lines is immediately followed by
;; N added lines (same as diff-highlight); otherwise lines stay whole-line lit.

;; Count of leading elements two CHAR LISTS share — one O(n) walk. (A per-char
;; `substring` loop is O(n²) here: Steel's substring is char-indexed, so every
;; call re-walks the string from the start, and long paired lines dominated the
;; render.) UTF-8 safe: string->list yields chars, not bytes.
(define (magit-common-prefix-chars a b)
  (let loop ((a a) (b b) (n 0))
    (if (and (pair? a) (pair? b) (equal? (car a) (car b)))
        (loop (cdr a) (cdr b) (+ n 1))
        n)))
;; Count of leading chars `a` and `b` share (char-indexed, UTF-8 safe).
(define (magit-common-prefix a b)
  (magit-common-prefix-chars (string->list a) (string->list b)))
;; Count of trailing chars shared, capped at `limit` so head and tail can't
;; overlap (e.g. a one-sided insertion).
(define (magit-common-suffix a b limit)
  (min limit (magit-common-prefix-chars (reverse (string->list a))
                                        (reverse (string->list b)))))
;; The changed middle of `a` relative to `b`, as a (start . end) CHAR range on
;; `a`. Empty (start = end) when `a`'s content is wholly shared (pure insertion
;; on the other side) — the caller then paints nothing.
(define (magit-change-span a b)
  (let* ((la (string->list a)) (lb (string->list b))
         (p (magit-common-prefix-chars la lb))
         (s (min (min (- (length la) p) (- (length lb) p))
                 (magit-common-prefix-chars (reverse la) (reverse lb)))))
    (cons p (- (length la) s))))

;; Emit diff line `l`, accenting its +/- marker AND its changed span vs the
;; paired line `other`. `removed?` selects the removed vs added word face. The
;; marker is one ASCII byte, so content starts at byte+1; the changed span's
;; byte offsets come from utf8-length of the content prefix (UTF-8 safe).
(define (magit-emit-intra-line l other removed?)
  (let ((byte (section-point-byte)))
    ;; Dim the whole modified line; the marker and changed span are re-brightened
    ;; by the overlays below. (Unpaired add/remove lines never reach here, so
    ;; they keep the medium body face from magit-emit-diff-line.)
    (magit-emit l (if removed? "magit-removed-dim" "magit-added-dim"))
    (section-accent byte (+ byte 1) (magit-diff-marker-face l))
    (let* ((c1 (substring l 1 (string-length l)))
           (c2 (substring other 1 (string-length other)))
           (span (magit-change-span c1 c2)))
      (when (> (cdr span) (car span))
        (section-accent (+ byte 1 (utf8-length (substring c1 0 (car span))))
                        (+ byte 1 (utf8-length (substring c1 0 (cdr span))))
                        (if removed? "magit-word-removed" "magit-word-added"))))))

;; The maximal leading run of `lines` whose members start with `prefix`, as
;; (run . rest).
(define (magit-split-run lines prefix)
  (let loop ((ls lines) (run '()))
    (if (and (not (null? ls)) (starts-with? (car ls) prefix))
        (loop (cdr ls) (cons (car ls) run))
        (cons (reverse run) ls))))

;; Emit a paired removed-run + added-run: the removed lines first (in file
;; order), each accented against its positional partner, then the added lines.
;; (Steel's `for-each` takes one list, so the runs are zipped with `map cons`.)
(define (magit-emit-paired-lines rem add)
  (for-each (lambda (p) (magit-emit-intra-line (car p) (cdr p) #t)) (map cons rem add))
  (for-each (lambda (p) (magit-emit-intra-line (car p) (cdr p) #f)) (map cons add rem)))

;; Emit a hunk body (the lines after the @@ header) with within-line word diff:
;; an N-removed / N-added block is paired and intra-highlighted; everything else
;; (context, the @@ line, unbalanced blocks) is emitted whole-line as before.
(define (magit-emit-hunk-body lines)
  (cond
    ((null? lines) #t)
    ((starts-with? (car lines) "-")
     (let* ((rsplit (magit-split-run lines "-"))
            (rem (car rsplit)) (after-rem (cdr rsplit))
            (asplit (magit-split-run after-rem "+"))
            (add (car asplit)) (after-add (cdr asplit)))
       (if (and (not (null? add)) (= (length rem) (length add)))
           (begin (magit-emit-paired-lines rem add)
                  (magit-emit-hunk-body after-add))
           (begin (for-each magit-emit-diff-line rem)
                  (magit-emit-hunk-body after-rem)))))
    (else
     (magit-emit-diff-line (car lines))
     (magit-emit-hunk-body (cdr lines)))))

;; Split a `git diff` into (header . hunks): the file header (lines before the
;; first @@) and each @@-hunk as its own string. The header + one hunk form a
;; valid patch for `git apply` (hunk-level staging).
(define (magit-parse-diff raw)
  (let loop ((lines (split-many raw "\n")) (header '()) (hunks '()) (cur '()) (in #f))
    (cond
      ((null? lines)
       (cons (join-lines (reverse header))
             (reverse (if in (cons (join-lines (reverse cur)) hunks) hunks))))
      ((starts-with? (car lines) "@@")
       (loop (cdr lines) header
             (if in (cons (join-lines (reverse cur)) hunks) hunks)
             (list (car lines)) #t))
      (in (loop (cdr lines) header hunks (cons (car lines) cur) #t))
      (else (loop (cdr lines) (cons (car lines) header) hunks cur #f)))))

;; --- bulk diff (one git call per section, not per file) ---------------------
;; A single `git diff` lists every changed file; splitting it per file lets one
;; invocation replace N. Each file's slice is byte-identical to what
;; `git diff -- <path>` prints, so magit-parse-diff and hunk-apply are unaffected.

;; Split bulk diff lines into per-file line-lists (each starts at "diff --git").
(define (split-diff-files lines)
  (let loop ((lines lines) (cur '()) (acc '()))
    (cond
      ((null? lines)
       (reverse (if (null? cur) acc (cons (reverse cur) acc))))
      ((and (starts-with? (car lines) "diff --git") (not (null? cur)))
       (loop (cdr lines) (list (car lines)) (cons (reverse cur) acc)))
      (else (loop (cdr lines) (cons (car lines) cur) acc)))))

;; The first line in `lines` with `prefix`, sans that prefix (or #f). File-header
;; lines precede any hunk body, so the header's path is always found first.
(define (chunk-prefixed lines prefix)
  (cond ((null? lines) #f)
        ((starts-with? (car lines) prefix)
         (substring (car lines) (string-length prefix) (string-length (car lines))))
        (else (chunk-prefixed (cdr lines) prefix))))

;; The work-tree path a chunk describes: from "+++ b/PATH", or "--- a/PATH" when
;; the new side is /dev/null (a deletion).
(define (chunk-path chunk)
  (or (chunk-prefixed chunk "+++ b/")
      (chunk-prefixed chunk "--- a/")))

;; path → diff-text hash per file in a bulk `git diff` (a hash, not an assoc
;; list — the per-file lookup would otherwise be O(files²)). trim-end first so
;; the last file's slice carries no trailing blank line (keeps its hunks
;; apply-clean).
(define (build-diff-map raw)
  (let loop ((chunks (if (equal? (trim-end raw) "")
                         '()
                         (split-diff-files (split-many (trim-end raw) "\n"))))
             (m (hash)))
    (if (null? chunks)
        m
        (loop (cdr chunks)
              (hash-insert m (chunk-path (car chunks)) (join-lines (car chunks)))))))

(define (diff-map-get dmap path)
  (or (hash-try-get dmap path) ""))

;; --- structural grouping ----------------------------------------------------
;; `git diff` already names the enclosing function on every hunk header
;; ("@@ -a,b +c,d @@ fn parse_expr"). The structural view groups a file's hunks
;; by that context under a collapsible sub-heading, so review steps function by
;; function (fold a reviewed fn, stage a whole fn) instead of hunk by hunk.
;; It's a pure reshaping of the SAME line hunks — no AST, no extra git. Toggle
;; with `v`. Live state is a plain global (like *magit-show-history*) so a toggle
;; takes effect on the very next render; set it in init.scm to change the default:
;;   (set! *magit-diff-view* "structural")
(define *magit-diff-view* "line")  ; "line" | "structural"
(define (magit-structural?) (equal? *magit-diff-view* "structural"))

;; The funcname context of a hunk: everything after the second "@@" on its
;; header line, trimmed. "" when git emitted no context (top-of-file changes, or
;; a language with no funcname pattern) — those collapse into one "(top level)".
(define (magit-ltrim s)
  (if (and (> (string-length s) 0) (equal? (substring s 0 1) " "))
      (magit-ltrim (substring s 1 (string-length s)))
      s))
(define (magit-hunk-context hunk-text)
  (let ((parts (split-many (car (split-many hunk-text "\n")) "@@")))
    (if (>= (length parts) 3)
        (magit-ltrim (trim-end (string-join (cdr (cdr parts)) "@@")))
        "")))
(define (magit-group-display ctx) (if (equal? ctx "") "(top level)" ctx))

;; Partition hunks into (context hunk…) groups, merging only CONSECUTIVE hunks
;; that share a context (a function occupies a contiguous span in a diff, so its
;; hunks are always adjacent). Preserves file order.
(define (magit-group-hunks hunks)
  (let loop ((hs hunks) (ctx #f) (cur '()) (acc '()))
    (cond
      ((null? hs)
       (reverse (if (null? cur) acc (cons (cons ctx (reverse cur)) acc))))
      (else
       (let ((c (magit-hunk-context (car hs))))
         (if (and ctx (equal? c ctx))
             (loop (cdr hs) ctx (cons (car hs) cur) acc)
             (loop (cdr hs) c (list (car hs))
                   (if (null? cur) acc (cons (cons ctx (reverse cur)) acc)))))))))

;; Emit one hunk's lines and record it in *magit-hunks* (shared by both views,
;; so per-hunk `s`/`u`/`k` and partial-region staging work identically). The @@
;; header is emitted plainly; the body goes through the word-diff emitter. Line
;; count is unchanged (one emit per line), so hunk/group spans stay correct.
(define (magit-emit-one-hunk header hunk type path)
  (let ((hstart (section-point-line))
        (lines (split-many hunk "\n")))
    (magit-emit-diff-line (car lines))
    (magit-emit-hunk-body (cdr lines))
    (set! *magit-hunks*
          (cons (list hstart (section-point-line) header hunk type path) *magit-hunks*))))

;; Line view: hunks back to back, exactly as before the structural view existed.
(define (magit-emit-hunks-flat header hunks type path)
  (for-each (lambda (hunk) (magit-emit-one-hunk header hunk type path)) hunks))

;; A group sub-heading "    <context>  (N hunks)", indented under the file name.
(define (magit-emit-group-heading ctx n)
  (magit-emit (string-append "    " (magit-group-display ctx)
                             "  (" (int->string n) (if (= n 1) " hunk)" " hunks)"))
              "magit-group"))

;; Structural view: a sub-heading per funcname group, its hunks foldable as a
;; unit. Records each group's line span in *magit-groups* for fold/stage/nav.
(define (magit-emit-hunks-grouped header hunks type path)
  (for-each
    (lambda (grp)
      (let ((ctx (car grp)) (ghunks (cdr grp)) (ghead (section-point-line)))
        (magit-emit-group-heading ctx (length ghunks))
        (let ((gbody (section-point-line)))
          (for-each (lambda (hunk) (magit-emit-one-hunk header hunk type path)) ghunks)
          (set! *magit-groups*
                (cons (list ghead gbody (section-point-line) type path ctx) *magit-groups*)))))
    (magit-group-hunks hunks)))

;; Group accessors over (heading body-start body-end type path context).
(define (grp-heading g) (nth g 0))
(define (grp-body-start g) (nth g 1))
(define (grp-body-end g) (nth g 2))
(define (grp-type g) (nth g 3))
(define (grp-path g) (nth g 4))
(define (grp-context g) (nth g 5))
(define (grp-key g) (list (grp-type g) (grp-path g) (grp-context g)))

;; Emit a file heading + its diff, one hunk at a time; record each hunk's line
;; range + the patch material so `s`/`u`/`k` can act on a single hunk. In the
;; structural view the hunks are first grouped by enclosing function.
;; `entry` is (label path); `diff-text` is this file's slice of the section's
;; bulk diff (see build-diff-map). Lazy body: sections are collapsed by
;; default, so the diff is parsed + emitted (word-diff, accents, hunk records)
;; only for files the user has expanded — a collapsed file costs one heading
;; line, which is what keeps a many-file refresh cheap. Point can't sit inside
;; a collapsed body, so the missing hunk records are unreachable anyway.
(define (magit-emit-diff entry type diff-text)
  (let* ((label (car entry))
         (path (cadr entry))
         (heading (section-point-line)))
    (magit-emit-file-heading label path)
    (let ((body-start (section-point-line)))
      (when (member (cons type path) *magit-expanded*)
        (let* ((parsed (magit-parse-diff diff-text))
               (header (car parsed))
               (hunks (cdr parsed)))
          (if (magit-structural?)
              (magit-emit-hunks-grouped header hunks type path)
              (magit-emit-hunks-flat header hunks type path))))
      (set! *magit-sections*
            (cons (list heading path type body-start (section-point-line)) *magit-sections*)))))

;; #t when any of `files` ((label path) …) of `type` has been expanded — the
;; render needs a section's bulk diff at all only when some file will show it.
(define (magit-any-expanded? files type)
  (let loop ((fs files))
    (cond ((null? fs) #f)
          ((member (cons type (cadr (car fs))) *magit-expanded*) #t)
          (else (loop (cdr fs))))))

;; `dmap` is the section's bulk-diff map; each file pulls its slice by path.
(define (magit-emit-diff-section title files type dmap)
  (when (not (null? files))
    (set! *magit-headings* (cons (cons (section-point-line) type) *magit-headings*))
    (magit-emit (string-append title " (" (int->string (length files)) ")")
                "magit-section-heading")
    (for-each (lambda (f) (magit-emit-diff f type (diff-map-get dmap (magit-newpath (cadr f))))) files)
    (magit-emit "" #f)))

;; Untracked files have no index entry, so `git diff` skips them. Render one's
;; content directly (read-file — no subprocess; the old `git diff --no-index`
;; spawn cost a fork/exec PER FILE PER REFRESH) as an added-lines diff body: a
;; synthesized @@ header plus each line prefixed "+". Empty and binary files
;; (read-file yields "" for both) get no body — nothing to expand.
(define (magit-untracked-diff path)
  (let ((raw (read-file (path-join *magit-dir* (magit-newpath path)))))
    (if (equal? raw "")
        '()
        (let ((body (map (lambda (l) (string-append "+" l))
                         (split-many (trim-end raw) "\n"))))
          (cons (string-append "@@ -0,0 +1," (int->string (length body)) " @@") body)))))

;; Render each untracked file as a foldable section: its content shown as added
;; lines, with a body range so TAB folds/expands it like a tracked file. We do
;; NOT register hunks — staging/discarding an untracked file acts on the whole
;; file (`git add` / `git clean`), so `s`/`k` keep using the file-level path.
(define (magit-emit-file-section title files type)
  (when (not (null? files))
    (set! *magit-headings* (cons (cons (section-point-line) type) *magit-headings*))
    (magit-emit (string-append title " (" (int->string (length files)) ")")
                "magit-section-heading")
    (for-each (lambda (f)
                (let ((heading (section-point-line)))
                  (magit-emit-file-heading "" f)
                  (let ((body-start (section-point-line)))
                    ;; Lazy body: content is read + emitted only once the user
                    ;; expands the file (TAB re-renders) — a collapsed section
                    ;; costs nothing, however many untracked files there are.
                    (when (member (cons type f) *magit-expanded*)
                      (for-each magit-emit-diff-line (magit-untracked-diff f)))
                    (set! *magit-sections*
                          (cons (list heading f type body-start (section-point-line))
                                *magit-sections*)))))
              files)
    (magit-emit "" #f)))

;; Emit one commit/stash line "  <ref> <subject>", accenting the leading ref
;; (short hash or stash ref) and recording (line . ref) so RET can show it.
;; A trailing ":" on a stash ref ("stash@{0}:") is stripped for `git show`.
(define (magit-emit-commit line)
  (let* ((parts (split-many line " "))
         (tok (if (null? parts) "" (car parts)))
         (ref (if (ends-with? tok ":") (substring tok 0 (- (string-length tok) 1)) tok))
         (ln (section-point-line))
         (byte (section-point-byte)))
    (magit-emit (string-append "  " line) #f)
    (when (not (equal? tok ""))
      (section-accent (+ byte 2) (+ byte 2 (utf8-length tok)) "magit-hash")
      (set! *magit-commit-lines* (cons (cons ln ref) *magit-commit-lines*)))))

;; A titled section of commit/stash lines (no diff bodies, nothing to fold).
(define (magit-emit-commit-section title lines)
  (when (not (null? lines))
    (magit-emit (string-append title " (" (int->string (length lines)) ")")
                "magit-section-heading")
    (for-each magit-emit-commit lines)
    (magit-emit "" #f)))

;; The commit/stash ref on `line`, or #f.
(define (magit-commit-at-line line)
  (let loop ((cs *magit-commit-lines*))
    (cond ((null? cs) #f)
          ((= (car (car cs)) line) (cdr (car cs)))
          (else (loop (cdr cs))))))

;; Emit a header line "<label>   <branch> <rest>", accenting the branch name and
;; dimming the rest, so the label column / branch / detail read as three tiers.
;; `rest` is "" or starts with its own leading space.
(define magit-header-width 10)
(define (magit-emit-header label branch rest)
  (let* ((labelcol (magit-pad label magit-header-width))
         (byte (section-point-byte))
         (bstart (+ byte (utf8-length labelcol)))
         (bend (+ bstart (utf8-length branch))))
    (magit-emit (string-append labelcol branch rest) "magit-header")
    (section-accent bstart bend "magit-branch")
    (when (> (utf8-length rest) 0)
      (section-accent bend (+ bend (utf8-length rest)) "magit-context"))))

;; The section matching both `path` and `type`, or #f. Type-qualified because a
;; partially-staged file appears in BOTH Unstaged and Staged — point restore must
;; find the one in the group the cursor was actually in.
(define (magit-section-by-path-type path type)
  (let loop ((secs *magit-sections*))
    (cond ((null? secs) #f)
          ((and (equal? (sec-path (car secs)) path) (equal? (sec-type (car secs)) type))
           (car secs))
          (else (loop (cdr secs))))))

;; Apply overlay accents + folds AFTER render (they target the now-active buffer).
;; clear-overlays first so a refresh doesn't stack stale accents/folds.
;; File sections need no folds: bodies render lazily, so a collapsed file has
;; no body lines to hide. Only collapsed function groups (structural view,
;; inside an expanded file) still fold real lines away.
(define (magit-apply-overlays)
  (clear-overlays)
  (section-apply-accents)
  (for-each (lambda (g)
              (when (and (member (grp-key g) *magit-collapsed-groups*)
                         (> (grp-body-end g) (grp-body-start g)))
                (fold-lines (grp-body-start g) (grp-body-end g))))
            *magit-groups*))

;; --- point restore across refresh -------------------------------------------
;; A refresh rebuilds the buffer. Two cases:
;;   * The file at point is still in the SAME section group (hunk-level stage,
;;     fold toggle, partial unstage) → follow it, keeping the in-section offset.
;;     This keeps you on the hunk you were editing.
;;   * The file LEFT its group (e.g. you fully staged it, so it jumped from
;;     Unstaged to Staged) → don't chase it. Stay at the same screen line, where
;;     the next sibling has now shifted up — so you can keep staging down the list,
;;     which is what magit does.
(define (magit-capture-point)
  (if (equal? (current-buffer) "*magit*")
      (let ((s (magit-section-at-line (buffer-line))))
        (if s
            (list "section" (sec-path s) (sec-type s)
                  (- (buffer-line) (sec-heading s)) (buffer-line))
            (list "line" (buffer-line))))
      #f))

(define (magit-restore-point cap)
  (when cap
    (cond
      ((equal? (car cap) "section")
       (let* ((path (nth cap 1)) (type (nth cap 2)) (delta (nth cap 3)) (absline (nth cap 4))
              (s (magit-section-by-path-type path type)))
         (if s
             ;; Same file, same group → follow. Keep the offset only when the body
             ;; is visible (a collapsed section folds it), else land on the head.
             (let* ((d (if (member (cons type path) *magit-expanded*) delta 0))
                    (target (+ (sec-heading s) d)))
               (goto-line (min target (max (sec-heading s) (- (sec-body-end s) 1)))))
             ;; Moved out of its group → hold the line; the next item sits there now.
             (goto-line absline))))
      ((equal? (car cap) "line") (goto-line (nth cap 1))))))

;; Render the status buffer for the current *magit-dir* (no re-resolution).
(define (magit-render)
  (let ((cap (magit-capture-point))
        ;; The cursor's on-screen row before the redraw, so we can pin it back
        ;; afterward and the buffer doesn't visually jump (only meaningful when
        ;; *magit* is the focused buffer — the interactive refresh path).
        (row (window-cursor-row)))
    (section-begin)
    (set! *magit-sections* '())
    (set! *magit-hunks* '())
    (set! *magit-groups* '())
    (set! *magit-commit-lines* '())
    (set! *magit-headings* '())
    ;; ONE status scan (`--branch`) feeds the working-tree sections AND the
    ;; branch/upstream/ahead-behind header — no separate rev-parse/rev-list calls.
    (let* ((porc      (magit-porcelain-full))
           (binfo     (car porc))
           (entries   (cdr porc))
           (untracked (magit-untracked entries))
           (unstaged  (magit-unstaged entries))
           (staged    (magit-staged entries))
           (branch    (nth binfo 0))
           (up        (nth binfo 1))
           (ab        (if (equal? up "") #f (cons (nth binfo 2) (nth binfo 3))))
           (subject   (gix-head-subject *magit-dir*)))
      ;; Keep the mode-line branch slot in step with magit — a checkout or
      ;; branch op refreshes here without needing a fresh file visit.
      (set-mode-line-slot "git" branch)
      ;; --- header ---
      (magit-emit-header "Head:" (if (equal? branch "") "(no commits)" branch)
                         (if (equal? subject "") "" (string-append " " subject)))
      (when (not (equal? up ""))
        (magit-emit-header "Merge:" up (magit-ab-string ab)))
      (when (magit-merging?)
        (magit-emit "Merging — resolve conflicts, stage, then commit."
                    "magit-label-deleted"))
      (when (magit-rebasing?)
        (magit-emit "Rebase in progress." "magit-label-deleted"))
      (magit-emit "" #f)
      ;; --- working tree ---
      (magit-emit-file-section "Untracked files" untracked "untracked")
      ;; The bulk diff is computed only when some file in the section is
      ;; expanded — the common all-collapsed refresh does no diffing at all.
      (magit-emit-diff-section "Unstaged changes" unstaged "unstaged"
                               (if (magit-any-expanded? unstaged "unstaged")
                                   (build-diff-map (gix-diff *magit-dir*)) (hash)))
      (magit-emit-diff-section "Staged changes" staged "staged"
                               (if (magit-any-expanded? staged "staged")
                                   (build-diff-map (gix-diff-cached *magit-dir*)) (hash)))
      (when (and (null? untracked) (null? unstaged) (null? staged))
        (magit-emit "Nothing to commit, working tree clean." "magit-file")
        (magit-emit "" #f))
      ;; --- stashes + ahead/behind (lazy: 3 git calls only when shown, toggle
      ;;     with H — recent commits below always render) ---
      (if *magit-show-history*
          (begin
            (magit-emit-commit-section "Stashes" (gix-stash-list *magit-dir*))
            (when (not (equal? up ""))
              (magit-emit-commit-section (string-append "Unpushed to " up)
                                         (gix-log-range *magit-dir* "HEAD" up *magit-unpushed-limit*))
              (magit-emit-commit-section (string-append "Unpulled from " up)
                                         (gix-log-range *magit-dir* up "HEAD" *magit-unpushed-limit*))))
          (magit-emit "Press H to show stashes / unpushed / unpulled."
                      "magit-context"))
      ;; --- recent commits (shown by default, one gix rev-walk) ---
      (magit-emit-commit-section "Recent commits"
                                 (gix-log-oneline *magit-dir* 10)))
    (if *magit-quiet*
        ;; Background refresh: update in place (works on an inactive pane) and
        ;; skip overlays — they'd paint onto whatever buffer is in front.
        (render-buffer-quiet "*magit*" "magit-status"
                             (section-text) (section-line-faces))
        (begin
          (render-buffer "*magit*" "magit-status"
                         (section-text) (section-line-faces))
          (magit-apply-overlays)
          (magit-restore-point cap)
          ;; Pin the cursor's line back to the same screen row it was on, so
          ;; restoring point doesn't scroll the buffer under the user.
          (scroll-cursor-to-row row)))
    ;; Watch the whole repo so changes from anywhere — CLI git, other editors,
    ;; saves in other windows — refresh the status live (see magit-on-fs-change).
    ;; Replace-per-owner, so switching repos re-points the watch.
    (when (and (not (equal? *magit-dir* "")) (not (equal? *magit-dir* ".")))
      (watch-tree "magit" (list *magit-dir*)))))

;; Optional scoping hook: a thunk returning the git work-tree magit should scope
;; to, or #f. perspectives.scm installs one so magit FOLLOWS THE TITLE-BAR
;; project — the title bar is the user's project selector, so magit tracks it
;; rather than whatever buffer happens to be current. Declared here (perspectives.scm
;; requires magit.scm) so the installer needn't be known to magit at
;; definition time; stays #f if perspectives is unloaded. Installers MUST use
;; the setter below: a raw top-level `set!` from a later-loaded file mutates a
;; binding magit-current-repo never reads (Steel resolves free identifiers at
;; definition time); a set! inside this magit-defined function mutates the one
;; it does.
(define *magit-scope-hook* #f)
(define (set-magit-scope-hook! f) (set! *magit-scope-hook* f))

;; The git work-tree magit should operate on, or #f when none applies. Prefers
;; the title-bar project (via *magit-scope-hook*); with no active perspective it
;; falls back to the CURRENT buffer's work-tree. A buffer that visits no file
;; (e.g. *scratch*, *Messages*) resolves to #f under the fallback — its
;; `buffer-directory` falls back to the process cwd, so without the file guard
;; launching magit from *scratch* would silently open whatever repo the app
;; happened to be started in.
(define (magit-current-repo)
  (or (and *magit-scope-hook* (*magit-scope-hook*))
      (and (not (equal? (buffer-file-name) ""))
           (let ((top (gix-toplevel (buffer-directory))))
             (and (not (equal? top "")) top)))))

;; Launch: scope to the title-bar project (or the current buffer's repo when no
;; perspective is active), then render. With neither available, a refresh from
;; inside a *magit…* buffer keeps the repo we're already viewing; any other
;; buffer isn't part of a repo, so refuse (like projectile).
(define-command (magit-status)
  (let ((repo (magit-current-repo)))
    (cond
      (repo (set! *magit-dir* repo) (magit-render))
      ((starts-with? (current-buffer) "*magit") (magit-render))
      (else (message "Not inside a git repository")))))

;; Open magit on an explicitly chosen directory (the override path — used by the
;; Helm picker below, or callable directly for any repo).
(define (magit-status-in dir)
  (set! *magit-dir* (magit-toplevel dir))
  (magit-render))

;; Point *magit-dir* at the title-bar project (or the current buffer's repo when
;; no perspective is active); returns #t on success, #f when nothing resolves (so
;; callers can refuse). M-x entry points reachable from a file buffer (log,
;; dispatch) need this, or they'd inherit a stale/default *magit-dir* and operate
;; on the wrong repo. Inside a *magit…* buffer with no resolvable repo the
;; directory is already resolved, so succeed.
(define (magit-ensure-dir)
  (let ((repo (magit-current-repo)))
    (cond
      (repo (set! *magit-dir* repo) #t)
      ((starts-with? (current-buffer) "*magit") #t)
      (else #f))))

;; Refresh re-runs git for the SAME repo (never re-resolves).
(define (magit-refresh) (magit-render))

;; H: toggle the stashes / unpushed / unpulled block. Hidden by default so the
;; common refresh skips its 3 extra `git log`/`stash` calls; recent commits
;; always render regardless.
(define (magit-toggle-history)
  (set! *magit-show-history* (not *magit-show-history*))
  (magit-refresh))

;; --- section at point -------------------------------------------------------

;; The section (file) whose heading or diff body contains `line`, or #f.
(define (magit-section-at-line line)
  (let loop ((secs *magit-sections*))
    (cond ((null? secs) #f)
          ((let ((s (car secs)))
             (or (= (sec-heading s) line)
                 (and (>= line (sec-body-start s)) (< line (sec-body-end s)))))
           (car secs))
          (else (loop (cdr secs))))))

;; The (type path) of the file at point, or #f.
(define (magit-current)
  (let ((s (magit-section-at-line (buffer-line))))
    (if s (list (sec-type s) (sec-path s)) #f)))

;; The section type whose top-level header is exactly `line` ("untracked",
;; "unstaged", "staged"), or #f. Lets `s`/`u` act on a whole section from its
;; header line.
(define (magit-heading-type-at-line line)
  (let loop ((hs *magit-headings*))
    (cond ((null? hs) #f)
          ((= (car (car hs)) line) (cdr (car hs)))
          (else (loop (cdr hs))))))

;; Every file path in the sections of `type`, deduped (a partially-staged file
;; can list twice within a type otherwise).
(define (magit-files-of-type type)
  (dedup
    (map sec-path (filter (lambda (s) (equal? (sec-type s) type)) *magit-sections*))))

;; The hunk whose line range contains `line`, or #f.
;; hunk = (start end header hunk-text type path).
(define (magit-hunk-at-line line)
  (let loop ((hs *magit-hunks*))
    (cond ((null? hs) #f)
          ((let ((h (car hs))) (and (>= line (nth h 0)) (< line (nth h 1)))) (car hs))
          (else (loop (cdr hs))))))

;; The group (heading or body lines) containing `line`, or #f. Structural view
;; only — *magit-groups* is empty in the line view, so this is a no-op there.
(define (magit-group-at-line line)
  (let loop ((gs *magit-groups*))
    (cond ((null? gs) #f)
          ((let ((g (car gs)))
             (or (= (grp-heading g) line)
                 (and (>= line (grp-body-start g)) (< line (grp-body-end g)))))
           (car gs))
          (else (loop (cdr gs))))))
;; The group whose heading is exactly `line`, or #f — used by `s`/`u`/`k` to act
;; on a whole function when point sits on its sub-heading.
(define (magit-group-on-heading line)
  (let loop ((gs *magit-groups*))
    (cond ((null? gs) #f)
          ((= (grp-heading (car gs)) line) (car gs))
          (else (loop (cdr gs))))))
;; The hunks belonging to group `g`, in file order, as a single applyable patch
;; (file header + each hunk body). #f when the group is somehow empty.
(define (magit-group-hunks-of g)
  (reverse
    (filter (lambda (h) (and (>= (nth h 0) (grp-body-start g))
                             (< (nth h 0) (grp-body-end g))))
            *magit-hunks*)))
(define (magit-group-patch g)
  (let ((hs (magit-group-hunks-of g)))
    (if (null? hs)
        #f
        (string-append (nth (car hs) 2) "\n"
                       (string-join (map (lambda (h) (nth h 3)) hs) "\n") "\n"))))

;; --- target resolution for the point-based verbs -----------------------------
;; What `s` / `u` / `k` should act on when there is no region: the FIRST match
;; of hunk → function group (structural view, on its sub-heading) → top-level
;; section header → file, as a tagged list — ('hunk h), ('group g),
;; ('section type), ('file type path) — or #f. One resolver, one precedence:
;; a group sub-heading always wins over the header/file fallbacks (it is never
;; the same line as a top-level header, but a single order keeps the three
;; verbs from drifting apart again — discard used to check headers first).
(define (magit-target-at-point)
  (let* ((line (buffer-line))
         (h (magit-hunk-at-line line)))
    (if h
        (list 'hunk h)
        (let ((g (and (magit-structural?) (magit-group-on-heading line))))
          (if g
              (list 'group g)
              (let ((ht (magit-heading-type-at-line line)))
                (if ht
                    (list 'section ht)
                    (let ((cur (magit-current)))
                      (if cur (list 'file (car cur) (cadr cur)) #f)))))))))
(define (target-tag t) (car t))
(define (target-arg t) (nth t 1))

;; --- error surfacing --------------------------------------------------------
;; Run git, refresh, echo `msg` on success — or pop the stderr into a buffer.
(define (magit-do args msg)
  (let ((r (call-process-full "git" (append (list "-C" *magit-dir*) args) "")))
    (magit-refresh)
    (if (equal? (car r) "0") (message msg) (magit-error (nth r 2)))))
(define (magit-error stderr)
  (render-buffer "*magit-process*" "magit-log-mode"
                 (if (equal? (trim-end stderr) "") "git failed (no output)" (trim-end stderr)) '())
  (message "git failed — see *magit-process*"))

;; Lists from git, one item per non-empty line.
(define (magit-branches) (gix-branches *magit-dir*))
(define (magit-remotes)  (gix-remotes  *magit-dir*))
(define (magit-tags)     (gix-tags     *magit-dir*))

;; --- staging (hunk- and region-level) ---------------------------------------
;; Apply a patch to the index (or, for discard, the worktree). `--recount` lets
;; git fix the @@ counts so a hand-built (isolated or partial) hunk applies.
(define (magit-apply-patch-text patch to-index reverse?)
  (let* ((args (append (list "-C" *magit-dir* "apply" "--recount")
                       ;; 'both: index AND worktree (discarding a staged change)
                       (cond ((eq? to-index 'both) (list "--index"))
                             (to-index (list "--cached"))
                             (else '()))
                       (if reverse? (list "--reverse") '())
                       (list "-")))
         (r (call-process-full "git" args patch)))
    (magit-refresh)
    (if (equal? (car r) "0") #t (begin (magit-error (nth r 2)) #f))))
(define (magit-apply-hunk h to-index reverse?)
  (magit-apply-patch-text (string-append (nth h 2) "\n" (nth h 3) "\n") to-index reverse?))

;; --- partial (selected-region) hunk application -----------------------------
;; 0-based line index of byte `b` (newlines before it). O(b) but the status
;; buffer is small; avoids a dedicated primitive.
(define (magit-byte->line b)
  (- (length (split-many (buffer-substring 0 b) "\n")) 1))

;; Rebuild a hunk to contain ONLY the changed lines whose buffer line is within
;; [lo, hi], transforming the rest so the patch still applies. The transform
;; differs by direction; both keep context, keep selected +/- lines, and recast
;; the *unselected* changed lines into the form that makes the patch's matched
;; side equal the current index/worktree (git apply checks content; --recount
;; relaxes only the counts). Returns #f when no changed line is selected.
;; Forward (stage): unselected '+' dropped, unselected '-' becomes context.
(define (magit-xf-stage l sel?)
  (cond ((starts-with? l "+") (if sel? l #f))
        ((starts-with? l "-") (if sel? l (string-append " " (substring l 1 (string-length l)))))
        (else l)))
;; Reverse (unstage / discard): unselected '+' becomes context, '-' dropped.
(define (magit-xf-reverse l sel?)
  (cond ((starts-with? l "+") (if sel? l (string-append " " (substring l 1 (string-length l)))))
        ((starts-with? l "-") (if sel? l #f))
        (else l)))
;; Transform one hunk to its selected-region form, returning
;; (cons changed? "@@…\n<body>") — `changed?` is #t iff a +/- line was selected.
;; Several of these concatenate under one file header into a multi-hunk patch.
(define (magit-partial-hunk-text h lo hi xf)
  (let* ((hstart (nth h 0))
         (hlines (split-many (nth h 3) "\n"))
         (atat (car hlines))
         (body (cdr hlines)))
    (let loop ((bs body) (i 0) (out '()) (changed #f))
      (if (null? bs)
          (cons changed (string-append atat "\n" (string-join (reverse out) "\n")))
          (let* ((l (car bs))
                 (bl (+ hstart 1 i))
                 (sel? (and (>= bl lo) (<= bl hi)))
                 (xl (xf l sel?))
                 (change? (or (starts-with? l "+") (starts-with? l "-"))))
            (loop (cdr bs) (+ i 1)
                  (if xl (cons xl out) out)
                  (or changed (and sel? change?))))))))
;; A patch over `hunks` (all one file) keeping only those with a selected change.
;; #f when nothing in the region changed.
(define (magit-partial-multi hunks lo hi xf)
  (if (null? hunks)
      #f
      (let* ((header (nth (car hunks) 2))
             (kept (filter car (map (lambda (h) (magit-partial-hunk-text h lo hi xf)) hunks))))
        (if (null? kept)
            #f
            (string-append header "\n" (string-join (map cdr kept) "\n") "\n")))))

;; The selected buffer-line span [lo hi]; hi from re-1 so a selection ending at a
;; line boundary doesn't pull in the next line.
(define (magit-region-lines)
  (let ((rb (region-beginning)) (re (region-end)))
    (cons (magit-byte->line rb) (magit-byte->line (max rb (- re 1))))))
;; File-section paths of `type` whose heading line falls in [lo hi].
(define (magit-files-in lo hi type)
  (map sec-path (filter (lambda (s) (and (equal? (sec-type s) type)
                                         (>= (sec-heading s) lo) (<= (sec-heading s) hi)))
                        *magit-sections*)))
;; Hunks of `type` whose line range overlaps [lo hi].
(define (magit-hunks-in lo hi type)
  (filter (lambda (h) (and (equal? (nth h 4) type)
                           (<= (nth h 0) hi) (> (nth h 1) lo)))
          *magit-hunks*))

;; A region selects a SET of things — whole files (when their headings are in the
;; region) and/or individual diff lines. Magit applies the op to all of them;
;; these handlers mirror that. A contiguous region can't span two files' diffs
;; without crossing the second file's heading, so the hunk path is single-file.

;; Stage every file in a working-tree section — `s` on the "Untracked files" or
;; "Unstaged changes" header. Both stage via `git add`.
(define (magit-stage-section type)
  (let ((files (magit-files-of-type type)))
    (if (null? files)
        (message "Nothing to stage in section")
        (magit-do (append (list "add" "--") (map magit-newpath files))
                  (string-append "Staged " (int->string (length files)) " file(s)")))))

;; Unstage every file in the "Staged changes" section — `u` on its header.
(define (magit-unstage-section)
  (let ((files (magit-files-of-type "staged")))
    (if (null? files)
        (message "Nothing to unstage in section")
        (magit-do (append (list "reset" "-q" "HEAD" "--")
                          (apply append (map magit-pathspecs files)))
                  (string-append "Unstaged " (int->string (length files)) " file(s)")))))

;; s: stage the region (multiple files and/or selected lines), else the section
;; on its header, else hunk/file at point.
(define (magit-stage)
  (if (region-active?)
      (let* ((r (magit-region-lines)) (lo (car r)) (hi (cdr r))
             (files (dedup (append (magit-files-in lo hi "untracked")
                                         (magit-files-in lo hi "unstaged")))))
        (if (not (null? files))
            (magit-do (append (list "add" "--") (map magit-newpath files))
                      (string-append "Staged " (int->string (length files)) " file(s)"))
            (let ((patch (magit-partial-multi (magit-hunks-in lo hi "unstaged") lo hi magit-xf-stage)))
              (if patch
                  (when (magit-apply-patch-text patch #t #f) (message "Staged region"))
                  (message "Nothing to stage in region")))))
      (let ((t (magit-target-at-point)))
        (cond
          ((not t) (message "Nothing to stage here"))
          ((eq? (target-tag t) 'hunk)
           (when (magit-apply-hunk (target-arg t) #t #f) (message "Staged hunk")))
          ((eq? (target-tag t) 'group)
           (let* ((g (target-arg t)) (patch (magit-group-patch g)))
             (when (and patch (magit-apply-patch-text patch #t #f))
               (message (string-append "Staged " (magit-group-display (grp-context g)))))))
          ((eq? (target-tag t) 'section)
           (if (member (target-arg t) (list "untracked" "unstaged"))
               (magit-stage-section (target-arg t))
               (message "Nothing to stage here")))
          ((equal? (nth t 1) "staged") (message "Already staged"))
          (else
           (magit-do (list "add" "--" (magit-newpath (nth t 2))) "Staged file"))))))

;; u: unstage the region (multiple files and/or selected lines), else hunk/file at point.
(define (magit-unstage)
  (if (region-active?)
      (let* ((r (magit-region-lines)) (lo (car r)) (hi (cdr r))
             (files (dedup (magit-files-in lo hi "staged"))))
        (if (not (null? files))
            (magit-do (append (list "reset" "-q" "HEAD" "--")
                              (apply append (map magit-pathspecs files)))
                      (string-append "Unstaged " (int->string (length files)) " file(s)"))
            (let ((patch (magit-partial-multi (magit-hunks-in lo hi "staged") lo hi magit-xf-reverse)))
              (if patch
                  (when (magit-apply-patch-text patch #t #t) (message "Unstaged region"))
                  (message "Nothing to unstage in region")))))
      (let ((t (magit-target-at-point)))
        (cond
          ((not t) (message "Nothing to unstage here"))
          ((eq? (target-tag t) 'hunk)
           (when (magit-apply-hunk (target-arg t) #t #t) (message "Unstaged hunk")))
          ((eq? (target-tag t) 'group)
           (let* ((g (target-arg t)) (patch (magit-group-patch g)))
             (when (and patch (magit-apply-patch-text patch #t #t))
               (message (string-append "Unstaged " (magit-group-display (grp-context g)))))))
          ((eq? (target-tag t) 'section)
           (if (equal? (target-arg t) "staged")
               (magit-unstage-section)
               (message "Nothing to unstage here")))
          ((not (equal? (nth t 1) "staged")) (message "Already unstaged"))
          (else
           (magit-do (append (list "reset" "-q" "HEAD" "--") (magit-pathspecs (nth t 2)))
                     "Unstaged file"))))))

(define (magit-stage-all)   (magit-do (list "add" "-A") "Staged all changes"))
(define (magit-unstage-all)
  (magit-confirm "Unstage all changes?"
    (lambda () (magit-do (list "reset" "-q" "HEAD") "Unstaged everything"))))

;; Discard every file in a whole section — `k`/`x` on its header. "untracked"
;; means `clean` the lot, "unstaged" means `checkout HEAD` the lot.
(define (magit-discard-section type)
  (let ((files (magit-files-of-type type)))
    (if (null? files)
        (message "Nothing to discard in section")
        (magit-confirm (string-append "Discard " (int->string (length files)) " file(s)?")
          (lambda ()
            (if (equal? type "untracked")
                (magit-discard-files files '())
                (magit-discard-files '() files)))))))

;; Discard one side wholesale: untracked files via `clean`, tracked via `checkout`.
;; Unstaged changes are restored from the INDEX (`checkout -- path`), so a file's
;; staged changes survive; `checkout HEAD --` would throw those away too.
(define (magit-discard-files untracked unstaged)
  (when (not (null? untracked))
    (magit-do (append (list "clean" "-f" "--") (map magit-newpath untracked))
              "Removed untracked file(s)"))
  (when (not (null? unstaged))
    (magit-do (append (list "checkout" "--") (apply append (map magit-pathspecs unstaged)))
              "Discarded file(s)")))

;; k: discard the region (multiple files and/or selected lines), else hunk/file
;; at point. Discarding is irreversible (working-tree loss), so it always confirms.
(define (magit-discard)
  (if (region-active?)
      (let* ((r (magit-region-lines)) (lo (car r)) (hi (cdr r))
             (untracked (dedup (magit-files-in lo hi "untracked")))
             (unstaged  (dedup (magit-files-in lo hi "unstaged"))))
        (if (or (not (null? untracked)) (not (null? unstaged)))
            (magit-confirm (string-append "Discard "
                             (int->string (+ (length untracked) (length unstaged))) " file(s)?")
              (lambda () (magit-discard-files untracked unstaged)))
            (let ((patch (magit-partial-multi (magit-hunks-in lo hi "unstaged") lo hi magit-xf-reverse)))
              (if (not patch)
                  (message "Nothing to discard in region")
                  (magit-confirm "Discard region?"
                    (lambda () (when (magit-apply-patch-text patch #f #t) (message "Discarded region"))))))))
      (let ((t (magit-target-at-point)))
        (cond
          ((not t) (message "Nothing to discard here"))
          ((eq? (target-tag t) 'hunk)
           (let ((h (target-arg t)))
             (magit-confirm "Discard hunk?"
               (lambda () (when (magit-apply-hunk h (if (equal? (nth h 4) "staged") 'both #f) #t)
                            (message "Discarded hunk"))))))
          ((eq? (target-tag t) 'group)
           (let* ((g (target-arg t)) (patch (magit-group-patch g)))
             (when patch
               (magit-confirm (string-append "Discard "
                                             (magit-group-display (grp-context g)) "?")
                 (lambda () (when (magit-apply-patch-text
                                    patch (if (equal? (grp-type g) "staged") 'both #f) #t)
                              (message "Discarded group")))))))
          ((eq? (target-tag t) 'section)
           (if (member (target-arg t) (list "untracked" "unstaged"))
               (magit-discard-section (target-arg t))
               (message "Nothing to discard here")))
          (else
           (let ((type (nth t 1)) (path (nth t 2)))
             (magit-confirm (string-append "Discard changes to " path "?")
               (lambda ()
                 (cond
                   ((equal? type "untracked")
                    (magit-do (list "clean" "-f" "--" (magit-newpath path))
                              (string-append "Removed " path)))
                   ;; Unstaged: back to the index, keeping what's staged.
                   ((equal? type "unstaged")
                    (magit-do (append (list "checkout" "--") (magit-pathspecs path))
                              (string-append "Discarded " path)))
                   (else
                    (magit-do (append (list "checkout" "HEAD" "--") (magit-pathspecs path))
                              (string-append "Discarded " path))))))))))))

;; --- commit (c): an editable *git-commit* buffer ----------------------------
;; Switch rows toggle commit flags; action rows read them. "Amend HEAD" reuses
;; the last message; --signoff / --no-verify reach the final `git commit`.
(define (magit-commit-toggle-amend)    (transient-toggle-arg "commit:amend" magit-commit-popup))
(define (magit-commit-toggle-signoff)  (transient-toggle-arg "commit:signoff" magit-commit-popup))
(define (magit-commit-toggle-noverify) (transient-toggle-arg "commit:noverify" magit-commit-popup))
(define (magit-commit-popup)
  (transient "Commit"
    (list
      (list "Switches"
            (list "a" (magit-sw-label "Amend HEAD"        "commit:amend")    'magit-commit-toggle-amend)
            (list "s" (magit-sw-label "Sign off (-s)"     "commit:signoff")  'magit-commit-toggle-signoff)
            (list "n" (magit-sw-label "No verify (-n)"    "commit:noverify") 'magit-commit-toggle-noverify))
      (list "Actions"
            (list "c" "Commit"             'magit-do-commit)
            (list "e" "Amend (no edit)"    'magit-do-amend)
            (list "w" "Reword last commit" 'magit-do-reword)))))
;; The extra `git commit` flags currently toggled on.
(define (magit-commit-flags)
  (magit-switch-args (list (cons "commit:signoff" "--signoff")
                           (cons "commit:noverify" "--no-verify"))))

;; Open the message buffer (empty for commit, last message for reword). `amend`
;; ("no" / "yes" / "reword") is stashed in a buffer-local so C-c C-c knows which
;; git command to run. A commit already being written is resumed, not appended
;; to (which duplicated the message and diff).
(define (magit-commit-buffer initial amend)
  (if (buffer-live? "*git-commit*")
      (begin (switch-to-buffer "*git-commit*")
             (message "Resuming the commit message in progress"))
      (magit-commit-buffer-new initial amend)))
(define (magit-commit-buffer-new initial amend)
  (append-to-buffer "*git-commit*" initial)
  ;; Show the staged diff (commented out) below the message, like `git commit`.
  (append-to-buffer "*git-commit*"
    (string-append "\n\n" magit-commit-comment-intro
                   (magit-comment-diff (gix-diff-cached *magit-dir*))))
  (set-major-mode "git-commit-mode")
  (buffer-local-set "magit-amend" amend)
  (goto-char 0) ; cursor at the top, ready to type the summary
  (message "Edit message — C-x k or C-c C-c to commit, C-c C-k to cancel"))
;; Save unsaved file buffers first, so the commit captures on-disk content the
;; user actually sees — magit's save-some-buffers-before-commit guard.
(define-command (magit-do-commit)
  (save-some-buffers)
  (if (transient-arg-active? "commit:amend")
      (magit-commit-buffer (gix-head-message *magit-dir*) "yes")
      (magit-commit-buffer "" "no")))
;; Reword edits only the message: `--only` keeps whatever is staged out of HEAD.
(define-command (magit-do-reword) (magit-commit-buffer (gix-head-message *magit-dir*) "reword"))
(define-command (magit-do-amend)
  (save-some-buffers)
  (magit-do (append (list "commit" "--amend" "--no-edit") (magit-commit-flags)) "Amended HEAD")
  (transient-clear-args "commit:"))

(define (magit-commit-confirm)
  (let* ((msg (magit-strip-comments (buffer-string)))
         (kind (buffer-local-get "magit-amend"))
         (amend (or (equal? kind "yes") (equal? kind "reword"))))
    (if (equal? msg "")
        (message "Aborting: empty commit message")
        ;; Resolve the real git dir: in a linked worktree or submodule `.git` is
        ;; a FILE (`gitdir: …`), so `<repo>/.git/…` can't be written.
        (let* ((gd (gix-git-dir *magit-dir*))
               (file (path-join (if (equal? gd "") (path-join *magit-dir* ".git") gd)
                                "MACROS_EDITMSG")))
          (write-file file msg)
          (let ((r (call-process-full "git"
                     (append (list "-C" *magit-dir* "commit")
                             (if amend (list "--amend") '())
                             (if (equal? kind "reword") (list "--only") '())
                             (magit-commit-flags)
                             (list "-F" file)) "")))
            ;; Keep the message buffer when the commit fails (a rejecting
            ;; hook, nothing staged): the typed message must not be lost.
            (if (equal? (car r) "0")
                (begin
                  (kill-buffer-named "*git-commit*")
                  (transient-clear-args "commit:")
                  (magit-render)
                  (message (if amend "Amended HEAD" "Committed")))
                (magit-error (nth r 2))))))))
(define (magit-commit-cancel)
  (kill-buffer-named "*git-commit*")
  (transient-clear-args "commit:")
  (magit-render)
  (message "Commit cancelled"))

;; Exiting the buffer (C-x k) finishes the commit, mirroring how a terminal
;; `git commit` commits when you save-and-close the editor: a non-empty message
;; commits, an empty one aborts (and closes the buffer either way).
(define (magit-commit-finish)
  (if (equal? (magit-strip-comments (buffer-string)) "")
      (magit-commit-cancel)
      (magit-commit-confirm)))
(define-key "git-commit-mode" "ctrl-c ctrl-c" 'magit-commit-confirm)
(define-key "git-commit-mode" "ctrl-c ctrl-k" 'magit-commit-cancel)
(define-key "git-commit-mode" "ctrl-x k"      'magit-commit-finish)

;; --- branch (b) -------------------------------------------------------------
(define (magit-branch-popup)
  (transient "Branch"
    (list (list "b" "Checkout"          'magit-checkout)
          (list "r" "Checkout remote"   'magit-checkout-remote)
          (list "c" "Create & checkout" 'magit-branch-create)
          (list "u" "Set upstream"      'magit-branch-set-upstream)
          (list "m" "Rename current"    'magit-branch-rename)
          (list "k" "Delete"            'magit-branch-delete))))
(define (magit-remote-branches) (gix-remote-branches *magit-dir*))
;; Check out a remote branch as a new local tracking branch ("origin/x" → "x").
(define-command (magit-checkout-remote)
  (helm (list (list "Checkout remote" (magit-remote-branches) 'magit-do-checkout-remote))))
(define (magit-do-checkout-remote rb)
  ;; Strip only the remote prefix: "origin/feature/login" → "feature/login".
  (let* ((parts (split-many rb "/"))
         (local (if (null? (cdr parts)) rb (string-join (cdr parts) "/"))))
    (magit-do (list "checkout" "-b" local "--track" rb) (string-append "Checked out " local))))
(define-command (magit-branch-set-upstream)
  (helm (list (list "Set upstream to" (magit-remote-branches) 'magit-do-set-upstream))))
(define (magit-do-set-upstream u) (magit-do (list "branch" "--set-upstream-to" u) (string-append "Upstream → " u)))
(define-command (magit-checkout)
  (helm (list (list "Checkout" (magit-branches) 'magit-do-checkout))))
(define (magit-do-checkout b) (magit-do (list "checkout" b) (string-append "Checked out " b)))
(define-command (magit-branch-create) (read-string "Create & checkout branch: " 'magit-do-branch-create))
(define (magit-do-branch-create name)
  (when (not (equal? (trim-end name) ""))
    (magit-do (list "checkout" "-b" name) (string-append "Created " name))))
(define-command (magit-branch-rename) (read-string "Rename current branch to: " 'magit-do-branch-rename))
(define (magit-do-branch-rename name)
  (when (not (equal? (trim-end name) ""))
    (magit-do (list "branch" "-m" name) (string-append "Renamed to " name))))
(define-command (magit-branch-delete)
  (helm (list (list "Delete branch" (magit-branches) 'magit-do-branch-delete))))
(define (magit-do-branch-delete b) (magit-do (list "branch" "-d" b) (string-append "Deleted " b)))

;; --- push / pull / fetch (P / F / f) ----------------------------------------
(define (magit-push-toggle-force)    (transient-toggle-arg "push:force" magit-push-popup))
(define (magit-push-toggle-noverify) (transient-toggle-arg "push:noverify" magit-push-popup))
(define (magit-push-flags)
  (magit-switch-args (list (cons "push:force" "--force-with-lease")
                           (cons "push:noverify" "--no-verify"))))
(define (magit-push-popup)
  (transient "Push"
    (list
      (list "Switches"
            (list "f" (magit-sw-label "Force with lease" "push:force")    'magit-push-toggle-force)
            (list "n" (magit-sw-label "No verify"        "push:noverify") 'magit-push-toggle-noverify))
      (list "Actions"
            (list "p" "Push"                'magit-push)
            (list "u" "Push & set upstream" 'magit-push-upstream)))))
;; The current branch's upstream from the porcelain header ("" when unset).
(define (magit-current-upstream)
  (nth (car (magit-porcelain-full)) 1))
;; Push the current branch. With no upstream configured a plain `git push`
;; aborts ("has no upstream branch"), so transparently fall back to `-u origin
;; HEAD` to create and track it on first push — git's push.autoSetupRemote.
(define-command (magit-push)
  (if (equal? (magit-current-upstream) "")
      (magit-run-async (append (list "push" "-u" "origin" "HEAD") (magit-push-flags))
                       "Pushed, upstream set")
      (magit-run-async (append (list "push") (magit-push-flags)) "Pushed"))
  (transient-clear-args "push:"))
(define-command (magit-push-upstream)
  (magit-run-async (append (list "push" "-u" "origin" "HEAD") (magit-push-flags)) "Pushed, upstream set")
  (transient-clear-args "push:"))

(define (magit-pull-toggle-rebase) (transient-toggle-arg "pull:rebase" magit-pull-popup))
(define (magit-pull-popup)
  (transient "Pull"
    (list
      (list "Switches"
            (list "r" (magit-sw-label "Rebase (--rebase)" "pull:rebase") 'magit-pull-toggle-rebase))
      (list "Actions"
            (list "p" "Pull" 'magit-pull)))))
(define-command (magit-pull)
  (magit-run-async (append (list "pull")
                           (magit-switch-args (list (cons "pull:rebase" "--rebase")))) "Pulled")
  (transient-clear-args "pull:"))

(define (magit-fetch-toggle-prune) (transient-toggle-arg "fetch:prune" magit-fetch-popup))
(define (magit-fetch-flags) (magit-switch-args (list (cons "fetch:prune" "--prune"))))
(define (magit-fetch-popup)
  (transient "Fetch"
    (list
      (list "Switches"
            (list "p" (magit-sw-label "Prune (--prune)" "fetch:prune") 'magit-fetch-toggle-prune))
      (list "Actions"
            (list "a" "Fetch all"    'magit-fetch-all)
            (list "o" "Fetch origin" 'magit-fetch-origin)))))
(define-command (magit-fetch-all)
  (magit-run-async (append (list "fetch" "--all") (magit-fetch-flags)) "Fetched all remotes")
  (transient-clear-args "fetch:"))
(define-command (magit-fetch-origin)
  (magit-run-async (append (list "fetch" "origin") (magit-fetch-flags)) "Fetched origin")
  (transient-clear-args "fetch:"))

;; --- stash (z) --------------------------------------------------------------
(define (magit-stash-popup)
  (transient "Stash"
    (list (list "z" "Stash"            'magit-stash)
          (list "m" "Stash w/ message" 'magit-stash-push-msg)
          (list "p" "Pop latest"       'magit-stash-pop)
          (list "shift-p" "Pop a stash…" 'magit-stash-pick)
          (list "a" "Apply latest"     'magit-stash-apply)
          (list "k" "Drop latest"      'magit-stash-drop)
          (list "shift-k" "Drop all"   'magit-stash-drop-all)
          (list "l" "List"             'magit-stash-list))))
(define-command (magit-stash-push-msg) (read-string "Stash message: " 'magit-do-stash-msg))
(define (magit-do-stash-msg m)
  (when (not (equal? (trim-end m) "")) (magit-do (list "stash" "push" "-m" m) "Stashed")))
;; "stash@{N}" names, newest first — the tag before the first ": " of each
;; gix-stash-list line (the `stash@{N}` tag itself never contains one).
(define (magit-stash-refs)
  (map (lambda (l) (car (split-many l ": "))) (gix-stash-list *magit-dir*)))
(define-command (magit-stash-pick)
  (helm (list (list "Pop stash" (magit-stash-refs) 'magit-do-stash-pop-ref))))
(define (magit-do-stash-pop-ref r) (magit-do (list "stash" "pop" r) (string-append "Popped " r)))
(define-command (magit-stash)       (magit-do (list "stash" "push") "Stashed"))
(define-command (magit-stash-pop)   (magit-do (list "stash" "pop") "Popped stash"))
(define-command (magit-stash-apply) (magit-do (list "stash" "apply") "Applied stash"))
(define-command (magit-stash-drop)
  (magit-confirm "Drop latest stash?"
    (lambda () (magit-do (list "stash" "drop") "Dropped stash"))))
(define-command (magit-stash-drop-all)
  (magit-confirm "Drop all stashes?"
    (lambda () (magit-do (list "stash" "clear") "Dropped all stashes"))))
(define-command (magit-stash-list)
  (magit-show-log "*magit-stash*" (string-join (gix-stash-list *magit-dir*) "\n")))

;; --- log (l) → a read-only *magit-log* buffer -------------------------------
(define (magit-log-toggle-graph) (transient-toggle-arg "log:graph" magit-log-popup))
(define-command (magit-log-popup)
  (if (not (magit-ensure-dir))  ; reachable via M-x from a file buffer
      (message "Not inside a git repository")
      (transient "Log"
        (list
          (list "Switches"
                (list "g" (magit-sw-label "Graph (--graph)" "log:graph") 'magit-log-toggle-graph))
          (list "Actions"
                (list "l" "Current branch" 'magit-log-current)
                (list "a" "All branches"   'magit-log-all)
                (list "f" "File at point"  'magit-log-file))))))
;; gix renders the oneline+decorate log (no subprocess); the `--graph` toggle
;; falls back to git, whose ASCII commit-graph art gix can't reproduce.
(define-command (magit-log-current)
  (magit-show-log "*magit-log*"
                  (if (transient-arg-active? "log:graph")
                      (git (append (list "log" "--oneline" "--decorate" "--graph")
                                   (list "-n" (int->string *magit-log-limit*))))
                      (string-join (gix-log-decorated *magit-dir* #f *magit-log-limit*) "\n"))))
(define-command (magit-log-all)
  (magit-show-log "*magit-log*"
                  (if (transient-arg-active? "log:graph")
                      (git (append (list "log" "--oneline" "--decorate" "--all" "--graph")
                                   (list "-n" (int->string *magit-log-limit*))))
                      (string-join (gix-log-decorated *magit-dir* #t *magit-log-limit*) "\n"))))
(define (magit-show-log name text)
  (render-buffer name "magit-log-mode"
                 (if (equal? (trim-end text) "") "No output." (trim-end text)) '()))
;; The short hash on the current log line, or "". The first whitespace-separated
;; token of length >= 7 — which skips any leading `--graph` glyphs (* | / \, all
;; single chars) and lands on the abbreviated hash.
(define (magit-log-hash)
  (let* ((b (line-bounds))
         (line (buffer-substring (car b) (cadr b)))
         (parts (filter (lambda (s) (>= (string-length s) 7)) (split-many line " "))))
    ;; A stash line's ref is "stash@{0}:" — drop the colon (`X:` names a tree).
    (if (null? parts) ""
        (let ((p (car parts)))
          (if (ends-with? p ":") (substring p 0 (- (string-length p) 1)) p)))))

;; RET in a log buffer: show the commit whose short hash starts the line, with
;; the same colorized diff as the status buffer (see magit-show-commit).
(define (magit-log-show)
  (let ((hash (magit-log-hash)))
    (when (not (equal? hash ""))
      (magit-show-commit hash))))

;; A (cherry-pick / revert) onto the commit at point in a log buffer.
(define (magit-log-cherry-pick)
  (let ((h (magit-log-hash)))
    (when (not (equal? h ""))
      (magit-do (list "cherry-pick" h) (string-append "Cherry-picked " h)))))
(define (magit-log-revert)
  (let ((h (magit-log-hash)))
    (when (not (equal? h ""))
      (magit-confirm (string-append "Revert " h "?")
        (lambda () (magit-do (list "revert" "--no-edit" h) (string-append "Reverted " h)))))))

;; Blame / log the file at point in the status buffer (read-only views).
;; gix blames the file when it matches HEAD; a modified file falls back to git,
;; which blames the working tree.
(define (magit-blame)
  (let ((cur (magit-current)))
    (if cur
        (let* ((path (magit-newpath (cadr cur)))
               (text (gix-blame-text *magit-dir* path)))
          (render-buffer "*magit-blame*" "magit-log-mode"
                         (if (equal? text "")
                             (git (list "blame" "--date=short" "--" path))
                             text)
                         '()))
        (message "No file at point"))))
(define (magit-log-file)
  (let ((cur (magit-current)))
    (if cur
        (magit-show-log "*magit-log*"
                        (string-join
                          (gix-log-file *magit-dir* (magit-newpath (cadr cur)) *magit-log-limit*) "\n"))
        (message "No file at point"))))

;; --- reset (X) --------------------------------------------------------------
(define (magit-reset-popup)
  (transient "Reset"
    (list (list "m" "Mixed — unstage, keep changes"   'magit-reset-mixed)
          (list "s" "Soft — undo last commit, keep"   'magit-reset-soft)
          (list "h" "Hard — DISCARD working changes"  'magit-reset-hard))))
(define-command (magit-reset-mixed) (magit-do (list "reset" "--mixed" "HEAD") "Reset (mixed)"))
(define-command (magit-reset-soft)  (magit-do (list "reset" "--soft" "HEAD~1") "Undid last commit (soft)"))
(define-command (magit-reset-hard)
  (magit-confirm "Hard reset to HEAD — discard ALL working changes?"
    (lambda () (magit-do (list "reset" "--hard" "HEAD") "Reset --hard HEAD"))))

;; --- remote (M) -------------------------------------------------------------
(define (magit-remote-popup)
  (transient "Remote"
    (list (list "a" "Add"    'magit-remote-add)
          (list "k" "Remove" 'magit-remote-remove))))
;; Two chained prompts; the name rides into the URL prompt's closure.
(define-command (magit-remote-add)
  (read-string "Remote name: "
    (lambda (name)
      (read-string "Remote URL: "
        (lambda (url) (magit-do (list "remote" "add" name url) "Remote added"))))))
(define-command (magit-remote-remove)
  (helm (list (list "Remove remote" (magit-remotes) 'magit-do-remote-remove))))
(define (magit-do-remote-remove r) (magit-do (list "remote" "remove" r) "Remote removed"))

;; --- tag (t) ----------------------------------------------------------------
(define (magit-tag-popup)
  (transient "Tag"
    (list (list "t" "Create" 'magit-tag-create)
          (list "k" "Delete" 'magit-tag-delete))))
(define-command (magit-tag-create) (read-string "Tag name: " 'magit-do-tag-create))
(define (magit-do-tag-create name)
  (when (not (equal? (trim-end name) ""))
    (magit-do (list "tag" name) (string-append "Tagged " name))))
(define-command (magit-tag-delete)
  (helm (list (list "Delete tag" (magit-tags) 'magit-do-tag-delete))))
(define (magit-do-tag-delete t) (magit-do (list "tag" "-d" t) (string-append "Deleted tag " t)))

;; --- merge (m) --------------------------------------------------------------
(define (magit-merge-popup)
  (transient "Merge"
    (list (list "m" "Merge branch"               'magit-merge)
          (list "o" "Take ours (file at point)"  'magit-take-ours)
          (list "t" "Take theirs (file at point)" 'magit-take-theirs)
          (list "a" "Abort"                      'magit-merge-abort))))
(define-command (magit-merge)
  (helm (list (list "Merge branch" (magit-branches) 'magit-do-merge))))
(define (magit-do-merge b) (magit-do (list "merge" b) (string-append "Merged " b)))
(define (magit-merge-abort) (magit-do (list "merge" "--abort") "Merge aborted"))

;; --- dispatch (?) : the top-level menu --------------------------------------
(define-command (magit-dispatch)
  (if (not (magit-ensure-dir))  ; reachable via M-x from a file buffer
      (message "Not inside a git repository")
      ;; Grouped (multi-column) transient: each inner list is a titled column.
      (transient "Magit"
        (list
          (list "Changes"
                (list "c"       "Commit" 'magit-commit-popup)
                (list "z"       "Stash"  'magit-stash-popup)
                (list "shift-x" "Reset"  'magit-reset-popup)
                (list "shift-h" "History" 'magit-toggle-history)
                (list "g"       "Refresh" 'magit-refresh))
          (list "Branch"
                (list "b"       "Branch" 'magit-branch-popup)
                (list "m"       "Merge"  'magit-merge-popup)
                (list "r"       "Rebase" 'magit-rebase-popup)
                (list "t"       "Tag"    'magit-tag-popup)
                (list "l"       "Log"    'magit-log-popup))
          (list "Remote"
                (list "shift-p" "Push"   'magit-push-popup)
                (list "shift-f" "Pull"   'magit-pull-popup)
                (list "f"       "Fetch"  'magit-fetch-popup)
                (list "shift-m" "Remote" 'magit-remote-popup))))))

;; --- TAB / RET (sections) ---------------------------------------------------
;; TAB: expand/collapse the section at point. Diff bodies render lazily — a
;; collapsed file emits no lines at all — so toggling a FILE section re-renders
;; the buffer (cheap: only expanded bodies are built). Toggling a function
;; group inside an expanded file keeps the in-place fold path: its lines
;; already exist, only their visibility changes.
(define (magit-toggle-fold)
  (let ((g (and (magit-structural?) (magit-group-at-line (buffer-line)))))
    (if g
        ;; Structural view, point in a function group → fold just that function.
        ;; `magit-apply-overlays` unfolds everything then re-folds, which churns
        ;; the scroll; pin the cursor's screen row so the buffer doesn't jump.
        (let ((row (window-cursor-row))
              (key (grp-key g)))
          (set! *magit-collapsed-groups*
                (if (member key *magit-collapsed-groups*)
                    (remove-equal key *magit-collapsed-groups*)
                    (cons key *magit-collapsed-groups*)))
          (magit-apply-overlays)
          (scroll-cursor-to-row row))
        ;; Otherwise (file heading, or line view) → toggle the file section and
        ;; re-render (magit-render restores point + pins the screen row itself).
        (let ((s (magit-section-at-line (buffer-line))))
          (when s
            (let ((key (sec-key s)))
              (set! *magit-expanded*
                    (if (member key *magit-expanded*)
                        (remove-equal key *magit-expanded*)
                        (cons key *magit-expanded*))))
            (magit-render))))))

;; The new-file starting line ("c" in "@@ -a,b +c,d @@") of a hunk header.
(define (magit-hunk-new-start header)
  (let loop ((parts (split-many header " ")))
    (cond ((null? parts) 1)
          ((starts-with? (car parts) "+")
           (or (string->number (car (split-many
                                      (substring (car parts) 1 (string-length (car parts))) ",")))
               1))
          (else (loop (cdr parts))))))

;; Work-tree line number for the diff line `line` inside hunk `h`: the hunk's
;; new-file start plus the count of context/added lines above `line` (removed
;; lines have no new-file counterpart, so they don't advance the count).
(define (magit-hunk-target-line h line)
  (let* ((idx    (- line (nth h 0)))            ; offset into the hunk (0 = @@)
         (hlines (split-many (nth h 3) "\n"))
         (c      (magit-hunk-new-start (car hlines))))
    (let loop ((i 1) (k 0))
      (if (or (>= i idx) (>= i (length hlines)))
          (+ c k)
          (let ((l (list-ref hlines i)))
            (loop (+ i 1)
                  (if (or (starts-with? l "+") (starts-with? l " ")) (+ k 1) k)))))))

;; Whole-line face for a `git show` line. `in-diff` is #t once we've passed the
;; first "diff --git"; before that we're in the commit metadata/message, where
;; only Author/Date/Merge get a dim header tone (the subject/body stay default,
;; the hash is accented separately). In the diff body we reuse the status
;; buffer's added/removed/hunk/context faces.
(define (magit-show-commit-face l in-diff)
  (if (not in-diff)
      (cond ((starts-with? l "Author:") "magit-header")
            ((starts-with? l "Date:")   "magit-header")
            ((starts-with? l "Merge:")  "magit-header")
            ((starts-with? l "Commit:") "magit-header")
            (else #f))                                  ; "commit <hash>", subject, body
      (cond ((starts-with? l "diff --git") "magit-file")
            ((starts-with? l "index ")     "magit-context")
            ((starts-with? l "--- ")       "magit-context")
            ((starts-with? l "+++ ")       "magit-context")
            ((starts-with? l "old mode")   "magit-context")
            ((starts-with? l "new mode")   "magit-context")
            ((starts-with? l "new file")   "magit-context")
            ((starts-with? l "deleted file") "magit-context")
            ((starts-with? l "rename ")    "magit-context")
            ((starts-with? l "similarity") "magit-context")
            ((starts-with? l "@@")         "magit-hunk")
            ((starts-with? l "+")          "magit-added")
            ((starts-with? l "-")          "magit-removed")
            (else                          "magit-context"))))  ; context (leading space)

;; Render `git show <ref>` into *magit-commit* (magit-log-mode) with the same
;; colorization as the status diffs: green/red bodies, accented +/- markers, an
;; accented commit hash. Builds local accumulators (NOT the status buffer's
;; render globals) so returning to *magit* leaves its navigation state intact.
(define (magit-show-commit ref . rest)
  (let* ((dir   (if (null? rest) *magit-dir* (car rest)))
         (raw   (trim-end (gix-show dir ref)))
         (lines (split-many raw "\n")))
    (let loop ((ls lines) (n 0) (byte 0) (in-diff #f) (faces '()) (accents '()))
      (if (null? ls)
          (begin
            (render-buffer "*magit-commit*" "magit-log-mode" raw (reverse faces))
            (clear-overlays)
            (overlay-set-faces (reverse accents)))
          (let* ((l    (car ls))
                 (ind  (or in-diff (starts-with? l "diff --git")))
                 (face (magit-show-commit-face l ind))
                 (faces2 (if face (cons (list n face) faces) faces))
                 ;; Accent the hash on the "commit <hash>" metadata line.
                 (accents2
                  (if (starts-with? l "commit ")
                      (let ((hl (utf8-length
                                 (car (split-many (substring l 7 (string-length l)) " ")))))
                        (cons (list (+ byte 7) (+ byte 7 hl) "magit-hash") accents))
                      accents))
                 ;; Brighten the +/- marker on diff body lines (skip +++/--- headers).
                 (accents3
                  (cond ((not ind) accents2)
                        ((or (starts-with? l "+++ ") (starts-with? l "--- ")) accents2)
                        ((starts-with? l "+")
                         (cons (list byte (+ byte 1) "magit-add-marker") accents2))
                        ((starts-with? l "-")
                         (cons (list byte (+ byte 1) "magit-remove-marker") accents2))
                        (else accents2))))
            (loop (cdr ls) (+ n 1) (+ byte (utf8-length l) 1) ind faces2 accents3))))))

;; RET: on a commit/stash line, show it; otherwise visit the file at point in a
;; split — landing on the exact line when point is inside a diff hunk.
(define (magit-visit-file)
  (let ((ref (magit-commit-at-line (buffer-line))))
    (if ref
        (magit-show-commit ref)
        (let ((s (magit-section-at-line (buffer-line))))
          (when s
            (let* ((h (magit-hunk-at-line (buffer-line)))
                   (target (if h (magit-hunk-target-line h (buffer-line)) 0)))
              (split-window-right)
              (other-window)
              ;; open-file is async: goto-line here would land in the old
              ;; buffer, so defer the jump until the file is current.
              (let ((path (path-join *magit-dir* (magit-newpath (sec-path s)))))
                (open-file path)
                (when (> target 0) (goto-when-visiting "" path (- target 1) 0)))))))))

;; --- inline blame (git-blame as virtual line suffixes) ----------------------
;; GitLens-style annotations on the END of each line, built on `set-line-suffix`.
;; Two modes share one parsed cache: full-file (`magit-blame-toggle`, every line)
;; and current-line (`magit-blame-line-toggle`, only point's line, follows the
;; cursor via post-command-hook). Each annotation shows short-hash, author, a
;; humanized date, and the commit summary, tinted by age.

;; Age-tinted faces for full-file mode: recent edits pop, ancient code recedes.
(set-face "magit-blame-recent" "#a6e22e")  ; < ~1 month
(set-face "magit-blame-mid"    "#b58900")  ; < ~1 year
(set-face "magit-blame-old"    "#6a6a6a")  ; older / dim
;; Current-line mode stays a quiet, uniform grey — it's a single annotation that
;; should sit behind the code, not a heat-map to scan.
(set-face "magit-blame-line"   "#5c5c5c")

;; `git blame --line-porcelain` repeats a full header block per line, so each
;; record is unambiguous: a 40-hex SHA header line, key/value lines, then the
;; source line (prefixed with a tab). Parse into `(final-line sha author epoch
;; summary)` records, in file order.
(define (magit-blame-hex40? line) (= (string-match "^[0-9a-f]{40} " line) 0))
(define (magit-blame-parse lines)
  (let loop ((ls lines) (sha "") (author "") (epoch 0) (summary "") (fline 0) (acc '()))
    (cond
      ((null? ls) (reverse acc))
      (else
        (let ((line (car ls)))
          (cond
            ;; The tab-prefixed content line ends the current record.
            ((and (> (string-length line) 0) (equal? (substring line 0 1) "\t"))
             (loop (cdr ls) "" "" 0 "" 0
                   (cons (list fline sha author epoch summary) acc)))
            ((starts-with? line "author ")
             (loop (cdr ls) sha (substring line 7 (string-length line)) epoch summary fline acc))
            ((starts-with? line "author-time ")
             (loop (cdr ls) sha author
                   (or (string->number (substring line 12 (string-length line))) 0)
                   summary fline acc))
            ((starts-with? line "summary ")
             (loop (cdr ls) sha author epoch (substring line 8 (string-length line)) fline acc))
            ((magit-blame-hex40? line)             ; "<sha> <orig> <final> [<n>]"
             (let ((toks (split-many line " ")))
               (loop (cdr ls) (list-ref toks 0) author epoch summary
                     (or (string->number (list-ref toks 2)) 0) acc)))
            (else (loop (cdr ls) sha author epoch summary fline acc))))))))

;; Age from the commit's author-time epoch — `current-time "%s"` is now in epoch
;; seconds, so no calendar arithmetic is needed.
(define (magit-blame-age-seconds epoch)
  (- (or (string->number (current-time "%s")) 0) epoch))
(define *magit-blame-day* 86400)
(define (magit-blame-humanize epoch)
  (let ((age (magit-blame-age-seconds epoch)) (day *magit-blame-day*))
    (cond ((< age 0) "in the future")
          ((< age day) "today")
          ((< age (* 2 day)) "yesterday")
          ((< age (* 7 day))   (string-append (number->string (quotient age day)) " days ago"))
          ((< age (* 30 day))  (string-append (number->string (quotient age (* 7 day))) " weeks ago"))
          ((< age (* 365 day)) (string-append (number->string (quotient age (* 30 day))) " months ago"))
          (else (string-append (number->string (quotient age (* 365 day))) " years ago")))))
(define (magit-blame-age-face epoch)
  (let ((age (magit-blame-age-seconds epoch)) (day *magit-blame-day*))
    (cond ((< age (* 30 day)) "magit-blame-recent")
          ((< age (* 365 day)) "magit-blame-mid")
          (else "magit-blame-old"))))

;; Record accessors + suffix rendering. rec = (final-line sha author epoch summary).
(define (magit-blame-rec-line rec) (list-ref rec 0))
(define (magit-blame-rec-sha rec)  (list-ref rec 1))
(define (magit-blame-uncommitted? rec)
  (let ((sha (magit-blame-rec-sha rec)))
    (equal? (substring sha 0 (min 8 (string-length sha))) "00000000")))
(define (magit-blame-trim s) (truncate-string s 50)) ; keep summaries from overflowing the line
(define (magit-blame-rec-suffix rec)
  (if (magit-blame-uncommitted? rec)
      "  Not committed yet"
      (string-append "  " (substring (magit-blame-rec-sha rec) 0 8)
                     "  " (list-ref rec 2)
                     ", " (magit-blame-humanize (list-ref rec 3))
                     " · " (magit-blame-trim (list-ref rec 4)))))
(define (magit-blame-rec-face rec)
  (if (magit-blame-uncommitted? rec) "magit-context" (magit-blame-age-face (list-ref rec 3))))

;; Parsed records for the current buffer, cached by line so current-line mode and
;; the at-point actions are O(1) per cursor move (no re-blame). Invalidated when
;; the active buffer changes.
(define *magit-blame-cache* (hash))
(define *magit-blame-buf* "")
;; gix-blame rows ("LINE\tSHA\tAUTHOR\tEPOCH\tSUMMARY") → the same
;; `(final-line sha author epoch summary)` records magit-blame-parse produces, so
;; all downstream rendering is shared.
(define (magit-blame-gix-records dir file)
  (map (lambda (row)
         (let ((f (split-many row "\t")))
           (list (or (string->number (list-ref f 0)) 0)   ; final line
                 (list-ref f 1)                            ; sha
                 (list-ref f 2)                            ; author
                 (or (string->number (list-ref f 3)) 0)   ; author-time epoch
                 (list-ref f 4))))                         ; summary
       (gix-blame dir file)))
;; Prefer gix (no subprocess). It returns rows only when the file matches HEAD
;; byte-for-byte, so a non-empty result is guaranteed to line up. For a modified
;; file gix yields nothing and we fall back to `git blame --line-porcelain`,
;; which blames the working tree and flags uncommitted lines.
(define (magit-blame-fetch dir file)
  (let ((recs (magit-blame-gix-records dir file)))
    (if (null? recs)
        (magit-blame-parse
          (split-many (call-process "git" (list "-C" dir "blame" "--line-porcelain" "--" file)) "\n"))
        recs)))
(define (magit-blame-store file recs)
  (set! *magit-blame-buf* file)
  (set! *magit-blame-cache* (hash))
  (for-each (lambda (rec)
              (set! *magit-blame-cache*
                    (hash-insert *magit-blame-cache* (magit-blame-rec-line rec) rec)))
            recs))
;; The file to blame: the buffer's real path — its name may be uniquified
;; (`mod.rs<parser>`), which git can't resolve. Falls back to the name.
(define (magit-blame-file) (buffer-path-or-name))
(define (magit-blame-ensure)
  (when (not (equal? (magit-blame-file) *magit-blame-buf*))
    (magit-blame-store (magit-blame-file) (magit-blame-fetch (buffer-directory) (magit-blame-file))))
  *magit-blame-cache*)
(define (magit-blame-rec-at line)     ; line is 1-based (git's final line)
  (hash-try-get (magit-blame-ensure) line))

;; --- full-file mode ---------------------------------------------------------
;; Annotate only the FIRST line of each run of consecutive lines from the same
;; commit, so a block authored in one commit reads cleanly instead of repeating
;; the same hash/date on every line.
(define *magit-blame-on* #f)
(define (magit-blame-annotate)
  (let* ((dir (buffer-directory)) (file (magit-blame-file))
         (recs (magit-blame-fetch dir file)))
    (if (null? recs)
        (message "git blame: nothing to annotate")
        (begin
          (magit-blame-store file recs)
          (let loop ((rs recs) (prev ""))
            (when (not (null? rs))
              (let* ((rec (car rs)) (sha (magit-blame-rec-sha rec)))
                (when (not (equal? sha prev))
                  (set-line-suffix (- (magit-blame-rec-line rec) 1)
                                   (magit-blame-rec-suffix rec) (magit-blame-rec-face rec)))
                (loop (cdr rs) sha))))
          (set! *magit-blame-on* #t)
          (message "Blame on — C-x v b toggles off")))))

;; --- current-line mode (off by default) -------------------------------------
;; Show blame only on point's line, re-placing it as the cursor moves. Driven by
;; `post-command-hook`, which fires (once per cursor-line change) only while the
;; hook is registered — so there's zero cost unless this mode is on.
(define *magit-blame-line-on* #f)
(define *magit-blame-line-last* -1)
(define (magit-blame-line-refresh)
  ;; Skip special buffers (`*magit*`, `*Help*`, …): they visit no file, so
  ;; blaming them just spawns a failing git on every cursor move.
  (when (and *magit-blame-line-on* (not (starts-with? (current-buffer) "*")))
    (let ((ln (+ (buffer-line) 1)))          ; buffer-line is 0-based; cache is 1-based
      (when (not (equal? ln *magit-blame-line-last*))
        (set! *magit-blame-line-last* ln)
        (clear-line-suffixes)
        (let ((rec (magit-blame-rec-at ln)))
          (when rec
            (set-line-suffix (- ln 1) (magit-blame-rec-suffix rec) "magit-blame-line")))))))

;; --- shared teardown + toggles ----------------------------------------------
(define (magit-blame-off)
  (clear-line-suffixes)
  (set! *magit-blame-on* #f)
  (when *magit-blame-line-on*
    (set! *magit-blame-line-on* #f)
    (set! *magit-blame-line-last* -1)
    (remove-hook "post-command-hook" "magit-blame-line-refresh")))
(define-command (magit-blame-toggle)
  (if *magit-blame-on*
      (begin (magit-blame-off) (message "Blame off"))
      (begin (magit-blame-off) (magit-blame-annotate))))
(define-command (magit-blame-line-toggle)
  (if *magit-blame-line-on*
      (begin (magit-blame-off) (message "Blame (current line) off"))
      (begin
        (magit-blame-off)                    ; full-file and line modes are exclusive
        (set! *magit-blame-line-on* #t)
        (add-hook "post-command-hook" "magit-blame-line-refresh")
        (magit-blame-line-refresh)
        (message "Blame (current line) on — follows point"))))

;; --- at-point actions (work in either mode) ---------------------------------
(define-command (magit-blame-show)              ; show the commit that last touched this line
  (let ((rec (magit-blame-rec-at (+ (buffer-line) 1))))
    (cond ((not rec) (message "No blame for this line"))
          ((magit-blame-uncommitted? rec) (message "Line not committed yet"))
          (else
           ;; `*magit-dir*` is only set once magit-status has opened, and inline
           ;; blame can run with no status buffer — so pass THIS buffer's repo
           ;; explicitly (a status buffer for another repo keeps its directory).
           (magit-show-commit (magit-blame-rec-sha rec) (buffer-directory))))))
(define-command (magit-blame-copy)              ; copy the commit hash to the clipboard
  (let ((rec (magit-blame-rec-at (+ (buffer-line) 1))))
    (cond ((not rec) (message "No blame for this line"))
          ((magit-blame-uncommitted? rec) (message "Line not committed yet"))
          (else (let ((sha (magit-blame-rec-sha rec)))
                  (clipboard-set sha)
                  (message (string-append "Copied " (substring sha 0 8))))))))

;; --- section navigation + visibility cycling --------------------------------
;; Navigation targets: every file-section heading + every commit/stash line.
(define (magit-nav-lines)
  (append (map sec-heading *magit-sections*)
          ;; In the structural view, function sub-headings are nav stops too, so
          ;; M-n / M-p step function by function within a file.
          (if (magit-structural?) (map grp-heading *magit-groups*) '())
          (map car *magit-commit-lines*)))
(define (magit-next-section)
  (let loop ((hs (magit-nav-lines)) (cur (buffer-line)) (best #f))
    (cond ((null? hs) (when best (goto-line best)))
          ((and (> (car hs) cur) (or (not best) (< (car hs) best)))
           (loop (cdr hs) cur (car hs)))
          (else (loop (cdr hs) cur best)))))
(define (magit-prev-section)
  (let loop ((hs (magit-nav-lines)) (cur (buffer-line)) (best #f))
    (cond ((null? hs) (when best (goto-line best)))
          ((and (< (car hs) cur) (or (not best) (> (car hs) best)))
           (loop (cdr hs) cur (car hs)))
          (else (loop (cdr hs) cur best)))))
;; S-TAB: collapse-all when anything is expanded, else expand-all (magit cycle).
;; Bodies render lazily, so both directions rebuild the buffer text.
(define (magit-cycle-all)
  (set! *magit-expanded*
        (if (null? *magit-expanded*) (map sec-key *magit-sections*) '()))
  (magit-render))

;; v: switch between the line view and the structural (group-by-function) view.
;; A full re-render is needed — the two lay the diff out differently.
(define-command (magit-toggle-view)
  (set! *magit-diff-view* (if (magit-structural?) "line" "structural"))
  (magit-refresh)
  (message (if (magit-structural?) "Structural diff view" "Line diff view")))

;; --- process buffer ($) + opportunistic refresh -----------------------------
(define (magit-process-buffer)
  (if (buffer-live? "*magit-process*")
      (switch-to-buffer "*magit-process*")
      (render-buffer "*magit-process*" "magit-log-mode" "(no git process output yet)" '())))
;; Refresh the status whenever *magit* is open — including while you're editing
;; and saving a file in another window. The render is quiet (no focus steal, in
;; place) since the save happens from a different buffer. Cheap now that the
;; whole render is gix-backed (no subprocess per refresh).
(define (magit-maybe-refresh)
  (when (buffer-live? "*magit*")
    (set! *magit-quiet* #t)
    (magit-render)
    (set! *magit-quiet* #f)))
(add-hook "after-save-hook" "magit-maybe-refresh")

;; Git-gutter (diff-hl): recompute the active buffer's working-tree-vs-HEAD
;; change bars when it's saved or first opened. Both hooks run with no args, so
;; the nullary `magit-gutter-refresh` command can be registered directly. The
;; Rust side no-ops cheaply when the gutter is off or the buffer has no file, so
;; this is safe to fire on every save. For a live, edit-by-edit gutter, also add
;; it to a more frequent hook in init.scm, e.g.
;;   (add-hook "post-command-hook" "magit-gutter-refresh")
(add-hook "after-save-hook" "magit-gutter-refresh")
(add-hook "find-file-hook" "magit-gutter-refresh")

;; Live refresh from the repo watch (watch-tree in magit-render). Fires once per
;; debounced change batch with the watched repo root. Render fully (overlays) when
;; *magit* is in front, quietly (in place, no overlays) when it's in a background
;; window — overlays target the active buffer, so a background refresh must skip
;; them. Only reacts to our own repo (the hook also fires for dired/dired-sidebar).
(define (magit-on-fs-change dir)
  (when (and (buffer-live? "*magit*")
             (equal? (strip-trailing-slash dir) (strip-trailing-slash *magit-dir*)))
    (if (equal? (current-buffer) "*magit*")
        (magit-render)
        (begin (set! *magit-quiet* #t) (magit-render) (set! *magit-quiet* #f)))))
(add-hook "directory-changed-hook" "magit-on-fs-change")

;; Stop watching the repo when the status buffer is closed.
(define (magit-quit)
  (watch-tree "magit" '())
  (kill-buffer))

;; --- copy (y) + gitignore (i) -----------------------------------------------
(define (magit-copy)
  (let ((ref (magit-commit-at-line (buffer-line))))
    (if ref
        (begin (clipboard-set ref) (message (string-append "Copied " ref)))
        (let ((cur (magit-current)))
          (if cur
              (let ((p (magit-newpath (cadr cur))))
                (clipboard-set p) (message (string-append "Copied " p)))
              (message "Nothing to copy here"))))))
(define (magit-gitignore)
  (let ((cur (magit-current)))
    (if (not cur)
        (message "No file at point")
        (let* ((path (magit-newpath (cadr cur)))
               (gi (path-join *magit-dir* ".gitignore"))
               (existing (read-file-strict gi))
               (sep (if (or (equal? existing "") (ends-with? existing "\n")) "" "\n")))
          (write-file gi (string-append existing sep path "\n"))
          (magit-refresh)
          (message (string-append "Added " path " to .gitignore"))))))

;; --- rebase (r) -------------------------------------------------------------
;; Non-interactive rebase + the in-progress controls. (Interactive todo editing
;; — squash/fixup/reorder — needs a sequence-editor callback the process layer
;; doesn't expose yet, so it's intentionally omitted.)
(define (magit-rebase-popup)
  (transient "Rebase"
    (list (list "e" "Rebase onto…" 'magit-rebase-onto)
          (list "c" "Continue"     'magit-rebase-continue)
          (list "s" "Skip"         'magit-rebase-skip)
          (list "a" "Abort"        'magit-rebase-abort))))
(define-command (magit-rebase-onto)
  (helm (list (list "Rebase onto" (magit-branches) 'magit-do-rebase-onto))))
(define (magit-do-rebase-onto b) (magit-do (list "rebase" b) (string-append "Rebased onto " b)))
(define-command (magit-rebase-continue) (magit-do (list "rebase" "--continue") "Rebase continued"))
(define-command (magit-rebase-skip)     (magit-do (list "rebase" "--skip") "Skipped commit"))
(define-command (magit-rebase-abort)
  (magit-confirm "Abort rebase?" (lambda () (magit-do (list "rebase" "--abort") "Rebase aborted"))))

;; --- merge-conflict resolution ----------------------------------------------
;; Conflicted files show with an "unmerged" label in the working tree. Take one
;; side wholesale, then stage (`s`) to mark resolved.
(define (magit-take-ours)
  (let ((cur (magit-current)))
    (if cur
        (magit-do (list "checkout" "--ours" "--" (magit-newpath (cadr cur))) "Took ours — stage to resolve")
        (message "No file at point"))))
(define (magit-take-theirs)
  (let ((cur (magit-current)))
    (if cur
        (magit-do (list "checkout" "--theirs" "--" (magit-newpath (cadr cur))) "Took theirs — stage to resolve")
        (message "No file at point"))))

;; The status / log porcelain are read-only views, not code — no line numbers.
;; (The commit-message buffer, git-commit-mode, keeps them: it's editable text.)
(set-option "line-numbers:magit-status" "false")
(set-option "line-numbers:magit-log-mode" "false")
;; Highlight the line at point (hl-line) — these are navigational list buffers
;; where "what's under the cursor" drives every single-key action.
(set-option "hl-line:magit-status" "true")
(set-option "hl-line:magit-log-mode" "true")

;; --- keymaps ----------------------------------------------------------------
;; magit-status (matches magit's keys; uppercase = shift-, shown prettified in
;; transient menus via Rust pretty_key).
(define-keys "magit-status"
  '(("s" magit-stage)
    ("shift-s" magit-stage-all)
    ("u" magit-unstage)
    ("shift-u" magit-unstage-all)
    ("k" magit-discard)
    ("c" magit-commit-popup)
    ("b" magit-branch-popup)
    ("shift-p" magit-push-popup)
    ("shift-f" magit-pull-popup)
    ("f" magit-fetch-popup)
    ("z" magit-stash-popup)
    ("l" magit-log-popup)
    ("shift-x" magit-reset-popup)
    ("shift-m" magit-remote-popup)
    ("t" magit-tag-popup)
    ("m" magit-merge-popup)
    ("shift-b" magit-blame)
    ("r" magit-rebase-popup)
    ("y" magit-copy)
    ("i" magit-gitignore)
    ("$" magit-process-buffer)
    ("shift-h" magit-toggle-history)
    ("?" magit-dispatch)
    ("g" magit-refresh)
    ("v" magit-toggle-view)
    ("n" next-line)
    ("p" previous-line)
    ("alt-n" magit-next-section)
    ("alt-p" magit-prev-section)
    ("q" magit-quit)
    ("tab" magit-toggle-fold)
    ("backtab" magit-cycle-all)
    ("shift-tab" magit-cycle-all)
    ("enter" magit-visit-file)
    ;; Shadows fundamental's C-g inside magit so it can cancel a live push/pull.
    ("ctrl-g" magit-keyboard-quit)))

;; Evil + Helix overlays (each consulted only under its own modal mode, before
;; the keymap above, so the Emacs defaults are untouched). The collection layer
;; gives every special buffer `j`/`k` motion, which takes `k` over from discard
;; — so re-expose discard on `x` (free in magit; `X`/shift-x is reset). `g g`
;; makes `g` a prefix, shadowing the single-`g` refresh, so re-expose refresh on
;; `g r`, matching evil-collection's magit bindings. Magit's modal keymap
;; doesn't bind `0` / `^` (only `$`, which stays `magit-process-buffer`), so
;; expose the vim line-start motions too. The motion helpers are evil.scm's
;; (plain point moves, modal-agnostic); everything else — s/u/c/b/P/F/z/l/… —
;; stays as magit bound it.
(for-each
 (lambda (prefix)
   (let ((km (string-append prefix "magit-status")))
     (define-keys km
       '(("x" magit-discard)
         ("g g" evil-goto-first-line)
         ("shift-g" evil-goto-last-line)
         ("g r" magit-refresh)
         ("0" move-beginning-of-line)
         ("^" evil-first-non-blank)
         ;; Escape = the modal C-g: cancels a live push/pull like C-g does.
         ("escape" magit-keyboard-quit)))))
 (list "evil-normal:" "helix-normal:"))
;; Helix goes to the bottom with `g e`, not `G` (which is vim's). Both are bound
;; under Helix so either muscle memory works, matching dired-sidebar.
(define-key "helix-normal:magit-status" "g e" 'evil-goto-last-line)

;; Log / commit / stash-list buffers.
(define-list-mode-keys "magit-log-mode" 'magit-log-show)
(define-key "magit-log-mode" "shift-a" 'magit-log-cherry-pick)
(define-key "magit-log-mode" "v" 'magit-log-revert)

;; Launch with C-x g (resolves the repo from the current buffer's directory).
(define-key "fundamental" "ctrl-x g" 'magit-status)

;; C-x M: pick a different repo via Helm, then open magit there (e.g. from
;; *scratch*, or to jump to another project).
(define-command (helm-magit)
  (helm (list (list "Open git repo in" "find-file-candidates" 'magit-status-in))))
(define-key "fundamental" "ctrl-x shift-m" 'helm-magit)

;; C-x v …: inline git-blame controls on the current file buffer (Emacs vc-prefix).
(define-keys "fundamental"
  '(("ctrl-x v b" magit-blame-toggle)             ; full-file blame
    ("ctrl-x v shift-b" magit-blame-line-toggle)  ; current-line blame
    ("ctrl-x v enter" magit-blame-show)           ; show commit at point
    ("ctrl-x v y" magit-blame-copy)))             ; copy hash at point

;; `magit` is the familiar alias for `magit-status`.
(define-command (magit) (magit-status))

;; M-x exposure: the entry points and standalone git operations above are all
;; `define-command`s, so they register as they're defined. Deliberately NOT
;; registered: the section / file-at-point operations (stage, unstage, discard,
;; blame-file, log-file, take-ours/theirs), which only make sense with point
;; inside a magit buffer, and the *-popup / accessor helpers.
