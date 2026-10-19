# Releasing Macros

GitHub (`github.com/macros-app/macros`) is the main repository: PRs, CI, and
day-to-day pushes happen there. Releases and web deploys run on the self-hosted
Forgejo instance, whose runners and secrets do the signing and publishing. It
only receives what you push to it when cutting a release — never commit there
directly, so it stays a copy of GitHub's `main`.

`.forgejo/workflows/release.yml` builds, signs, and publishes every platform
artifact for a tagged release. The version always comes from `Cargo.toml`'s
`[package].version`; every job verifies the tag/input matches it and fails
fast otherwise.

## What it produces

| Platform | Artifact | Signed / Notarized |
|---|---|---|
| macOS arm64 | `Macros-macos-aarch64-<ver>.zip` (`Macros.app`) | Developer ID + **notarized & stapled** |
| macOS x86_64 | `Macros-macos-x86_64-<ver>.zip` (`Macros.app`) | Developer ID + **notarized & stapled** |
| Linux arm64 | `macros_<ver>_arm64.deb`, `macros-<ver>-1.aarch64.rpm` | — |
| Linux x86_64 | `macros_<ver>_amd64.deb`, `macros-<ver>-1.x86_64.rpm` | — |
| Windows x86_64 | `Macros-windows-x86_64-<ver>.msi` | **Azure Trusted Signing** (exe + msi) |

All artifacts are uploaded to **Cloudflare R2** by the final `publish` job,
which then HEAD-verifies each public URL serves the exact bytes back. Alongside
each artifact it uploads a `<artifact>.sha256` sidecar (sha256sum format) that
the in-app updater verifies before installing.

## Auto-update

The desktop app updates itself without a backend database (see `src/update.rs`):

1. **The web app advertises the latest version.** `web/scripts/gen-version.mjs`
   bakes a static `latest.json` from `Cargo.toml`'s version on every web deploy
   (`deploy-web.yml`), served at `https://macros.dev/latest.json`. It lists the
   version and the R2 download URLs — the same filenames this pipeline uploads.
2. **The app checks once a day** (and on demand via `M-x check-for-updates`),
   semver-comparing `latest.json` against its built-in `CARGO_PKG_VERSION`.
3. **macOS installs in place:** download the `.zip`, verify it against the
   `<url>.sha256` sidecar, atomically swap the `Macros.app` bundle, and relaunch.
   macOS also offers, at launch, to move a bundle running from a read-only volume
   (a mounted DMG or Gatekeeper App Translocation) into `/Applications` so future
   updates can install.
4. **Windows installs on restart:** download the `.msi`, verify the sidecar
   and that its Authenticode signer matches the running exe's, then — once the
   app quits — run `msiexec /passive` elevated (one UAC prompt) into the
   current install dir and relaunch.
5. **Linux installs in place:** if `/usr/bin/macros` is owned by our `macros`
   package, download the matching `.deb`/`.rpm`, verify the sidecar, and
   install it via `pkexec` (`apt-get install` / `dpkg -i` / `rpm -U`), then
   relaunch on restart. Other installs open the downloads page. The Linux
   packages are unsigned, so the SHA-256 sidecar is their only integrity check.

So a release reaches users in **two** steps: cut the release (below) to publish
the artifacts, then deploy the web app (`deploy-web.yml`, run on Forgejo) to
point `latest.json` at the new version.

## How to cut a release

1. Bump the version: `cargo set-version <x.y.z>` (or edit `[package].version`)
   in a PR on GitHub, and merge it.
2. Tag the merged `main` and push it to GitHub **and** Forgejo — the tag push to
   the `forgejo` remote is what starts the release:
   ```sh
   git switch main && git pull origin main
   git tag v<x.y.z>
   git push origin v<x.y.z>
   git push forgejo main v<x.y.z>
   ```
   (Or push `main` to `forgejo` and run the workflow manually via
   **workflow_dispatch**, passing `version`.)
3. The pipeline runs: macOS (notarized), Linux (deb/rpm), Windows (signed msi),
   then publishes everything to R2.

## Supporting files

| File | Purpose |
|---|---|
| `scripts/build_macos.sh` | Assemble + sign `Macros.app` (host or cross-compiled Intel) |
| `scripts/build_icons.sh` | Regenerate `assets/brand/` icons from `logo.svg` |
| `scripts/build_win.ps1` | Build an **unsigned** MSI locally for testing |
| `nfpm.yaml` | `.deb` / `.rpm` layout + runtime deps |
| `wix/main.wxs`, `wix/License.rtf` | WiX v4 MSI definition |
| `build.rs` | Embeds the app icon into `macros.exe` (Windows only) |
| `scripts/publish-release.mjs` | Hash → upload to R2 (+ `.sha256` sidecar) → verify public URL |
| `web/scripts/gen-version.mjs` | Bake `latest.json` update manifest from `Cargo.toml` on web deploy |
| `scripts/linux/postinst.sh`, `postrm.sh` | Refresh desktop/icon caches |

## Required CI secrets

**macOS notarization** — see [MACOS_NOTARIZATION.md](MACOS_NOTARIZATION.md):
`MACOS_KEYCHAIN_PASSWORD`, `MACOS_SIGN_IDENTITY`, `APPLE_API_KEY_P8_BASE64`,
`APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`.

**Windows (Azure Trusted Signing):** `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
`AZURE_CLIENT_SECRET`, `TRUSTED_SIGNING_ENDPOINT`, `TRUSTED_SIGNING_ACCOUNT`,
`TRUSTED_SIGNING_PROFILE`.

**Cloudflare R2 publishing:** `R2_ACCOUNT_ID`, `R2_ACCESS_KEY_ID`,
`R2_SECRET_ACCESS_KEY`, `R2_BUCKET`, `UPDATE_DOWNLOAD_BASE` (public https URL
prefix for the bucket, no trailing slash).

**Checkout:** `ACCESS_TOKEN` (a Forgejo token with repo read access).

All of the above are Forgejo secrets; GitHub CI needs none.

## Runner labels

The jobs target self-hosted runners by label: `osx-runner` (Apple Silicon),
`ubuntu-latest` (Linux arm64), `ubuntu-x86` (Linux x86_64), `windows`. Adjust
these to match your registered runners if they differ.

## Building a local (unsigned) MSI

On a Windows dev box with Rust, the .NET SDK, and WiX installed:
```pwsh
pwsh -File scripts\build_win.ps1
```
This proves the WiX packaging path without signing or pushing to CI.
