;; requires: core

;; Title-bar menus — dropdowns in the title bar, right of the traffic lights.
;; Each menu is named by its title (the trigger's label) and holds rows of
;; (label . command); clicking a row runs its command. Menus appear left to
;; right in the order they were first defined, rows in insertion order. The
;; built-in "projects" switcher (perspectives.scm) is one of these menus.
;;
;;   (titlebar-menu-add "Dashboards" "Postgres"   'psql-dashboard)
;;   (titlebar-menu-add "Dashboards" "Kubernetes" 'k8s-dashboard)
;;   (titlebar-menu-add "Tools" "Scratch" (lambda () (switch-to-buffer "*scratch*")))
;;
;;   (titlebar-menu-hide "projects")   ; turn off the projects switcher
;;
;; A command is a function name (symbol or string) or a no-argument procedure.
;; Prefer a name for anything a package defines: it's looked up when the row is
;; clicked, so it works even before the package has loaded. A lambda's body is
;; resolved when the lambda is written, so it may only call functions that
;; already exist.
;;
;; Generated menus (like projects) replace their rows wholesale with
;; `titlebar-menu-set!`, pairing labels with "" commands and an 'on-select
;; function that receives the picked label (see `titlebar-menu-configure!`).
;;
;; All the logic lives here — Rust only draws the dropdowns (workspace.rs) and
;; calls the clicked row's command by name (`set-titlebar-menu`).

;; alist: title -> settings alist with keys
;;   rows      list of (label . command-name)
;;   active    the checkmarked label ("" for none)
;;   section   the heading over the rows ("" = the title)
;;   on-select function name called with the label of a row whose command is ""
;;   hidden    #t keeps the menu out of the title bar
(define *titlebar-menus* '())

(define (titlebar-menu-get title key default)
  (let* ((menu (or (alist-ref *titlebar-menus* title) '()))
         (hit (assoc key menu)))
    (if hit (cdr hit) default)))

;; Publish `title` to the title bar (an empty or hidden menu shows nothing but
;; keeps its place).
(define (titlebar-menu-sync title)
  (let ((rows (if (titlebar-menu-get title 'hidden #f)
                  '()
                  (titlebar-menu-get title 'rows '()))))
    (set-titlebar-menu title
                       (map car rows)
                       (map cdr rows)
                       (titlebar-menu-get title 'active "")
                       (titlebar-menu-get title 'section "")
                       (titlebar-menu-get title 'on-select ""))))

;; Set settings `key` -> `value` pairs (a flat list) on `title`, creating the
;; menu on first use, then publish it.
(define (titlebar-menu-update! title kvs)
  (define (apply-kvs menu kvs)
    (if (or (null? kvs) (null? (cdr kvs)))
        menu
        (apply-kvs (cons (cons (car kvs) (cadr kvs)) (alist-remove menu (car kvs)))
                   (cddr kvs))))
  (let ((menu (alist-ref *titlebar-menus* title)))
    (set! *titlebar-menus*
          (if menu
              (map (lambda (kv) (if (equal? (car kv) title) (cons title (apply-kvs menu kvs)) kv))
                   *titlebar-menus*)
              (append *titlebar-menus* (list (cons title (apply-kvs '() kvs)))))))
  (titlebar-menu-sync title))

;; Set options on the menu `title` (created, empty, on first use — which fixes
;; its position): 'section "Heading over the rows", 'on-select 'fn (called with
;; the label of a row whose command is "").
;;   (titlebar-menu-configure! "projects" 'section "Recent Projects" 'on-select 'perspective-select)
(define (titlebar-menu-configure! title . opts)
  (titlebar-menu-update!
   title
   (let loop ((opts opts))
     (if (or (null? opts) (null? (cdr opts)))
         '()
         (cons (car opts)
               (cons (if (equal? (car opts) 'on-select) (hook->name (cadr opts)) (cadr opts))
                     (loop (cddr opts))))))))

;; Replace all of `title`'s rows (a list of (label . command)) and its active
;; label in one publish.
(define (titlebar-menu-set! title rows active)
  (titlebar-menu-update!
   title
   (list 'rows (map (lambda (r) (cons (car r) (hook->name (cdr r)))) rows)
         'active active)))

;; Add the row `label` running `command` to the menu `title` (created on first
;; use). Re-adding a label replaces its command in place.
(define (titlebar-menu-add title label command)
  (let ((name (hook->name command))
        (rows (titlebar-menu-get title 'rows '())))
    (titlebar-menu-update!
     title
     (list 'rows
           (if (alist-ref rows label)
               (map (lambda (kv) (if (equal? (car kv) label) (cons label name) kv)) rows)
               (append rows (list (cons label name))))))))

;; Drop the row `label` from the menu `title`.
(define (titlebar-menu-remove title label)
  (titlebar-menu-update! title (list 'rows (alist-remove (titlebar-menu-get title 'rows '()) label))))

;; Drop every row from the menu `title` (it disappears until rows are added).
(define (titlebar-menu-clear title)
  (titlebar-menu-update! title (list 'rows '() 'active "")))

;; Hide / show the menu `title` without touching its rows. A hidden menu stays
;; hidden however its owner refreshes it.
(define (titlebar-menu-hide title) (titlebar-menu-update! title (list 'hidden #t)))
(define (titlebar-menu-show title) (titlebar-menu-update! title (list 'hidden #f)))
