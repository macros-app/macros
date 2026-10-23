;; requires: core completion hooks modeline

;; abbrev — Emacs-style abbreviations, the Macros port of `abbrev`.
;; See web/content/docs/abbrev.md for the user-facing docs (including the
;; snippet-clash guidance). An abbrev is a short word that expands to something longer via an explicit
;; command (`expand-abbrev`, bound to C-x ' by default — rebind freely; if the
;; binding clashes with yasnippet, that binding choice is yours).
;;
;;   (define-abbrev 'global-abbrev-table "teh" "the")          ; global
;;   (define-abbrev 'rust-mode-abbrev-table "fnr" "fn $0")     ; rust-mode only
;;   (define-abbrev 'global-abbrev-table "ie" "i.e." #f
;;                  '(:case-fixed #t))                         ; with props
;;
;; Props (plist): :case-fixed, :enable-function, :ts-context.
;;
;; `abbrev-mode` is the buffer-local minor mode: while on in a buffer, an
;; abbrev expands automatically when you finish typing it (a word terminator
;; — space, punct, … — triggers expansion, Emacs-style) and abbrevs appear as
;; a completion-at-point source. `abbrev-mode` toggles the current buffer;
;; `set-abbrev-mode-default` turns it on everywhere by default (buffers with
;; an explicit toggle win). Tables auto-save silently on exit
;; (`save-abbrevs` 'silent, set-save-abbrevs to change) to
;; `abbrev-file-name` (set-abbrev-file-name), and the file is read back on
;; after-init-hook. Defines made while startup runs count as :system, so the
;; file only holds abbrevs added interactively.
;;
;; There is no command-free magic beyond that: besides `abbrev-mode`'s
;; typing-time expansion, expansion happens when you run `expand-abbrev` (or,
;; optionally, on insert-exit — see set-abbrev-expand-on-insert-exit). Case
;; rules: TEH → THE, Teh → The, and
;; `:case-fixed` expansions insert exactly as defined. Anonymous procedures as
;; hooks/predicates don't persist to the abbrev file — use 'symbol names.
;;
;; COMMANDS & API
;;   expand-abbrev (C-x ')   replace the word before point with its expansion;
;;                           silent no-op when :enable-function/:ts-context decline
;;   abbrev-mode (M-x)       buffer-local minor mode: typing-time abbrev
;;                           expansion + the abbrev completion source
;;   set-abbrev-mode-default #t   enable abbrev-mode by default in every
;;                           buffer without an explicit toggle (init.scm)
;;   add-global-abbrev (C-x a g)     define an abbrev interactively: the word
;;   add-mode-abbrev (C-x a m)       (or region) before point is the expansion,
;;   inverse-add-global-abbrev       the name (or, for the inverse pair, the
;;   inverse-add-mode-abbrev         expansion) is read from the minibuffer —
;;                           inverse-add then expands the word in place
;;   unexpand-abbrev (M-x)   restore the abbrev of the last expansion
;;   abbrev-prefix-mark (C-x a -)    prefix trick: type prefix, mark, type
;;                           abbrev, expand — the hyphen is removed
;;   define-abbrev           (TABLE NAME EXPANSION &optional HOOK PROPS) —
;;                           Emacs signature; TABLE is a quoted symbol;
;;                           EXPANSION #f undefines the abbrev
;;   define-global-abbrev    (NAME EXPANSION &optional HOOK PROPS) — plain
;;                           user abbrev in the global table (not :system)
;;   define-abbrev-table     (SYMBOL DEFINITIONS &optional DOCSTRING PROPS);
;;                           :parents names tables tried before the global one;
;;                           definitions default to :system #t (Emacs rule)
;;   clear-abbrev, abbrev-symbol, abbrev-table-p
;;   write-abbrev-file / read-abbrev-file      explicit persistence (Scheme data)
;;   set-save-abbrevs / set-abbrev-file-name  'silent by default (auto-save on
;;                           exit to ~/.config/macros/abbrevs.scm, no messages);
;;                           no file is written while no abbrevs exist
;;   set-abbrev-capf-min-prefix           shortest prefix abbrev completion
;;                                        answers (default 2)
;;   set-abbrev-expand-on-insert-exit     expand silently when leaving evil /
;;                                        helix insert mode with escape
;;
;; PROPS
;;   :case-fixed #t         insert the expansion exactly as defined
;;   :enable-function pred  proc or 'symbol taking the abbrev; #f declines
;;                          silently (checked first)
;;   :ts-context "X"        expand only inside a matching tree-sitter node
;;                          (space-separated list; '@' optional). Resolved
;;                          synchronously at expansion time via ts-ancestors;
;;                          in a grammar-less buffer the abbrev never expands.
;;                          With multiple cursors this follows the primary
;;                          cursor only.
;;   :system #t / 'force    "system" abbreviation: NOT saved to the abbrev
;;                          file (define it in init.scm instead). A system
;;                          abbrev will not overwrite a non-system abbrev of
;;                          the same name unless :system is 'force.

;; table-name -> hash of abbrev -> property alist
(define *abbrev-tables* (hash))

;; #t while startup runs (bundled scripts + init.scm): define-abbrev then
;; marks its definitions :system unless they name their own :system. Flipped
;; off on after-init-hook (abbrev--after-init), together with reading the
;; saved abbrev file back in.
(define *abbrev--init-phase* #t)

;; :ts-context — syntax-context predicate --------------------------------------
;; (define-abbrev 'prog-mode-abbrev-table "x" "y" #f '(:ts-context "@comment"))
;; expands only when the abbrev sits inside a comment (a space-separated list
;; also works: '(:ts-context "@comment @string")). Names may carry the '@' of
;; tree-sitter highlight captures or be bare words; either way the enclosing
;; node KIND just has to contain the name (so "comment" matches the
;; line_comment/block_comment/comment node kinds across grammars).
;;
;; (ts-ancestors pos) is a synchronous read over the published snapshot, so
;; :ts-context resolves the enclosing node chain at expansion time — no cache,
;; no hook, no staleness. In a grammar-less buffer it returns '() and an abbrev
;; with :ts-context simply never expands — a visible, documented degradation.

;; Drop the leading '@' of a capture-style name, if any.
(define (abbrev--strip-at n)
  (if (and (> (string-length n) 0) (string=? "@" (substring n 0 1)))
      (substring n 1 (string-length n))
      n))

;; #t when NODE-KIND is one of NAMES (a '@'-stripped context name), either
;; exactly or by substring — so "comment" matches line_comment/block_comment.
(define (abbrev--kind-match? kind names)
  (cond ((null? names) #f)
        ((or (string=? kind (car names))
             (string-contains? kind (car names)))
         #t)
        (else (abbrev--kind-match? kind (cdr names)))))

;; True when the entry has no :ts-context, or one of its context names matches
;; an enclosing node kind whose span overlaps START..END (byte offsets).
;; (ts-ancestors) yields a flat (kind start end …) list, innermost first.
(define (abbrev--ts-context-ok entry start end)
  (let ((want (cdr (assoc 'ts-context entry))))
    (or (not want)
        (let ((names (map abbrev--strip-at (non-empty-lines (replace-regexp-in-string " " "\n" want)))))
          (let loop ((cs (ts-ancestors start)))
            (cond ((< (length cs) 3) #f)
                  ((and (abbrev--kind-match? (nth cs 0) names)
                        (<= (nth cs 1) end)     ; overlap test: the
                        (>= (nth cs 2) start))  ; spans share bytes
                   #t)
                  (else (loop (cdr (cdr (cdr cs)))))))))))

;; --- tables ------------------------------------------------------------------
;; An abbrev table is a Steel hash: abbrev string -> property alist:
;;   ((expansion . "…") (hook . #f|proc-or-symbol) (case-fixed . bool)
;;    (enable-function . #f|proc-or-symbol) (ts-context . #f|"@comment"))
;; Tables live in *abbrev-tables* keyed by name; "global-abbrev-table" always
;; exists; a mode's table is "<mode>-abbrev-table", created on demand.

(define (abbrev--get-table name)
  (let ((t (hash-try-get *abbrev-tables* name)))
    (if t t (hash))))

(define (abbrev--put-table name t)
  (set! *abbrev-tables* (hash-insert *abbrev-tables* name t)))

(define (abbrev--plist-get plist key)
  (cond ((null? plist) #f)
        ((eq? (car plist) key) (abbrev--unquote (cadr plist)))
        (else (abbrev--plist-get (cddr plist) key))))

;; Inside a quoted plist, '(:enable-function 'pred) stores (quote pred), not
;; pred — peel one quote level so prop values come out usable.
(define (abbrev--unquote x)
  (if (and (pair? x) (eq? (car x) 'quote))
      (cadr x)
      x))

;; #t when KEY occurs in PLIST (even with a false value) — unlike
;; abbrev--plist-get, which cannot distinguish (:system #f) from absence.
(define (abbrev--plist-has? key plist)
  (cond ((null? plist) #f)
        ((eq? (car plist) key) #t)
        (else (abbrev--plist-has? key (cddr plist)))))

;; (define-abbrev-table SYMBOL DEFINITIONS &optional DOCSTRING &rest PROPS)
;; Emacs signature. SYMBOL is the quoted table name; DEFINITIONS is a list of
;; (NAME EXPANSION [HOOK [PROPS]]) lists. PROPS: (:parents '(other-table …)).
;; The table need not pre-exist an abbreviation — define-abbrev creates any
;; table on demand — but this is how users declare a table for a mode (or a
;; mode GROUP, via :parents) explicitly, Emacs-style.
;;   (define-abbrev-table 'prog-mode-abbrev-table '()
;;     "Shared across all prog modes" '(:parents '()))
;;   (define-abbrev-table 'rust-mode-abbrev-table '()
;;     nil '(:parents '(prog-mode-abbrev-table)))
(define (define-abbrev-table symbol definitions . rest)
  (let* ((props (let scan ((xs rest))
                  ;; The PROPS plist is the last trailing list argument that
                  ;; starts with a keyword; skip a docstring/#f before it.
                  ;; (steel-core's `last` misbehaves on this shape, so walk
                  ;; manually.)
                  (cond ((null? xs) '())
                        ((null? (cdr xs))
                         (if (and (pair? (car xs))
                                  (not (string? (car (car xs)))))
                             (car xs)
                             '()))
                        (else (scan (cdr xs))))))
         (parents (abbrev--plist-get props ':parents))
         (tname (symbol->string symbol))
         (t (abbrev--get-table tname)))
    (when parents (abbrev--put-parents tname parents))
    ;; definitions: ((name expansion [hook [props]]) …)
    ;; Emacs rule: abbrevs declared via define-abbrev-table are SYSTEM abbrevs
    ;; (:system #t) — they are not saved to the user's abbrev file. A definition
    ;; that names its own :system keeps that value.
    (for-each
     (lambda (d)
       ;; no cadddr in this Steel build — use list-ref
       (let ((dprops (if (> (length d) 3) (list-ref d 3) '())))
         (apply define-abbrev
                symbol (car d) (cadr d)
                (append (if (> (length d) 2) (list (list-ref d 2)) '(#f))
                        ;; Default to :system #t only when the definition
                        ;; doesn't name its own :system (a bare (:system #f)
                        ;; means the user wants a savable abbrev here).
                        (list (if (abbrev--plist-has? ':system dprops)
                                  dprops
                                  (append dprops '(:system #t))))))))
     definitions)))

;; :parents — list of table names tried after the local table, before global.
(define *abbrev-table-parents* (hash))   ; table-name -> list of parent names
(define (abbrev--put-parents tname parents)
  (set! *abbrev-table-parents*
        (hash-insert *abbrev-table-parents* tname
                     ;; Accept both '(:parents (a b)) and '(:parents '(a b)):
                     ;; the inner quote leaves (quote a) elements behind.
                     ;; Names normalize to strings (table keys are strings).
                     (map (lambda (p)
                            (let ((u (abbrev--unquote p)))
                              (if (symbol? u) (symbol->string u) u)))
                          parents))))

;; (abbrev-table-p SYMBOL) — #t when SYMBOL names an existing abbrev table.
(define (abbrev-table-p symbol)
  (let ((t (hash-try-get *abbrev-tables* (symbol->string symbol))))
    (if t #t #f)))

;; (define-abbrev TABLE NAME EXPANSION &optional HOOK PROPS) — Emacs signature.
;; TABLE is a quoted symbol ('global-abbrev-table, 'rust-mode-abbrev-table, …).
;; HOOK is a procedure or 'symbol run after expansion (pass #f for none).
;; PROPS is a plist: (:case-fixed #t :enable-function 'pred :ts-context "X"
;;                    :system #t|'force).
;; :system marks a "system" abbrev: it is excluded from the saved abbrev file
;; (define those in init.scm) and will not overwrite an existing NON-system
;; abbrev of the same name unless :system is 'force (Emacs rule). Defines made
;; during the init phase become system abbrevs automatically — see
;; *abbrev--init-phase*.
;; Redefining an abbrev REPLACES it (hash-insert semantics).
(define (define-abbrev table name expansion . rest)
  (let* ((hook (if (null? rest) #f (car rest)))
         (props (if (or (null? rest) (null? (cdr rest))) '() (cadr rest)))
         (tname (symbol->string table))
         (t (abbrev--get-table tname))
         (existing (hash-try-get t name))
         ;; Anything defined while the init phase runs (bundled scripts +
         ;; init.scm) counts as a system abbrev unless it names its own
         ;; :system — the abbrev file only holds abbrevs added interactively,
         ;; so startup definitions never double up with the saved file.
         (system-flag (if (and *abbrev--init-phase*
                               (not (abbrev--plist-has? ':system props)))
                          #t
                          (abbrev--plist-get props ':system)))
         ;; Store #t for 'force — like Emacs, the force value itself is not kept.
         (system-val (if (eq? system-flag 'force) #t system-flag))
         (entry (list (cons 'expansion expansion)
                      (cons 'hook hook)
                      (cons 'case-fixed (abbrev--plist-get props ':case-fixed))
                      (cons 'enable-function (abbrev--plist-get props ':enable-function))
                      (cons 'ts-context (abbrev--plist-get props ':ts-context))
                      (cons 'system system-val))))
    ;; A system abbrev does not overwrite a non-system abbrev that already has
    ;; an expansion, unless :system is 'force (Emacs's protection rule).
    ;; EXPANSION #f undefines the abbrev (Emacs rule).
    (cond ((not expansion)
           (abbrev--put-table tname (hash-remove t name))
           ;; Say so only when something was actually removed — a misspelled
           ;; name shouldn't announce an undefinition that never happened.
           (when existing
             (message (string-append "Undefined abbrev: " name)))
           name)
          ((and system-flag
                (not (eq? system-flag 'force))
                existing
                (cdr (assoc 'expansion existing))
                (not (cdr (assoc 'system existing))))
           ;; Skipped — the user's abbrev wins. Say so: a silent no-op here
           ;; would look like the system abbrev "didn't take".
           (message (string-append "abbrev: system definition of \"" name
                                   "\" in " tname
                                   " skipped — a user abbrev already defines it"))
           name)
          (else
           (abbrev--put-table tname (hash-insert t name entry))
           name))))

;; (define-global-abbrev NAME EXPANSION &optional HOOK PROPS) — convenience
;; form: define-abbrev into 'global-abbrev-table. Not :system; for abbrevs
;; that should never reach the abbrev file use define-abbrev with :system.
(define (define-global-abbrev name expansion . rest)
  (apply define-abbrev 'global-abbrev-table name expansion rest))

;; (clear-abbrev [table]) — remove every abbrev from TABLE (default global).
(define (clear-abbrev . table)
  (abbrev--put-table
   (symbol->string (if (null? table) 'global-abbrev-table (car table))) (hash)))

;; (abbrev-symbol abbrev [local-table]) — the property alist for ABBREV.
;; Resolution order: local (mode) table → :parents (in list order, via
;; *abbrev-table-parents*) → global. #f when none has it. LOCAL-TABLE may be
;; given as the bare language name ("rust"), a mode symbol ('rust-mode), or a
;; full table name ('rust-mode-abbrev-table); when omitted, the current
;; buffer's major mode table is used.
;;
;; Table-name normalization: `major-mode` returns the bare language name
;; ("rust", "python" — see register_snapshot_read "major-mode"), while a
;; mode's abbrev table is "<mode>-abbrev-table" (rust → rust-mode-abbrev-table,
;; per the (add-hook "rust" …) naming in language-modes.scm). A name that
;; already ends in -mode takes just "-abbrev-table"; a full table name is kept.
(define (abbrev--ends-with? n suffix)
  (let ((gl (string-length n)) (sl (string-length suffix)))
    (and (>= gl sl)
         (string=? (substring n (- gl sl) gl) suffix))))

(define (abbrev--local-table-name given)
  (let ((n (if (symbol? given) (symbol->string given) given)))
    (cond ((abbrev--ends-with? n "-abbrev-table") n)
          ((abbrev--ends-with? n "-mode") (string-append n "-abbrev-table"))
          (else (string-append n "-mode-abbrev-table")))))

;; The tables to search for the current buffer, in order: local, its :parents,
;; global. Shared by abbrev-symbol and abbrev-capf so the two can't drift.
;; LOCAL is a bare language name, a mode symbol, or a full table name.
(define (abbrev--table-chain local)
  (let* ((local-name (abbrev--local-table-name local))
         (parents (or (hash-try-get *abbrev-table-parents* local-name) '()))
         (chain (cons local-name parents)))
    ;; Global last, unless the chain already reaches it (local table IS
    ;; global, or a parent declares it) — searching it twice is harmless but
    ;; the capf would pay for the wasted pass.
    (if (member "global-abbrev-table" chain)
        chain
        (append chain (list "global-abbrev-table")))))

(define (abbrev-symbol abbrev . local)
  (abbrev--find-in (abbrev--table-chain
                    (if (null? local) (major-mode) (car local)))
                   abbrev))

(define (abbrev--find-in names abbrev)
  ;; Emacs looks the abbrev up case-sensitively first, then case-folded —
  ;; unless the entry is :case-fixed, which demands an exact match.
  (if (null? names)
      #f
      (let* ((table (abbrev--get-table (car names)))
             (hit (hash-try-get table abbrev)))
        (if hit
            hit
            (let ((folded (hash-try-get table (string-downcase abbrev))))
              (if (and folded
                       (not (cdr (assoc 'case-fixed folded))))
                  folded
                  (abbrev--find-in (cdr names) abbrev)))))))

;; --- expansion ---------------------------------------------------------------
(define (abbrev--word-char? s) (>= (string-match "[A-Za-z0-9_]" s) 0))

;; (start . word) for the identifier ending at byte POS, or #f. Char-safe:
;; buffer offsets are BYTES but the scan walks the CHARACTERS of a byte
;; window (string->list; the window read snaps to char boundaries), then
;; converts the word back to a byte span with utf8-length — multi-byte text
;; (`café`) is never sliced mid-codepoint and never shifts the span. A word
;; longer than the window is not an abbrev; #f.
(define (abbrev--trailing-word-at pos)
  (let* ((from (max 0 (- pos 256)))
         (cs (reverse (string->list (buffer-substring from pos)))))
    (let loop ((rest cs) (wordc '()))
      (cond ((and (pair? rest) (abbrev--word-char? (string (car rest))))
             (if (= (length wordc) 128)
                 #f  ; over-long word: not an abbrev
                 (loop (cdr rest) (cons (car rest) wordc))))
            ((null? wordc) #f)
            (else (let ((word (list->string wordc)))
                    (cons (- pos (utf8-length word)) word)))))))

;; (start . word) for the identifier ending at point, or #f.
(define (abbrev--word-before) (abbrev--trailing-word-at (point)))

;; Case rule (Emacs default; skipped when :case-fixed is set):
;;   typed all-caps -> expansion all-caps     TEH -> THE
;;   Capitalized    -> Capitalized expansion  Teh -> The
;;   otherwise      -> expansion as defined
;; ASCII letters only: non-ASCII abbrevs (`café`) get the literal expansion.
(define (abbrev--apply-case word expansion entry)
  (if (cdr (assoc 'case-fixed entry))
      expansion
      (let ((all-upper? (abbrev--all-upper? word))
            (first-upper? (let ((cs (filter abbrev--alpha? (string->list word))))
                            (and (> (length cs) 0) (abbrev--upper? (car cs))))))
        (cond (all-upper? (string-upcase expansion))
              (first-upper?
               (string-append (string-upcase (substring expansion 0 1))
                              (substring expansion 1 (string-length expansion))))
              (else expansion)))))

;; NOTE: steel-core has no every? / char-upper-case? — hand-rolled below.
(define (abbrev--alpha? c) (or (abbrev--upper? c) (char<=? #\a c #\z)))
(define (abbrev--upper? c) (char<=? #\A c #\Z))
(define (abbrev--all-upper? word)
  (let ((cs (filter abbrev--alpha? (string->list word))))
    (and (> (length cs) 0)
         (let loop ((cs cs))
           (cond ((null? cs) #t)
                 ((abbrev--upper? (car cs)) (loop (cdr cs)))
                 (else #f))))))

;; HOOK and :enable-function accept a procedure or a 'symbol name (the same
;; convention as hooks.scm). Called with one argument.
(define (abbrev--call-pred f arg)
  (cond ((procedure? f) (f arg))
        ;; Escape ARG for the string literal — an expansion containing `"`
        ;; must not break out of the eval-string (code injection).
        ((symbol? f) (eval-string (string-append "(" (symbol->string f)
                                                 " \"" (abbrev--escape arg) "\")")))
        (else #f)))

;; Shared expansion core: replace the word (START . END) — WORD is the abbrev
;; name as typed — with EXP, then run HOOK. Queued edits apply in order
;; (yasnippet.scm): delete the abbrev, then insert the expansion at its
;; start. Used by expand-abbrev, the typing-time expansion and the
;; Records (WORD EXP START BUFFER) for unexpand-abbrev.
(define *abbrev-last* #f)

(define (abbrev--expand-at start end word exp hook)
  (set! *abbrev-last*
        (list word exp start (current-buffer)))
  (delete-region start end)
  (goto-char start)
  (insert exp)
  (when hook (abbrev--call-pred hook exp)))

;; The predicate chain shared by every expansion entry point: find the
;; abbrev, run :enable-function then :ts-context, apply case rules, expand.
;; Replaces the byte span START..END (WORD is the abbrev name as typed —
;; it may be shorter than the span, e.g. the prefix-mark hyphen). Returns #f
;; when a predicate declined (silent, like Emacs).
(define (abbrev--expand-named start end word)
  (let ((entry (abbrev-symbol word)))
    (cond
      ((not entry)
       (message (string-append "No abbrev: " word)))
      ((let ((pred (cdr (assoc 'enable-function entry))))
         (and pred (not (abbrev--call-pred pred word))))
       #f)  ; predicate declined — stay silent, like Emacs
      ;; Buffer offsets are BYTES for the span; WORD only shapes the lookup
      ;; and the case rules, so no char/byte arithmetic on it here.
      ((not (abbrev--ts-context-ok entry start end))
       #f)  ; wrong syntax context — silent
      (else
       (let ((exp (abbrev--apply-case word
                                      (cdr (assoc 'expansion entry))
                                      entry)))
         (abbrev--expand-at start end word exp
                            (cdr (assoc 'hook entry)))
         (message (string-append "Expanded: " word)))))))

;; expand-abbrev — replace the word before point with its expansion (local
;; table first, then :parents, then global). Works whether or not
;; abbrev-mode is on, like Emacs. :enable-function runs first, then
;; :ts-context; the HOOK runs after a successful expansion. Primary cursor
;; only. With a prefix marked (abbrev-prefix-mark), the abbrev starts at the
;; mark instead of at the word before point.
(define (expand-abbrev)
  (if *abbrev--prefix-mark*
      (abbrev--expand-prefix-marked)
      (let ((wb (abbrev--word-before)))
        (if (not wb)
            (message "No word before point")
            (abbrev--expand-named (car wb) (point) (cdr wb))))))

(register-command 'expand-abbrev)
(define-key "fundamental" "ctrl-x '" 'expand-abbrev)

;; --- interactive definition (Emacs add-*-abbrev) -----------------------------
;; Emacs's define-at-point commands: the expansion is the word (or region)
;; before point, the name is read from the minibuffer — and the inverse pair
;; reads the expansion instead, then expands in place. No prefix-argument
;; word counts; the region or one word is the expansion (arg 0 / default).

(define (abbrev--define-interactively tname kind)
  (let ((exp (if (region-active?)
                 (buffer-substring (region-beginning) (region-end))
                 (let ((wb (abbrev--word-before)))
                   (if wb (cdr wb) #f)))))
    (if (not exp)
        (message (string-append "No " kind " abbrev: nothing before point"))
        (read-string
         (string-append "Define " kind " abbrev that expands into \"" exp "\": ")
         (lambda (name)
           (if (string=? name "")
               (message "No abbrev name given")
               (begin
                 (define-abbrev (string->symbol tname) name exp)
                 (message (string-append "Defined " kind " abbrev: "
                                         name " -> " exp)))))))))

(define (abbrev--inverse-add-interactively tname kind)
  (let ((wb (abbrev--word-before)))
    (if (not wb)
        (message (string-append "No " kind " abbrev: nothing before point"))
        (read-string
         (string-append "Expansion for " kind " abbrev \"" (cdr wb) "\": ")
         (lambda (exp)
           (if (string=? exp "")
               (message "No expansion given")
               (begin
                 (define-abbrev (string->symbol tname) (cdr wb) exp)
                 ;; Expand the word in place, like inverse-add-abbrev.
                 (delete-region (car wb) (point))
                 (goto-char (car wb))
                 (insert exp)
                 (message (string-append "Defined " kind " abbrev: "
                                         (cdr wb))))))))))

(define (add-global-abbrev)
  (abbrev--define-interactively "global-abbrev-table" "global"))

(define (add-mode-abbrev)
  (abbrev--define-interactively
   (string-append (major-mode) "-mode-abbrev-table") "mode"))

(define (inverse-add-global-abbrev)
  (abbrev--inverse-add-interactively "global-abbrev-table" "global"))

(define (inverse-add-mode-abbrev)
  (abbrev--inverse-add-interactively
   (string-append (major-mode) "-mode-abbrev-table") "mode"))

(register-command 'add-global-abbrev)
(register-command 'add-mode-abbrev)
(register-command 'inverse-add-global-abbrev)
(register-command 'inverse-add-mode-abbrev)
(define-key "fundamental" "ctrl-x a g" 'add-global-abbrev)
(define-key "fundamental" "ctrl-x a m" 'add-mode-abbrev)
(define-key "fundamental" "ctrl-x a i g" 'inverse-add-global-abbrev)
(define-key "fundamental" "ctrl-x a i m" 'inverse-add-mode-abbrev)

;; --- unexpand-abbrev ----------------------------------------------------------
;; Undo the last expansion without touching anything edited since: the
;; expansion text is replaced by the abbrev name as it was typed. The record
;; (kept by abbrev--expand-at, above) is scoped to the buffer it expanded in —
;; a stale record from another buffer must not rewrite text here. (Unbound by
;; default, like Emacs: undo lives on C-x C-u / C-/ and C-x u is too easy to
;; fat-finger for it.)

(define (unexpand-abbrev)
  (let ((rec *abbrev-last*))
    (if (and rec (string=? (nth rec 3) (current-buffer)))
        (let* ((word (nth rec 0))
               (exp (nth rec 1))
               (start (nth rec 2))
               ;; buffer offsets are BYTES; string-length counts chars.
               (end (+ start (utf8-length exp))))
          (if (and (>= start 0)
                   (<= end (point-max))
                   (string=? (buffer-substring start end) exp))
              (begin
                (delete-region start end)
                (goto-char start)
                (insert word)
                (set! *abbrev-last* #f)
                (message "Unexpanded"))
              (message "No abbrev expansion at its original location")))
        (message "No abbrev expansion to undo"))))

(register-command 'unexpand-abbrev)

;; --- abbrev-prefix-mark ---------------------------------------------------------
;; Emacs's prefix trick: type the prefix, run this (a hyphen is inserted),
;; type the abbrev, then expand — the expansion replaces the abbrev and the
;; hyphen goes away, leaving prefix + expansion. The mark carries its buffer:
;; switching buffers disarms it rather than expanding at a stale position.
(define *abbrev--prefix-mark* #f)   ; (position buffer) while armed

(define (abbrev-prefix-mark)
  (set! *abbrev--prefix-mark* (list (point) (current-buffer)))
  (insert "-")
  (message "Abbrev prefix marked"))

;; The abbrev starts at the mark; the hyphen the mark inserted is part of the
;; replaced span, so prefix + expansion remain and the hyphen goes away. The
;; word is read from the UN-deleted buffer — queued edits (the hyphen insert,
;; the expansion's own edits) are not visible until the command batch
;; applies — and the lookup strips the hyphen instead of relying on deleting
;; it mid-eval. A stale mark (marked in another buffer) falls back to
;; word-before-point rather than touching text at a random position; nothing
;; typed after the hyphen falls back the same way.
(define (abbrev--expand-prefix-marked)
  (let ((start (nth *abbrev--prefix-mark* 0))
        (marked-buffer (nth *abbrev--prefix-mark* 1)))
    (set! *abbrev--prefix-mark* #f)
    (if (not (string=? marked-buffer (current-buffer)))
        (let ((wb (abbrev--word-before)))
          (if (not wb)
              (message "No word before point")
              (abbrev--expand-named (car wb) (point) (cdr wb))))
        (if (and (< start (point))
                 (string=? (buffer-substring start (+ start 1)) "-"))
            ;; Lookup skips the hyphen; the replacement span keeps it, so the
            ;; expansion overwrites mark..point wholesale.
            (abbrev--expand-named start (point)
                                  (buffer-substring (+ start 1) (point)))
            (let ((wb (abbrev--word-before)))
              (if (not wb)
                  (message "No word before point")
                  (abbrev--expand-named (car wb) (point) (cdr wb))))))))

(register-command 'abbrev-prefix-mark)
(define-key "fundamental" "ctrl-x a -" 'abbrev-prefix-mark)

;; --- abbrev-mode: the buffer-local minor mode ---------------------------------
;; The Emacs `abbrev-mode` minor mode (Macros has no minor-mode framework yet
;; — this follows spell.scm's hand-rolled shape). Like Emacs, the mode is a
;; buffer-local value with a global default:
;;   * `abbrev-mode` (M-x) toggles the CURRENT buffer, recording an explicit
;;     value that shadows the default (a buffer-local variable write);
;;   * `set-abbrev-mode-default` sets the default — #t enables the mode in
;;     every buffer that has no explicit toggle (Emacs's "Customize this
;;     variable to non-nil to enable Abbrev mode by default in all buffers").
;; Hooks register while the mode is live anywhere (default on, or any buffer
;; explicitly on) and come down when it is not; the mode-line slot names the
;; focused buffer's state.

(define *abbrev-state* '())          ; buffer name -> explicit #t/#f, when set
(define *abbrev-mode-default* #f)    ; the global default value
(define *abbrev-capf-registered* #f)
(define *abbrev-capf-min-prefix* 2)  ; decline prefixes shorter than this
(define (set-abbrev-capf-min-prefix n) (set! *abbrev-capf-min-prefix* n))

;; An absent per-buffer entry inherits the default, like a buffer-local
;; variable with no local value. Explicit values are stored WRAPPED in a list
;; — buffer-state-get cannot tell a stored #f from "no entry", so the wrapper
;; preserves the tri-state (default / explicitly on / explicitly off).
(define (abbrev--on?)
  (let ((e (alist-ref *abbrev-state* (current-buffer))))
    (if e (car e) *abbrev-mode-default*)))
(define (abbrev--on-set! v)
  (set! *abbrev-state* (alist-set *abbrev-state* (current-buffer) (list v))))

;; Keep the edit hook registered exactly while the mode is live somewhere.
(define (abbrev--update-hook)
  (if (or *abbrev-mode-default*
          (not (null? (filter (lambda (e) (car (cdr e))) *abbrev-state*))))
      (add-hook "after-change-functions" "abbrev--auto-expand")
      (remove-hook "after-change-functions" "abbrev--auto-expand")))

(define (set-abbrev-mode-default v)
  (set! *abbrev-mode-default* v)
  (abbrev--update-hook)
  (abbrev--update-modeline))

;; Mode-line lighter (a slot: names the focused buffer, the same trade
;; spell-mode makes until per-buffer lighters exist).
(mode-line-add-segment (ml-slot* "abbrev" "" "  " ""))
(define (abbrev--update-modeline)
  (set-mode-line-slot "abbrev" (if (abbrev--on?) "abbrev" "")))
(define (abbrev--buffer-switch) (abbrev--update-modeline))
(add-hook "buffer-switch-hook" "abbrev--buffer-switch")

;; Completes a prefix against the full table chain (local, :parents, global —
;; the same resolution abbrev-symbol uses), earlier tables shadowing later.
;; Candidates are (text annotation) pairs — annotation = the expansion.
;; Flag-gated (there is no remove-completion-at-point-function primitive):
;; the CAPF registers once, on first enable, and declines with '() in
;; buffers where the mode is off. Registration order = priority order, so a
;; user who wants abbrevs before dabbrev enables abbrev-mode early in
;; init.scm.
(define (abbrev-capf prefix)
  (if (or (not (abbrev--on?))
          (< (string-length prefix) *abbrev-capf-min-prefix*))
      '()
      (let outer ((tables (abbrev--table-chain (major-mode))) (out '()))
        (if (null? tables)
            (reverse out)
            (let inner ((entries (hash->list (abbrev--get-table (car tables))))
                        (out out))
              (if (null? entries)
                  (outer (cdr tables) out)
                  (let ((a (car (car entries))) (entry (cdr (car entries))))
                    (if (and (abbrev--prefix? prefix a)
                             (not (string=? prefix a))
                             (not (assoc a out)))
                        (inner (cdr entries)
                               (cons (list a (cdr (assoc 'expansion entry))) out))
                        (inner (cdr entries) out)))))))))

(define (abbrev--prefix? prefix s)
  ;; steel-core has no string-prefix? — hand-rolled substring compare.
  (and (>= (string-length s) (string-length prefix))
       (string=? prefix (substring s 0 (string-length prefix)))))

;; Typing-time expansion (Emacs abbrev-mode semantics): after each change,
;; when the character just inserted is NOT a word character (space, punct,
;; newline, …), the word ending just before it is expanded — silently, and
;; the terminator stays. Runs only in buffers where the mode is on; cost is
;; one flag read per change plus, on a real trigger, one windowed substring
;; read and hash lookup. Follows the primary cursor; a grammar-less buffer
;; just never passes the :ts-context check.
;;
;; Re-entrancy guard (Emacs's abbrev-start-location). Editor::edit runs
;; after-change-functions on EVERY applied edit — including each half of the
;; delete-region+insert pair an expansion queues, and including plain
;; deletions. So the guard is a persistent record (start exp buffer), not a
;; one-shot flag:
;;   * the DELETE half of our own expansion re-fires with point on the mark —
;;     stand down, and KEEP the guard armed for the insert half;
;;   * the INSERT half (or anything later that finds the expansion text still
;;     sitting exactly where we put it) stands down too — re-expanding would
;;     be a no-op edit that re-queues the same pair forever (`etc` -> `etc.`);
;;   * any other fire just fails the match and expands normally.
;; Disarming the guard from the hook was tried and is wrong: the delete fire
;; clears it before the insert fire arrives, reopening the loop.
(define *abbrev--auto-guard* #f)   ; (start-byte expansion buffer)

;; #t when GUARD matches the current buffer and its expansion text sits at
;; its recorded start — the signature of our own queued insert.
(define (abbrev--guard-in-place? guard)
  (and guard
       (string=? (nth guard 2) (current-buffer))
       (let ((end (+ (nth guard 0) (utf8-length (nth guard 1)))))
         (and (<= end (point-max))
              (string=? (nth guard 1)
                        (buffer-substring (nth guard 0) end))))))

(define (abbrev--auto-expand)
  (when (and (abbrev--on?) (> (point) 1))
    (let ((guard *abbrev--auto-guard*))
      ;; Only insertions expand. `(last-change)` is the Emacs
      ;; `(beg end old-len)` triple of the edit that fired this hook; a
      ;; deletion or replacement (old-len > 0 — undo, kill, our own queued
      ;; DeleteRegion) must not expand, however terminator-like the buffer
      ;; happens to look at point now.
      (when (= (nth (last-change) 2) 0)
        ;; The delete half of our own expansion re-fires with point on the
        ;; mark, expansion text gone. Stand down entirely — the insert half
        ;; fires next and must still find the guard armed.
        (unless (and guard
                     (= (point) (nth guard 0))
                     (string=? (nth guard 2) (current-buffer)))
          ;; The terminator is the last CHAR before point — read a small byte
          ;; window (buffer-substring snaps to char boundaries) and take its
          ;; last character, so multi-byte terminators (`é`, `…`) never slice
          ;; badly.
          (let* ((tail (reverse (string->list
                                 (buffer-substring (max 0 (- (point) 4)) (point)))))
                 (term (and (pair? tail) (car tail))))
            (when (and term (not (abbrev--word-char? (string term))))
              (let* ((end (- (point) (char-bytes term)))
                     (wb (abbrev--trailing-word-at end)))
                (when wb
                  (let* ((word (cdr wb))
                         (start (car wb))
                         (entry (abbrev-symbol word)))
                    (when (and (not (abbrev--guard-in-place? guard))
                               entry
                               (let ((pred (cdr (assoc 'enable-function entry))))
                                 (not (and pred (not (abbrev--call-pred pred word)))))
                               (abbrev--ts-context-ok entry start end))
                      (let ((exp (abbrev--apply-case
                                  word
                                  (cdr (assoc 'expansion entry))
                                  entry)))
                        (set! *abbrev--auto-guard*
                              (list start exp (current-buffer)))
                        (abbrev--expand-at start end word exp
                                           (cdr (assoc 'hook entry)))))))))))))))

(define (abbrev-mode)
  (if (abbrev--on?)
      (begin
        (abbrev--on-set! #f)
        (message "abbrev-mode disabled"))
      (begin
        (unless *abbrev-capf-registered*
          (set! *abbrev-capf-registered* #t)
          (add-completion-at-point-function "abbrev-capf"))
        (abbrev--on-set! #t)
        (message "abbrev-mode enabled")))
  (abbrev--update-hook)
  (abbrev--update-modeline))

(register-command 'abbrev-mode)

;; --- expand-on-insert-exit ---------------------------------------------------
;; The `evil-want-abbrev-expand-on-insert-exit` analog: while enabled, leaving
;; insert mode (escape) expands the abbrev before point — silently. Mechanism:
;; `escape` in the evil/helix INSERT keymaps is rebound to a wrapper here
;; (never editing evil.scm/helix.scm); the state-exit command is invoked by
;; 'symbol name through eval-string, so this stays load-order safe no matter
;; where evil.scm/helix.scm land in the startup graph.
;; Disabling RESTORES the bindings that were in place before enabling
;; (key-binding), so a user's own escape binding is never overwritten — when
;; nothing was bound, the plain state-exit command goes back.

(define *abbrev-expand-on-insert-exit* #f)
;; ((mode . command) …) — the escape bindings captured on enable.
(define *abbrev--saved-escape* '())

;; Remember the current escape binding of MODE (an insert keymap), falling
;; back to DEFAULT when nothing is bound there yet.
(define (abbrev--save-escape mode default)
  (let ((cmd (key-binding mode "escape")))
    (set! *abbrev--saved-escape*
          (cons (cons mode (if (string=? cmd "") default cmd))
                (filter (lambda (b) (not (string=? (car b) mode)))
                        *abbrev--saved-escape*)))))

(define (set-abbrev-expand-on-insert-exit v)
  (set! *abbrev-expand-on-insert-exit* v)
  (if v
      (begin
        (abbrev--save-escape "evil-insert" "evil-normal-state")
        (abbrev--save-escape "helix-insert" "helix-normal-state")
        (bind-key "evil-insert" "escape" 'abbrev--evil-insert-exit)
        (bind-key "helix-insert" "escape" 'abbrev--helix-insert-exit))
      (begin
        (for-each (lambda (b) (bind-key (car b) "escape" (cdr b)))
                  *abbrev--saved-escape*)
        (set! *abbrev--saved-escape* '()))))

;; Expansion core shared with expand-abbrev: full predicate chain (case rules,
;; :enable-function, :ts-context) + hook, but SILENT — escape should not nag.
;; #t when an expansion happened.
(define (abbrev--maybe-expand-word)
  (let ((wb (abbrev--word-before)))
    (if (not wb)
        #f
        (let ((entry (abbrev-symbol (cdr wb))))
          (cond ((not entry) #f)
                ((let ((pred (cdr (assoc 'enable-function entry))))
                   (and pred (not (abbrev--call-pred pred (cdr wb)))))
                 #f)
                ((not (abbrev--ts-context-ok entry (car wb) (point)))
                 #f)
                (else
                 (abbrev--expand-at
                  (car wb) (point)
                  (cdr wb)
                  (abbrev--apply-case (cdr wb)
                                      (cdr (assoc 'expansion entry))
                                      entry)
                  (cdr (assoc 'hook entry)))
                 #t))))))

;; Shared insert-exit body: expand, then fall through to the state's own exit
;; command (by name — see the load-order note above).
(define (abbrev--insert-exit-common exit-name)
  (when *abbrev-expand-on-insert-exit*
    (abbrev--maybe-expand-word))
  (eval-string (string-append "(" exit-name ")")))

(define (abbrev--evil-insert-exit) (abbrev--insert-exit-common "evil-normal-state"))
(define (abbrev--helix-insert-exit) (abbrev--insert-exit-common "helix-normal-state"))

;; --- persistence -------------------------------------------------------------
;; The abbrev file is Scheme data: exactly the (define-abbrev …) forms that
;; rebuild the tables — so `read-abbrev-file` is just `load`, and any string
;; (quotes, newlines, tabs, any control char) round-trips through the escaped
;; literal. The file is read back on after-init-hook and rewritten on exit;
;; the in-memory tables are the source of truth, and re-reading replaces
;; entries (hash-insert semantics), never duplicates.
;; It is written only when at least one savable abbrev exists — an all-system
;; or all-anonymous table serializes to just ;; comments, and that is not a
;; save. Skipped system redefinitions (a user abbrev winning) announce
;; themselves in the echo area rather than failing silently.
;;
;; Round-trips: expansions, 'symbol hooks, prop plists, :parents (emitted as a
;; define-abbrev-table form). Does NOT round-trip: anonymous procedures (a
;; lambda hook/predicate) — the entry is skipped with a ;; comment naming the
;; abbrev; keep those definitions in init.scm.

(define *abbrev-file-name* "")   ; default set at the bottom of this section
(define *save-abbrevs* 'silent)  ; 'silent — auto-save on exit, no messages

;; Scheme string literal escaping for the serialized forms: backslash and
;; quote, then ANY control char (<32 — \n, \t, \r, …) uniformly as a \u{hex};
;; escape (steel's lexer reads \u{…} in string literals), so every control
;; character round-trips.
(define (abbrev--escape s)
  (let ((unquoted (string-replace
                   (string-replace s "\\" "\\\\")
                   "\"" "\\\"")))
    (let loop ((cs (string->list unquoted)) (out (list)))
      (cond ((null? cs) (list->string (reverse out)))
            ((< (char->integer (car cs)) 32)
             (loop (cdr cs)
                   (append (reverse (string->list
                                     (string-append "\\u{"
                                                    (number->string
                                                     (char->integer (car cs)) 16)
                                                    "};")))
                           out)))
            (else (loop (cdr cs) (cons (car cs) out)))))))

;; Prop fragments: " :key value" pieces; only truthy props are emitted.
;; Anonymous procedures are not serializable — the whole entry is skipped.
(define (abbrev--prop-frag key val)
  (cond ((eq? val #t) (string-append " :" (symbol->string key) " #t"))
        ((symbol? val) (string-append " :" (symbol->string key) " '"
                                      (symbol->string val)))
        ((string? val) (string-append " :" (symbol->string key) " \""
                                      (abbrev--escape val) "\""))
        (else "")))

;; One (define-abbrev …) form for an entry, or #f when it can't round-trip.
;; System abbrevs are never serialized (Emacs rule: the abbrev file holds only
;; user abbrevs) — the caller leaves a ;; comment naming them.
(define (abbrev--entry-form tname name entry)
  (let ((exp (cdr (assoc 'expansion entry)))
        (hook (cdr (assoc 'hook entry)))
        (enable (cdr (assoc 'enable-function entry))))
    (cond ((cdr (assoc 'system entry)) #f)  ; system abbrev — never saved
          ((or (and hook (procedure? hook))
               (and enable (procedure? enable)))
           #f)  ; anonymous procedure — caller leaves a ;; comment
          (else
           (let* ((props (string-append
                          (if enable (abbrev--prop-frag 'enable-function enable) "")
                          (if (cdr (assoc 'case-fixed entry))
                              (abbrev--prop-frag 'case-fixed #t)
                              "")
                          (let ((ts (cdr (assoc 'ts-context entry))))
                            (if ts (abbrev--prop-frag 'ts-context ts) ""))))
                  (props-arg (if (> (string-length props) 0)
                                 (string-append " '(" props ")")
                                 ""))
                  (hook-arg (cond ((not hook) "#f")
                                  ((symbol? hook)
                                   (string-append "'" (symbol->string hook)))
                                  (else "#f"))))
             (string-append "(define-abbrev '" tname
                            " \"" (abbrev--escape name) "\""
                            " \"" (abbrev--escape exp) "\""
                            " " hook-arg props-arg ")"))))))

;; (define-abbrev-table 'name '() #f '(:parents '(…))) for :parents declarations.
(define (abbrev--parents-forms)
  (let loop ((ts (hash->list *abbrev-table-parents*)) (out '()))
    (if (null? ts)
        (apply string-append (reverse out))
        (let* ((tname (car (car ts)))
               (parents (cdr (car ts)))
               (names (apply string-append
                             (map (lambda (p)
                                    (string-append " '" p))
                                  parents))))
          (loop (cdr ts)
                (cons (string-append "(define-abbrev-table '"
                                     tname " '() #f '(:parents '(" names
                                     ")))\n")
                      out))))))

;; The whole abbrev file: parents declarations, then one form per entry.
;; Entries with anonymous hooks/predicates become ;; comments (see above).
(define (abbrev--serialize)
  (let loop ((ts (hash->list *abbrev-tables*)) (out '()))
    (if (null? ts)
        (string-append (abbrev--parents-forms)
                       (apply string-append (reverse out)))
        (let* ((tname (car (car ts)))
               (entries (hash->list (cdr (car ts))))
               (forms (let inner ((es entries) (acc '()))
                        (if (null? es)
                            (reverse acc)
                            (let ((name (car (car es)))
                                  (entry (cdr (car es))))
                              (let ((form (abbrev--entry-form tname name entry)))
                                (inner (cdr es)
                                       (cons (if form
                                                 (string-append form "\n")
                                                 (if (cdr (assoc 'system entry))
                                                     (string-append ";; system abbrev (not saved): "
                                                                    tname " " name "\n")
                                                     (string-append ";; not saved (anonymous procedure): "
                                                                    tname " " name "\n")))
                                             acc))))))))
          (loop (cdr ts) (append forms out))))))

;; Silent internal saver — never messages (the 'silent contract). No file is
;; written when *save-abbrevs* is #f or nothing savable exists: a serialization
;; of only ;; comments (all-system / all-anonymous tables) is not a save.
(define (abbrev--save)
  (when *save-abbrevs*
    (let ((s (abbrev--serialize)))
      (when (and (> (string-length s) 0)
                 (string-contains? s "(define-abbrev"))
        (write-file *abbrev-file-name* s)))))

;; User-facing explicit commands.
(define (write-abbrev-file . file)
  (write-file (if (null? file) *abbrev-file-name* (car file))
              (abbrev--serialize)))

;; (read-abbrev-file [path]) — the file is Scheme data, so reading is loading.
(define (read-abbrev-file . file)
  (load (if (null? file) *abbrev-file-name* (car file))))

(define (set-abbrev-file-name path) (set! *abbrev-file-name* path))
(define (set-save-abbrevs val) (set! *save-abbrevs* val))

;; after-init-hook: the init phase (bundled scripts + init.scm) is over —
;; defines from here on are user abbrevs — and the saved abbrev file is read
;; back, Emacs's abbrev_defs style. Reading replaces entries (hash-insert
;; semantics), so re-loading never duplicates; init-time defines were marked
;; :system, so they don't appear in the file in the first place.
(define (abbrev--after-init)
  (set! *abbrev--init-phase* #f)
  (when (file-exists? *abbrev-file-name*)
    (read-abbrev-file)))

(add-hook "kill-emacs-hook" "abbrev--save")
(add-hook "after-init-hook" "abbrev--after-init")

;; Default location; init.scm can override.
(set! *abbrev-file-name*
      (string-append (getenv "HOME") "/.config/macros/abbrevs.scm"))

(register-command 'write-abbrev-file)
(register-command 'read-abbrev-file)
