---
title: Title-bar menus
description: Add dropdowns of labelled quick actions to the title bar from your init.scm, or hide the built-in projects switcher.
---

# Title-bar menus

The title bar can hold dropdown menus, just right of the traffic lights. Each menu has a title (the label you click) and a list of rows, and each row runs a command. The built-in [projects switcher](/docs/projects#perspectives) is one of these menus, and you can add your own from `init.scm` — handy for the commands you'd otherwise reach for with <kbd>M-x</kbd>: dashboards, notes, a scratch buffer.

Menus are defined entirely in Scheme. Macros only draws them and runs the command you pick.

## Adding a menu

`titlebar-menu-add` adds a row to a menu, creating the menu the first time you name it:

```scheme
(titlebar-menu-add "dashboards" "postgres"   'psql-dashboard)
(titlebar-menu-add "dashboards" "kubernetes" 'k8s-dashboard)
(titlebar-menu-add "dashboards" "analytics"  'analytics)
```

That puts a **dashboards ▾** trigger in the title bar. Clicking it unfolds the three rows; clicking a row folds the menu and runs its command.

Menus appear left to right in the order they're first defined, after the projects switcher. Rows keep the order you add them in. Adding a label that's already in the menu replaces its command and keeps its place.

Like the projects switcher, every menu has a search field: type to filter the rows, use <kbd>↑</kbd>/<kbd>↓</kbd> (or <kbd>C-p</kbd>/<kbd>C-n</kbd>) and <kbd>Enter</kbd> to pick from the keyboard, and press <kbd>Escape</kbd> (or click outside) to fold the menu without running anything.

### Commands: names or lambdas

A row's command is either the **name** of a function (a quoted symbol or a string) or a **no-argument procedure**:

```scheme
(titlebar-menu-add "tools" "scratch" (lambda () (switch-to-buffer "*scratch*")))
(titlebar-menu-add "tools" "notes"   (lambda () (open-file "~/notes.org")))
```

Prefer a name for anything a [package](/docs/packages) defines. A name is looked up when you click the row, so it works no matter when the package loads. A lambda's body is resolved when the lambda is written (Steel resolves free identifiers at definition time), so it can only call functions that already exist at that point in your config.

## Changing a menu

| Function | What it does |
| --- | --- |
| `(titlebar-menu-add title label command)` | Add a row, or replace the command of an existing `label` |
| `(titlebar-menu-remove title label)` | Remove one row |
| `(titlebar-menu-clear title)` | Remove every row (the menu disappears until rows are added) |
| `(titlebar-menu-hide title)` | Take the menu out of the title bar, keeping its rows |
| `(titlebar-menu-show title)` | Put a hidden menu back |

A menu with no rows isn't drawn, but it keeps its position: add rows to it again and it reappears in the same slot.

## Hiding the projects switcher

The projects switcher is the menu named `projects`. It's on by default; to turn it off:

```scheme
(titlebar-menu-hide "projects")
```

It stays hidden even as you visit files and its project list refreshes. Perspectives keep tracking in the background, and <kbd>C-c p s</kbd> (`perspective-switch`) still switches between them. See [Projects & search](/docs/projects#perspectives).

## Generated menus

For a menu whose rows come from data — like the projects switcher, which lists every project you've touched — replace all the rows at once with `titlebar-menu-set!`, and send picks to a single function with `titlebar-menu-configure!`:

```scheme
(titlebar-menu-configure! "branches"
                          'section "Local branches"     ; heading over the rows
                          'on-select 'my-checkout)      ; called with the picked label

(define (my-refresh-branches)
  (titlebar-menu-set! "branches"
                      (map (lambda (b) (cons b "")) (my-branch-names))
                      (my-current-branch)))             ; checkmarked row ("" for none)
```

`titlebar-menu-set!` takes a list of `(label . command)` pairs and the active label. A row whose command is `""` calls the menu's `'on-select` function with its label. The active label gets a checkmark in the list.

`'section` sets the dim heading over the rows; it defaults to the menu's title. Calling `titlebar-menu-configure!` early (before any rows exist) also reserves the menu's position in the title bar — that's how the projects switcher stays leftmost.
