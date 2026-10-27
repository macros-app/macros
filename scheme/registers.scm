;; requires: core rectangles

;; registers.scm — Emacs registers (`register.el`): single-character slots
;; holding text, a number, a rectangle, a buffer position or a window
;; configuration. Each command prompts for the register's character.
;;
;;   C-x r s / x  copy-to-register (C-u: delete the region too)
;;   C-x r i / g  insert-register  (point before the text; C-u: after)
;;   C-x r r      copy-rectangle-to-register (C-u: delete it too)
;;   C-x r SPC    point-to-register
;;   C-x r j      jump-to-register (positions and window configurations)
;;   C-x r n      number-to-register (C-u N stores N; else the number at point)
;;   C-x r +      increment-register (numbers by the prefix, default 1; text
;;                registers get the region appended)
;;   C-x r w      window-configuration-to-register
;;   M-x view-register / list-registers
;;
;; These are Emacs registers, separate from evil's `"a` registers.
;; A position register holds a buffer-bound marker (`make-buffer-marker`), so
;; it keeps pointing at the same text as the buffer is edited — also while
;; another buffer is current. If its buffer was killed, jumping visits the file
;; again at the remembered position. Registers last for the session.
;;
;; Contents, keyed by the register's one-character string:
;;   ("text" . STRING)   ("number" . N)   ("rectangle" . LINES)
;;   ("point" BUFFER PATH MARKER)          ("window" . LAYOUT)

