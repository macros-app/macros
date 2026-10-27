;; requires: core help abbrev spell

;; help-extras.scm — C-h v describe-variable and C-h m describe-mode.

;; --- describe-variable (C-h v) ------------------------------------------------
;; Variables are the Scheme globals named `*like-this*` (the convention every
;; bundled file follows) plus the editor options. Any other global can still
;; be typed by name.
(define (help--variable-name? n)
  (and (> (string-length n) 2) (starts-with? n "*") (ends-with? n "*")))
(define (variable-list)
  (sort-strings (dedup (append (filter help--variable-name? (function-list)) (option-list)))))

;; A value as readable text: strings quoted, everything else as Steel prints it.
(define (help--value-text v)
  (cond ((string? v) (string-append "\"" v "\""))
        ((boolean? v) (if v "#t" "#f"))
        (else (to-string v))))

(define (describe-variable)
  (completing-read "Describe variable: " (variable-list) 'describe-variable-show))
(define (describe-variable-show name)
  (cond
    ((equal? name "") #f)
    ((member name (option-list)) (describe-option name))
    ((not (member name (function-list)))
     (message (string-append "No variable named " name)))
    (else
     (let ((doc (function-doc name)))
       (render-buffer "*help*" "help-mode"
                      (string-append
                       name " is a Scheme global.\n\n"
                       "Value: " (help--value-text (eval-string name)) "\n\n"
                       (if (equal? doc "") "(no documentation)" doc) "\n\n"
                       "Set it from your config or M-: with\n"
                       "  (set! " name " <value>)")
                      '())))))

;; --- describe-mode (C-h m) ----------------------------------------------------
;; The buffer's major mode, the minor modes on in it, and the bindings each
;; keymap adds. Minor modes are registered here with a predicate so packages
;; (and your config) can list theirs: (register-minor-mode "name" (lambda () …)).
(define *minor-modes* '())   ; ((name . on?-thunk) …)
(define (register-minor-mode name on?)
  (set! *minor-modes* (alist-set *minor-modes* name on?)))
(register-minor-mode "abbrev-mode" (lambda () (abbrev--on?)))
(register-minor-mode "spell-mode" (lambda () (spell--on?)))
(register-minor-mode "evil-mode" (lambda () (and (evil-enabled?) (not (helix-enabled?)))))
(register-minor-mode "helix-mode" (lambda () (helix-enabled?)))

(define (enabled-minor-modes)
  (sort-strings (map car (filter (lambda (m) ((cdr m))) *minor-modes*))))

;; A keymap's bindings as indented "key    command" rows, or a note when empty.
(define (help--keymap-rows km)
  (let ((rows (sort-strings (keys-with-prefix km ""))))
    (if (null? rows)
        "  (no bindings of its own)"
        (join-lines (map (lambda (l) (string-append "  " (string-replace l "\t" "    "))) rows)))))

(define (describe-mode-text)
  (let* ((major (major-mode))
         (major (if (equal? major "") "fundamental" major))
         (lang (buffer-language))
         (doc (function-doc (string-append major "-mode")))
         (minors (enabled-minor-modes))
         (modal (current-modal-keymap)))
    (join-lines
     (append
      (list (string-append "Major mode: " major (if (equal? lang "") "" (string-append "  (language: " lang ")")))
            "")
      (if (equal? doc "") '() (list doc ""))
      (list (string-append "Key bindings (" major "):") (help--keymap-rows major) "")
      (list "Enabled minor modes:"
            (if (null? minors) "  (none)" (join-lines (map (lambda (m) (string-append "  " m)) minors)))
            "")
      (if modal
          (list (string-append "Modal key bindings (" modal "):") (help--keymap-rows modal) "")
          '())
      (list "Global bindings: C-h B (describe-bindings).")))))

(define (describe-mode)
  (render-buffer "*help*" "help-mode" (describe-mode-text) '()))

(define-key "fundamental" "ctrl-h v" 'describe-variable)
(define-key "fundamental" "ctrl-h m" 'describe-mode)
(register-command 'describe-variable)
(register-command 'describe-mode)
