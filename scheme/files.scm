;; requires: core
;; Emacs file and buffer commands: rename-buffer, insert-file,
;; find-file-at-point (ffap), the C-x 4 other-window family and
;; read-only-mode's key. `write-file` (C-x C-w) lives in core.scm, since
;; scripts call its two-argument form. Each command prompts when called with
;; no arguments (M-x, a key) and acts directly when given one.

;; M-x rename-buffer — give the current buffer a new, unused name.
(define (rename-buffer . args)
  (if (null? args)
      (read-string "Rename buffer (to new name): " 'rename-buffer-impl (current-buffer))
      (rename-buffer-impl (car args))))

;; C-x i — insert a file's contents at point (point stays before them).
(define (insert-file . args)
  (if (null? args)
      (read-file-name "Insert file: " 'insert-file-impl)
      (insert-file-impl (car args))))

;; C-x 4 f — visit a file in another window (splitting if there's only one).
(define (find-file-other-window . args)
  (if (null? args)
      (read-file-name "Find file in other window: " 'find-file-other-window-impl)
      (find-file-other-window-impl (car args))))

;; C-x 4 b — switch to a buffer in another window.
(define (switch-to-buffer-other-window . args)
  (if (null? args)
      (completing-read "Switch to buffer in other window: " (buffer-names)
                       'switch-to-buffer-other-window-impl)
      (switch-to-buffer-other-window-impl (car args))))

;; C-x 4 C-o — show a buffer in another window without selecting it.
(define (display-buffer . args)
  (if (null? args)
      (completing-read "Display buffer: " (buffer-names) 'display-buffer-impl)
      (display-buffer-impl (car args))))

;; --- find-file-at-point -----------------------------------------------------

;; Characters that end a file name or URL in running text.
(define (ffap-stop-char? c)
  (or (char-whitespace? c)
      (memv c '(#\" #\' #\` #\( #\) #\[ #\] #\{ #\} #\< #\> #\, #\;))))

;; Trailing punctuation that belongs to the sentence, not the name.
(define (ffap-trim-end s)
  (if (and (> (string-length s) 0)
           (memv (string-ref s (- (string-length s) 1)) '(#\. #\: #\! #\?)))
      (ffap-trim-end (substring s 0 (- (string-length s) 1)))
      s))

(define (ffap-digits? s)
  (and (> (string-length s) 0)
       (let loop ((cs (string->list s)))
         (or (null? cs)
             (and (char<=? #\0 (car cs) #\9) (loop (cdr cs)))))))

(define (ffap-take lst n)
  (if (or (= n 0) (null? lst)) '() (cons (car lst) (ffap-take (cdr lst) (- n 1)))))

;; The name around char index `i` of `line`, "" when point is on a separator.
(define (ffap-token line i)
  (let* ((n (string-length line))
         (start (let loop ((j i))
                  (if (and (> j 0) (not (ffap-stop-char? (string-ref line (- j 1)))))
                      (loop (- j 1))
                      j)))
         (end (let loop ((j i))
                (if (and (< j n) (not (ffap-stop-char? (string-ref line j))))
                    (loop (+ j 1))
                    j))))
    (ffap-trim-end (substring line start end))))

;; Split "path:LINE" / "path:LINE:COL" into (path line), line #f when absent.
(define (ffap-split-line token)
  (let* ((parts (split-many token ":"))
         (k (length parts))
         (nums (let loop ((ps (reverse parts)) (acc '()))
                 (if (and (pair? ps) (ffap-digits? (car ps)) (< (length acc) 2)
                          (pair? (cdr ps)))
                     (loop (cdr ps) (cons (car ps) acc))
                     acc))))
    (if (null? nums)
        (list token #f)
        (list (string-join (ffap-take parts (- k (length nums))) ":")
              (string->number (car nums))))))

(define (ffap-url? s)
  (or (starts-with? s "http://") (starts-with? s "https://")
      (starts-with? s "ftp://") (starts-with? s "mailto:")))

;; A relative name is relative to the buffer's directory.
(define (ffap-expand path)
  (cond ((starts-with? path "file://") (substring path 7 (string-length path)))
        ((or (starts-with? path "/") (starts-with? path "~")) path)
        (else (path-join (buffer-directory) path))))

;; What `find-file-at-point` would do with the text at point:
;; (url "https://…"), (file "/abs/path" line-or-#f), or #f for nothing.
(define (ffap-guess)
  (let* ((p (point))
         (ls (line-start-pos p))
         (line (buffer-substring ls (line-end-pos p)))
         (col (string-length (buffer-substring ls p)))
         (token (ffap-token line col)))
    (cond ((equal? token "") #f)
          ((ffap-url? token) (list 'url token))
          (else
           (let* ((split (ffap-split-line token))
                  (path (ffap-expand (car split))))
             (and (file-exists? path) (list 'file path (cadr split))))))))

;; M-x find-file-at-point (alias ffap) — open the file or URL at point. A
;; `file:LINE` (or `file:LINE:COL`) reference lands on that line. With nothing
;; recognizable at point it's plain find-file.
(define (find-file-at-point)
  (let ((guess (ffap-guess)))
    (cond ((not guess) (find-file))
          ((eq? (car guess) 'url) (open-url (cadr guess)))
          ((caddr guess) (find-file-goto-line (cadr guess) (max 0 (- (caddr guess) 1))))
          (else (open-file (cadr guess))))))
(define (ffap) (find-file-at-point))

(register-command 'rename-buffer)
(register-command 'insert-file)
(register-command 'find-file-other-window)
(register-command 'switch-to-buffer-other-window)
(register-command 'display-buffer)
(register-command 'find-file-at-point)
(register-command 'ffap)

;; --- keys ------------------------------------------------------------------
(define-key "fundamental" "ctrl-x ctrl-w" 'write-file)
(define-key "fundamental" "ctrl-x i" 'insert-file)
;; Dired binds its own C-x C-q (wdired) in the dired keymap, which wins there.
(define-key "fundamental" "ctrl-x ctrl-q" 'read-only-mode)
(define-key "fundamental" "ctrl-x 4 f" 'find-file-other-window)
(define-key "fundamental" "ctrl-x 4 ctrl-f" 'find-file-other-window)
(define-key "fundamental" "ctrl-x 4 b" 'switch-to-buffer-other-window)
(define-key "fundamental" "ctrl-x 4 0" 'kill-buffer-and-window)
(define-key "fundamental" "ctrl-x 4 ctrl-o" 'display-buffer)
