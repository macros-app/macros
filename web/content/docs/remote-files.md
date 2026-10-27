---
title: Remote files (ssh)
description: TRAMP-style editing of files on other machines over ssh — find-file, save, revert and dired on /ssh:host:/path, using your own ssh config, agent and keys.
---

# Remote files (ssh)

Macros edits files on other machines the way Emacs's **TRAMP** does: give any file command a path of the form

```
/ssh:[user@]host[#port]:/path/to/file
```

and it reads and writes that file over ssh. `/scp:` works as a synonym.

| Path | Means |
| --- | --- |
| `/ssh:web1:/etc/nginx/nginx.conf` | an absolute path on `web1` |
| `/ssh:deploy@web1#2222:/srv/app/` | as user `deploy`, port 2222 |
| `/ssh:web1:notes.org` or `/ssh:web1:~/notes.org` | relative to your home directory there |
| `/ssh:web1:` | your home directory there |
| `/ssh:root@[fe80::1]:/` | an IPv6 address, in brackets |

`host` can be anything ssh accepts, including a `Host` alias from `~/.ssh/config`. Its `User`, `Port`, `IdentityFile`, `ProxyJump` and so on all apply.

## What works

- **Finding files.** <kbd>C-x C-f</kbd> (`helm-find-files`) completes remote directories: type `/ssh:host:/` and the picker lists that directory. As with local paths, a `//` anywhere in the prompt drops what came before it, so you can type `/ssh:…` straight after the pre-filled directory. Finding a directory opens it in [dired](/docs/dired).
- **Saving.** <kbd>C-x C-s</kbd>, `save-some-buffers`, `:w` and the quit prompt all write remote buffers.
- **Reverting.** `revert-buffer` (`:e!`) re-reads the file from the host.
- **Dired.** <kbd>C-x d</kbd> in a remote buffer lists its directory on the host. <kbd>RET</kbd> visits, <kbd>^</kbd> goes up, <kbd>g</kbd> refreshes, and copy (<kbd>C</kbd>), rename (<kbd>R</kbd>), mkdir (<kbd>+</kbd>) and delete (<kbd>d</kbd> / <kbd>x</kbd>) run on the host. Copying or moving *between* hosts (or between a host and this machine) isn't supported yet and says so.
- **Scripts.** `read-file`, `read-file-strict`, `write-file`, `delete-file`, `file-exists?`, `file-directory?` and `list-directory` accept remote paths. `(file-remote? path)` tells you whether a path is remote. `(remote-call-process program args)` runs a program on the host named by its `/ssh:` arguments and returns `(exit-code stdout stderr)`, like `call-process-full`.

## How it connects

Every operation runs your system `ssh`, so your config, agent, keys, jump hosts and `known_hosts` all apply. Macros never handles passwords. ssh runs with `BatchMode=yes`, so a host that would prompt for a password, a key passphrase or an unknown host key fails straight away with a message saying what to fix:

- *authentication needs a password or passphrase* — load a key into your agent (`ssh-add`), or name one with `IdentityFile` in `~/.ssh/config`.
- *the host key is unknown or changed* — run `ssh host` once in a terminal to verify and accept it.

On macOS and Linux the first operation opens a shared connection (`ControlMaster` / `ControlPersist`, with the socket in a private `/tmp/macros-ssh-<uid>/` directory) that stays up for 10 minutes after the last use. Later reads, saves and listings reuse it, so they take about a round trip. On Windows, OpenSSH has no connection sharing, so each operation connects afresh. It works if `ssh.exe` is on your `PATH`, just more slowly.

While the connection is being set up, the echo area shows **Connecting to host… (C-g to cancel)**. Visits, saves and reverts run off the UI thread, so the editor stays responsive. <kbd>C-g</kbd> cancels any remote operation that is still waiting on the network.

Completion and dired run in the scripting engine, not the UI thread. Directory listings are cached for a few seconds, so typing a completion pattern doesn't fetch the same directory on every keystroke. Any write or file operation on a host clears that host's cache.

## Saves are safe

Remote saves keep the same guarantees as local ones:

- **Atomic.** The text goes to a temporary file next to the target and is `mv`ed over it, so a dropped connection never leaves a half-written file. The file keeps its permissions. A symlink is written through, so the link stays a link. A hard-linked file, or one owned by another user, is rewritten in place instead, as with local saves.
- **No silent clobbering.** If the file changed on the host since you visited it, the save is refused and you're asked *changed on the remote host since it was read; save over it anyway?*. The check runs on the host, in the same command as the write. A rewrite that leaves the bytes identical (a `touch`) doesn't count as a change.
- **Ordered.** Two saves of one file can't land out of order.
- **Quitting** waits for remote saves still in flight, and is called off if one fails.

One difference: saves made as part of quitting (the quit prompt, `:wq`) and `:w other-file` wait for the write to finish, as they do for local files. Over a slow link the editor pauses for that long, bounded by ssh's 15-second connect timeout.

## What remote buffers skip

These features need the file on this machine, so they're switched off for remote buffers:

- **Language servers** don't start for a remote file, and a running local server never receives it.
- **The git gutter** (`magit-gutter`) stays empty.
- **Auto-revert** doesn't watch remote files, and dired doesn't refresh a remote directory live. Use `revert-buffer` or <kbd>g</kbd> to re-read.
- **No backup or lock files** are written on either machine.
- **Shell commands** (`compile`, `shell`, `call-process`) still run locally. Only `remote-call-process` runs on the host.

## Requirements on the host

Any POSIX `/bin/sh` with the standard tools: `cat`, `stat` (GNU, BSD or busybox), `mv`, `chmod`, `dd`, `readlink`. Your login shell can be anything, including bash, zsh, fish or csh. Macros hands it one fixed command that starts `/bin/sh`, and sends the real script on stdin, so your shell's quoting rules never touch a file name.
