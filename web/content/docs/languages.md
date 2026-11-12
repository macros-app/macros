---
title: Languages & modes
description: Add syntax highlighting for a file type, bundle a new tree-sitter grammar, and build a major mode.
---

# Languages & modes

Support for a language in Macros has two independent layers:

1. **Syntax highlighting** — driven by [tree-sitter](https://tree-sitter.github.io/). A grammar parses the buffer and a *highlights query* tags nodes with capture names, which map to theme faces.
2. **A major mode** — a keymap plus commands and behavior (folding, navigation, editing helpers), activated when a matching file opens.

You can do either without the other: a file type can get highlighting with no custom mode, and a mode can add behavior on top of the built-in highlighting.

One thing to know up front: a file type whose grammar is *already bundled* needs only `init.scm` config. For a *brand-new* grammar, `install-language` downloads, compiles, and loads it at runtime — no rebuild, all from your config. Both are covered below.

## How highlighting works

A grammar's highlights query tags nodes with standard capture names. The highlighter maps each capture to a theme face — by the capture's dotted prefix, so `constant.builtin` and `string.special` resolve to `constant` and `string`. The recognized faces are:

`comment` · `string` · `keyword` · `function` · `number` · `constant` · `type` · `operator` · `punctuation` · `variable`

Because the mapping is by capture name, any standard tree-sitter `highlights.scm` query colors correctly with no per-language wiring. To restyle, set those faces in your theme — see [Themes & faces](/docs/themes-and-faces).

## Highlighting a file type (bundled grammar)

These grammars ship in the binary:

`scheme` · `rust` · `python` · `javascript` · `typescript` · `tsx` · `json` · `c` · `cpp` · `go` · `java` · `scala` · `clojure` · `elixir` · `ruby` · `lua` · `zig` · `r` · `ocaml` · `haskell` · `sql` · `bash` · `powershell` · `html` · `css` · `yaml` · `toml` · `markdown` · `latex` · `typst` · `mermaid`

Map a file extension to one of them with `set-extension`, and highlighting (and the LSP language id) follows automatically:

```scheme
;; Highlight .conf files using the bundled TOML grammar.
(set-extension "conf" "toml")
```

When the file type also has a language server, configure both at once with the `lsp-language` form (see [Language servers](/docs/lsp)) — its `'extensions` already calls `set-extension` for you:

```scheme
(lsp-language "kotlin"
  'extensions     '("kt" "kts")
  'command        "kotlin-language-server"
  'format-on-save 'lsp)
```

> `lsp-language` wires the *server* and the language id, but highlighting still needs a bundled grammar for that id. If none is bundled (Kotlin isn't), add it as below.

## Installing a grammar at runtime

For a language Macros doesn't bundle, `install-language` downloads a tree-sitter grammar from git, compiles it, and loads it — no rebuild. Put a call in your `init.scm`, pinning a revision so the build is reproducible:

```scheme
(install-language "nim"
                  "https://github.com/alaviss/tree-sitter-nim"
                  "a4b3e3a"          ; commit, tag, or branch
                  '("nim" "nims"))   ; file extensions (no dot)
```

Once installed, opening any `.nim` file highlights automatically — the extension lookup falls back to runtime grammars, so you don't need a separate `set-extension`. A runtime grammar also gets an `<id>-mode` command (`M-x nim-mode`), exactly like a bundled language.

**You need a C/C++ toolchain.** Published grammars ship generated C; Macros compiles it with the host compiler (`cc`/`clang` on macOS and Linux, MSVC or MinGW on Windows) — the same toolchain a source build needs. The grammar repo must commit its generated `parser.c` (almost all do) and a `queries/highlights.scm`; the query is what produces color, so a parser with no query gives a tree but no highlighting.

**The first install from a repo asks for confirmation** in the minibuffer — loading a grammar runs native code in-process, so a new source is trusted once (recorded in `trusted-grammars` in your config dir) and isn't re-prompted on later launches, even though the `install-language` call re-runs every startup. The compiled library is cached under the OS cache dir, keyed by language id and revision, so relaunching reuses it instead of recompiling.

**Grammars in a subdirectory or with a different parser name** take two optional trailing arguments — `symbol` then `subdir`. Some repos nest the grammar (tree-sitter-typescript keeps it in `typescript/`) or export a parser symbol (`tree_sitter_<symbol>`) that differs from the id you chose:

```scheme
(install-language "typescript"
                  "https://github.com/tree-sitter/tree-sitter-typescript"
                  "v0.23.2" '("ts")
                  "typescript"        ; symbol → tree_sitter_typescript
                  "typescript")       ; subdir within the repo
```

The capture names in the grammar's own `highlights.scm` flow straight into the faces above, so no query of your own is needed. To build a grammar yourself — for a new or in-house language — see [Authoring a grammar repo](#authoring-a-grammar-repo) below.

## Authoring a grammar repo

Most languages already have a community grammar — search GitHub for `tree-sitter-<language>` and point `install-language` straight at it. Write your own only for a brand-new or in-house language. Either way it helps to know the repo layout Macros consumes.

### Repo layout

`install-language` expects a standard tree-sitter grammar repo:

```
tree-sitter-foo/
├── grammar.js              # the grammar, in tree-sitter's JS DSL (source of truth)
├── src/
│   ├── parser.c            # GENERATED from grammar.js — MUST be committed
│   ├── tree_sitter/        # generated headers, committed alongside parser.c
│   │   └── parser.h
│   └── scanner.c           # optional hand-written scanner (or scanner.cc for C++)
└── queries/
    └── highlights.scm      # node patterns → capture names → faces
```

Macros only ever touches three things in it: it **compiles `src/parser.c`** (plus `src/scanner.c` / `scanner.cc` if present), **reads `queries/highlights.scm`**, and resolves the parser entry point `tree_sitter_<name>`, where `<name>` is the `name:` field in `grammar.js`. It does **not** run `grammar.js` — that's why `src/parser.c` must be committed (it's the generated output; nearly every published grammar commits it). If `<name>` differs from the id you pass to `install-language`, pass it as the `symbol` argument; if the grammar lives in a subdirectory, pass `subdir`.

### Creating one from scratch

1. **Install the tree-sitter CLI** — the tool that turns `grammar.js` into `parser.c`:

   ```sh
   npm install -g tree-sitter-cli      # or: cargo install tree-sitter-cli
   ```

2. **Write `grammar.js`.** The `name` becomes the parser symbol; the rules describe the syntax. A minimal grammar for a `key = value` config language:

   ```js
   module.exports = grammar({
     name: 'kv',                 // → tree_sitter_kv; this is your install-language id
     extras: $ => [/\s/, $.comment],   // whitespace + comments may appear between tokens
     rules: {
       document: $ => repeat($.pair),
       pair:    $ => seq(field('key', $.key), '=', field('value', $.value)),
       key:     $ => /[A-Za-z_][A-Za-z0-9_]*/,
       value:   $ => choice($.number, $.string),
       number:  $ => /\d+/,
       string:  $ => /"[^"]*"/,
       comment: $ => /#.*/,
     },
   });
   ```

   The grammar DSL — `seq`, `choice`, `repeat`, `field`, precedence, conflicts, and hand-written external scanners for context-sensitive tokens (indentation, heredocs) — is a subject of its own; see tree-sitter's [Creating parsers](https://tree-sitter.github.io/tree-sitter/creating-parsers) guide.

3. **Generate and check:**

   ```sh
   tree-sitter generate            # writes src/parser.c and src/tree_sitter/*.h
   tree-sitter parse example.kv    # print the parse tree to sanity-check it
   ```

4. **Write `queries/highlights.scm`** (next section).

5. **Commit** `src/parser.c`, `src/tree_sitter/`, any `src/scanner.*`, and `queries/highlights.scm`. Tag a release or note the commit SHA — that's the `rev` you pin in `install-language`.

6. **Point `install-language` at it:**

   ```scheme
   (install-language "kv" "https://github.com/you/tree-sitter-kv" "v0.1.0" '("kv" "conf"))
   ```

### Writing the highlights query

`queries/highlights.scm` is a list of tree-sitter [query patterns](https://tree-sitter.github.io/tree-sitter/using-parsers#query-syntax), each tagging a node with an `@capture`. The capture name — by its dotted prefix — selects the face. For the `kv` grammar above:

```scheme
(comment) @comment
(key)     @property        ; @property aliases to the `variable` face
(number)  @number
(string)  @string
"=" @operator              ; anonymous tokens are matched as string literals
```

- `(rule_name) @cap` tags a *named* node; `"literal" @cap` tags an *anonymous* token — a keyword or punctuation written literally in the grammar.
- Only the dotted **prefix** matters: `@string.special` → `string`, `@keyword.control` → `keyword`. Stick to the conventional [nvim-treesitter capture names](https://github.com/nvim-treesitter/nvim-treesitter/blob/master/CONTRIBUTING.md#highlights) and existing themes color your language with no extra work. A capture Macros doesn't recognize simply renders uncolored — never an error — so an over-specific query degrades gracefully.

Macros resolves captures to these faces (direct prefix matches plus the common aliases):

| Face | Captures that resolve to it |
|------|------------------------------|
| `comment` | `comment` |
| `string` | `string`, `character`, `escape` |
| `keyword` | `keyword`, `conditional`, `repeat`, `include`, `exception`, `label`, `preproc`, `storageclass`, `tag` |
| `function` | `function`, `method` |
| `number` | `number`, `float` |
| `constant` | `constant`, `boolean` |
| `type` | `type`, `constructor`, `namespace`, `module` |
| `operator` | `operator` |
| `punctuation` | `punctuation`, `delimiter` |
| `variable` | `variable`, `property`, `field`, `parameter`, `attribute` |

Markup languages can also use the `@markup.*` / `@text.*` families (headings, bold, links) — those map onto the faces above too; the bundled `latex` and `markdown` queries are worked examples. To restyle any of these, set the face in your theme (see [Themes & faces](/docs/themes-and-faces)).

## Auto-indentation (indent query)

When `electric-indent` is on (the default), pressing **RET** indents the new line and **TAB** reindents the current one. How the target indent is computed depends on what the language provides, in this order:

1. **Lisps** (`scheme`, `clojure`, …) use a built-in paren-aligning indenter — continuation lines align under a call's first argument, special-form bodies indent two columns. No query needed.
2. **A tree-sitter `indents.scm` query** drives structural indentation for every other language that ships (or installs) one — a brace block indents its body and dedents on `}`, and so on.
3. **No query?** The new line simply copies the previous line's leading whitespace. A reasonable default — just not structure-aware.

Turn the whole feature off with `(set-option "electric-indent" "false")`.

### How the indent query works

`queries/indents.scm` tags nodes with a small set of captures. The indenter walks the syntax around the line and tallies them:

| Capture | Meaning |
|---------|---------|
| `@indent` | This node's body is indented one step — a brace block, a bracketed list, an argument list. Counts only while the node is genuinely open past the line. |
| `@outdent` | A closing token (`}`, `)`, `end`) whose own line dedents one step. |
| `@extend` | Indent the lines that follow this node **even when its body is still empty** — for layout languages (`if x:` …) where there is no delimiter token to open a block. |
| `@indent.always` | Like `@indent`, but exempt from the "one step per start row" dedup (rarely needed). |
| `@align` + `@anchor` | Column alignment (Helix-style): continuation lines inside the `@align` node line up under the `@anchor` node's column instead of stepping in a fixed amount — e.g. wrapped call arguments under the first one. Capture the container `@align` and the node to align to `@anchor` in the same pattern. |

A step is one indent width — the global [`tab-width`](/docs/configuration), or a per-language `tab-width:<id>` override where set (OCaml is 2, matching ocamlformat). Two `@indent` nodes that open on the *same* row count once, so capturing both an `if` statement and its block won't double-indent.

A brace-delimited language is usually just its block-like nodes plus the closing brackets. For Rust:

```scheme
[
  (block)
  (declaration_list)
  (field_declaration_list)
  (arguments)
  (parameters)
  (match_block)
] @indent

["}" ")" "]"] @outdent
```

A layout language (no closing token for blocks) leans on `@extend` for its compound statements and `@indent` only for brackets. For Python — capture the *top* compound statements, not their `elif`/`else`/`except` clauses, so a clause body still resolves to one level:

```scheme
[
  (if_statement)
  (for_statement)
  (while_statement)
  (function_definition)
  (class_definition)
] @extend

[(list) (dictionary) (argument_list) (parameters)] @indent

[")" "]" "}"] @outdent
```

Keyword-delimited languages (Ruby, Lua) combine both: `@extend` on `def`/`class`/`do` blocks, with `"end"` as an `@outdent` token.

The bundled queries live in `grammars/queries/indents/` and are the best templates. If a query won't compile against its grammar, the language silently falls back to copy-previous-indent — so a typo degrades gracefully rather than breaking. Lisps deliberately have **no** indent query (the built-in aligner is better at column alignment than the step model).

### Trailing-token rules

Some distinctions aren't structural. In OCaml a match arm whose `->` **dangles** at the end of a line should indent its body, but a `let … in` or `match … with` line should keep the next line at the same column — and the syntax tree can't tell a dangling `->` from a satisfied one (`| A -> a`). For these, a per-language token layer runs on top of the structural indent:

```scheme
;; A line ENDING in one of these opens the next line one step deeper.
(set-option "indent-after:ocaml" "->")
;; A line ENDING in one of these keeps the next line at the current column.
(set-option "align-after:ocaml" "in with")
```

Each value is a whitespace-separated list of tokens (compared to the line's last whitespace-delimited token, so `main` never matches `in`). `indent-after` only ever *raises* the indent — it never reduces a deeper structural result — and `align-after` pins to the current line's column. They're per-language options (see [Per-mode options](/docs/configuration#per-mode-options)); the bundled set lives in `scheme/indent.scm`.

This is the same layered approach Neovim's per-language indent scripts take, for the cases pure tree-sitter can't express. When in doubt, keep the structural query simple and reach for a token rule only for a genuinely ambiguous keyword.

### Adding an indent query

- **Bundled grammar:** drop a `queries/indents/<id>.scm` next to the others and add one `include_str!` arm to `indent_query` in `src/highlight.rs`. The `bundled_indent_queries_compile` test confirms it compiles against the grammar.
- **Runtime grammar:** if the repo you point `install-language` at ships a `queries/indents.scm`, it's picked up automatically and enables auto-indent — no extra arguments. (It must compile against the grammar, or the install fails, the same as the highlights query.)

To discover the node names a grammar uses, open a file and run `(tree-sitter-node-at <byte> "callback")` (see [Custom fontification](#custom-fontification-from-scheme)) or read the grammar's own `queries/indents.scm` upstream.

## Building a major mode

A major mode is a keymap named after the mode, plus commands, activated on `find-file-hook`. The minimal recipe:

```scheme
;; env-mode: a tiny major mode for `.env` files (highlighted as shell).

;; 1. Highlight the extension with a bundled grammar.
(set-extension "env" "bash")

;; 2. Commands.
(define (env-uncomment-line)
  ;; … edit the current line …
  (message "uncommented"))

;; 3. A keymap named after the mode; bind the commands.
(define-key "env-mode" "ctrl-c ctrl-c" 'env-uncomment-line)

;; 4. Make commands discoverable from M-x.
(register-command 'env-uncomment-line)

;; 5. Activate the mode when a matching file opens.
(define (env--maybe-activate)
  (when (ends-with? (current-buffer) ".env")
    (set-major-mode "env-mode")))
(add-hook "find-file-hook" 'env--maybe-activate)
```

`set-major-mode` installs the keymap whose name you pass, so the mode name and the keymap name must match. It is also highlighting-aware: if the mode name matches a bundled grammar, switching to it attaches that grammar (and, via the `fundamental` `enable-lsp` hook, its language server *if one is configured* — see [Language servers](/docs/lsp)) and runs the mode's hooks. Bindings can use [`bind-key`](/docs/keybindings#rebinding), which takes the command as a `'name` symbol (or a bare function value) rather than a command-name string.

The bundled modes are the best templates: `scheme/markdown.scm` is a compact tree-sitter-backed mode (the grammar highlights; the mode adds heading navigation and folding), and `scheme/org.scm` is a full mode with its own parser and fontification.

## Language modes and entry hooks

Every bundled grammar also gets a major mode named after it, entered automatically when a matching file opens — a `.rs` file lands in `rust` mode, a `.scala` file in `scala` mode. Each is an M-x command too (`rust-mode`, `scala-mode`, `clojure-mode`, …), so you can switch any buffer's mode by hand with no per-language wiring.

To configure what happens **on entering a mode** — the analog of Emacs's `rust-mode-hook` — register a function under the mode name. The hook runs on both entry paths (opening a file of that type, or `M-x rust-mode`):

```scheme
(define (my-rust-setup)
  (set-option "word-wrap:rust" #f)                  ; don't wrap long lines in Rust
  (define-key "rust" "ctrl-c ctrl-t" 'lsp-hover))

(add-hook "rust-mode-hook" 'my-rust-setup)
```

`add-hook` accepts a command-name string, a `'symbol`, or an anonymous `(lambda () …)`. The public hook spellings are the Emacs ones: `rust-mode-hook` (the bare mode name `"rust"` also fires as a back-compat alias, but it is not the documented API). `fundamental-mode-hook` is the catch-all that runs for every buffer. The registry is **global** (shared by all buffers in that mode) rather than buffer-local, so hooks should be idempotent — *set* buffer-local state, don't *append* to it.

## Custom fontification from Scheme

When the standard tree-sitter highlighting isn't enough — you want depth-based coloring, semantic rules, or a language with no grammar — paint faces yourself.

**Query the parse tree.** `(tree-sitter-query "<query>" "callback")` runs a tree-sitter query over the active buffer's tree and calls your Scheme function with the captures as newline-separated, tab-delimited records (`name⇥start⇥end⇥kind`). Paint each with `overlay-set-face`:

```scheme
(define (color-types records)
  (for-each
    (lambda (rec)
      (let ((p (split-many rec "\t")))   ; (name start end kind)
        (overlay-set-face (string->number (nth p 1))
                          (string->number (nth p 2))
                          "type")))
    (non-empty-lines records)))

(tree-sitter-query "(type_identifier) @t" 'color-types)
```

`scheme/rainbow.scm` is the full worked example (depth-colored brackets via a query plus `overlay-set-face`), and re-runs on `find-file-hook` and `after-change-functions` so it stays live as you type.

**Ask what you're inside.** `(tree-sitter-node-at byte "callback")` reports the chain of nodes covering a byte (innermost first) — useful to, say, skip an action when point sits inside a string or comment node.

**No grammar?** Fontify with regexes and `put-text-property` / `overlay-set-face`, the way `scheme/org.scm` colors headings and TODO keywords without any tree-sitter grammar.
