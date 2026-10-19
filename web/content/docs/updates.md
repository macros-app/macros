---
title: Updates
description: How Macros checks for and installs new releases, and how to turn auto-updating off.
---

# Updates

Macros keeps itself current by checking for new releases against `macros.dev`. This covers the **editor binary and the built-in Scheme files that ship inside it** — it does *not* touch the third-party packages you install with `package-install`, which update separately (see [Packages installed separately](#packages-installed-separately)).

## How the check works

On launch, Macros fetches a small version manifest from `https://macros.dev/latest.json` and compares it against the running build. The check is **gated to once a day** — most launches skip the network entirely, and you only see a `Checking for updates…` line in `*Messages*` on the launches that actually hit the network.

When a newer release exists, nothing downloads automatically. Instead an **update pill** appears on the right of the title bar. Click it to download (with a live progress ticker), then click again to restart into the new version.

- **macOS** downloads the release archive, verifies its SHA-256 checksum, and atomically swaps the running `Macros.app` in place. If the app is running from a read-only location (a mounted DMG, or a quarantined download under App Translocation), Macros offers to move it into `/Applications` first so updates can install.
- **Windows** downloads the `.msi`, verifies its SHA-256 checksum and that it is signed by the same publisher as the running app, and installs it when you restart: Macros quits, the installer runs with a progress bar (Windows asks for administrator approval once, since Macros installs for all users), and the new version opens. If you decline the prompt, the current version reopens and the update is offered again later.
- **Linux** downloads the `.deb` or `.rpm` — whichever package manager installed Macros — verifies its SHA-256 checksum, and installs it through `pkexec`, so your desktop asks for your password once. The running editor keeps working; restarting opens the new version.

Builds that can't update themselves — an unsigned development build, or a Linux binary that didn't come from the Macros package (built from source, a tarball) — open the [downloads page](https://macros.dev/download/) instead.

### Checking on demand

<kbd>M-x</kbd> `check-for-updates` forces a check right away, ignoring the once-a-day gate. It reports the result in the echo area and, if an update is available, downloads and installs it directly. This command works **regardless** of the auto-update setting below.

## Turning auto-updating off

The launch-time check is on by default. To disable it, add this to your `init.scm`:

```scheme
;; No network check at launch, no background download, no "restart to update" notice.
(set-auto-update #f)
```

Re-enable it with `(set-auto-update #t)`. This is backed by the `auto-update` editor option, so `(set-option "auto-update" #f)` is equivalent.

With auto-update off, Macros never reaches out to `macros.dev` on its own — you stay on your current version until you run <kbd>M-x</kbd> `check-for-updates` manually.

## Packages installed separately

Auto-update only manages the Macros application itself (the binary and its bundled Scheme). Extensions you pull in with `package-install` are **not** part of this and are updated on their own:

- Pinning a package to a `'ref` keeps it fixed to a commit or tag.
- <kbd>M-x</kbd> `package-update` fast-forwards branch-tracking installs to upstream.

See [Packages](/docs/packages) for the full story on installing and updating extensions.
