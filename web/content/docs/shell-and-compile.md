---
title: Shell & compile
description: Run shell commands, filter the region through them, compile, walk errors, grep any directory, and use the basic shell.
---

# Shell & compile

Macros integrates with external processes the Emacs way: one-shot **shell commands** (<kbd>M-!</kbd>, <kbd>M-|</kbd>, <kbd>M-&</kbd>), a **`compile`** command that runs a build into a read-only buffer and parses its errors, plus error navigation that jumps straight to the offending source line, `grep` / `rgrep` / `lgrep` over any directory, and a basic interactive shell.

## Shell commands

| Key | Command | Action |
| --- | --- | --- |
| <kbd>M-!</kbd> | `shell-command` | Run a command in the buffer's directory and show its output |
| <kbd>C-u M-!</kbd> | | …and insert the output at point instead |
| <kbd>M-\|</kbd> | `shell-command-on-region` | Pipe the region to a command and show its output |
| <kbd>C-u M-\|</kbd> | | …and **replace** the region with the output |
| <kbd>M-&</kbd> | `async-shell-command` | Run a command in the background, streaming into `*Async Shell Command*` |

Commands run through your shell, `$SHELL -c` (`cmd /C` on Windows), so pipes, globs, environment variables and your shell's own syntax all work. They run in the current buffer's directory (Emacs's `default-directory`): the visited file's folder, or the editor's working directory for a buffer with no file.

- **Output.** stderr is merged into the output, as in Emacs. Output of one line is shown in the echo area. Longer output goes to `*Shell Command Output*`, which opens below without taking focus; press <kbd>q</kbd> there to close it. No output at all is reported as `(Shell command succeeded with no output)`.
- **Exit status.** A non-zero exit is always reported, for example `Shell command exited with code 2`. If the shell couldn't be started at all, you get the error message instead.
- **Filtering the region.** <kbd>C-u M-|</kbd> is the one to remember. It sends the region to the command's stdin and puts the result in its place: <kbd>C-u M-| sort</kbd>, <kbd>C-u M-| jq .</kbd>, <kbd>C-u M-| column -t</kbd>. The replacement is a single undo step. If the command fails, its output still replaces the region (undo with <kbd>C-/</kbd>) and the exit status is shown. A command that can't be started leaves the region untouched.
- **Asynchronous.** <kbd>M-&</kbd>, or an <kbd>M-!</kbd> command that ends in `&`, runs without blocking the editor. Output streams into `*Async Shell Command*`, and the exit status is appended when the command finishes. Only one runs at a time. <kbd>C-g</kbd> in its buffer, or <kbd>M-x</kbd> `kill-async-shell-command`, stops it, and killing the buffer kills the process.

<kbd>M-!</kbd> and <kbd>M-|</kbd> wait for the command to finish, like in Emacs. Use <kbd>M-&</kbd> for anything long-running.

## compile

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c b</kbd> | `compile` | Prompt for a command and run it |
| | `recompile` | Re-run the last compile (also <kbd>g</kbd> in the buffer) |

Output streams into a `*compilation*` buffer. When the process finishes, Macros parses each line for an error location and faces it. It recognizes the common compiler and grep formats (the first matching pattern wins, like Emacs's `compilation-error-regexp-alist`).

### In the compilation buffer

| Key | Action |
| --- | --- |
| <kbd>Enter</kbd> | visit the source for the error on this line |
| <kbd>g</kbd> | recompile |
| <kbd>n</kbd> / <kbd>p</kbd> | move |
| <kbd>q</kbd> | close |

## Walking errors anywhere

| Key | Command |
| --- | --- |
| <kbd>M-g n</kbd> | `next-error` |
| <kbd>M-g p</kbd> | `previous-error` |

`next-error` / `previous-error` step through the parsed error list without needing the compilation buffer focused. They're shared with [diagnostics](/docs/diagnostics) and grep: the most recently used source claims them.

## grep

| Command | Prompts for | Searches |
| --- | --- | --- |
| <kbd>M-x</kbd> `grep` | a regexp | every file under the buffer's directory |
| <kbd>M-x</kbd> `rgrep` | regexp, file glob, directory | matching files under that directory, recursively |
| <kbd>M-x</kbd> `lgrep` | regexp, file glob, directory | matching files in that directory only |

These work anywhere, not only inside a project. They search in-process with ripgrep's engine, so no `grep` binary is needed. The search skips hidden files and whatever `.gitignore` excludes, and it honours the `search-*` options described under [Projects & search](/docs/projects). The regexp uses ripgrep (Rust regex) syntax. The file glob takes one or more space-separated patterns such as `*.rs *.toml`; `*` (the default) means every file.

Results open in the `*grep*` buffer, one `path:line:text` per match, with paths relative to the searched directory:

| Key | Action |
| --- | --- |
| <kbd>Enter</kbd> / click | visit the match |
| <kbd>n</kbd> / <kbd>p</kbd> | next / previous match |
| <kbd>g</kbd> | run the search again |
| <kbd>e</kbd> | edit the results in place, wgrep-style (<kbd>C-c C-c</kbd> writes the changes back) |
| <kbd>q</kbd> | close |

The matches also become the error list, so <kbd>M-g n</kbd> / <kbd>M-g p</kbd> step through them from any buffer, just as after a `compile`. Project-wide grep (<kbd>C-c p g</kbd>, <kbd>C-c p G</kbd>) is described under [Projects & search](/docs/projects) and uses the same `*grep*` buffer.

## Shell

| Key | Command | Action |
| --- | --- | --- |
| <kbd>C-c s</kbd> | `shell` | Open a basic interactive shell buffer |
| <kbd>Enter</kbd> | `shell-send-input` | Send the current input line |

The `shell` command gives you a simple line-oriented shell inside a buffer. For a **full** terminal — ANSI/VT, curses apps, REPLs, `htop` — use the [embedded terminal](/docs/terminal) instead.

## Running processes from Scheme

All of this is built on `call-process`, which you can call directly in your own commands:

```scheme
(define (current-branch)
  (call-process "git" '("rev-parse" "--abbrev-ref" "HEAD")))
```

See [Scripting with Steel](/docs/scripting#running-processes) for more on `call-process`, the async `make-process`, and giving a child extra environment (`call-process-env`, `with-process-env`). That's also how to pass it a password safely; see [Passwords & secrets](/docs/secrets#handing-a-secret-to-a-subprocess).
