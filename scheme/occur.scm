;; requires: core lib-results

;; occur.scm — Emacs `occur`: list every line matching a regexp in `*occur*`.
;; The results buffer itself (visiting hits, n/p, and the edit-and-apply cycle of
;; occur-edit-mode / wgrep) is the shared framework in lib-results.scm.

;; --- occur ---------------------------------------------------------------------
;; List every line matching a regexp (case-insensitive when the regexp is all
;; lower case, like Emacs's case-fold-search) in *occur*, shown in a window
;; below the source. Searching runs in Rust over the buffer snapshot
;; (`buffer-search-all`), so no copy of the whole buffer crosses into Scheme —
;; only the matching lines do.
(define *occur-last* "")

(define-command (occur)
  (read-string (if (equal? *occur-last* "")
                   "List lines matching regexp: "
                   (string-append "List lines matching regexp (default " *occur-last* "): "))
               'occur-run))

;; Smart case: an all-lower-case regexp matches case-insensitively.
(define (occur-fold-pattern re)
  (if (equal? re (string-downcase re)) (string-append "(?i)" re) re))

;; `(match-count . records)` for every line of the active buffer holding a
;; non-empty match of `pat`, or the regexp error string.
(define (occur-collect pat)
  (let ((ms (buffer-search-all pat (point-min) (point-max)))
        (src (current-buffer))
        (path (buffer-file-name)))
    (if (string? ms)
        ms
        (let loop ((ms ms) (last-line -1) (acc '()) (count 0))
          (if (null? ms)
              (cons count (reverse acc))
              (let* ((m (car ms)) (s (car m)))
                (cond
                  ((= s (cadr m)) (loop (cdr ms) last-line acc count))
                  ((= (line-of-pos s) last-line) (loop (cdr ms) last-line acc (+ count 1)))
                  (else
                   (let ((line (line-of-pos s)))
                     (loop (cdr ms) line
                           (cons (results-record src path line
                                                 (string-append
                                                  (pad-left (int->string (+ line 1)) 7) ":")
                                                 (buffer-substring (line-start-pos s)
                                                                   (line-end-pos s)))
                                 acc)
                           (+ count 1)))))))))))

(define (occur-run input)
  (let ((re (if (equal? input "") *occur-last* input)))
    (if (equal? re "")
        (message "No regexp given")
        (let* ((pat (occur-fold-pattern re))
               (res (occur-collect pat)))
          (set! *occur-last* re)
          (cond
            ((string? res) (message (string-append "Invalid regexp: " res)))
            ((null? (cdr res)) (message (string-append "No matches for \"" re "\"")))
            (else
             (let* ((count (car res)) (records (cdr res)) (nl (length records))
                    (summary (string-append (int->string count) " match"
                                            (if (= count 1) "" "es") " in "
                                            (int->string nl) " line" (if (= nl 1) "" "s")
                                            " for \"" re "\"")))
               (results-set! "*occur*"
                             (string-append summary " in buffer: " (current-buffer))
                             "occur-mode" pat records)
               (occur-display)
               (message summary))))))))

;; Show *occur* in a window below the source (reusing it when already shown),
;; with point on the first hit, and select it so n/p/RET work at once.
(define (occur-display)
  (let ((w (get-buffer-window "*occur*")))
    (if (>= w 0)
        (select-window w)
        (begin (split-window-below) (other-window)))
    (results-render "*occur*")
    (goto-line 1)))

(results-bind-view-keys "occur-mode")

(define-key "fundamental" "ctrl-c o" 'occur)
