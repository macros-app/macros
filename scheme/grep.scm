;; requires: core lib-results shell projectile

;; grep.scm — Emacs's M-x grep / rgrep / lgrep, outside any project. They search
;; in-process with ripgrep's searcher (`grep-files-in`, gitignore-aware and
;; honouring the search-* options), so nothing needs grep installed:
;;
;;   grep    regexp → every file under the buffer's directory
;;   rgrep   regexp, file glob, directory → recursive (Emacs's find + grep)
;;   lgrep   regexp, file glob, directory → that directory only
;;
;; Results land in the same *grep* buffer as projectile-grep-buffer
;; (lib-results.scm): RET / mouse visits a hit, n / p step, `g` reruns the
;; search and `e` makes it editable wgrep-style. The hits are also loaded as
;; the compile error list, so M-g n / M-g p (next-error / previous-error) walk
;; through them from any buffer, as after M-x compile.

;; The last search, for `g` in *grep*: (dir regexp glob recursive?).
(define *grep-last* #f)
;; The *grep* header our last search rendered: `g` reruns ours when *grep*
;; still shows it, else hands over to projectile-grep-buffer.
(define *grep-last-header* "")

;; Records for `hits` (as grep-files-in reports them), paths shown relative
;; to `dir`.
(define (grep--records dir hits)
  (filter (lambda (r) r) (map (lambda (hit) (projectile-grep-record dir hit)) hits)))

;; Point next-error / previous-error at `records`: they become the compile
;; error list (shell.scm), each `(results-line path line col face)`.
(define (grep--set-next-error dir records)
  (set! *compile-dir* dir)
  (set! *compile-errors*
        (reverse
         (let loop ((rs records) (i 1) (acc '()))
           (if (null? rs)
               (reverse acc)
               (loop (cdr rs) (+ i 1)
                     (cons (list i (record-path (car rs)) (+ (record-line (car rs)) 1) 1
                                 "compilation-error")
                           acc))))))
  (set! *compile-idx* -1)
  (set! *next-error-source* "compile"))

;; Search `dir` for `regexp` in files matching `glob`, show the hits in *grep*.
(define (grep-run dir regexp glob recursive?)
  (let ((dir (trim-end-slash (expand-file-name dir))))
    (cond
      ((equal? regexp "") (message "No regexp given"))
      ((not (file-directory? dir)) (message (string-append "No such directory: " dir)))
      (else
       (set! *grep-last* (list dir regexp glob recursive?))
       (let* ((hits (grep-files-in dir regexp glob recursive?))
              (records (grep--records dir hits))
              (n (length records)))
         (if (= n 0)
             (message (string-append "No matches for \"" regexp "\" in " dir))
             (let ((header (string-append
                            (int->string n) " match" (if (= n 1) "" "es")
                            " for \"" regexp "\""
                            (if (member glob '("" "*")) "" (string-append " in " glob))
                            " under " dir (if recursive? "" " (not recursive)"))))
               (set! *grep-last-header* header)
               (results-set! "*grep*" header "grep-mode" regexp records)
               (results-render "*grep*")
               (goto-line 1)
               (grep--set-next-error dir records)
               (message (string-append (int->string n) " match" (if (= n 1) "" "es")
                                       " — M-g n / M-g p step through them")))))))))

;; "/a/b/" → "/a/b" (but "/" stays "/"), so relative paths render cleanly.
(define (trim-end-slash p)
  (if (and (> (string-length p) 1) (ends-with? p "/"))
      (substring p 0 (- (string-length p) 1))
      p))

(define-command (grep)
  (let ((dir (buffer-directory)))
    (read-string "Grep (regexp): "
                 (lambda (re) (grep-run dir re "" #t)))))

;; rgrep / lgrep: regexp, then the file glob, then the directory.
(define (grep--prompt-all label recursive?)
  (let ((dir (buffer-directory)))
    (read-string
     (string-append label " (regexp): ")
     (lambda (re)
       (read-string
        (string-append "Search for \"" re "\" in files (glob): ")
        (lambda (glob)
          (read-string "In directory: "
                       (lambda (d)
                         (grep-run (if (equal? (trim d) "") dir d) re glob recursive?))
                       (path-join dir "")))
        "*")))))

(define-command (rgrep) (grep--prompt-all "Recursive grep" #t))
(define-command (lgrep) (grep--prompt-all "Local grep" #f))

;; `g` in *grep*: rerun whichever search filled it.
(define-command (grep-again)
  (let ((spec (results-spec "*grep*")))
    (if (and *grep-last* spec (equal? (results-header spec) *grep-last-header*))
        (apply grep-run *grep-last*)
        (projectile-grep-buffer))))

(define-key "grep-mode" "g" 'grep-again)
