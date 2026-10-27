;; requires: core

;; org-roam.scm — a Zettelkasten over your .org files, in the org-roam style,
;; built entirely in Scheme on the editor primitives. No SQLite: the node index
;; is scanned from disk (ripgrep-fast `walk-files`) and cached in memory; a
;; `(sql-query …)` bridge could replace `*org-roam-nodes*` later without touching
;; the commands.
;;
;; A "node" is an .org file under the roam directory with a top `:ID:` property
;; and a `#+title:`. Links between nodes use `[[id:UUID][desc]]`. Commands:
;;   C-c n f  find/visit a node (creates it if the title is new)
;;   C-c n i  insert an [[id:…]] link to a node (creates if new)
;;   C-c n c  capture a new node
;;   C-c n l  show backlinks to the node at point
;;   C-c n s  re-scan the roam directory
;;   C-c C-o  open the link at point (id: / file: / http)
;;
;; This file leans only on core.scm + primitives (plus `org-id-new` /
;; `current-time`), so it loads independently of org.scm.

;; ===========================================================================
;; State (defined first so the config setters below can touch them)
;; ===========================================================================
;; The node index: each entry is (id title file (tag …)).
(define *org-roam-nodes* '())
;; Whether the index has been scanned this session.
(define *org-roam-synced* #f)
;; Alias and ref indexes, (alias/ref . node), populated by org-roam-db-sync.
(define *org-roam-aliases* '())
(define *org-roam-refs* '())
;; Sub-directory (under the roam dir) holding daily notes.
(define *org-roam-dailies-directory* "daily")
;; Capture templates: (KEY DESC FILENAME-TEMPLATE BODY-TEMPLATE). Templates may
;; use ${title} ${slug} ${id} ${date} ${time}. Empty ⇒ the simple title prompt.
(define *org-roam-capture-templates* '())

;; ===========================================================================
;; Configuration + small helpers
;; ===========================================================================
;; The directory scanned for nodes. Empty = ~/org-roam. Set from init.scm with
;; (org-roam-set-directory "~/notes").
(define *org-roam-directory* "")
;; With no argument (M-x org-roam-set-directory) it prompts for the directory.
(define-command (org-roam-set-directory . rest)
  (if (null? rest)
      (read-string "org-roam directory: "
                   (lambda (d)
                     (let ((d (string-trim d)))
                       (when (not (equal? d ""))
                         (org-roam-set-directory d)
                         (message (string-append "org-roam directory: " d)))))
                   (if (equal? *org-roam-directory* "") "~/org-roam" *org-roam-directory*))
      (begin
        (set! *org-roam-directory* (car rest))
        (set! *org-roam-synced* #f))))

;; Command used to open external (http / file) links. macOS "open"; set to
;; "xdg-open" on Linux from init.scm via (set! *org-open-command* "xdg-open").
(define *org-open-command* "open")

;; Inline display width (px) of the graph image, and the graphviz render DPI.
;; Raise the width to see a large graph bigger (it's capped to its natural size,
;; so the DPI controls how much detail there is to show). Set from init.scm:
;;   (set! *org-roam-graph-width* 1600)
;;   (set! *org-roam-graph-dpi* 200)
(define *org-roam-graph-width* 1200)
(define *org-roam-graph-dpi* 150)

(define (org-roam-directory)
  (expand-file-name
   (if (equal? *org-roam-directory* "")
       (path-join (getenv "HOME") "org-roam")
       *org-roam-directory*)))

;; Resolve a (possibly relative) link target against the active buffer's dir.
(define (org-roam--resolve p)
  (if (>= (string-match "^[/~]" p) 0) p (path-join (buffer-directory) p)))

;; `$` in a replace-regexp-in-string replacement is a group reference; double
;; it so user text (a title like "Cost in $USD") is inserted literally.

;; A filesystem-safe slug from a title: lowercased, non-alphanumerics collapsed
;; to single dashes, trimmed.
(define (org-roam--slug title)
  (let* ((s (string-downcase title))
         (s (replace-regexp-in-string "[^a-z0-9]+" "-" s))
         (s (replace-regexp-in-string "^-+" "" s))
         (s (replace-regexp-in-string "-+$" "" s)))
    (if (equal? s "") "untitled" s)))

;; ":a:b:" filetags string → ("a" "b").
(define (org-roam--split-tags s)
  (remove-equal "" (split-many (string-trim s) ":")))

;; ===========================================================================
;; The node index, scanned and cached
;; ===========================================================================
;; Each node is a list: (id title file (tag …) line). `line` is the 0-based line
;; to jump to — 0 for a file-level node, the headline's line for a subtree node.
(define (org-roam--node-line n) (if (> (length n) 4) (nth n 4) 0))

;; Lines before the first headline (the file's "front matter").
(define (org-roam--front-matter lines)
  (let loop ((ls lines) (acc '()))
    (cond ((null? ls) (reverse acc))
          ((>= (string-match "^\\*+[ \t]" (car ls)) 0) (reverse acc))
          (else (loop (cdr ls) (cons (car ls) acc))))))

;; A file-level node (line 0) if the front matter has an :ID: and there's a
;; #+title:, else #f.
(define (org-roam--file-level-node text lines path)
  (let ((idc (regexp-captures "(?im)^[ \t]*:ID:[ \t]+(\\S+)" (join-lines (org-roam--front-matter lines))))
        (ttc (regexp-captures "(?im)^#\\+title:[ \t]*(.*)$" text)))
    (if (or (null? idc) (null? ttc))
        #f
        (list (nth idc 1) (string-trim (nth ttc 1)) path
              (let ((tc (regexp-captures "(?im)^#\\+filetags:[ \t]*(.*)$" text)))
                (if (null? tc) '() (org-roam--split-tags (nth tc 1))))
              0))))

;; The cleaned title of a headline (stars + trailing tags removed), or #f.
(define (org-roam--heading-text line)
  (let ((c (regexp-captures "^\\*+[ \t]+(.*)$" line)))
    (if (null? c)
        #f
        (string-trim
         (replace-regexp-in-string "[ \t]*:[A-Za-z0-9_@#%:]+:[ \t]*$" "" (nth c 1))))))

;; How many lines below a headline the :PROPERTIES: drawer scan looks before
;; giving up (a well-formed drawer sits directly under its heading).
(define *org-roam-drawer-scan-limit* 12)

;; The :ID: of a :PROPERTIES: drawer in the lines following a headline (scanning
;; up to the drawer's :END: or the next headline), or #f.
(define (org-roam--drawer-id lines-after)
  (let loop ((ls lines-after) (k 0) (in-drawer #f))
    (cond ((or (null? ls) (> k *org-roam-drawer-scan-limit*)) #f)
          ((>= (string-match "^[ \t]*:PROPERTIES:[ \t]*$" (car ls)) 0) (loop (cdr ls) (+ k 1) #t))
          ((>= (string-match "^[ \t]*:END:[ \t]*$" (car ls)) 0) #f)
          ((>= (string-match "^\\*+[ \t]" (car ls)) 0) #f) ; next headline
          (in-drawer
           (let ((c (regexp-captures "^[ \t]*:ID:[ \t]+(\\S+)" (car ls))))
             (if (null? c) (loop (cdr ls) (+ k 1) #t) (nth c 1))))
          ((equal? (string-trim (car ls)) "") (loop (cdr ls) (+ k 1) in-drawer))
          (else #f)))) ; non-blank, non-drawer content before a drawer → none

;; All nodes in one file: the file-level node (if any) plus every headline that
;; carries an :ID: property (subtree nodes).
(define (org-roam--scan-file path)
  (let* ((text (read-file path))
         (lines (split-many text "\n"))
         (fl (org-roam--file-level-node text lines path)))
    (let loop ((ls lines) (i 0) (acc (if fl (list fl) '())))
      (if (null? ls)
          (reverse acc)
          (let ((ht (org-roam--heading-text (car ls))))
            (if ht
                (let ((id (org-roam--drawer-id (cdr ls))))
                  (loop (cdr ls) (+ i 1)
                        (if id (cons (list id ht path '() i) acc) acc)))
                (loop (cdr ls) (+ i 1) acc)))))))

;; The value of a file-level meta line (:KEY: … in front matter, or #+KEY: …).
(define (org-roam--file-meta text key)
  (let ((c (regexp-captures (string-append "(?im)^[ \t]*:" key ":[ \t]+(.*)$") text)))
    (if (not (null? c)) (string-trim (nth c 1))
        (let ((c2 (regexp-captures (string-append "(?im)^#\\+" key ":[ \t]*(.*)$") text)))
          (if (null? c2) "" (string-trim (nth c2 1)))))))
;; A ROAM_ALIASES/ROAM_REFS value → list: "quoted" tokens, else whitespace split.
(define (org-roam--parse-quoted-list s)
  (let loop ((str s) (acc '()))
    (let ((m (string-match "\"([^\"]*)\"" str)))
      (if (< m 0)
          (if (null? acc)
              (remove-equal "" (split-many (string-trim s) " "))
              (reverse acc))
          (let* ((c (regexp-captures "\"([^\"]*)\"" str)) (whole (car c)))
            (loop (byte-drop str (+ m (utf8-length whole))) (cons (nth c 1) acc)))))))
(define (org-roam--file-level-of nodes)
  (let loop ((ns nodes))
    (cond ((null? ns) #f) ((= (org-roam--node-line (car ns)) 0) (car ns)) (else (loop (cdr ns))))))

;; C-c n s — (re)scan the roam directory into the index (nodes + aliases + refs).
(define-command (org-roam-db-sync)
  (set! *org-roam-nodes* '())
  (set! *org-roam-aliases* '())
  (set! *org-roam-refs* '())
  (let ((dir (org-roam-directory)))
    (when (file-exists? dir)
      (for-each
       (lambda (f)
         (when (ends-with? f ".org")
           (let ((nodes (org-roam--scan-file f)))
             (for-each (lambda (n) (set! *org-roam-nodes* (cons n *org-roam-nodes*))) nodes)
             (let ((fl (org-roam--file-level-of nodes)))
               (when fl
                 (let ((text (read-file f)))
                   (for-each (lambda (a) (set! *org-roam-aliases* (cons (cons a fl) *org-roam-aliases*)))
                             (org-roam--parse-quoted-list (org-roam--file-meta text "ROAM_ALIASES")))
                   (for-each (lambda (r) (set! *org-roam-refs* (cons (cons r fl) *org-roam-refs*)))
                             (org-roam--parse-quoted-list (org-roam--file-meta text "ROAM_REFS")))))))))
       (walk-files dir))))
  (set! *org-roam-synced* #t)
  (message (string-append "org-roam: indexed " (int->string (length *org-roam-nodes*)) " node(s), "
                          (int->string (length *org-roam-aliases*)) " alias(es)")))

(define (org-roam--ensure-index)
  (when (not *org-roam-synced*) (org-roam-db-sync)))

;; Completion titles include node titles and any registered aliases.
(define (org-roam--titles)
  (append (map (lambda (n) (nth n 1)) *org-roam-nodes*) (map car *org-roam-aliases*)))
(define (org-roam--node-by-id id)
  (let loop ((ns *org-roam-nodes*))
    (cond ((null? ns) #f) ((equal? (car (car ns)) id) (car ns)) (else (loop (cdr ns))))))
;; Resolve a title, falling back to the alias index.
(define (org-roam--node-by-title t)
  (let loop ((ns *org-roam-nodes*))
    (cond ((null? ns)
           (let al ((as *org-roam-aliases*))
             (cond ((null? as) #f) ((equal? (car (car as)) t) (cdr (car as))) (else (al (cdr as))))))
          ((equal? (nth (car ns) 1) t) (car ns))
          (else (loop (cdr ns))))))
(define (org-roam--node-by-file f)
  (let loop ((ns *org-roam-nodes*))
    (cond ((null? ns) #f) ((equal? (nth (car ns) 2) f) (car ns)) (else (loop (cdr ns))))))

;; ===========================================================================
;; Creating nodes
;; ===========================================================================
;; Write a new node file (`<timestamp>-<slug>.org`) with an :ID: and #+title:,
;; add it to the index, and return the node record.
(define (org-roam--create-node title)
  (org-roam--ensure-index)
  (let* ((dir (org-roam-directory))
         (id (org-id-new))
         (file (path-join dir (string-append (current-time "%Y%m%d%H%M%S")
                                             "-" (org-roam--slug title) ".org")))
         (node (list id title file '() 0)))
    (call-process "mkdir" (list "-p" dir))
    (write-file file
      (string-append ":PROPERTIES:\n:ID:       " id "\n:END:\n#+title: " title "\n\n"))
    (set! *org-roam-nodes* (cons node *org-roam-nodes*))
    node))

;; ===========================================================================
;; Find / insert / capture
;; ===========================================================================
;; C-c n f — pick a node by title (fuzzy) and visit it; a new title creates one.
(define-command (org-roam-node-find)
  (org-roam--ensure-index)
  (completing-read "Node: " (org-roam--titles) 'org-roam--find-finish))
(define (org-roam--find-finish title)
  (when (not (equal? (string-trim title) ""))
    (let ((n (or (org-roam--node-by-title title)
                 (org-roam--create-node (string-trim title)))))
      (open-file (nth n 2)))))

;; C-c n i — insert an [[id:…][title]] link to a node (creating it if new).
(define-command (org-roam-node-insert)
  (org-roam--ensure-index)
  (completing-read "Insert node: " (org-roam--titles) 'org-roam--insert-finish))
(define (org-roam--insert-finish title)
  (when (not (equal? (string-trim title) ""))
    (let ((n (or (org-roam--node-by-title title)
                 (org-roam--create-node (string-trim title)))))
      (insert (string-append "[[id:" (car n) "][" (nth n 1) "]]")))))

;; C-c n c — capture a brand-new node and open it. With capture templates
;; configured, the title prompt is followed by a template chooser.
(define-command (org-roam-capture) (read-string "Title: " 'org-roam--capture-finish))
(define (org-roam--capture-finish title)
  (when (not (equal? (string-trim title) ""))
    (if (null? *org-roam-capture-templates*)
        (open-file (nth (org-roam--create-node (string-trim title)) 2))
        ;; The title rides into the template chooser's closure.
        (let ((cap-title (string-trim title)))
          (completing-read "Template: "
            (map (lambda (t) (string-append (nth t 0) " — " (nth t 1))) *org-roam-capture-templates*)
            (lambda (cand) (org-roam--capture-tmpl-pick cap-title cand)))))))
(define (org-roam--capture-tmpl-pick title cand)
  (let* ((key (car (split-many cand " ")))
         (t (let loop ((ts *org-roam-capture-templates*))
              (cond ((null? ts) #f) ((equal? (nth (car ts) 0) key) (car ts)) (else (loop (cdr ts)))))))
    (if (not t) (message "No such template")
        (open-file (nth (org-roam--create-node-templated title t) 2)))))

;; ===========================================================================
;; Following links (id: / file: / http)
;; ===========================================================================
(define (org-roam--current-line)
  (let ((ls (buffer-lines)) (i (buffer-line)))
    (if (< i (length ls)) (nth ls i) "")))

;; (prefix . target) for the link on the current line, or #f. `prefix` is one of
;; "id:", "file:", "http://", "https://", or "".
(define *org-roam-link-re* "\\[\\[(id:|file:|https?://)?([^]]+?)\\](\\[[^]]*\\])?\\]")
;; The link under point when the line has several (point inside its `[[…]]`),
;; else the line's first link — it used to always be the first.
(define (org-roam--link-at-point)
  (let* ((b (line-bounds)) (p (point))
         (hits (buffer-search-all *org-roam-link-re* (car b) (cadr b)))
         (hits (if (string? hits) '() hits))
         (under (filter (lambda (h) (and (<= (nth h 0) p) (< p (nth h 1)))) hits))
         (h (cond ((not (null? under)) (car under))
                  ((not (null? hits)) (car hits))
                  (else #f))))
    (and h (cons (nth h 3) (nth h 4)))))

(define (org-roam--visit-id id)
  (org-roam--ensure-index)
  (let ((n (org-roam--node-by-id id)))
    (if (not n)
        (message (string-append "org-roam: no node " id))
        ;; open-file is async: a goto-line in the same command would land in
        ;; the OLD buffer, so jump once the file's buffer is current.
        (begin (open-file (nth n 2))
               (when (> (org-roam--node-line n) 0)
                 (goto-when-visiting "" (nth n 2) (org-roam--node-line n) 0))))))

;; C-c C-o — open the link at point.
(define-command (org-open-at-point)
  (let ((lk (org-roam--link-at-point)))
    (if (not lk)
        (message "No link at point")
        (let ((prefix (car lk)) (target (cdr lk)))
          (cond
            ((equal? prefix "id:") (org-roam--visit-id target))
            ((and (not (equal? prefix "")) (>= (string-match "^https?://" prefix) 0))
             (call-process *org-open-command* (list (string-append prefix target))))
            ((>= (string-match "^https?://" target) 0)
             (call-process *org-open-command* (list target)))
            ((equal? prefix "file:") (open-file (org-roam--resolve target)))
            (else (open-file (org-roam--resolve target))))))))

;; ===========================================================================
;; Backlinks ("what links here")
;; ===========================================================================
;; The :ID: of the node in the active buffer, or #f.
(define (org-roam--current-id)
  (let ((c (regexp-captures "(?im)^[ \t]*:ID:[ \t]+(\\S+)" (buffer-string))))
    (if (null? c) #f (nth c 1))))

;; "path:line:text" → (path line-number text), or #f. Path is non-greedy up to
;; the first ":<digits>:" so it tolerates colons elsewhere.
(define (org-roam--parse-hit h)
  (let ((c (regexp-captures "^(.+?):([0-9]+):(.*)$" h)))
    (if (null? c) #f (list (nth c 1) (string->number (nth c 2)) (nth c 3)))))

;; agenda-buffer line -> (source-file . 0-based-line), for RET.
(define *org-roam-backlink-targets* '())
;; (id . title) of the node the backlinks buffer currently shows, so `g` can
;; refresh it from inside *org-roam-backlinks* (which has no :ID: of its own).
(define *org-roam-backlinks-node* #f)

(define (org-roam--render-backlinks id title)
  (set! *org-roam-backlinks-node* (cons id title))
  (set! *org-roam-backlink-targets* '())
  (let ((hits (grep-files (org-roam-directory) (string-append "\\[\\[id:" id))))
    (let loop ((hs hits) (i 2) (out '()))
      (if (null? hs)
          (render-buffer "*org-roam-backlinks*" "org-roam-backlinks-mode"
            (join-lines
             (append (list (string-append "Backlinks to: " title
                                          "  (" (int->string (length hits)) ")")
                           "")
                     (reverse out)))
            '())
          (let ((p (org-roam--parse-hit (car hs))))
            (if (not p)
                (loop (cdr hs) i out)
                (let* ((path (nth p 0)) (lnum (nth p 1)) (txt (nth p 2))
                       (src (org-roam--node-by-file path))
                       (label (if src (nth src 1) (basename path))))
                  (set! *org-roam-backlink-targets*
                        (cons (cons i (cons path (- lnum 1))) *org-roam-backlink-targets*))
                  (loop (cdr hs) (+ i 1)
                        (cons (string-append label "  :  " (string-trim txt)) out)))))))))

;; C-c n l — show backlinks to the node at point in a side buffer.
(define-command (org-roam-buffer-toggle)
  (org-roam--ensure-index)
  (let ((id (org-roam--current-id)))
    (if (not id)
        (message "Not in an org-roam node (no :ID:)")
        (let ((n (org-roam--node-by-id id)))
          (org-roam--render-backlinks id (if n (nth n 1) (basename (current-buffer))))))))

;; `g` in *org-roam-backlinks* — re-render for the node it was opened on.
(define-command (org-roam-backlinks-refresh)
  (let ((node *org-roam-backlinks-node*))
    (if (not node)
        (message "No org-roam node to refresh")
        (org-roam--render-backlinks (car node) (cdr node)))))

(define (org-roam-backlinks-visit)
  (let ((t (alist-ref *org-roam-backlink-targets* (buffer-line))))
    (when t (open-file (car t)) (goto-when-visiting "" (car t) (cdr t) 0))))

;; ===========================================================================
;; Graph view (graphviz → inline image)
;; ===========================================================================
;; Emit a DOT graph of the node network and render it with `dot -Tpng`, shown
;; inline via the Org image pipeline. Needs graphviz (`dot`) on PATH.
;; Every node ID referenced by `[[id:…]]` links in `text`, in order.
(define (org-roam--link-ids text)
  (let loop ((s text) (acc '()))
    (let ((m (string-match "\\[\\[id:([^]\n]+)" s)))
      (if (< m 0)
          (reverse acc)
          (let* ((c (regexp-captures "\\[\\[id:([^]\n]+)" s))
                 (id (string-trim (nth c 1)))
                 (consumed (+ m (utf8-length (car c)))))
            (loop (byte-drop s consumed) (cons id acc)))))))

;; (source-id . target-id) edges between known nodes (self-loops dropped).
(define (org-roam--edges)
  (let loop ((ns *org-roam-nodes*) (acc '()))
    (if (null? ns)
        acc
        (let* ((n (car ns)) (sid (car n)))
          (loop (cdr ns)
                (append acc
                  (filter (lambda (e)
                            (and (not (equal? (car e) (cdr e))) (org-roam--node-by-id (cdr e))))
                          (map (lambda (t) (cons sid t))
                               (org-roam--link-ids (read-file (nth n 2)))))))))))

;; Make a title safe inside a DOT "label": quotes → apostrophes, backslashes and
;; newlines → spaces. (Sanitising avoids DOT's escaping rules entirely.)
(define (org-roam--dot-escape s)
  (replace-regexp-in-string "\"" "'"
    (replace-regexp-in-string "\\\\" " "
      (replace-regexp-in-string "\n" " " s))))

(define (org-roam--dot)
  (string-append
   "digraph roam {\n  rankdir=LR;\n  bgcolor=\"transparent\";\n"
   "  node [shape=box, style=\"rounded,filled\", fillcolor=\"#3c3836\", fontcolor=\"#ebdbb2\", color=\"#83a598\", fontname=\"sans\"];\n"
   "  edge [color=\"#928374\"];\n"
   (string-concat
    (map (lambda (n)
           (string-append "  \"" (car n) "\" [label=\"" (org-roam--dot-escape (nth n 1)) "\"];\n"))
         *org-roam-nodes*))
   (string-concat
    (map (lambda (e) (string-append "  \"" (car e) "\" -> \"" (cdr e) "\";\n"))
         (org-roam--edges)))
   "}\n"))

(define (org-roam--graph-tmpdir)
  (let ((t (getenv "TMPDIR"))) (path-join (if (equal? t "") "/tmp" t) "macros-org-roam")))

;; C-c n g — render the node graph and show it inline.
(define-command (org-roam-graph)
  (org-roam--ensure-index)
  (if (null? *org-roam-nodes*)
      (message "org-roam: no nodes to graph (try C-c n s)")
      (let* ((dir (org-roam--graph-tmpdir))
             (dotf (path-join dir "graph.dot"))
             (png (path-join dir "graph.png")))
        (call-process "mkdir" (list "-p" dir))
        (write-file dotf (org-roam--dot))
        (call-process-full "dot"
          (list "-Tpng" (string-append "-Gdpi=" (int->string *org-roam-graph-dpi*))
                "-o" png dotf) "")
        (if (not (file-exists? png))
            (message "org-roam: graphviz `dot` failed or not installed")
            (begin
              (render-buffer "*org-roam-graph*" "org-roam-graph-mode"
                (string-append "org-roam graph — " (int->string (length *org-roam-nodes*))
                               " nodes\n" png "\n")
                '())
              (clear-line-images)
              (set-line-image 1 png *org-roam-graph-width*)
              (message "org-roam graph rendered"))))))

;; ===========================================================================
;; Daily notes (org-roam-dailies)
;; ===========================================================================
;; Dailies live in <roam>/<*org-roam-dailies-directory*>/YYYY-MM-DD.org. Date
;; arithmetic comes from core.scm's Gregorian helpers.
(define (org-roam--dailies-dir) (path-join (org-roam-directory) *org-roam-dailies-directory*))
(define (org-roam--daily-file date) (path-join (org-roam--dailies-dir) (string-append date ".org")))
;; Gregorian day arithmetic comes from core.scm (ymd->days / days->ymd / pad2),
;; which this file requires; org-roam stays independent of org.scm.
(define (org-roam--date-str->days s) (or (iso-date->days s) 0))
(define (org-roam--date-str days) (days->iso-date days))
(define (org-roam--ensure-daily date)
  (let ((f (org-roam--daily-file date)))
    (when (not (file-exists? f))
      (call-process "mkdir" (list "-p" (org-roam--dailies-dir)))
      (write-file f (string-append ":PROPERTIES:\n:ID:       " (org-id-new) "\n:END:\n#+title: " date "\n\n")))
    (set! *org-roam-synced* #f)
    f))
;; C-c n d t — open today's daily note (creating it).
(define-command (org-roam-dailies-goto-today) (open-file (org-roam--ensure-daily (current-time "%Y-%m-%d"))))
(define-command (org-roam-dailies-capture-today) (org-roam-dailies-goto-today))
;; C-c n d d — open a specific date's note.
(define-command (org-roam-dailies-goto-date) (read-string "Date (YYYY-MM-DD): " 'org-roam--dailies-date-finish))
(define (org-roam--dailies-date-finish input)
  (let ((d (if (>= (string-match "^[0-9]{4}-[0-9]{2}-[0-9]{2}$" (string-trim input)) 0)
               (string-trim input) (current-time "%Y-%m-%d"))))
    (open-file (org-roam--ensure-daily d))))
;; The date of the daily note in the current buffer, or #f.
(define (org-roam--current-daily-date)
  (let ((c (regexp-captures "([0-9]{4}-[0-9]{2}-[0-9]{2})\\.org$" (current-buffer))))
    (if (null? c) #f (nth c 1))))
(define (org-roam--dailies-step delta)
  (let ((d (org-roam--current-daily-date)))
    (if (not d) (message "Not in a daily note")
        (open-file (org-roam--ensure-daily (org-roam--date-str (+ delta (org-roam--date-str->days d))))))))
;; C-c n d f / b — next / previous day.
(define-command (org-roam-dailies-goto-next) (org-roam--dailies-step 1))
(define-command (org-roam-dailies-goto-previous) (org-roam--dailies-step -1))

;; ===========================================================================
;; Tag filtering + subtree (headline) nodes
;; ===========================================================================
(define (org-roam--add-unique lst x) (if (member x lst) lst (cons x lst)))
(define (org-roam--all-tags)
  (let loop ((ns *org-roam-nodes*) (acc '()))
    (if (null? ns) acc
        (loop (cdr ns)
              (let inner ((ts (nth (car ns) 3)) (a acc))
                (if (null? ts) a (inner (cdr ts) (org-roam--add-unique a (car ts)))))))))
;; C-c n t — pick a tag, then a node carrying it.
(define-command (org-roam-tag-find)
  (org-roam--ensure-index)
  (if (null? (org-roam--all-tags))
      (message "org-roam: no tags indexed")
      (completing-read "Tag: " (org-roam--all-tags) 'org-roam--tag-find-pick)))
(define (org-roam--tag-find-pick tag)
  (completing-read "Node: "
    (map (lambda (n) (nth n 1)) (filter (lambda (n) (member tag (nth n 3))) *org-roam-nodes*))
    'org-roam--find-finish))

;; C-c n h — make the heading at point a node by giving it an :ID: property, so
;; it is indexed as a subtree node on the next sync. Self-contained (doesn't lean
;; on org.scm's org-id-get-create) since this file loads first.
(define (org-roam--list-drop lst n) (if (or (<= n 0) (null? lst)) lst (org-roam--list-drop (cdr lst) (- n 1))))
(define-command (org-roam-node-from-heading)
  (let* ((lines (buffer-lines)) (L (buffer-line))
         (H (let loop ((i L))
              (cond ((< i 0) #f)
                    ((>= (string-match "^\\*+[ \t]" (nth lines i)) 0) i)
                    (else (loop (- i 1)))))))
    (if (not H) (message "Not on a heading")
        (let ((existing (org-roam--drawer-id (org-roam--list-drop lines (+ H 1)))))
          (if existing (message (string-append "Already a node: " existing))
              (let ((id (org-id-new))
                    (pos (+ (lines-byte-offset lines H) (utf8-length (nth lines H)))))
                (insert-at pos (string-append "\n:PROPERTIES:\n:ID:       " id "\n:END:"))
                (set! *org-roam-synced* #f)
                (message (string-append "org-roam node id: " id))))))))

;; ===========================================================================
;; Renaming a node (updates #+title and every inbound link description)
;; ===========================================================================
(define (org-roam--rewrite-links text id newtitle)
  (replace-regexp-in-string
   (string-append "\\[\\[id:" (regexp-quote id) "\\]\\[[^]]*\\]\\]")
   (replacement-literal (string-append "[[id:" id "][" newtitle "]]")) text))
;; C-c n r — rename the current node (the node id rides in the closure).
(define-command (org-roam-rename-node)
  (let ((id (org-roam--current-id)))
    (if (not id) (message "Not in an org-roam node")
        (read-string "New title: "
                     (lambda (newtitle) (org-roam--rename-apply id newtitle))
                     (let ((n (org-roam--node-by-id id))) (if n (nth n 1) ""))))))
(define (org-roam--rename-apply id newtitle)
  (let ((nt (string-trim newtitle)))
    (when (not (equal? nt ""))
      ;; current buffer: rewrite #+title and any self-links
      (let* ((text (buffer-string))
             (t2 (replace-regexp-in-string "(?im)^#\\+title:[ \t]*.*$"
                                           (replacement-literal (string-append "#+title: " nt))
                                           text)))
        (replace-region 0 (buffer-size) (org-roam--rewrite-links t2 id nt)))
      ;; every other file: rewrite inbound link descriptions
      (let ((cur (let ((f (buffer-file-name)))
                   (if (equal? f "") (path-join (buffer-directory) (current-buffer)) f))))
        (for-each (lambda (f)
                    (when (and (ends-with? f ".org") (not (equal? f cur)))
                      (let* ((t (read-file f)) (t2 (org-roam--rewrite-links t id nt)))
                        (when (not (equal? t t2)) (write-file f t2)))))
                  (walk-files (org-roam-directory))))
      (set! *org-roam-synced* #f)
      (message (string-append "Renamed to: " nt)))))

;; ===========================================================================
;; Unlinked references (mentions of this node's title that are not links)
;; ===========================================================================
(define *org-roam-unlinked-targets* '())
(define-command (org-roam-unlinked-references)
  (org-roam--ensure-index)
  (let* ((id (org-roam--current-id)) (n (and id (org-roam--node-by-id id))))
    (if (not n) (message "Not in an org-roam node")
        (let* ((title (nth n 1))
               (hits (grep-files (org-roam-directory) (regexp-quote title))))
          (set! *org-roam-unlinked-targets* '())
          (let loop ((hs hits) (i 2) (out '()))
            (if (null? hs)
                (render-buffer "*org-roam-unlinked*" "org-roam-unlinked-mode"
                  (join-lines (append (list (string-append "Unlinked references to: " title) "")
                                      (reverse out))) '())
                (let ((p (org-roam--parse-hit (car hs))))
                  (if (or (not p)
                          (>= (string-match (string-append "id:" (regexp-quote id)) (nth p 2)) 0)
                          (>= (string-match "(?i)^#\\+title:" (string-trim (nth p 2))) 0))
                      (loop (cdr hs) i out)
                      (let* ((path (nth p 0)) (lnum (nth p 1)) (txt (nth p 2))
                             (src (org-roam--node-by-file path))
                             (label (if src (nth src 1) (basename path))))
                        (set! *org-roam-unlinked-targets*
                              (cons (cons i (cons path (- lnum 1))) *org-roam-unlinked-targets*))
                        (loop (cdr hs) (+ i 1)
                              (cons (string-append label "  :  " (string-trim txt)) out)))))))))))

;; ===========================================================================
;; Persistent index cache (TSV: id <tab> title <tab> file <tab> tags <tab> line)
;; ===========================================================================
(define (org-roam--cache-file) (path-join (org-roam-directory) ".org-roam-cache.tsv"))
(define-command (org-roam-db-save)
  (org-roam--ensure-index)
  (write-file (org-roam--cache-file)
    (join-lines (map (lambda (n)
                       (string-join (list (nth n 0) (nth n 1) (nth n 2)
                                          (string-join (nth n 3) ",")
                                          (int->string (org-roam--node-line n))) "\t"))
                     *org-roam-nodes*)))
  (message "org-roam: cache saved"))
(define-command (org-roam-db-load)
  (let ((f (org-roam--cache-file)))
    (if (not (file-exists? f)) (message "org-roam: no cache (run C-c n s)")
        (begin
          (set! *org-roam-nodes*
            (filter (lambda (x) x)
              (map (lambda (line)
                     (let ((parts (split-many line "\t")))
                       (if (< (length parts) 5) #f
                           (list (nth parts 0) (nth parts 1) (nth parts 2)
                                 (remove-equal "" (split-many (nth parts 3) ","))
                                 (string->number (nth parts 4))))))
                   (remove-equal "" (split-many (read-file f) "\n")))))
          (set! *org-roam-synced* #t)
          (message (string-append "org-roam: loaded " (int->string (length *org-roam-nodes*)) " node(s) from cache"))))))

;; ===========================================================================
;; Templated node creation
;; ===========================================================================
(define (org-roam--expand-tmpl s title slug id)
  (let* ((s (replace-regexp-in-string "\\$\\{title\\}" (replacement-literal title) s))
         (s (replace-regexp-in-string "\\$\\{slug\\}" (replacement-literal slug) s))
         (s (replace-regexp-in-string "\\$\\{id\\}" (replacement-literal id) s))
         (s (replace-regexp-in-string "\\$\\{date\\}" (current-time "%Y-%m-%d") s))
         (s (replace-regexp-in-string "\\$\\{time\\}" (current-time "%Y%m%d%H%M%S") s)))
    s))
(define (org-roam--create-node-templated title tmpl)
  (org-roam--ensure-index)
  (let* ((dir (org-roam-directory)) (id (org-id-new)) (slug (org-roam--slug title))
         (file (path-join dir (org-roam--expand-tmpl (nth tmpl 2) title slug id)))
         (body (org-roam--expand-tmpl (nth tmpl 3) title slug id))
         (node (list id title file '() 0)))
    (call-process "mkdir" (list "-p" (parent-directory file)))
    (write-file file body)
    (set! *org-roam-nodes* (cons node *org-roam-nodes*))
    node))

;; ===========================================================================
;; Keymap + M-x
;; ===========================================================================
;; The C-c n prefix, available globally and in org buffers.
(define-key "fundamental" "ctrl-c n f" 'org-roam-node-find)
(define-key "fundamental" "ctrl-c n c" 'org-roam-capture)
(define-key "fundamental" "ctrl-c n s" 'org-roam-db-sync)
(define-key "fundamental" "ctrl-c n g" 'org-roam-graph)
(define-key "org-mode" "ctrl-c n f" 'org-roam-node-find)
(define-key "org-mode" "ctrl-c n i" 'org-roam-node-insert)
(define-key "org-mode" "ctrl-c n c" 'org-roam-capture)
(define-key "org-mode" "ctrl-c n l" 'org-roam-buffer-toggle)
(define-key "org-mode" "ctrl-c n s" 'org-roam-db-sync)
(define-key "org-mode" "ctrl-c n g" 'org-roam-graph)
(define-key "org-mode" "ctrl-c ctrl-o" 'org-open-at-point)
;; Tagging, subtree nodes, rename, unlinked refs, persistent cache.
(define-key "org-mode" "ctrl-c n t" 'org-roam-tag-find)
(define-key "fundamental" "ctrl-c n t" 'org-roam-tag-find)
(define-key "org-mode" "ctrl-c n h" 'org-roam-node-from-heading)
(define-key "org-mode" "ctrl-c n r" 'org-roam-rename-node)
(define-key "org-mode" "ctrl-c n u" 'org-roam-unlinked-references)
;; Daily notes (C-c n d …).
(define-key "org-mode" "ctrl-c n d t" 'org-roam-dailies-goto-today)
(define-key "org-mode" "ctrl-c n d d" 'org-roam-dailies-goto-date)
(define-key "org-mode" "ctrl-c n d f" 'org-roam-dailies-goto-next)
(define-key "org-mode" "ctrl-c n d b" 'org-roam-dailies-goto-previous)
(define-key "fundamental" "ctrl-c n d t" 'org-roam-dailies-goto-today)
(define-key "fundamental" "ctrl-c n d d" 'org-roam-dailies-goto-date)

;; *org-roam-backlinks* buffer.
(define-list-mode-keys "org-roam-backlinks-mode" 'org-roam-backlinks-visit)
(define-key "org-roam-backlinks-mode" "g" 'org-roam-backlinks-refresh)

;; *org-roam-graph* buffer.
(define-key "org-roam-graph-mode" "g" 'org-roam-graph)
(define-key "org-roam-graph-mode" "q" 'kill-buffer)
(set-option "line-numbers:org-roam-graph-mode" "false")

;; *org-roam-unlinked* buffer.
(define-list-mode-keys "org-roam-unlinked-mode" 'org-roam-unlinked-visit)


;; *org-roam-unlinked* reuses the backlinks keymap; map RET to its visitor.
(define-command (org-roam-unlinked-visit)
  (let ((t (alist-ref *org-roam-unlinked-targets* (buffer-line))))
    (when t (open-file (car t)) (goto-when-visiting "" (car t) (cdr t) 0))))
