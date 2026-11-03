;; requires: core evil modeline minor-mode

;; spell.scm — flyspell-style live spell checking with squiggly underlines.
;;
;; No dictionaries or checkers are bundled. Two backends, probed in order the
;; first time spell-mode is enabled:
;;   * 'native — the OS spell service (macOS NSSpellChecker): the system
;;     dictionaries, the user's learned words, automatic language detection.
;;     Nothing to install.
;;   * 'pipe — a persistent `aspell -a` subprocess speaking the classic
;;     ispell protocol, for platforms without an OS checker (`brew install
;;     aspell` / `apt install aspell`). hunspell and enchant speak the same
;;     protocol — point at them with (spell-set-program ...).
;;
;; `M-x spell-mode` toggles checking for the current buffer. While on, the
;; visible region is rechecked (debounced) after every edit and misspellings
;; are painted with the "spell-error" face (wavy underline). Evil normal mode:
;;   ] s  spell-next-error      [ s  spell-prev-error
;;   z =  spell-correct-word    z g  spell-learn-word-at-point
;;
;; From ~/.config/macros/init.scm:
;;   (spell-auto-enable '(".org" ".md" ".txt"))   ; spell-mode on at find-file
;;   (spell-set-delay 600)                        ; recheck debounce (ms)
;;   (spell-set-program "hunspell" '("-a"))       ; alternative pipe checker

;; --- faces --------------------------------------------------------------------
;; Defaults so squiggles work even if the active theme predates the face; the
;; bundled themes re-declare both (load-theme wipes all face attributes).
(define (spell--register-faces)
  (set-face "spell-error" "#f14c4c")
  (set-face-wavy "spell-error" #t))
(spell--register-faces)

;; --- state ----------------------------------------------------------------------
(define *spell-backend* #f)           ; 'native | 'pipe, resolved on first enable
(define *spell-delay* 400)            ; recheck debounce (ms)
(define *spell-errors* '())           ; ((start end word) ...) absolute bytes, ascending
(define *spell-errors-buffer* "")     ; buffer the ranges belong to
(define *spell-suggest-cache* (hash)) ; word -> suggestions, from the last pipe pass
(define *spell-auto-exts* '())        ; find-file auto-enable extensions
(define *spell-ignored* '())          ; words accepted for this session (both backends)

;; On/off is the buffer's mode stack (spell-mode is a real stack entry). Reads
;; the PRE-command snapshot — see the caveat in minor-mode.scm — so code that
;; runs in the same command as the toggle must not gate on it.
(define (spell--on?) (minor-mode-active? 'spell-mode))

;; The cached error list, valid only for the buffer it was computed in.
(define (spell--errors)
  (if (equal? *spell-errors-buffer* (current-buffer)) *spell-errors* '()))
;; Store a fresh error list, dropping session-ignored words — the single
;; choke point through which every backend's results flow.
(define (spell--set-errors errors)
  (set! *spell-errors*
        (filter (lambda (e) (not (member (nth e 2) *spell-ignored*))) errors))
  (set! *spell-errors-buffer* (current-buffer)))

;; --- configuration ----------------------------------------------------------------
(define *spell-program* "aspell")
(define *spell-program-args* '("-a" "--encoding=utf-8"))

;; Use a different ispell-protocol checker, e.g. (spell-set-program "hunspell" '("-a")).
(define (spell-set-program program args)
  (set! *spell-program* program)
  (set! *spell-program-args* args)
  (spell--pipe-stop))

(define (spell-set-delay ms) (set! *spell-delay* ms))

;; Turn spell-mode on automatically when a file with one of these extensions
;; is opened: (spell-auto-enable '(".org" ".md" ".txt"))
(define (spell-auto-enable exts) (set! *spell-auto-exts* exts))

;; --- backend probe --------------------------------------------------------------
(define (spell--pipe-program-exists?)
  (> (string-length
      (call-process "sh" (list "-c" (string-append "command -v " (shell-quote *spell-program*)))))
     0))

(define (spell--resolve-backend)
  (cond ((spell-native-available?) 'native)
        ((spell--pipe-program-exists?) 'pipe)
        (else #f)))

;; --- painting ---------------------------------------------------------------------
(define (spell--repaint)
  (clear-overlay-faces-matching "spell-error")
  ;; One batched overlay-set-faces call — a prose buffer can carry hundreds of
  ;; squiggles, and per-span overlay-set-face calls dominated the repaint.
  (overlay-set-faces
    (map (lambda (e) (list (nth e 0) (nth e 1) "spell-error")) (spell--errors))))

;; Cache and paint a backend's finished result set, whose byte offsets were
;; computed against `buffer` at edit tick `tick` — both captured when the text
;; was SENT for checking, an eval (and a subprocess round-trip) ago. If the
;; buffer has been edited or switched since, the offsets are stale: caching or
;; painting them would land squiggles shifted off their words, so skip and
;; recheck once the current burst settles. When the reply IS fresh, the
;; guard-buffer-tick covers the last gap — an edit between this eval and the
;; moment its overlay commands apply makes Rust drop the paint whole and
;; dispatch `spell--schedule` to reschedule.
(define (spell--apply-results errors tick buffer)
  (if (and (equal? buffer (current-buffer)) (= tick (buffer-tick)))
      (begin
        (guard-buffer-tick buffer tick "spell--schedule")
        (spell--set-errors errors)
        (spell--repaint))
      (spell--schedule)))

;; Byte range of the lines currently on screen.
(define (spell--visible-region)
  (let ((vl (visible-lines)))
    (cons (position->byte (car vl) 0)
          (position->byte (+ (cadr vl) 1) 0))))

;; --- native backend (NSSpellChecker via spell-check-text) --------------------------
;; Requests are answered asynchronously in order, so a FIFO of
;; (region-start edit-tick buffer) entries pairs each reply with the offsets —
;; and the exact buffer text — it was computed against.
(define *spell-native-fifo* '())

(define (spell--check-native start text)
  (set! *spell-native-fifo*
        (append *spell-native-fifo*
                (list (list start (buffer-tick) (current-buffer)))))
  (spell-check-text text "spell--native-apply"))

;; Callback: "start\tend\tword" rows, byte offsets relative to the checked text.
(define (spell--native-apply rows)
  (unless (null? *spell-native-fifo*)
    (let* ((entry (car *spell-native-fifo*))
           (base (nth entry 0)))
      (set! *spell-native-fifo* (cdr *spell-native-fifo*))
      (spell--apply-results
       (map (lambda (row)
              (let ((p (split-many row "\t")))
                (list (+ base (string->number (nth p 0)))
                      (+ base (string->number (nth p 1)))
                      (nth p 2))))
            (non-empty-lines rows))
       (nth entry 1) (nth entry 2)))))

;; --- pipe backend (aspell/hunspell/enchant -a) --------------------------------------
;; One persistent process; each buffer line is sent `^`-escaped, and the
;; checker answers with zero or more `&`/`#` records then a blank terminator
;; per line. A FIFO of (line-start line-text final? edit-tick buffer) entries
;; pairs replies with the line they describe; `final?` marks a region's last
;; line, whose terminator triggers the repaint (tick + buffer let it prove the
;; offsets still describe the current text — see spell--apply-results).
(define *spell-proc* #f)
(define *spell-pipe-buf* "")
(define *spell-pipe-fifo* '())
(define *spell-pipe-acc* '())

(define (spell--pipe-ensure)
  (unless *spell-proc*
    (set! *spell-pipe-buf* "")
    (set! *spell-pipe-fifo* '())
    (set! *spell-pipe-acc* '())
    (set! *spell-proc*
          (make-process *spell-program* *spell-program-args*
                        "spell--pipe-output" "spell--pipe-exit"))
    ;; Terse mode: report only misspellings (no per-word `*` acks).
    (process-send-string *spell-proc* "!\n"))
  *spell-proc*)

(define (spell--pipe-stop)
  (when *spell-proc* (kill-process *spell-proc*))
  (set! *spell-proc* #f))

(define (spell--pipe-exit code) (set! *spell-proc* #f))

(define (spell--check-pipe start text)
  (let ((id (spell--pipe-ensure))
        (tick (buffer-tick))
        (buf (current-buffer)))
    (when (null? *spell-pipe-fifo*) (set! *spell-pipe-acc* '()))
    (let loop ((ls (split-many text "\n")) (off start))
      (unless (null? ls)
        (let ((line (car ls)))
          (set! *spell-pipe-fifo*
                (append *spell-pipe-fifo*
                        (list (list off line (null? (cdr ls)) tick buf))))
          (process-send-string id (string-append "^" line "\n"))
          (loop (cdr ls) (+ off (utf8-length line) 1)))))))

;; Stdout arrives in arbitrary chunks; keep the trailing partial line buffered.
(define (spell--pipe-output chunk)
  (set! *spell-pipe-buf* (string-append *spell-pipe-buf* chunk))
  (let* ((parts (split-many *spell-pipe-buf* "\n"))
         (n (length parts)))
    (when (> n 1)
      (set! *spell-pipe-buf* (nth parts (- n 1)))
      (let loop ((k 0))
        (when (< k (- n 1))
          (spell--pipe-line (nth parts k))
          (loop (+ k 1)))))))

(define (spell--pipe-line line)
  (cond
    ;; Version greeting, printed once at startup.
    ((starts-with? line "@") #f)
    ((or (starts-with? line "& ") (starts-with? line "# "))
     (spell--pipe-record line))
    ;; Blank terminator: the line at the head of the FIFO is fully answered.
    ((equal? line "")
     (unless (null? *spell-pipe-fifo*)
       (let ((entry (car *spell-pipe-fifo*)))
         (set! *spell-pipe-fifo* (cdr *spell-pipe-fifo*))
         (when (nth entry 2)
           (let ((acc (reverse *spell-pipe-acc*)))
             (set! *spell-pipe-acc* '())
             (spell--apply-results acc (nth entry 3) (nth entry 4)))))))
    (else #f)))

;; Parse one ispell record against the line context `(line-start line-text _)`:
;;   "& <word> <count> <offset>: <s1>, <s2>, ..."   (with suggestions)
;;   "# <word> <offset>"                            (none)
;; The offset is a CHARACTER position in the sent line and counts the `^`
;; escape prefix, so `offset - 1` chars precede the word in the real line
;; text; convert to bytes for the overlay (never index the buffer by chars).
(define (spell--pipe-parse record entry)
  (let* ((line-start (nth entry 0))
         (line-text (nth entry 1))
         (parts (split-many record ": "))
         (fields (split-many (car parts) " "))
         (word (nth fields 1))
         (char-off (string->number (nth fields (- (length fields) 1))))
         (prefix-chars (min (max 0 (- char-off 1)) (string-length line-text)))
         (start (+ line-start (utf8-length (substring line-text 0 prefix-chars)))))
    (list start (+ start (utf8-length word)) word
          (if (> (length parts) 1)
              (split-many (string-join (cdr parts) ": ") ", ")
              '()))))

(define (spell--pipe-record record)
  (unless (null? *spell-pipe-fifo*)
    (let* ((parsed (spell--pipe-parse record (car *spell-pipe-fifo*)))
           (word (nth parsed 2))
           (suggestions (nth parsed 3)))
      (unless (null? suggestions)
        (set! *spell-suggest-cache*
              (hash-insert *spell-suggest-cache* word suggestions)))
      (set! *spell-pipe-acc*
            (cons (list (nth parsed 0) (nth parsed 1) word) *spell-pipe-acc*)))))

;; --- checking -------------------------------------------------------------------
(define (spell--check-now)
  (when (spell--on?) (spell--check-visible)))

;; Unguarded: the enable path calls this directly, since its own snapshot
;; still says the mode is off.
(define (spell--check-visible)
  (let* ((region (spell--visible-region))
         (start (car region))
         (end (cdr region)))
    (when (> end start)
      (let ((text (buffer-substring start end)))
        (cond ((eq? *spell-backend* 'native) (spell--check-native start text))
              ((eq? *spell-backend* 'pipe) (spell--check-pipe start text))
              (else #f))))))

(define spell--debounce
  (make-debounce "spell" 400 (lambda () (when (spell--on?) (spell--check-now)))))
(define (spell--schedule)
  (when (spell--on?) (spell--debounce *spell-delay*)))

;; Cursor-line motion (paging included) can reveal unchecked text.
(define *spell-last-visible* '(-1 -1))
(define (spell--follow-view)
  (when (spell--on?)
    (let ((vl (visible-lines)))
      (unless (equal? vl *spell-last-visible*)
        (set! *spell-last-visible* vl)
        (spell--schedule)))))

;; --- mode stack entry ------------------------------------------------------------
;; spell-mode is a real stack entry: the " spell" lighter renders from the mode
;; line, C-h m lists it, and it survives a major-mode switch. Registered here
;; with the raw primitive, not `define-minor-mode` — see spell-mode below.
(register-minor-mode-def "spell-mode" " spell" #f)

;; The error cache is per-buffer; a switch rechecks the newly shown buffer
;; (a no-op where the mode is off).
(define (spell--buffer-switch)
  (spell--schedule))

;; --- the minor mode -----------------------------------------------------------------
;; The per-edit and per-motion hooks are registered on FIRST enable, not at
;; load: spell-mode is off by default, and an unregistered hook costs nothing
;; (registered, it dispatches to the engine on every edit even when the guard
;; then no-ops).
(define *spell-hooks-registered* #f)
(define (spell--ensure-hooks)
  (unless *spell-hooks-registered*
    (set! *spell-hooks-registered* #t)
    (add-hook "after-change-functions" "spell--schedule")
    (add-hook "post-command-hook" "spell--follow-view")))

;; Not `define-minor-mode`: its body runs BEFORE the stack push is queued and
;; cannot veto it, but spell-mode must refuse to turn on when no checker
;; backend exists. So the toggle calls enable-minor-mode / disable-minor-mode
;; itself, only once it knows the flip will happen, and keeps its own
;; messages. The editor fires spell-mode-hook from those primitives, as for
;; any minor mode.
(define-command (spell-mode)
  (if (spell--on?)
      (begin
        (disable-minor-mode "spell-mode")
        (spell--set-errors '())
        (clear-overlay-faces-matching "spell-error")
        (message "spell-mode off"))
      (begin
        (unless *spell-backend* (set! *spell-backend* (spell--resolve-backend)))
        (if (not *spell-backend*)
            (message (string-append "spell-mode: no checker — install aspell, or "
                                    "(spell-set-program \"hunspell\" '(\"-a\"))"))
            (begin
              (spell--ensure-hooks)
              (enable-minor-mode "spell-mode")
              (spell--check-visible)
              (message (string-append "spell-mode on ("
                                      (symbol->string *spell-backend*) ")")))))))

(define (spell--maybe-auto-enable)
  (when (and (not (null? *spell-auto-exts*)) (not (spell--on?)))
    (when (ends-with-any? (buffer-path-or-name) *spell-auto-exts*)
      (spell-mode))))

;; --- navigation -----------------------------------------------------------------
(define (spell--error-after pos)
  (let loop ((es (spell--errors)))
    (cond ((null? es) #f)
          ((> (car (car es)) pos) (car es))
          (else (loop (cdr es))))))

(define (spell--error-before pos)
  (let loop ((es (spell--errors)) (best #f))
    (cond ((null? es) best)
          ((< (car (car es)) pos) (loop (cdr es) (car es)))
          (else best))))

;; Native fallback for ]s past the cached (visible) errors: scan the rest of
;; the buffer on demand — the whole-buffer "keep hitting next" flow. Skips
;; session-ignored words (the OS scan doesn't know about them).
(define *spell-next-base* 0)
(define (spell--native-next-apply rows)
  (let loop ((lines (non-empty-lines rows)))
    (if (null? lines)
        (message "No more spelling errors")
        (let ((p (split-many (car lines) "\t")))
          (if (member (nth p 2) *spell-ignored*)
              (loop (cdr lines))
              (goto-char (+ *spell-next-base* (string->number (nth p 0)))))))))

(define-command (spell-next-error)
  (let ((e (spell--error-after (point))))
    (cond (e (goto-char (nth e 0)))
          ((eq? *spell-backend* 'native)
           (let ((from (min (point-max) (+ (point) 1))))
             (set! *spell-next-base* from)
             (spell-check-text (buffer-substring from (point-max))
                               "spell--native-next-apply")))
          (else (message "No more spelling errors")))))

(define-command (spell-prev-error)
  (let ((e (spell--error-before (point))))
    (if e
        (goto-char (nth e 0))
        (message "No previous spelling error"))))

;; --- corrections ----------------------------------------------------------------
(define (spell--error-at pos)
  (let loop ((es (spell--errors)))
    (cond ((null? es) #f)
          ((and (<= (car (car es)) pos) (< pos (nth (car es) 1))) (car es))
          (else (loop (cdr es))))))

;; The word to operate on: the cached error under point, else the plain word
;; at point (so z= / zg work even before a check pass has run).
(define (spell--word-range)
  (let ((e (spell--error-at (point))))
    (if e (cons (nth e 0) (nth e 1)) (evil-word-at-point))))

;; Add `word` to the permanent dictionary (backend-appropriate).
(define (spell--learn word)
  (if (eq? *spell-backend* 'native)
      (spell-learn-word word)
      ;; ispell protocol: `*word` adds to the personal dictionary,
      ;; `#` saves it to disk.
      (process-send-string (spell--pipe-ensure)
                           (string-append "*" word "\n#\n")))
  (message (string-append "Added to dictionary: " word))
  (spell--schedule))

;; Accept `word` for this session only: a Scheme-side ignore list filtered in
;; spell--set-errors, so it works identically on both backends.
(define (spell--ignore word)
  (unless (member word *spell-ignored*)
    (set! *spell-ignored* (cons word *spell-ignored*)))
  (spell--set-errors (spell--errors))
  (spell--repaint)
  (message (string-append "Ignoring: " word)))

;; Corrections as a Helm session: RET replaces the word with the selected
;; suggestion; M-a offers "add to dictionary" / "ignore" for the ORIGINAL
;; word (so a suggestion doesn't have to be picked to accept the spelling).
(define (spell--offer-corrections word start end suggestions)
  (if (null? suggestions)
      (message (string-append "No suggestions for " word))
      (helm (list (list (string-append "Corrections: " word)
                        suggestions
                        (list (list "Replace"
                                    (callback->name
                                     (lambda (choice)
                                       (replace-region start end choice)
                                       (spell--schedule))))
                              (list (string-append "Add \"" word "\" to dictionary")
                                    (callback->name (lambda (_) (spell--learn word))))
                              (list (string-append "Ignore \"" word "\" this session")
                                    (callback->name (lambda (_) (spell--ignore word))))))))))

;; Stash for the native suggestion round-trip (one interactive query at a time).
(define *spell-offer* #f)
(define (spell--suggest-apply rows)
  (when *spell-offer*
    (let ((offer *spell-offer*))
      (set! *spell-offer* #f)
      (offer (non-empty-lines rows)))))

(define-command (spell-correct-word)
  (let ((r (spell--word-range)))
    (if (not r)
        (message "No word at point")
        (let* ((start (car r))
               (end (cdr r))
               (word (buffer-substring start end)))
          (if (eq? *spell-backend* 'native)
              (begin
                (set! *spell-offer*
                      (lambda (suggestions)
                        (spell--offer-corrections word start end suggestions)))
                (spell-suggest word "spell--suggest-apply"))
              (let ((cached (hash-try-get *spell-suggest-cache* word)))
                (spell--offer-corrections word start end
                                          (if cached cached '()))))))))

;; Run `action` on the word at point, or complain if there isn't one.
(define (spell--with-word-at-point action)
  (let ((r (spell--word-range)))
    (if (not r)
        (message "No word at point")
        (action (buffer-substring (car r) (cdr r))))))

(define-command (spell-learn-word-at-point)
  (spell--with-word-at-point spell--learn))

(define-command (spell-ignore-word-at-point)
  (spell--with-word-at-point spell--ignore))

;; --- triggers / keys -------------------------------------------------------------
;; (after-change-functions and post-command-hook register on first enable —
;; see spell--ensure-hooks. These two are infrequent and stay cheap.)
(add-hook "buffer-switch-hook" "spell--buffer-switch")
(add-hook "find-file-hook" "spell--maybe-auto-enable")

;; Modal keys go on BOTH Normal maps: `evil-normal` alone left Helix users with
;; no spell keys at all (the two styles have separate keymaps — see
;; `evil_keymap`). `]s` / `[s` / `z=` / `zg` are the same in vim and Helix.
(for-each
 (lambda (km)
   (define-key km "] s" 'spell-next-error)
   (define-key km "[ s" 'spell-prev-error)
   (define-key km "z =" 'spell-correct-word)
   (define-key km "z g" 'spell-learn-word-at-point))
 (list "evil-normal" "helix-normal"))
