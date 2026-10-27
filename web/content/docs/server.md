---
title: Edit server
description: Open files in the running editor from a terminal with macros --client, and use Macros as $EDITOR for git commit.
---

# Edit server

A running Macros listens for files to open, like Emacs's `server-start`. `macros --client` is the `emacsclient` that talks to it: instead of starting a second editor, it hands its files to the one you already have open and brings that window to the front.

```sh
macros --client notes.md            # open, and wait until you're done with it
macros --client -n +42 src/main.rs  # open at line 42 and return at once
macros --client -n +42:7 a.rs b.rs  # line 42, column 7 of a.rs; b.rs as is
```

## Using it as `$EDITOR`

```sh
export EDITOR="macros --client"
```

`git commit` (or `crontab -e`, `kubectl edit`, …) now opens the message in your running Macros and waits. Write the message, then press <kbd>C-x #</kbd>: Macros saves the buffer, closes it (unless it was already open before the client asked), and tells the client it's done, so git carries on. If the client sent several files, it waits until you've pressed <kbd>C-x #</kbd> in each; after each one Macros switches to the next file still being waited on. Killing a client's buffer with <kbd>C-x k</kbd> also releases it.

On macOS the `macros` program is inside the app bundle: use `/Applications/Macros.app/Contents/MacOS/macros --client`, or link it onto your `PATH`.

## Options

| Flag | Meaning |
|---|---|
| `--wait`, `-w` | Wait until every file is finished with <kbd>C-x #</kbd>. The default, as with `emacsclient`. |
| `-n`, `--no-wait` | Return as soon as the files are open. |
| `+LINE` / `+LINE:COLUMN` | Put point at that line (and column) of the next file. Both count from 1. |
| `-a CMD`, `--alternate-editor=CMD` | Run `CMD FILE…` when no Macros is serving. `$ALTERNATE_EDITOR` works too. |
| `--` | Everything after it is a file name, even if it starts with `-` or `+`. |

When no Macros is running (or its server is off), the client starts Macros on the files instead, or runs the alternate editor. With `--wait` it waits for that editor to exit, so `$EDITOR` keeps working either way.

## The server

The server is **on by default**. It accepts connections only from your user account, and without it `macros --client` would open a second editor every time. Turn it off in `init.scm`:

```scheme
(set-option "server" #f)
```

or for this session with <kbd>M-x</kbd> `server-stop`; <kbd>M-x</kbd> `server-start` turns it back on. When you run several Macros at once, the first one serves and the others don't.

How it listens:

- **macOS and Linux:** a Unix socket named `server` in `$XDG_RUNTIME_DIR/macros/`, or, when that isn't set, in `macros-<uid>/` in your temp directory (`$TMPDIR` on macOS). The directory is mode `0700` and the socket `0600`, so only you can connect. Macros refuses a directory that another user owns.
- **Windows:** a random port on `127.0.0.1`. Macros writes its address and a fresh random token to `%LOCALAPPDATA%\macros\server`, and refuses any request without that token. The file is protected by your profile folder's permissions.

A socket left behind by a Macros that crashed is detected and replaced on the next start.

| Key / command | What it does |
|---|---|
| <kbd>C-x #</kbd> `server-edit` | Save this client buffer, release the client, move to the next client buffer |
| `server-start` | Turn the server on (sets the `server` option) |
| `server-stop` | Turn it off |
