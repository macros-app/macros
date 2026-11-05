;; requires: core helm

;; help.scm — discoverability: an M-x that lists Scheme commands too, plus
;; describe-key / apropos / describe-bindings. Requires helm.scm so it loads
;; after it and its M-x rebinding wins.

;; Drop consecutive duplicates from a SORTED string list (one O(n) pass).
(define (help--dedup-sorted xs)
  (let loop ((xs xs) (acc '()))
    (cond ((null? xs) (reverse acc))
          ((and (pair? acc) (equal? (car xs) (car acc))) (loop (cdr xs) acc))
          (else (loop (cdr xs) (cons (car xs) acc))))))

;; A VARIABLE name: the Scheme global convention every bundled file follows,
;; `*like-this*`. function-list (the doc index) is every top-level define —
;; functions AND variables — so the pickers filter on this, not on the index.
(define (help--variable-name? n)
  (and (> (string-length n) 2) (starts-with? n "*") (ends-with? n "*")))

;; Function-picker candidates: everything in the doc index EXCEPT variables
;; (`*star*` globals) and editor options, both of which belong to
;; describe-variable / describe-option instead.
(define (function-candidates)
  (filter (lambda (n)
            (and (not (help--variable-name? n))
                 (not (member n (option-list)))))
          (function-list)))

;; All invocable command names: built-ins + registered Scheme commands. Deduped,
;; because a command can legitimately appear in BOTH lists — a Rust nullary
;; builtin that a Scheme file also `register-command`s — and M-x must list it once.
(define (all-commands)
  (help--dedup-sorted (sort-strings (append (command-list) *interactive-commands*))))

;; Run a command (built-in OR Scheme fn) by name — `(name)` via eval-string.
(define (execute-command name)
  (when (not (equal? name ""))
    (eval-string (string-append "(" name ")"))))

;; M-x: fuzzy-pick any command and run it.
(define (helm-execute-command)
  (helm (list (list "M-x" (all-commands) 'execute-command))))
(define-key "fundamental" "alt-x" 'helm-execute-command)

;; Every help command below searches `active-keymaps` (core.scm) — the modal
;; Normal map, when one is active, then `fundamental` — rather than assuming
;; "fundamental". Without it `C-h k j` answered "j is unbound" to a vim user and
;; `C-h b` never listed a single modal binding, leaving the whole modal keymap
;; undiscoverable from inside the editor.
;;
;; describe-key (C-h k): what does a KEY do? Like Emacs, the next keystroke(s)
;; are captured AS KEYS — press `C-x C-f`, don't type its name (the old
;; typed-name read left every real keypress swallowed while the prompt was
;; open, which read as "stuck"). A sequence that is still a prefix of some
;; binding (`C-x`) keeps capturing; one that resolves describes its command;
;; anything else reports "undefined". C-g disarms the capture (Rust does it
;; before delivering the key, so the collect callback sees it once).
(define *describe-key--seq* '())
(define (describe-key)
  (set! *describe-key--seq* '())
  (read-key-start "Describe key: " 'describe-key-collect))

;; The command bound to `seq` in the active keymaps (modal map first), or "".
(define (describe-key--binding seq)
  (let loop ((kms (active-keymaps)))
    (if (null? kms)
        ""
        (let ((hit (key-binding (car kms) seq)))
          (if (equal? hit "")
              (loop (cdr kms))
              hit)))))

;; Does some binding in the active keymaps EXTEND `seq` (a longer chord with
;; `seq` as a whole-token prefix)? `keys-with-prefix` matches raw string
;; prefixes — "c" would "extend" to "ctrl-x" — so compare against `seq + " "`:
;; an exact match was already handled by describe-key--binding above.
(define (describe-key--prefix? seq)
  (let ((pre (string-append seq " ")))
    (let loop ((kms (active-keymaps)))
      (cond ((null? kms) #f)
            ((let probe ((rows (keys-with-prefix (car kms) seq)))
               (cond ((null? rows) #f)
                     ((starts-with? (car rows) pre) #t)
                     (else (probe (cdr rows))))) #t)
            (else (loop (cdr kms)))))))

(define (describe-key-collect key)
  ;; Escape / C-g arrive here only to REPORT — Rust already disarmed the
  ;; capture before delivering them (see deliver_read_key), like Emacs aborting
  ;; the read rather than describing keyboard-quit. Reset and stay quiet.
  (if (or (equal? key "escape") (equal? key "ctrl-g"))
      (set! *describe-key--seq* '())
      (let ((seq (string-join (append *describe-key--seq* (list key)) " ")))
        (cond
          ((not (equal? (describe-key--binding seq) ""))
           (set! *describe-key--seq* '())
           (read-key-stop)
           (describe-key-finish seq))
          ((describe-key--prefix? seq)
           (set! *describe-key--seq* (append *describe-key--seq* (list key)))
           (message (string-append seq "-")))
          (else
           (set! *describe-key--seq* '())
           (read-key-stop)
           (message (string-append seq " is undefined")))))))

;; The typed-name variant ("ctrl-x ctrl-f"), kept for programmatic use.
;; Both paths land in a *Help* buffer (Emacs-style) rather than a passing
;; echo-area message: chord, command, keymap and the command's full doc.
(define (describe-key-finish chord)
  (let loop ((kms (active-keymaps)))
    (cond ((null? kms)
           (message (string-append chord " is unbound")))
          ((not (equal? (key-binding (car kms) chord) ""))
           (describe-key-show chord (key-binding (car kms) chord) (car kms)))
          (else (loop (cdr kms))))))

(define (describe-key-show chord cmd km)
  (let ((doc (function-doc cmd)))
    (render-buffer "*Help*" "help-mode"
                   (string-append
                    chord " runs " cmd " (" km ")\n\n"
                    (if (equal? doc "") "(no documentation)" doc))
                   (list (list 0 "tabulated-header")))))


;; First line of a function's doc (its source comment), or "".
(define (doc-summary name)
  (let ((doc (function-doc name)))
    (if (equal? doc "") "" (car (split-many doc "\n")))))

;; Every name apropos / helm-apropos searches: commands, Scheme functions and
;; variables, and editor options (so "font" surfaces the `font-size` option).
(define (all-names)
  (sort-strings (dedup (append (all-commands) (function-list) (option-list)))))

;; "name  —  one-line summary" for the apropos listing (bare name if undocumented).
(define (apropos-row n)
  (let ((s (doc-summary n)))
    (if (equal? s "") n (string-append n "  —  " s))))

;; Does `tok` occur in `hay`? Substring match, plus a cheap plural/singular
;; tolerance so a query word like "sizes" still finds the singular "size" in
;; "font-size" (and vice versa). Both args are expected already lower-cased.
(define (token-matches? tok hay)
  (or (string-contains? hay tok)
      (and (> (string-length tok) 3)
           (ends-with? tok "s")
           (string-contains? hay (substring tok 0 (- (string-length tok) 1))))))

;; #t when EVERY whitespace-separated word of `pat` occurs in `hay` — word order
;; and extra words don't matter (the Emacs `apropos` multi-keyword behaviour).
;; Case-insensitive. Blank tokens (from runs of spaces) are ignored.
(define (all-words-match? pat hay)
  (let ((h (string-downcase hay)))
    (define (go toks)
      (cond ((null? toks) #t)
            ((equal? (car toks) "") (go (cdr toks)))
            ((token-matches? (car toks) h) (go (cdr toks)))
            (else #f)))
    (go (split-many (string-downcase pat) " "))))

;; Names matching `pat` across their own text AND their doc-comment, treated as
;; one haystack so any word may match either. The shared engine behind both the
;; static `apropos` buffer and the live `helm-apropos` picker.
(define (apropos-matches pat)
  (filter (lambda (n) (all-words-match? pat (string-append n " " (function-doc n))))
          (all-names)))

;; apropos (C-h a): names OR docs containing a pattern → a help buffer.
(define (apropos) (read-string "Apropos: " 'apropos-finish))
(define (apropos-finish pat)
  (let ((matches (apropos-matches pat)))
    (render-buffer "*apropos*" "help-mode"
                   (if (null? matches)
                       (string-append "Nothing matching \"" pat "\"")
                       (string-append "Matches for \"" pat "\":\n\n"
                                      (join-lines (map apropos-row matches))))
                   '())))

;; describe-function (C-h f): pick a function in Helm — a vertical, type-to-
;; narrow list, each candidate annotated with its one-line doc (dimmed, right-
;; aligned). RET shows the doc; M-a → Jump to definition opens the source.
;; Completion ranges over every Scheme-defined function.
(define (describe-function)
  (helm (list (list "Functions"
                    (map apropos-pair (function-candidates))
                    (list (list "Describe"           'describe-function-show)
                          (list "Jump to definition" 'jump-to-definition))))))
(define (describe-function-show name)
  (when (not (equal? name ""))
    (if (member name (option-list))
        (describe-option name)
        (let ((doc (function-doc name)))
          (render-buffer "*help*" "help-mode"
                         (string-append name "\n\n"
                                        (if (equal? doc "") "(no documentation)" doc))
                         '())))))

;; A `get-option` result (string / number / boolean) as display text. Steel has
;; no generic `->string`, so dispatch on type.
(define (option-display v)
  (cond ((string? v) v)
        ((number? v) (number->string v))
        ((boolean? v) (if v "true" "false"))
        (else "")))

;; Options have no source to jump to; describe one instead — its current value
;; and how to set it. (`get-option` returns #false for an option left at its
;; built-in default.)
(define (describe-option name)
  (render-buffer "*help*" "help-mode"
                 (string-append
                  name "\n\n"
                  "Editor option.\n\n"
                  "Current value: " (option-display (get-option name)) "\n\n"
                  "Set it in your config:\n"
                  "  (set-option \"" name "\" <value>)\n\n"
                  "See the Configuration docs for every option and its type.")
                 '()))

;; A candidate as a `(name summary)` pair so Helm dims the one-line doc as a
;; right-aligned annotation; matching/the action still see the bare `name`.
;; Options are tagged "option" (with their value when set); a name with no doc
;; shows as itself.
(define (apropos-pair n)
  (cond
    ((member n (option-list))
     (list n (string-append "option · " (option-display (get-option n)))))
    (else (let ((s (doc-summary n)))
            (if (equal? s "") n (list n s))))))

;; jump-to-definition: open the source of `name` at its define and put point
;; there. `function-source` → `(file line)` (1-based) for anything in the doc
;; index — built-in or dynamically loaded package alike. When the file is absent
;; (an installed binary has no on-disk built-in sources) we fall back to showing
;; the documentation, the way Emacs does for a C-defined primitive.
(define (jump-to-definition name)
  (cond
    ((equal? name "") #f)
    ;; Options have no source line — describe (current value + how to set).
    ((member name (option-list)) (describe-option name))
    (else
     (let ((loc (function-source name)))
       (if (and (not (null? loc)) (file-exists? (car loc)))
           (begin
             (open-file (car loc))
             ;; function-source reports 1-based lines; the jump is 0-based.
             ;; open-file is async, so wait for the file's buffer to be
             ;; current (a bare goto-line would move the OLD buffer).
             (goto-when-visiting "" (car loc) (max 0 (- (cadr loc) 1)) 0))
           (describe-function-show name))))))

;; A LIVE (self-filtered) Helm source: type words from a name or doc-comment
;; ("font size") and the matching functions, variables, and options narrow as you
;; go, each annotated with its summary. >=2 chars so the first keystroke doesn't
;; list the whole world. RET jumps to the implementation (or describes an option);
;; M-a → Describe shows the doc.
(define (helm-apropos-candidates pattern)
  (if (< (string-length pattern) 2)
      '()
      (map apropos-pair (apropos-matches pattern))))

;; helm-apropos (C-h C-a): discoverability search over every loaded definition's
;; doc-comment — built-ins and dynamically loaded packages together.
(define (helm-apropos)
  (helm (list (list "Apropos"
                    (list 'raw "helm-apropos-candidates")
                    (list (list "Jump to definition" 'jump-to-definition)
                          (list "Describe"           'describe-function-show))))))
(define-key "fundamental" "ctrl-h ctrl-a" 'helm-apropos)

;; version (M-x version): show the running Macros version in the echo area.
;; `macros-version` is a Rust builtin returning the compiled-in package version.
(define (version)
  (message (string-append "Macros " (macros-version))))

;; describe-bindings (C-h B): every binding in effect — the active modal keymap
;; (if any) AND fundamental, each under its own heading. Listing only
;; `fundamental` hid every vim/Helix key from the one command whose whole job is
;; to show you what the keys do.
(define (help--binding-rows km)
  (map (lambda (l) (string-replace l "\t" "    "))
       (sort-strings (keys-with-prefix km ""))))
(define (help--binding-section km)
  (join-lines (cons (string-append "Key bindings (" km "):") (cons "" (help--binding-rows km)))))
(define (describe-bindings)
  (render-buffer "*bindings*" "help-mode"
                 (join-lines (map help--binding-section (active-keymaps)))
                 '()))

(define-key "help-mode" "q" 'kill-buffer)
(define-key "help-mode" "n" 'next-line)
(define-key "help-mode" "p" 'previous-line)

;; C-h help prefix.
(define-key "fundamental" "ctrl-h k" 'describe-key)
(define-key "fundamental" "ctrl-h f" 'describe-function)
(define-key "fundamental" "ctrl-h a" 'apropos)
;; `C-h B`, NOT `C-h b` — which-key.scm binds that (and loads later, so this was
;; dead). `C-h b` stays the which-key browser; `C-h B` is the plain dump.
(define-key "fundamental" "ctrl-h shift-b" 'describe-bindings)
(define-key "fundamental" "ctrl-h e" 'list-messages)   ; echo-area scrollback
(define-key "fundamental" "ctrl-h l" 'list-lsp-log)    ; language-server stderr
;; C-h t (help-with-tutorial) is defined in tutorial.scm, loaded after this.

(register-command 'describe-key)
(register-command 'describe-function)
(register-command 'apropos)
(register-command 'helm-apropos)
(register-command 'describe-bindings)
(register-command 'version)

;; *Messages* / *lsp-log* are normal, editable buffers (copy/paste, edit; new
;; entries append). No line-number gutter, and no syntax highlighting/LSP runs
;; on them (their major mode declares no language).
(set-option "line-numbers:messages-mode" "false")
