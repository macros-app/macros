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
;;   C-c n e  pick a ROAM_REFS ref and visit its node
;;   C-c n s  re-scan the roam directory
;;   C-c C-o  open the link at point (id: / file: / http)
;;
;; This file leans only on core.scm + primitives (plus `org-id-new` /
;; `current-time`), so it loads independently of org.scm.

;; ===========================================================================
;; State (defined first so the config setters below can touch them)
;; ===========================================================================
;; The node index: each entry is (id title file (tag …) line (alias …)).
;; `aliases` is the node's ROAM_ALIASES — the file-level `:ROAM_ALIASES:` for a
;; file node, the drawer's for a subtree node (element present only when
;; non-empty; use org-roam--node-aliases to read it).
(define *org-roam-nodes* '())
;; Whether the index has been scanned this session.
(define *org-roam-synced* #f)
;; Alias and ref indexes, (alias/ref . node), populated by org-roam-db-sync.
(define *org-roam-aliases* '())
(define *org-roam-refs* '())
;; One scan record per indexed file: (file nodes refs links mtime) — read
;; exactly once per file. Kept beside the flat node list so per-file sync can
;; replace a record in place (the Vulpea incremental-indexing pattern). Aliases
;; live ON the nodes, so they reindex with them.
(define *org-roam-files* '())
;; Hash indexes over the records, rebuilt at the end of every sync (O(n) —
;; simpler than maintaining them incrementally).
(define *org-roam-by-id* (hash))
(define *org-roam-by-title* (hash))
(define *org-roam-by-alias* (hash))
(define *org-roam-by-file* (hash))
;; Reverse link index: link-target-id → (source-node …). Backlinks come from
;; here — no vault grep per C-c n l.
(define *org-roam-by-link* (hash))
;; Set once the roam directory's fs watch has been registered.
(define *org-roam-watching* #f)
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
        ;; The watch was registered on the OLD vault; clear the flag so the
        ;; next sync re-registers (WatchTree is replace-per-owner) against the
        ;; new directory instead of leaving it watched-but-stale.
        (set! *org-roam-watching* #f)
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

;; Is PATH the roam directory itself or anything under it? Anchored to the
;; path's start and a separator — NOT an unanchored substring: a sibling
;; directory whose name extends the vault's (~/notes vs ~/notes-archive) must
;; not pass, or the incremental per-file sync admits files outside the vault
;; that the full walk-files scan would never index.
(define (org-roam--under-vault? path)
  (let ((dir (org-roam-directory)))
    ;; expand-file-name preserves a trailing slash, which would break the
    ;; anchored pattern ("^dir//(/|$)") — strip it.
    (let ((dir (if (and (> (string-length dir) 1)
                        (equal? (substring dir (- (string-length dir) 1) (string-length dir)) "/"))
                   (substring dir 0 (- (string-length dir) 1))
                   dir)))
      (>= (string-match (string-append "^" (regexp-quote dir) "(/|$)") path) 0))))

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

;; First-seen dedupe (for scan-record link-ids).
(define (org-roam--dedupe lst)
  (let loop ((xs lst) (seen '()) (acc '()))
    (if (null? xs)
        (reverse acc)
        (loop (cdr xs)
              (cons (car xs) seen)
              (if (member (car xs) seen) acc (cons (car xs) acc))))))

;; ===========================================================================
;; The node index, scanned and cached
;; ===========================================================================
;; Each node is a list: (id title file (tag …) line (alias …)). `line` is the
;; 0-based line to jump to — 0 for a file-level node, the headline's line for a
;; subtree node. `aliases` carries the node's ROAM_ALIASES when non-empty.
(define (org-roam--node-line n) (if (> (length n) 4) (nth n 4) 0))
(define (org-roam--node-aliases n) (if (> (length n) 5) (nth n 5) '()))

;; Lines before the first headline (the file's "front matter").
(define (org-roam--front-matter lines)
  (let loop ((ls lines) (acc '()))
    (cond ((null? ls) (reverse acc))
          ((>= (string-match "^\\*+[ \t]" (car ls)) 0) (reverse acc))
          (else (loop (cdr ls) (cons (car ls) acc))))))

;; A file-level node (line 0) if the front matter has an :ID: and there's a
;; #+title:, else #f. The file's ROAM_ALIASES ride the node (per-node aliases) —
;; parsed from the FRONT MATTER only, or a subtree drawer's :ROAM_ALIASES:
;; further down would leak onto the file node.
(define (org-roam--file-level-node text lines path)
  (let* ((front (join-lines (org-roam--front-matter lines)))
         (idc (regexp-captures "(?im)^[ \\t]*:ID:[ \\t]+(\\S+)" front))
         (ttc (regexp-captures "(?im)^#\\+title:[ \\t]*(.*)$" text)))
    (if (or (null? idc) (null? ttc))
        #f
        (list (nth idc 1) (string-trim (nth ttc 1)) path
              (let ((tc (regexp-captures "(?im)^#\\+filetags:[ \\t]*(.*)$" text)))
                (if (null? tc) '() (org-roam--split-tags (nth tc 1))))
              0
              (org-roam--parse-quoted-list (org-roam--file-meta front "ROAM_ALIASES"))))))

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
  (let ((info (org-roam--drawer-info lines-after)))
    (if info (car info) #f)))

;; (id aliases) for a drawer, or #f — the :ID: plus the drawer's :ROAM_ALIASES:
;; (so a subtree node carries its own aliases, not just the file's). Scans to
;; the drawer's :END: (or the next headline), since :ROAM_ALIASES: may sit
;; after the :ID: line.
(define (org-roam--drawer-info lines-after)
  (let loop ((ls lines-after) (k 0) (in-drawer #f) (id #f) (als #f))
    (cond ((or (null? ls) (> k *org-roam-drawer-scan-limit*))
           (if id (list id (or als '())) #f))
          ((>= (string-match "^[ \\t]*:PROPERTIES:[ \\t]*$" (car ls)) 0)
           (loop (cdr ls) (+ k 1) #t id als))
          ((>= (string-match "^[ \\t]*:END:[ \\t]*$" (car ls)) 0)
           (if id (list id (or als '())) #f))
          ((>= (string-match "^\\*+[ \\t]" (car ls)) 0) ; next headline
           (if id (list id (or als '())) #f))
          (in-drawer
           (let ((c (regexp-captures "^[ \\t]*:ID:[ \\t]+(\\S+)" (car ls)))
                 (a (if als #f
                        (regexp-captures "^[ \\t]*:ROAM_ALIASES:[ \\t]+(.*)$" (car ls)))))
             (loop (cdr ls) (+ k 1) #t
                   (if (null? c) id (nth c 1))
                   (cond (als als)
                         ((null? a) #f)
                         (else (org-roam--parse-quoted-list (nth a 1)))))))
          ((equal? (string-trim (car ls)) "") (loop (cdr ls) (+ k 1) in-drawer id als))
          (else #f)))) ; non-blank, non-drawer content before a drawer → none

;; All nodes in one file: the file-level node (if any) plus every headline that
;; carries an :ID: property (subtree nodes).
;;
;; One read per file. Returns a record:
;;   (file nodes refs links mtime)
;;     nodes   — (id title file (tag …) line (alias …)); aliases parsed from the
;;               file's :ROAM_ALIASES: (file node) or the node's drawer (subtree).
;;     refs    — (string …) parsed from ROAM_REFS
;;   links   — per-node outbound [[id:…]] targets (deduped):
;;             ((node-id link-id …) …). Each link is attributed to the
;;             nearest node at or above its line — the file-level node for
;;             front matter, a subtree node from its own heading line down
;;             (org-roam's per-node edge semantics).
;;   mtime   — the file's modification time (Unix seconds) at scan time; the
;;             cache loader re-checks it to rescan only what changed.
(define (org-roam--record-file r) (car r))
(define (org-roam--record-nodes r) (nth r 1))
(define (org-roam--record-refs r) (nth r 2))
(define (org-roam--record-links r) (nth r 3))
(define (org-roam--record-mtime r) (if (> (length r) 4) (nth r 4) 0))

;; Append IDS to NODE-ID's entry in a per-node link alist ((id links …) …).
(define (org-roam--links-add alist node-id ids)
  (if (null? ids)
      alist
      (let hit ((rs alist))
        (cond ((null? rs)
               ;; Just the new entry — the caller reconstructs the walked
               ;; prefix around us; returning ALIST here would duplicate it
               ;; (latent until a file attributed links to one node, then a
               ;; different one, then back).
               (list (cons node-id ids)))
              ((equal? (car (car rs)) node-id)
               (cons (cons node-id (append (cdr (car rs)) ids)) (cdr rs)))
              (else (cons (car rs) (hit (cdr rs))))))))
;; Drop empty per-node entries and dedupe each list.
(define (org-roam--links-finish alist)
  (let loop ((rs alist) (acc '()))
    (if (null? rs)
        (reverse acc)
        (let ((links (org-roam--dedupe (cdr (car rs)))))
          (loop (cdr rs)
                (if (null? links) acc (cons (cons (car (car rs)) links) acc)))))))

(define (org-roam--scan-file path)
  (let* ((text (read-file path))
         (lines (split-many text "\n"))
         (fl (org-roam--file-level-node text lines path)))
    (let loop ((ls lines) (i 0)
               (acc (if fl (list fl) '()))
               (cur (if fl (car fl) #f))
               ;; Outline level of the node links currently attribute to
               ;; (0 = file-level). Lets an ID-less heading revert
               ;; attribution to the file node once the ID'd subtree ENDS —
               ;; a sibling heading is not inside the previous one.
               (cur-level (if fl 0 #f))
               (links '()))
      (if (null? ls)
          (list path
                (reverse acc)
                (if fl (org-roam--parse-quoted-list (org-roam--file-meta text "ROAM_REFS")) '())
                (org-roam--links-finish links)
                (file-mtime path))
          ;; A heading that introduces a subtree node takes over attribution
          ;; from this line on (its own heading text included). A heading
          ;; WITHOUT an :ID: ends the ID'd subtree it follows when it is at
          ;; the same or a shallower level — links under it belong to the
          ;; file-level node (org-roam climbs to the nearest ancestor node;
          ;; a sibling is not one) — while a DEEPER ID-less heading is still
          ;; inside the subtree and stays attributed to it. The first-char
          ;; check skips the heading regex on the (vast majority of) body
          ;; lines — the hottest regex work in the scan loop.
          (let* ((l (car ls))
                 (ht (if (and (> (string-length l) 0)
                              (equal? (string-ref l 0) #\*))
                         (org-roam--heading-text l)
                         #f))
                 (stars (if ht
                            (let ((c (regexp-captures "^(\\*+)" l)))
                              (if (null? c) 0 (utf8-length (nth c 1))))
                            0))
                 (di (if ht (org-roam--drawer-info (cdr ls)) #f))
                 (id (if di (car di) #f))
                 (cur (cond (id id)
                            ((and ht cur-level (<= stars cur-level))
                             (if fl (car fl) #f))
                            (else cur)))
                 (cur-level (if ht stars cur-level))
                 (acc (if id
                          (cons (list id ht path '() i (if di (cadr di) '())) acc)
                          acc))
                 (targets (org-roam--link-ids (car ls))))
            (loop (cdr ls) (+ i 1) acc cur cur-level
                  (if (and cur (not (null? targets)))
                      (org-roam--links-add links cur targets)
                      links)))))))

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

;; Register the vault's fs watch + hook handler once per session — from the
;; first full sync AND from a cache load, so a cold start from cache still
;; sees external edits and new files.
(define (org-roam--register-watch)
  (when (not *org-roam-watching*)
    (set! *org-roam-watching* #t)
    (watch-tree "org-roam" (list (org-roam-directory)))
    (add-hook "directory-changed-hook" "org-roam-on-fs-change")))

;; C-c n s — (re)scan the roam directory into the index.
;;
;; Each file is read exactly once: `org-roam--scan-file` returns a per-file
;; record and the sync collects them into `*org-roam-files*`, then
;; `org-roam--rebuild-indexes` derives the flat node list, alias/ref lists and
;; the four hash indexes. No mtime field — staleness is handled by fs-watch and
;; the after-save per-file sync (scaling plan Task 5b).
(define (org-roam--sync)
  (set! *org-roam-files* '())
  (let ((dir (org-roam-directory)))
    (when (file-exists? dir)
      (for-each
       (lambda (f)
         ;; A file can vanish between walk-files and its read; reading a
         ;; missing file yields "" and would replace its record with an empty
         ;; one, so skip it — the record from the previous scan stays.
         (when (and (ends-with? f ".org") (file-exists? f))
           (set! *org-roam-files* (cons (org-roam--scan-file f) *org-roam-files*))))
       (walk-files dir))
      ;; Register the fs watch once, on the first successful index — repeat
      ;; syncs must not pile up watchers or hook entries.
      (org-roam--register-watch)))
  (org-roam--rebuild-indexes)
  (org-roam--save-cache-quiet)
  (set! *org-roam-synced* #t)
  (message (string-append "org-roam: indexed " (int->string (length *org-roam-nodes*)) " node(s), "
                          (int->string (length *org-roam-aliases*)) " alias(es)")))

(define-command (org-roam-db-sync) (org-roam--sync))
;; Debounced entry point for fs-watch bursts (a save fans out into several
;; directory-changed events; only the last one within 500ms re-scans).
(define org-roam--sync-debounced (make-debounce "org-roam-sync" 500 org-roam--sync))

;; Rebuild the flat node list, alias/ref lists and hash indexes from
;; *org-roam-files* — O(n) per sync, simpler than maintaining incrementally.
;; First-seen wins for id/title/file so a file's file-level node (first in its
;; record) stays the `by-file` label source, as the old linear scans did.
;; Aliases index from the nodes themselves (file-level `:ROAM_ALIASES:` for a
;; file node, its drawer's for a subtree node); the reverse link index
;; (`*org-roam-by-link*`) maps each link target to its source nodes.
(define (org-roam--rebuild-indexes)
  (let ((by-id (hash)) (by-title (hash)) (by-alias (hash)) (by-file (hash))
        (by-link (hash))
        (nodes '()) (aliases '()) (refs '()))
    (for-each
     (lambda (r)
       (for-each
        (lambda (n)
          (set! nodes (cons n nodes))
          (when (not (hash-contains? by-id (car n)))
            (set! by-id (hash-insert by-id (car n) n)))
          (when (not (hash-contains? by-title (nth n 1)))
            (set! by-title (hash-insert by-title (nth n 1) n)))
          (for-each
           (lambda (a)
             (set! aliases (cons (cons a n) aliases))
             (when (not (hash-contains? by-alias a))
               (set! by-alias (hash-insert by-alias a n))))
           (org-roam--node-aliases n))
          (when (or (= (org-roam--node-line n) 0)
                    (not (hash-contains? by-file (nth n 2))))
            (set! by-file (hash-insert by-file (nth n 2) n))))
        (org-roam--record-nodes r))
       (let ((fl (org-roam--file-level-of (org-roam--record-nodes r))))
         (when fl
           (for-each (lambda (x) (set! refs (cons (cons x fl) refs)))
                     (org-roam--record-refs r))))
       ;; Reverse edges: every [[id:…]] target gets its source nodes.
       (for-each
        (lambda (le)
          (let ((src (hash-try-get by-id (car le))))
            (when src
              (for-each
               (lambda (t)
                 (set! by-link
                       (hash-insert by-link t
                                    (cons src (or (hash-try-get by-link t) '())))))
               (cdr le)))))
        (org-roam--record-links r)))
     *org-roam-files*)
    (set! *org-roam-nodes* (reverse nodes))
    (set! *org-roam-aliases* (reverse aliases))
    (set! *org-roam-refs* (reverse refs))
    (set! *org-roam-by-id* by-id)
    (set! *org-roam-by-title* by-title)
    (set! *org-roam-by-alias* by-alias)
    (set! *org-roam-by-file* by-file)
    (set! *org-roam-by-link* by-link)))

;; Fold ONE record into the live indexes — the incremental half of a per-file
;; sync. Mirrors rebuild-indexes' per-record loop exactly (first-seen-wins for
;; id/title/file), so add-after-remove lands where a full rebuild would.
(define (org-roam--index-record r)
  (for-each
   (lambda (n)
     (set! *org-roam-nodes* (cons n *org-roam-nodes*))
     (when (not (hash-contains? *org-roam-by-id* (car n)))
       (set! *org-roam-by-id* (hash-insert *org-roam-by-id* (car n) n)))
     (when (not (hash-contains? *org-roam-by-title* (nth n 1)))
       (set! *org-roam-by-title* (hash-insert *org-roam-by-title* (nth n 1) n)))
     (for-each
      (lambda (a)
        (set! *org-roam-aliases* (cons (cons a n) *org-roam-aliases*))
        (when (not (hash-contains? *org-roam-by-alias* a))
          (set! *org-roam-by-alias* (hash-insert *org-roam-by-alias* a n))))
      (org-roam--node-aliases n))
     (when (or (= (org-roam--node-line n) 0)
               (not (hash-contains? *org-roam-by-file* (nth n 2))))
       (set! *org-roam-by-file* (hash-insert *org-roam-by-file* (nth n 2) n))))
   (org-roam--record-nodes r))
  (let ((fl (org-roam--file-level-of (org-roam--record-nodes r))))
    (when fl
      (for-each (lambda (x) (set! *org-roam-refs* (cons (cons x fl) *org-roam-refs*)))
                (org-roam--record-refs r))))
  ;; Reverse edges for the new record (its source nodes are indexed above).
  (for-each
   (lambda (le)
     (let ((src (hash-try-get *org-roam-by-id* (car le))))
       (when src
         (for-each
          (lambda (t)
            (set! *org-roam-by-link*
                  (hash-insert *org-roam-by-link* t
                               (cons src (or (hash-try-get *org-roam-by-link* t) '())))))
          (cdr le)))))
   (org-roam--record-links r)))

;; Strip ONE record's contributions from the live indexes — the removal half
;; of a per-file sync. Guarded removes: an entry is only dropped when it
;; actually points at THIS record's node (another file may legitimately hold
;; the same title, and first-seen-wins means the hash can hold either).
(define (org-roam--unindex-record r)
  (let ((file (org-roam--record-file r)))
    (for-each
     (lambda (n)
       (let ((id (car n)))
         (when (hash-contains? *org-roam-by-id* id)
           (set! *org-roam-by-id* (hash-remove *org-roam-by-id* id)))
         (let ((hit (hash-try-get *org-roam-by-title* (nth n 1))))
           (when (and hit (equal? (car hit) id))
             (set! *org-roam-by-title* (hash-remove *org-roam-by-title* (nth n 1)))))))
     (org-roam--record-nodes r))
    (when (hash-contains? *org-roam-by-file* file)
      (set! *org-roam-by-file* (hash-remove *org-roam-by-file* file)))
    (for-each
     (lambda (n)
       (for-each
        (lambda (a)
          (let ((hit (hash-try-get *org-roam-by-alias* a)))
            (when (and hit (equal? (nth hit 2) file))
              (set! *org-roam-by-alias* (hash-remove *org-roam-by-alias* a)))))
        (org-roam--node-aliases n)))
     (org-roam--record-nodes r))
    ;; Drop this record's outbound edges from the reverse index (every source
    ;; node it contributed, keyed by the record's own node ids).
    (for-each
     (lambda (le)
       (let ((src-id (car le)))
         (for-each
          (lambda (t)
            (let ((cur (hash-try-get *org-roam-by-link* t)))
              (when cur
                (let ((kept (filter (lambda (s) (not (equal? (car s) src-id))) cur)))
                  (set! *org-roam-by-link*
                        (if (null? kept)
                            (hash-remove *org-roam-by-link* t)
                            (hash-insert *org-roam-by-link* t kept)))))))
          (cdr le))))
     (org-roam--record-links r))
    (set! *org-roam-nodes*
          (filter (lambda (n) (not (equal? (nth n 2) file))) *org-roam-nodes*))
    (set! *org-roam-aliases*
          (filter (lambda (p) (not (equal? (nth (cdr p) 2) file))) *org-roam-aliases*))
    (set! *org-roam-refs*
          (filter (lambda (p) (not (equal? (nth (cdr p) 2) file))) *org-roam-refs*))))

;; --- Live sync (fs-watch + after-save) --------------------------------------
;; A change under the roam dir (new files, external edits, git) triggers a
;; debounced full re-sync. Edits saved from a buffer take the cheap per-file
;; path below, so the common case never rescans the tree.
;; Events within a second of our own cache write are that write's fs echo —
;; reacting would rescan the whole tree after every save. (A genuine external
;; edit inside the window is caught by the next save or full sync.) A negative
;; diff means the wall clock jumped backwards since the write (NTP correction,
;; sleep/resume drift) — the timestamp can no longer be trusted to age out, so
;; forget it and let the event through rather than suppressing indefinitely.
(define (org-roam--fs-event-fresh?)
  (let ((diff (- (string->number (current-time "%s")) *org-roam-last-cache-write*)))
    (if (< diff 0)
        (begin (set! *org-roam-last-cache-write* 0) #t)
        (> diff 1))))

(define (org-roam-on-fs-change dir)
  (when (and *org-roam-synced*
             (string? dir)
             (org-roam--under-vault? dir)
             (org-roam--fs-event-fresh?))
    (org-roam--sync-debounced)))

;; Vulpea pattern: replace one file's record in place and update ONLY that
;; record's index contributions (a full rebuild costs O(all nodes) per save).
;; Guarded to .org files under the roam directory — anything else is a silent
;; no-op, so a stray hook fire or hand call can't inject a foreign record.
(define (org-roam-db-sync-file path)
  (when (and *org-roam-synced*
             (string? path)
             (ends-with? path ".org")
             (org-roam--under-vault? path))
    (let ((old (let hit ((rs *org-roam-files*))
                 (cond ((null? rs) #f)
                       ((equal? (org-roam--record-file (car rs)) path) (car rs))
                       (else (hit (cdr rs)))))))
      (set! *org-roam-files*
            (filter (lambda (r) (not (equal? (org-roam--record-file r) path)))
                    *org-roam-files*))
      (when old (org-roam--unindex-record old))
      (when (file-exists? path)
        (let ((rec (org-roam--scan-file path)))
          (set! *org-roam-files* (cons rec *org-roam-files*))
          (org-roam--index-record rec)))
      ;; Stamp the fs window NOW: the .org write that triggered this sync is
      ;; ours, and the debounced cache write (below) doesn't land for 500ms —
      ;; without the stamp its events would schedule a full rescan first.
      (set! *org-roam-last-cache-write* (string->number (current-time "%s")))
      (org-roam--save-cache-debounced))))

;; after-save-hook: the common case — the user edits an open roam buffer and
;; saves. Registered at load; guarded so it is a no-op before the first sync
;; and outside the roam directory.
(define (org-roam-on-save)
  (let ((f (buffer-file-name)))
    (when (and *org-roam-synced*
               (not (equal? f ""))
               (ends-with? f ".org")
               (org-roam--under-vault? f))
      (org-roam-db-sync-file f)
      ;; The .org write is OUR save — its fs events within the window are that
      ;; save's echo, and the per-file sync above already indexed the content.
      ;; (save-cache-quiet alone can't stamp it: a plain body edit leaves the
      ;; cache bytes unchanged, so nothing is written and nothing is stamped —
      ;; the .org event would then schedule a full rescan of the vault.)
      (set! *org-roam-last-cache-write* (string->number (current-time "%s"))))))
(add-hook "after-save-hook" "org-roam-on-save")

(define (org-roam--ensure-index)
  (when (not *org-roam-synced*)
    ;; A cache load only makes sense when there is no in-session index yet;
    ;; after e.g. a rename (which sets *org-roam-synced* #f) the records are
    ;; still current, so fall through to a full sync instead.
    (if (null? *org-roam-files*)
        (when (not (org-roam--load-cache)) (org-roam--sync))
        (org-roam--sync))))

;; Completion candidates for the find/insert/tag pickers: node titles and
;; registered aliases. A title held by MORE THAN ONE node is annotated with the
;; node's file (— <basename>) so the candidates are distinguishable; the
;; label→node map rides in *org-roam-find-targets* and the picker callbacks
;; resolve through it. Unambiguous titles stay bare (plain completion behavior).
(define *org-roam-find-targets* (hash))
(define (org-roam--picker-candidates ns)
  (set! *org-roam-find-targets* (hash))
  (let ((counts (hash)))
    (for-each (lambda (n)
                (let ((t (nth n 1)))
                  (set! counts (hash-insert counts t
                                            (+ 1 (or (hash-try-get counts t) 0))))))
              ns)
    (let loop ((ns ns) (acc '()))
      (if (null? ns)
          ;; Aliases last (as org-roam--titles ordered them). An alias maps to
          ;; one node; keep its bare spelling as the label.
          (let ((aliases (let al-loop ((as *org-roam-aliases*) (a-acc '()))
                           (if (null? as)
                               (reverse a-acc)
                               (al-loop (cdr as)
                                        (let ((label (car (car as))))
                                          (when (not (hash-contains? *org-roam-find-targets* label))
                                            (set! *org-roam-find-targets*
                                                  (hash-insert *org-roam-find-targets* label (cdr (car as)))))
                                          (cons label a-acc)))))))
            (append (reverse acc) aliases))
          (let* ((n (car ns))
                 (title (nth n 1))
                 (label (if (> (or (hash-try-get counts title) 0) 1)
                            (string-append title " — " (basename (nth n 2)))
                            title)))
            (when (not (hash-contains? *org-roam-find-targets* label))
              (set! *org-roam-find-targets* (hash-insert *org-roam-find-targets* label n)))
            (loop (cdr ns) (cons label acc)))))))
;; Resolve a picker selection: exact label first (covers annotated duplicates),
;; then the plain title, then it's a new node.
(define (org-roam--picker-resolve label)
  (or (hash-try-get *org-roam-find-targets* label)
      (org-roam--node-by-title label)))
;; Hash-table lookups (rebuilt by org-roam--rebuild-indexes) — O(1) instead of
;; the old linear scans. Title lookup falls back to the alias index.
(define (org-roam--node-by-id id)
  (hash-try-get *org-roam-by-id* id))
(define (org-roam--node-by-title t)
  (or (hash-try-get *org-roam-by-title* t)
      (hash-try-get *org-roam-by-alias* t)))
(define (org-roam--node-by-file f)
  (hash-try-get *org-roam-by-file* f))
;; Insert a single freshly created node into the flat list + indexes without a
;; rebuild (create-node paths; the fs watch picks up the new file afterwards).
(define (org-roam--index-insert n)
  (set! *org-roam-nodes* (cons n *org-roam-nodes*))
  (set! *org-roam-by-id* (hash-insert *org-roam-by-id* (car n) n))
  (set! *org-roam-by-title* (hash-insert *org-roam-by-title* (nth n 1) n))
  (when (not (hash-contains? *org-roam-by-file* (nth n 2)))
    (set! *org-roam-by-file* (hash-insert *org-roam-by-file* (nth n 2) n))))

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
    ;; Route through the per-file sync so the scan record + cache pick the node
    ;; up immediately (index-insert alone would leave the record list — and
    ;; thus edges and the TSV — without it). The file always ends in .org.
    (org-roam-db-sync-file file)
    node))

;; ===========================================================================
;; Find / insert / capture
;; ===========================================================================
;; C-c n f — pick a node by title (fuzzy) and visit it; a new title creates one.
(define-command (org-roam-node-find)
  (org-roam--ensure-index)
  (completing-read "Node: " (org-roam--picker-candidates *org-roam-nodes*) 'org-roam--find-finish))
(define (org-roam--find-finish title)
  (when (not (equal? (string-trim title) ""))
    (let ((n (or (org-roam--picker-resolve (string-trim title))
                 (org-roam--create-node (string-trim title)))))
      (open-file (nth n 2)))))

;; C-c n i — insert an [[id:…][title]] link to a node (creating it if new).
(define-command (org-roam-node-insert)
  (org-roam--ensure-index)
  (completing-read "Insert node: " (org-roam--picker-candidates *org-roam-nodes*) 'org-roam--insert-finish))
(define (org-roam--insert-finish title)
  (when (not (equal? (string-trim title) ""))
    (let ((n (or (org-roam--picker-resolve (string-trim title))
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
  ;; Sources come from the reverse link index (*org-roam-by-link*) — no vault
  ;; grep, and each hit is the NODE the link lives in (a subtree link names its
  ;; subtree), not a raw grep line.
  (let ((srcs (let ((hit (hash-try-get *org-roam-by-link* id)))
                (if hit hit '()))))
    (let loop ((ns srcs) (i 2) (out '()))
      (if (null? ns)
          (render-buffer "*org-roam-backlinks*" "org-roam-backlinks-mode"
            (join-lines
             (append (list (string-append "Backlinks to: " title
                                          "  (" (int->string (length srcs)) ")")
                           "")
                     (reverse out)))
            '())
          (let* ((n (car ns))
                 (label (nth n 1))
                 (file (nth n 2)))
            (set! *org-roam-backlink-targets*
                  (cons (cons i (cons file (org-roam--node-line n))) *org-roam-backlink-targets*))
            (loop (cdr ns) (+ i 1)
                  (cons (string-append label "  :  " (basename file)) out)))))))

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
;; Every node ID referenced by `[[id:…]]` links in `text`, in order. One
;; regexp-all-matches call: the whole match is "[[id:<id>" (the pattern stops
;; before `]`), so the id is the match minus its 5-char prefix — no per-link
;; string-match + regexp-captures double scan.
(define (org-roam--link-ids text)
  (map (lambda (m) (string-trim (substring m 5 (string-length m))))
       (regexp-all-matches "\\[\\[id:[^]\n]+" text)))

;; ((source-id . target-id) …) edges between known nodes (self-loops dropped).
;; Sourced from the scan records' per-node link lists — no file re-reads, and
;; links inside a subtree are attributed to that subtree's node, not the file's.
(define (org-roam--edges)
  ;; cons + one reverse — appending inside the loop copies the whole
  ;; accumulator per link (quadratic at vault scale).
  (let loop ((rs *org-roam-files*) (acc '()))
    (if (null? rs)
        (reverse acc)
        (let inner ((ls (org-roam--record-links (car rs))) (acc acc))
          (if (null? ls)
              (loop (cdr rs) acc)
              (inner (cdr ls)
                     (let add ((ts (cdr (car ls))) (acc acc))
                       (if (null? ts)
                           acc
                           (let ((t (car ts)))
                             (add (cdr ts)
                                  (if (or (equal? (car (car ls)) t)
                                          (not (hash-try-get *org-roam-by-id* t)))
                                      acc
                                      (cons (cons (car (car ls)) t) acc))))))))))))

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
    ;; Index the new daily note in place instead of invalidating the whole
    ;; index (a no-op when the session isn't synced yet — the first sync
    ;; catches it).
    (org-roam-db-sync-file f)
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
    (org-roam--picker-candidates (filter (lambda (n) (member tag (nth n 3))) *org-roam-nodes*))
    'org-roam--find-finish))

;; C-c n e — pick a ROAM_REFS ref (a URL / citex key) and visit its node.
(define-command (org-roam-ref-find)
  (org-roam--ensure-index)
  (if (null? *org-roam-refs*)
      (message "org-roam: no refs indexed")
      (completing-read "Ref: " (map car *org-roam-refs*) 'org-roam--ref-find-finish)))
(define (org-roam--ref-find-finish ref)
  (let ((hit (assoc ref *org-roam-refs*)))
    (if (not hit)
        (message (string-append "org-roam: no node for ref " ref))
        (open-file (nth (cdr hit) 2)))))

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
                ;; No index invalidation: the buffer's file is re-indexed
                ;; per-file by `org-roam-on-save` when the user saves.
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
             (t2 (replace-regexp-in-string "(?im)^#\\+title:[ \\t]*.*$"
                                           (replacement-literal (string-append "#+title: " nt))
                                           text)))
        (replace-region 0 (buffer-size) (org-roam--rewrite-links t2 id nt)))
      ;; every other file: rewrite inbound link descriptions, and sync each
      ;; changed file's record INCREMENTALLY (a full rescan of the vault — the
      ;; old cost — is unnecessary when we know exactly which files changed).
      (let ((cur (let ((f (buffer-file-name)))
                   (if (equal? f "") (path-join (buffer-directory) (current-buffer)) f)))
            (changed '()))
        (for-each (lambda (f)
                    (when (and (ends-with? f ".org") (not (equal? f cur)))
                      (let* ((t (read-file f)) (t2 (org-roam--rewrite-links t id nt)))
                        (when (not (equal? t t2))
                          (write-file f t2)
                          (set! changed (cons f changed))))))
                  (walk-files (org-roam-directory)))
        (for-each org-roam-db-sync-file changed))
      ;; The current buffer is UNSAVED (its edits are queued commands), so its
      ;; on-disk record still carries the old title — update the node in the
      ;; live indexes now; org-roam-on-save re-scans the file when it lands.
      (let ((n (org-roam--node-by-id id)))
        (when n
          (set! *org-roam-by-title* (hash-remove *org-roam-by-title* (nth n 1)))
          (set! *org-roam-by-title* (hash-insert *org-roam-by-title* nt n))
          (set! *org-roam-nodes*
                (map (lambda (x)
                       (if (equal? (car x) id)
                           (list id nt (nth x 2) (nth x 3) (nth x 4)
                                 (if (> (length x) 5) (nth x 5) '()))
                           x))
                     *org-roam-nodes*))))
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
                          (equal? (nth p 0) (org-roam--cache-file))
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
;; Persistent index cache
;; ===========================================================================
;; TSV, one row per node: id <tab> title <tab> file <tab> tags <tab> line — plus
;; a sixth column with that node's outbound link-ids (,-joined), a seventh with
;; the node's ROAM_ALIASES (file-level `:ROAM_ALIASES:` for a file node, its
;; drawer's for a subtree node), an eighth with the file's ROAM_REFS on the
;; file-level row (line 0) only, and a LAST column with the file's mtime on
;; every row — so graph edges, alias/ref lookups and staleness detection all
;; work straight from cache. (Older caches lacked the per-row alias column on
;; subtree rows: there the mtime sat in column 7 and rows were one column
;; shorter; the loader distinguishes the formats by row length.)
;;
;; The cache lives BESIDE the notes (per-vault, travels with it), so two things
;; need care: (1) writing it fires the vault's fs watch — `save-cache-quiet`
;; stamps the write time and `org-roam-on-fs-change` ignores events within the
;; self-write window instead of feeding a full rescan with itself; (2) its title
;; column could pollute grep-based searches — `walk-files` only indexes .org, and
;; `org-roam-unlinked-references` drops hits whose path is the cache file.
(define (org-roam--cache-file) (path-join (org-roam-directory) ".org-roam-cache.tsv"))
;; Unix seconds of the last cache write (0 = never). fs events arriving within
;; the window are assumed to be that write's echo, not user edits.
(define *org-roam-last-cache-write* 0)
(define (org-roam--node-links-of links id)
  (let loop ((ls links))
    (cond ((null? ls) '()) ((equal? (car (car ls)) id) (cdr (car ls))) (else (loop (cdr ls))))))

;; --- TSV field escaping ------------------------------------------------------
;; A raw tab in a field shifts the column split; a raw comma in a joined list
;; (tags / aliases / link-ids) shifts the list split. Escape on write — backslash
;; first, then tab, then comma — and unescape on load. `replacement-literal`
;; keeps the replacements from being read as regex group references.
(define (org-roam--tsv-escape s)
  (let* ((s (replace-regexp-in-string "\\\\" (replacement-literal "\\\\") s))
         (s (replace-regexp-in-string "\t" (replacement-literal "\\t") s))
         (s (replace-regexp-in-string "," (replacement-literal "\\c") s)))
    s))
;; Inverse of org-roam--tsv-escape. Scanner form — string-match offsets are
;; BYTE-based, so advance with byte-drop and take the prefix with byte-take
;; (same shape as org-roam--parse-quoted-list).
(define (org-roam--tsv-unescape s)
  (let loop ((str s) (acc '()))
    (let ((m (string-match "\\\\(.)" str)))
      (if (< m 0)
          (apply string-append (reverse (cons str acc)))
          (let* ((c (regexp-captures "\\\\(.)" str))
                 (whole (car c))
                 (lit (cond ((equal? (nth c 1) "t") "\t")
                            ((equal? (nth c 1) "c") ",")
                            ((equal? (nth c 1) "\\") "\\")
                            (else whole))))
            (loop (byte-drop str (+ m (utf8-length whole)))
                  (cons lit (cons (byte-take str m) acc))))))))

(define (org-roam--node-row n links refs mtime)
  ;; Escape ELEMENTS, not the joined field: the join commas must stay raw for
  ;; the load-side split, while a comma inside a tag/alias becomes \c.
  (let ((tags (string-join (map org-roam--tsv-escape (nth n 3)) ","))
        (lnks (string-join (map org-roam--tsv-escape (org-roam--node-links-of links (car n))) ","))
        (als (string-join (map org-roam--tsv-escape (org-roam--node-aliases n)) ","))
        (line (int->string (org-roam--node-line n)))
        (mt (int->string mtime)))
    (if (= (org-roam--node-line n) 0)
        (string-join (list (nth n 0) (org-roam--tsv-escape (nth n 1)) (org-roam--tsv-escape (nth n 2))
                           tags line lnks als
                           (string-join (map org-roam--tsv-escape refs) ",")
                           mt) "\t")
        (string-join (list (nth n 0) (org-roam--tsv-escape (nth n 1)) (org-roam--tsv-escape (nth n 2))
                           tags line lnks als mt) "\t"))))
(define (org-roam--cache-content)
  (join-lines
   (apply append
     (map (lambda (r)
            (let ((refs (org-roam--record-refs r))
                  (mt (org-roam--record-mtime r)))
              (map (lambda (n) (org-roam--node-row n (org-roam--record-links r) refs mt))
                   (org-roam--record-nodes r))))
          *org-roam-files*))))
;; Write the cache silently, but only when its content actually changed: the
;; file lives in the watched roam dir, and writing unchanged content would make
;; the fs-watch → sync → save cycle feed itself.
(define (org-roam--save-cache-quiet)
  (let* ((f (org-roam--cache-file))
         (content (org-roam--cache-content)))
    (when (or (not (file-exists? f))
              (not (equal? (read-file f) content)))
      ;; Stamp only when a write actually happens — a no-op save shouldn't
      ;; suppress the window for a genuine external edit.
      (set! *org-roam-last-cache-write* (string->number (current-time "%s")))
      (write-file f content))))
;; Debounced cache write for the per-file sync path: rebuilding the whole TSV
;; per save is O(vault), and rapid saves would pay it every time. The debounced
;; write stamps the fs window when it actually lands; the caller stamps its own
;; .org write immediately (see org-roam-on-save).
(define org-roam--save-cache-debounced
  (make-debounce "org-roam-cache-save" 500 org-roam--save-cache-quiet))
(define-command (org-roam-db-save)
  (org-roam--ensure-index)
  (org-roam--save-cache-quiet)
  (message "org-roam: cache saved"))
(define-command (org-roam-db-load)
  (if (org-roam--load-cache)
      (message (string-append "org-roam: loaded " (int->string (length *org-roam-nodes*)) " node(s) from cache"))
      (message "org-roam: no cache (run C-c n s)")))
;; Fold the TSV rows into per-file records; #t on success, #f when absent.
;; Rows are written grouped by file, and the loader exploits that: a row whose
;; file matches the HEAD record merges in O(1) — no accumulator scan, no
;; filter-rebuild (both were quadratic at vault scale). A `seen` hash catches
;; hand-edited caches where a file's rows appear again later; those take the
;; old linear-merge path.
(define (org-roam--load-cache)
  (let ((f (org-roam--cache-file)))
    (if (not (file-exists? f))
        #f
        (let loop ((rows (remove-equal "" (split-many (read-file f) "\n")))
                   (acc '())
                   (seen (hash)))
          (if (null? rows)
              (begin
                (set! *org-roam-files* (org-roam--reconcile-cache acc))
                (org-roam--rebuild-indexes)
                (org-roam--register-watch)
                (set! *org-roam-synced* #t)
                #t)
              (let* ((parts (split-many (car rows) "\t"))
                     (np (length parts))
                     ;; A hand-edited/corrupted row with a non-numeric line
                     ;; column is dropped like a short row — admitting the node
                     ;; with a #f line would error in org-roam--rebuild-indexes
                     ;; ((= line 0) on #f) and abort the whole load.
                     (line (if (< np 5) #f (string->number (nth parts 4)))))
                (if (not line)
                    (loop (cdr rows) acc seen)
                    ;; The FILE path is the record key — it must be unescaped
                    ;; BEFORE any comparison, or a file like `foo, bar.org`
                    ;; loads under its escaped form and per-file syncs then
                    ;; add a second record for the real path.
                    ;;
                    ;; Column layout (mtime LAST): aliases live on every row
                    ;; with 8+ columns (older caches: subtree rows were 7 wide
                    ;; with the mtime in column 7); refs ride the file-level
                    ;; row (line 0) when 9 columns are present.
                    (let* ((file (org-roam--tsv-unescape (nth parts 2)))
                           (als (if (>= np 8)
                                    (map org-roam--tsv-unescape
                                         (remove-equal "" (split-many (nth parts 6) ",")))
                                    '()))
                           (refs (if (and (= line 0) (>= np 9))
                                     (map org-roam--tsv-unescape
                                          (remove-equal "" (split-many (nth parts 7) ",")))
                                     '()))
                           (mtime (or (string->number (nth parts (- np 1))) 0))
                           (node (list (nth parts 0)
                                       (org-roam--tsv-unescape (nth parts 1))
                                       file
                                       (map org-roam--tsv-unescape
                                            (remove-equal "" (split-many (nth parts 3) ",")))
                                       line
                                       als))
                           (ids (if (> np 5)
                                    (map org-roam--tsv-unescape
                                         (remove-equal "" (split-many (nth parts 5) ",")))
                                    '()))
                           (links (if (null? ids)
                                      '()
                                      (list (cons (nth parts 0) ids)))))
                      (cond
                       ;; Fast path: rows arrive grouped by file, so a match
                       ;; against the head record covers every row the writer
                       ;; produced. Refs ride the file-level row (line 0) —
                       ;; i.e. the group's FIRST row, which created this
                       ;; record.
                       ((and (pair? acc)
                             (equal? (org-roam--record-file (car acc)) file))
                        (let ((head (car acc)))
                          (loop (cdr rows)
                                (cons (list file
                                            ;; Rows arrive in ascending line
                                            ;; order, so APPEND keeps the node
                                            ;; order a fresh scan would
                                            ;; produce — first-seen-wins in
                                            ;; the indexes then matches a
                                            ;; rescan.
                                            (append (org-roam--record-nodes head) (list node))
                                            (if (null? (org-roam--record-refs head)) refs (org-roam--record-refs head))
                                            (append links (org-roam--record-links head))
                                            (org-roam--record-mtime head))
                                      (cdr acc))
                                seen)))
                       ;; Hand-edited cache: this file's rows continue (or
                       ;; resume) after other files intervened — linear merge.
                       ((hash-contains? seen file)
                        (let hit ((rs acc))
                          (if (null? rs)
                              (loop (cdr rows) acc seen)
                              (if (equal? (org-roam--record-file (car rs)) file)
                                  ;; Merge the row into the existing record.
                                  (let ((merged (list file
                                                      (append (org-roam--record-nodes (car rs)) (list node))
                                                      (if (null? (org-roam--record-refs (car rs))) refs (org-roam--record-refs (car rs)))
                                                      (append links (org-roam--record-links (car rs)))
                                                      (org-roam--record-mtime (car rs)))))
                                    (loop (cdr rows)
                                          (cons merged
                                                (filter (lambda (r) (not (equal? (org-roam--record-file r) file)))
                                                        acc))
                                          seen))
                                  (hit (cdr rs))))))
                       (else
                        (loop (cdr rows)
                              (cons (list file (list node) refs links mtime) acc)
                              (hash-insert seen file #t))))))))))))

;; Bring the records a cache load produced back in line with the disk — what
;; makes a cold start honest:
;;   deleted — records whose file no longer exists are dropped;
;;   changed — records whose stored mtime differs from the file's current
;;             mtime are re-scanned (a row with no readable mtime — an older
;;             cache format — is treated as changed);
;;   added   — .org files on disk that the cache never heard of are scanned.
;; Only the differing files are read; unchanged ones keep their cached records.
(define (org-roam--reconcile-cache acc)
  (let ((keep (let scan ((rs acc) (out '()))
                (if (null? rs)
                    out
                    (let* ((r (car rs))
                           (f (org-roam--record-file r)))
                      (cond ((not (file-exists? f)) (scan (cdr rs) out))
                            ((not (= (org-roam--record-mtime r) (file-mtime f)))
                             (scan (cdr rs) (cons (org-roam--scan-file f) out)))
                            (else (scan (cdr rs) (cons r out)))))))))
    (let ((known (let tab ((rs keep) (h (hash)))
                   (if (null? rs) h (tab (cdr rs) (hash-insert h (org-roam--record-file (car rs)) #t))))))
      (let loop ((fs (walk-files (org-roam-directory))) (out keep))
        (if (null? fs)
            out
            (loop (cdr fs)
                  (if (and (ends-with? (car fs) ".org")
                           (not (hash-contains? known (car fs))))
                      (cons (org-roam--scan-file (car fs)) out)
                      out)))))))

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
    (if (ends-with? file ".org")
        (begin
          (org-roam-db-sync-file file)
          ;; A template body without an :ID: yields a scan record with no
          ;; nodes, so the per-file sync indexes nothing — keep the in-memory
          ;; node the old path inserted so the capture is still reachable.
          (when (not (org-roam--node-by-id id))
            (org-roam--index-insert node)))
        (org-roam--index-insert node))
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
(define-key "org-mode" "ctrl-c n e" 'org-roam-ref-find)
(define-key "fundamental" "ctrl-c n e" 'org-roam-ref-find)
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