(define *registers* '())

;; The contents of register `r` (a one-character string), or #f.
(define (get-register r) (alist-ref *registers* r))

;; Store `value` in register `r`, dropping the marker of a position it held.
(define (set-register r value)
  (let ((old (get-register r)))
    (when (and old (equal? (car old) "point"))
      (delete-marker (list-ref old 3))))
  (set! *registers* (cons (cons r value)
                          (filter (lambda (e) (not (equal? (car e) r))) *registers*))))

(define (reg--kind v) (car v))

;; --- prompting -----------------------------------------------------------------
;; A command reads its prefix argument and region when invoked, then prompts
;; for the register; the finisher runs with the register's character.
(define *reg-arg* #f)      ; the command's (current-prefix-arg)
(define *reg-region* #f)   ; its (start end) region, or #f

(define (reg--prompt prompt finisher)
  (set! *reg-arg* (current-prefix-arg))
  (set! *reg-region* (if (region-active?) (list (region-beginning) (region-end)) #f))
  (read-char prompt finisher))

(define (reg--need-region)
  (message "The mark is not set now, so there is no region"))

;; --- text ----------------------------------------------------------------------

;; C-x r s — copy the region into a register; with C-u, delete it too.
(define-command (copy-to-register)
  (reg--prompt "Copy to register: " 'copy-to-register-finish))
(define (copy-to-register-finish r)
  (let ((rg *reg-region*))
    (if (not rg)
        (reg--need-region)
        (let ((s (car rg)) (e (cadr rg)))
          (set-register r (cons "text" (buffer-substring s e)))
          (if *reg-arg* (delete-region s e) (deactivate-mark))
          (message (string-append "Copied to register " r))))))

;; C-x r i — insert a register's text, number or rectangle at point. Point is
;; left before the inserted text (after it with C-u), as in Emacs.
(define-command (insert-register)
  (reg--prompt "Insert register: " 'insert-register-finish))
(define (insert-register-finish r)
  (let ((v (get-register r)) (p (point)))
    (cond ((not v) (message (string-append "Register " r " is empty")))
          ((equal? (reg--kind v) "rectangle") (insert-rectangle (cdr v)))
          ((member (reg--kind v) '("text" "number"))
           (let ((text (if (equal? (reg--kind v) "number") (number->string (cdr v)) (cdr v))))
             (insert-at p text)
             (goto-char (if *reg-arg* (+ p (utf8-length text)) p))))
          (else (message (string-append "Register " r " does not contain text"))))))

;; C-x r r — copy the region-rectangle into a register; with C-u, delete it.
(define-command (copy-rectangle-to-register)
  (let ((b (rectangle-bounds)))
    (set! *reg-rect* b)
    (reg--prompt "Copy rectangle to register: " 'copy-rectangle-to-register-finish)))
(define *reg-rect* #f)
(define (copy-rectangle-to-register-finish r)
  (let ((b *reg-rect*))
    (if (not b)
        (reg--need-region)
        (begin
          (set-register r (cons "rectangle" (apply rectangle-extract b)))
          (if *reg-arg* (apply delete-rectangle-bounds b) (deactivate-mark))
          (message (string-append "Copied rectangle to register " r))))))

;; Delete the rectangle (top bottom left right), for C-u C-x r r.
(define (delete-rectangle-bounds top bottom left right)
  (rect--delete top bottom left right))

;; --- numbers -------------------------------------------------------------------

;; The decimal integer starting at `p` as (value end), or #f.
(define (reg--number-at p)
  (let* ((le (line-end-pos p)) (text (buffer-substring p le))
         (caps (regexp-captures "^-?[0-9]+" text)))
    (and (pair? caps)
         (list (string->number (car caps)) (+ p (utf8-length (car caps)))))))

;; C-x r n — store a number: the prefix argument, or else the number at point
;; (point moves past it), or 0.
(define-command (number-to-register)
  (reg--prompt "Number to register: " 'number-to-register-finish))
(define (number-to-register-finish r)
  (let ((n (if *reg-arg*
               *reg-arg*
               (let ((hit (reg--number-at (point))))
                 (if hit (begin (goto-char (cadr hit)) (car hit)) 0)))))
    (set-register r (cons "number" n))
    (message (string-append "Register " r " = " (number->string n)))))

;; C-x r + — add the prefix argument (default 1) to a number register; for a
;; text register, append the region to it instead.
(define-command (increment-register)
  (reg--prompt "Increment register: " 'increment-register-finish))
(define (increment-register-finish r)
  (let ((v (get-register r)))
    (cond ((and v (equal? (reg--kind v) "number"))
           (let ((n (+ (cdr v) (or *reg-arg* 1))))
             (set-register r (cons "number" n))
             (message (string-append "Register " r " = " (number->string n)))))
          ((and v (equal? (reg--kind v) "text") *reg-region*)
           (set-register r (cons "text" (string-append (cdr v)
                                                       (apply buffer-substring *reg-region*))))
           (deactivate-mark)
           (message (string-append "Appended to register " r)))
          (else (message (string-append "Register " r " does not contain a number or text"))))))

;; --- positions and window configurations ------------------------------------

;; C-x r SPC — remember point (in this buffer) in a register.
(define-command (point-to-register)
  (reg--prompt "Point to register: " 'point-to-register-finish))
(define (point-to-register-finish r)
  (set-register r (list "point" (current-buffer) (buffer-file-name)
                        (make-buffer-marker (point))))
  (message (string-append "Saved point to register " r)))

;; C-x r w — remember the window layout in a register.
(define *reg-window-target* #f)
(define-command (window-configuration-to-register)
  (reg--prompt "Window configuration to register: " 'window-configuration-to-register-finish))
(define (window-configuration-to-register-finish r)
  (set! *reg-window-target* r)
  ;; The layout is published on demand; read it in the callback.
  (session-refresh 'reg--window-save))
(define (reg--window-save)
  (let ((layout (window-layout)) (r *reg-window-target*))
    (when (and r layout)
      (set-register r (cons "window" layout))
      (message (string-append "Saved window configuration to register " r)))))

;; C-x r j — jump to a position register (switching buffer, or re-visiting
;; its file if the buffer is gone) or restore a window configuration.
(define-command (jump-to-register)
  (reg--prompt "Jump to register: " 'jump-to-register-finish))
(define (jump-to-register-finish r)
  (let ((v (get-register r)))
    (cond ((not v) (message (string-append "Register " r " is empty")))
          ((equal? (reg--kind v) "window") (set-window-layout (cdr v)))
          ((equal? (reg--kind v) "point")
           (let ((buf (list-ref v 1)) (path (list-ref v 2))
                 (pos (marker-position (list-ref v 3))))
             (cond ((equal? buf (current-buffer)) (goto-char pos))
                   ((member buf (buffer-names)) (switch-to-buffer buf) (goto-char pos))
                   ((not (equal? path "")) (open-file path) (goto-char pos))
                   (else (message (string-append "Buffer " buf " no longer exists"))))))
          (else (message (string-append "Register " r
                                        " doesn't contain a buffer position or configuration"))))))

;; --- viewing -------------------------------------------------------------------

;; The first line of `s`, cut to 60 chars, for one-line descriptions.
(define (reg--first-line s)
  (let* ((lines (split-many s "\n")) (l (if (null? lines) "" (car lines))))
    (if (> (string-length l) 60) (string-append (substring l 0 60) "…") l)))

;; A one-line description of register `r`'s contents `v`.
(define (describe-register-contents r v)
  (string-append
   "Register " r " contains "
   (let ((k (reg--kind v)))
     (cond ((equal? k "text") (string-append "text: " (reg--first-line (cdr v))))
           ((equal? k "number") (string-append "the number " (number->string (cdr v))))
           ((equal? k "rectangle")
            (string-append "a rectangle starting with: "
                           (if (null? (cdr v)) "" (reg--first-line (car (cdr v))))))
           ((equal? k "point")
            (let ((pos (marker-position (list-ref v 3))))
              (string-append "a buffer position: buffer " (list-ref v 1)
                             ", position " (number->string pos))))
           ((equal? k "window") "a window configuration")
           (else "something unknown")))))

;; M-x view-register — describe one register in the echo area.
(define-command (view-register)
  (read-char "View register: " 'view-register-finish))
(define (view-register-finish r)
  (let ((v (get-register r)))
    (message (if v (describe-register-contents r v) (string-append "Register " r " is empty")))))

;; M-x list-registers — every non-empty register, in a *Register List* buffer
;; (q to close).
(define-command (list-registers)
  (let ((rs (sort-strings (map car *registers*))))
    (render-buffer "*Register List*" "help-mode"
                   (if (null? rs)
                       "No registers are set."
                       (join-lines (map (lambda (r) (describe-register-contents r (get-register r)))
                                        rs)))
                   '())))

(define-key "fundamental" "ctrl-x r s" 'copy-to-register)
(define-key "fundamental" "ctrl-x r x" 'copy-to-register)
(define-key "fundamental" "ctrl-x r i" 'insert-register)
(define-key "fundamental" "ctrl-x r g" 'insert-register)
(define-key "fundamental" "ctrl-x r r" 'copy-rectangle-to-register)
(define-key "fundamental" "ctrl-x r space" 'point-to-register)
(define-key "fundamental" "ctrl-x r ctrl-space" 'point-to-register)
(define-key "fundamental" "ctrl-x r j" 'jump-to-register)
(define-key "fundamental" "ctrl-x r n" 'number-to-register)
(define-key "fundamental" "ctrl-x r +" 'increment-register)
(define-key "fundamental" "ctrl-x r w" 'window-configuration-to-register)
