---
title: Hex editor
description: Open binary files as bytes, edit them in a hex or text column with Emacs, evil, or helix keys, search for byte patterns, read values with the data inspector, and save byte-exact.
---

# Hex editor

Any file that isn't text opens as a **hex buffer**: with <kbd>C-x C-f</kbd>, from the command line, or by dragging it onto a window. A file counts as binary when it isn't valid UTF-8 or has a NUL byte in its first 8 KiB. Text files still open as text, CSV files as a [grid](/docs/grid), and a dropped folder opens in [dired](/docs/dired).

Each row shows an offset, the bytes in hex, and the same bytes as ASCII. Bytes are coloured by kind: NUL, printable, whitespace, other control characters, and bytes with the high bit set. The strip at the bottom shows where point is, whether you're overwriting or inserting, and the **data inspector**: the bytes at point read as binary, `u8`/`i8` through `u64`/`i64`, `f32`, `f64`, and UTF-8.

To open any file as hex, text included, use <kbd>M-x hex-find-file-prompt</kbd>. <kbd>M-x hex-mode</kbd> shows the current text buffer as hex, and <kbd>C-c C-t</kbd> switches back to text when the bytes are valid UTF-8.

## Typing

- **Hex column:** `0`–`9` and `a`–`f` set the nibble under the cursor, high then low.
- **Text column:** <kbd>Tab</kbd> switches columns. Printable keys write their characters.
- **Overwrite or insert:** typing overwrites by default, so the file keeps its size. <kbd>Insert</kbd> (or <kbd>C-c C-o</kbd>) switches to inserting bytes.
- Typing past the last byte appends.
- A run of typing undoes as one step, and saving an unedited buffer writes back exactly the bytes it read.

Under evil or helix, plain keys are commands. <kbd>i</kbd> or <kbd>R</kbd> starts typing, <kbd>Esc</kbd> stops, and <kbd>g i</kbd> toggles insert.

## Keys

The usual motion and editing commands act on bytes, so the keys you already use work: <kbd>C-f</kbd>/<kbd>C-b</kbd> by byte, <kbd>C-n</kbd>/<kbd>C-p</kbd> by row, <kbd>M-f</kbd>/<kbd>M-b</kbd> by group, <kbd>C-a</kbd>/<kbd>C-e</kbd>, <kbd>C-v</kbd>/<kbd>M-v</kbd>, <kbd>M-&lt;</kbd>/<kbd>M-&gt;</kbd>, <kbd>C-SPC</kbd>, <kbd>C-x h</kbd>, <kbd>C-w</kbd>, <kbd>M-w</kbd>, <kbd>C-y</kbd>, <kbd>C-/</kbd>, and <kbd>C-x C-s</kbd>. Under evil, `:w`, `:wq`, your leader key, and normal-state chords like <kbd>C-w v</kbd> all work.

| Emacs | Evil | Does |
|---|---|---|
| <kbd>C-s</kbd> / <kbd>C-r</kbd> | <kbd>/</kbd> / <kbd>?</kbd> | Search forward / backward. An empty answer repeats the last search. |
| <kbd>F3</kbd> / <kbd>S-F3</kbd> | <kbd>n</kbd> / <kbd>N</kbd> | Next / previous match |
| <kbd>M-g g</kbd> | <kbd>g o</kbd> | Go to an offset |
| <kbd>C-c c h</kbd>, <kbd>c c</kbd>, <kbd>c b</kbd>, <kbd>c t</kbd> | <kbd>y</kbd> (hex) | Copy as hex, a C array, base64, or text |
| <kbd>C-y</kbd> | <kbd>p</kbd> | Paste: hex text (`de ad`) becomes those bytes, anything else its UTF-8 |
| <kbd>C-c C-f</kbd> | | Fill the selection with a repeated byte pattern |
| <kbd>C-c i</kbd> / <kbd>C-c 0</kbd> | | Insert bytes / a NUL byte at point |
| <kbd>C-c w</kbd> / <kbd>C-c W</kbd> | | Wider / narrower rows (8, 16, 32, 64, or fit the window) |
| <kbd>C-c g</kbd> | | Group bytes by 1, 2, 4 or 8 |
| <kbd>C-c C-i</kbd> / <kbd>C-c C-e</kbd> | | Hide the inspector / switch it between little- and big-endian |

With no selection, a copy takes the byte at point. Click a byte to move there, drag to select, and shift-click to extend the selection.

## Search patterns and offsets

A search pattern is hex bytes with optional spaces and `0x`: `de ad be ef`, `deadbeef`, `0x7f454c46`. `??` matches any byte. Text in double quotes, like `"PNG"`, searches for those characters, and so does anything that isn't hex. Searches wrap around the ends of the buffer.

The goto prompt reads `0x1f0`, `1f0h` or `$1f0` as hex, `496` as decimal, `+16` or `-0x10` relative to point, and `50%` as a fraction of the file.

## Scripting

| Function | Returns / does |
|---|---|
| `(hex-buffer?)` `(hex-offset)` `(hex-size)` | Whether this is a hex buffer, point, length |
| `(hex-byte)` | The byte at point, or `#f` past the end |
| `(hex-selection)` | `(start end)`, end exclusive, or `'()` |
| `(hex-goto-offset "0x10")` | Move point |
| `(hex-search-forward-for "de ad ?? ef")` / `(hex-search-backward-for …)` | Search and select the match |
| `(hex-fill "00")` `(hex-insert "de ad")` `(hex-type "41")` | Edit |
| `(hex-set-row-width 16)` `(hex-set-group-size 4)` | Layout (`0` fits the window) |
| `(hex-find-file "path")` | Open any file as hex |

Every binding lives in `scheme/hex.scm`, in the `hex-mode` keymap and the `evil-normal:hex-mode` and `helix-normal:hex-mode` overlays, so you can rebind anything with `define-key`. `hex-mode-hook` runs once per hex buffer. The faces `hex-null`, `hex-whitespace`, `hex-control` and `hex-high` set the byte colours.

Files are read into memory whole, so very large files (several GB) aren't a good fit yet.
