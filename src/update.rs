//! Self-update.
//!
//! Mirrors the sigma updater, minus the license/JWT layer. macros has no
//! backend database: the "latest version" lives in a static `latest.json`
//! manifest the web deploy bakes from `Cargo.toml` (see
//! `web/scripts/gen-version.mjs`), served at `https://macros.dev/latest.json`.
//! Release artifacts and their `*.sha256` sidecars live on R2 behind
//! `https://downloads.macros.dev` (see `scripts/publish-release.mjs`).
//!
//! Flow:
//!   1. GET the manifest, parse `{ version, notes, downloads }`.
//!   2. semver-compare `version` against this build's `CARGO_PKG_VERSION`.
//!   3. If newer, download this platform's artifact and verify its SHA-256
//!      against the `<url>.sha256` sidecar, then:
//!      - macOS: verify the bundle's Team ID matches the running app's and
//!        atomically swap the running `.app` bundle in place ([`install`]).
//!        [`relaunch`] re-opens it once this process exits.
//!      - Windows: verify the `.msi`'s Authenticode signer matches the running
//!        exe's and stage it. [`relaunch`] waits for this process to exit (the
//!        exe is locked while it runs), runs the installer silently — one UAC
//!        prompt, since the MSI is per-machine — and re-opens the app.
//!      - Linux: install the `.deb`/`.rpm` matching the package manager that
//!        owns `/usr/bin/macros`, as root via `pkexec` (one polkit prompt).
//!        dpkg/rpm replace the binary by rename, so the running process is
//!        unaffected; [`relaunch`] re-opens it once this process exits.
//!
//! An install the updater can't verify or own — an unsigned dev build, a binary
//! not installed from our package — opens the downloads page instead.
//!
//! macOS also gets a launch-time check ([`needs_relocation`]) for a bundle
//! running off a read-only volume (a mounted DMG, or Gatekeeper's App
//! Translocation mount for a quarantined download); [`relocate`] copies it into
//! `/Applications` so future auto-updates can install.
//!
//! Every HTTP call is blocking (`ureq`); run them on gpui's background executor,
//! never the main thread.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// The version compiled into this build (`Cargo.toml` `[package].version`).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where the latest-version manifest lives. Debug builds only: overridable via
/// `MACROS_UPDATE_MANIFEST` to point at a local `next dev` server. Release
/// builds ignore it, so the environment can't redirect the updater.
fn manifest_url() -> String {
    manifest_url_from(
        std::env::var("MACROS_UPDATE_MANIFEST").ok(),
        cfg!(debug_assertions),
    )
}

fn manifest_url_from(env_override: Option<String>, allow_override: bool) -> String {
    env_override
        .filter(|s| allow_override && !s.is_empty())
        .unwrap_or_else(|| "https://macros.dev/latest.json".to_string())
}

/// Stable per-`(os, arch)` key. Doubles as the manifest `downloads` key and the
/// artifact-name infix (`Macros-<key>-<version>.zip`).
pub fn platform_key() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => "unknown",
    }
}

/// The static manifest served at [`manifest_url`].
#[derive(Deserialize)]
struct Manifest {
    /// Latest released semver, e.g. `"0.2.0"`.
    version: String,
    /// Per-platform download URLs, keyed by [`platform_key`]-style strings
    /// (Linux adds a package suffix: `linux-x86_64-deb`).
    #[serde(default)]
    downloads: std::collections::HashMap<String, String>,
}

/// A newer release than this build, resolved from the manifest.
#[derive(Clone)]
pub struct UpdateAvailable {
    /// The latest version string from the manifest.
    pub version: String,
    /// The manifest's download URLs; [`install`] picks this platform's.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
        allow(dead_code)
    )]
    downloads: std::collections::HashMap<String, String>,
}

/// What [`install`] did, so the caller knows whether to relaunch.
pub enum InstallOutcome {
    /// The update is installed (macOS, Linux) or staged to install on exit
    /// (Windows); the caller should [`relaunch`] and quit.
    #[cfg_attr(
        not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
        allow(dead_code)
    )]
    Staged,
    /// The downloads page was opened in the browser; nothing to relaunch. Used
    /// when the running build can't be updated in place: an unsigned dev build,
    /// a Linux binary not installed from our package, or no artifact for this
    /// platform in the manifest.
    OpenedDownloadPage,
}

/// Where a build that can't update itself sends the user instead.
const DOWNLOAD_PAGE: &str = "https://macros.dev/download/";

/// Fetch the manifest and return a newer release if one exists. `Ok(None)` when
/// already current. Blocking — run off-thread.
pub fn check() -> Result<Option<UpdateAvailable>, String> {
    let body = http_get_string(&manifest_url())?;
    let manifest: Manifest =
        serde_json::from_str(&body).map_err(|e| format!("malformed update manifest: {e}"))?;

    let latest = semver::Version::parse(manifest.version.trim())
        .map_err(|e| format!("manifest version '{}' is not semver: {e}", manifest.version))?;
    let current = semver::Version::parse(current_version())
        .map_err(|e| format!("build version '{}' is not semver: {e}", current_version()))?;

    if latest <= current {
        return Ok(None);
    }

    Ok(Some(UpdateAvailable {
        version: manifest.version,
        downloads: manifest.downloads,
    }))
}

/// Like [`check`], but skips the network entirely unless at least 24h have
/// passed since the last check (recorded in the cache dir). Used for the
/// launch-time auto-check so every start doesn't hit the network. The manual
/// `check-for-updates` command calls [`check`] directly to force a check.
pub fn check_gated() -> Result<Option<UpdateAvailable>, String> {
    if !due_for_check() {
        return Ok(None);
    }
    // Record the attempt up front so a transient network error doesn't cause a
    // check on every subsequent launch.
    record_check_now();
    check()
}

/// Download + install the update for this platform.
///
/// Every platform downloads the artifact and verifies its SHA-256 against the
/// `<url>.sha256` sidecar first, then:
/// - macOS: verifies the extracted bundle's code signature and that its Team ID
///   matches the running app's, and swaps the `.app` bundle in place.
/// - Windows: verifies the `.msi`'s Authenticode signature and that its signer
///   matches the running exe's, and stages it for [`relaunch`] to run.
/// - Linux: installs the `.deb`/`.rpm` for the package manager that owns the
///   running binary, as root via `pkexec`.
///
/// and returns [`InstallOutcome::Staged`]. A build with nothing to verify the
/// update against (unsigned dev build, a binary not installed from our
/// package) never auto-installs: it opens the downloads page and returns
/// [`InstallOutcome::OpenedDownloadPage`].
///
/// `on_progress` is called with the download's percent complete (0–100) as bytes
/// arrive, so the caller can drive a progress indicator.
///
/// Blocking — run off-thread.
#[cfg(target_os = "macos")]
pub fn install(
    update: &UpdateAvailable,
    on_progress: impl FnMut(u8),
) -> Result<InstallOutcome, String> {
    let url = update
        .downloads
        .get(platform_key())
        .ok_or("the update manifest lists no macOS download")?;

    // The new bundle must be signed by the same team as this one. With no
    // team to pin to (unsigned dev build, not a bundle), there is nothing to
    // verify an update against — hand the user the download page instead.
    let Some(team_id) = macos::current_app_path()
        .ok()
        .and_then(|app| macos::team_id(&app))
    else {
        open_url(DOWNLOAD_PAGE)?;
        return Ok(InstallOutcome::OpenedDownloadPage);
    };

    let dest = cache_dir()?.join(format!("Macros-{}-{}.zip", platform_key(), update.version));

    let expected = fetch_sha256(url)?;
    download_verified(url, &expected, &dest, on_progress)?;
    macos::stage_in_place(&dest, &team_id)?;
    Ok(InstallOutcome::Staged)
}

#[cfg(target_os = "windows")]
pub fn install(
    update: &UpdateAvailable,
    on_progress: impl FnMut(u8),
) -> Result<InstallOutcome, String> {
    // The installer must be signed by the same publisher as this exe. An
    // unsigned exe (dev build) has nothing to pin to; no MSI for this arch
    // (e.g. arm64) has nothing to install.
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let (Some(signer), Some(url)) = (
        windows::authenticode_signer(&exe),
        update.downloads.get(platform_key()),
    ) else {
        open_url(DOWNLOAD_PAGE)?;
        return Ok(InstallOutcome::OpenedDownloadPage);
    };

    let fallback = format!("Macros-{}-{}.msi", platform_key(), update.version);
    let dest = cache_dir()?.join(artifact_file_name(url, "msi").unwrap_or(fallback));

    let expected = fetch_sha256(url)?;
    download_verified(url, &expected, &dest, on_progress)?;
    match windows::authenticode_signer(&dest) {
        Some(s) if s == signer => {}
        Some(s) => {
            let _ = std::fs::remove_file(&dest);
            return Err(format!("the update is signed by {s}, expected {signer}"));
        }
        None => {
            let _ = std::fs::remove_file(&dest);
            return Err("the update's signature is missing or invalid".to_string());
        }
    }
    windows::stage(dest);
    Ok(InstallOutcome::Staged)
}

#[cfg(target_os = "linux")]
pub fn install(
    update: &UpdateAvailable,
    on_progress: impl FnMut(u8),
) -> Result<InstallOutcome, String> {
    // Only a binary our own .deb/.rpm installed is ours to replace; a cargo
    // build, tarball or distro repackage gets the download page instead.
    let kind = linux::installed_package();
    let url = kind.and_then(|k| {
        update
            .downloads
            .get(&format!("{}-{}", platform_key(), k.ext()))
    });
    let (Some(kind), Some(url)) = (kind, url) else {
        open_url(DOWNLOAD_PAGE)?;
        return Ok(InstallOutcome::OpenedDownloadPage);
    };

    let fallback = format!(
        "macros-{}-{}.{}",
        update.version,
        platform_key(),
        kind.ext()
    );
    let dest = cache_dir()?.join(artifact_file_name(url, kind.ext()).unwrap_or(fallback));

    let expected = fetch_sha256(url)?;
    download_verified(url, &expected, &dest, on_progress)?;
    let result = linux::install_package(kind, &dest);
    let _ = std::fs::remove_file(&dest);
    result?;
    Ok(InstallOutcome::Staged)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub fn install(
    _update: &UpdateAvailable,
    _on_progress: impl FnMut(u8),
) -> Result<InstallOutcome, String> {
    open_url(DOWNLOAD_PAGE)?;
    Ok(InstallOutcome::OpenedDownloadPage)
}

/// Set once a relaunch helper is running, so a second [`relaunch`] (the
/// `check-for-updates` command spawns one, then the quit-and-relaunch path
/// asks again) doesn't start a second helper — on Windows, a second installer.
static RELAUNCH_SPAWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// After [`InstallOutcome::Staged`], spawn the detached helper that finishes
/// the update once this process exits (Windows: runs the staged installer) and
/// re-opens the app. The caller quits immediately afterward. Idempotent.
pub fn relaunch() -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if RELAUNCH_SPAWNED.load(Ordering::SeqCst) {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    macos::relaunch()?;
    #[cfg(target_os = "windows")]
    windows::relaunch()?;
    #[cfg(target_os = "linux")]
    linux::relaunch()?;
    RELAUNCH_SPAWNED.store(true, Ordering::SeqCst);
    Ok(())
}

/// Per-user cache dir for staged downloads (`~/Library/Caches/macros/updates`,
/// `~/.cache/macros/updates`, `%LOCALAPPDATA%\macros\updates`).
fn cache_dir() -> Result<std::path::PathBuf, String> {
    let d = dirs::cache_dir()
        .ok_or("no cache dir")?
        .join("macros")
        .join("updates");
    std::fs::create_dir_all(&d).map_err(|e| format!("creating {d:?}: {e}"))?;
    Ok(d)
}

/// The artifact's own file name (the last path segment of `url`), if it is a
/// plain `[A-Za-z0-9._-]` name ending in `.<ext>` — installers key off the
/// extension, and the name ends up on a command line.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn artifact_file_name(url: &str, ext: &str) -> Option<String> {
    let name = url.split(['?', '#']).next()?.rsplit('/').next()?;
    let plain = !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    (plain && name.ends_with(&format!(".{ext}"))).then(|| name.to_string())
}

/// True when the running bundle sits somewhere that blocks in-place self-update
/// and we should offer to reinstall into the system app dir. macOS: a read-only
/// volume (mounted DMG / App Translocation). Always false elsewhere, and false
/// for non-bundled dev builds (`cargo run`).
pub fn needs_relocation() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::needs_move_to_applications()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Copy the running bundle into `/Applications`, strip quarantine, and spawn a
/// helper that relaunches from there once this process exits. The caller quits
/// on success. macOS only.
pub fn relocate() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        macos::move_to_applications()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("relocation is only supported on macOS".to_string())
    }
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

/// A blocking HTTP agent with sane timeouts and a descriptive User-Agent
/// (see [`crate::net::download_agent`]).
fn agent() -> ureq::Agent {
    crate::net::download_agent(&format!(
        "Macros/{} ({})",
        current_version(),
        std::env::consts::OS
    ))
}

/// GET `url` and return the body as a string (for the small JSON manifest /
/// sidecar). `ureq` caps `read_to_string` well above either payload.
fn http_get_string(url: &str) -> Result<String, String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("reading {url}: {e}"))
}

/// Fetch and parse the `<url>.sha256` sidecar. Accepts either a bare 64-hex
/// digest or `sha256:<hex>` / `<hex>  <filename>` (sha256sum) formats — the
/// first 64-hex token wins.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
fn fetch_sha256(url: &str) -> Result<String, String> {
    let body = http_get_string(&format!("{url}.sha256"))?;
    let token = body
        .split_whitespace()
        .map(|t| t.strip_prefix("sha256:").unwrap_or(t))
        .find(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("no sha256 digest in {url}.sha256"))?;
    Ok(token.to_lowercase())
}

/// Hard cap on a downloaded artifact so a poisoned CDN can't fill the disk
/// before the checksum mismatch fires. Well above any real macros artifact.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
const MAX_DOWNLOAD_BYTES: u64 = 500 * 1024 * 1024;

/// Stream `url` to `dest`, hashing as we go. Writes to `<dest>.partial` and only
/// renames to `dest` once the SHA-256 matches `expected_sha256_hex`. Refuses
/// non-`https` URLs (except `http` in debug builds, for local dev servers).
///
/// `on_progress` receives the percent complete (0–100) each time it advances by a
/// whole point, computed from the `Content-Length`; if the server sends no length
/// it is never called (the caller leaves its indicator indeterminate).
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
fn download_verified(
    url: &str,
    expected_sha256_hex: &str,
    dest: &Path,
    mut on_progress: impl FnMut(u8),
) -> Result<(), String> {
    use sha2::{Digest, Sha256};

    let lc = url.to_ascii_lowercase();
    let scheme_ok =
        lc.starts_with("https://") || (cfg!(debug_assertions) && lc.starts_with("http://"));
    if !scheme_ok {
        return Err(format!("refusing to download from non-https url: {url}"));
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating cache dir: {e}"))?;
    }
    let partial = dest.with_extension("partial");
    let _ = std::fs::remove_file(&partial);

    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;

    let total = resp.body().content_length();
    if let Some(len) = total
        && len > MAX_DOWNLOAD_BYTES
    {
        return Err(format!(
            "update artifact is {len} bytes, over the {MAX_DOWNLOAD_BYTES}-byte cap"
        ));
    }

    let mut reader = resp.into_body().into_reader();
    let mut file =
        std::fs::File::create(&partial).map_err(|e| format!("creating {partial:?}: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    let mut last_pct: u8 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("reading {url}: {e}"))?;
        if n == 0 {
            break;
        }
        written = written.saturating_add(n as u64);
        if written > MAX_DOWNLOAD_BYTES {
            let _ = std::fs::remove_file(&partial);
            return Err(format!(
                "update artifact exceeded the {MAX_DOWNLOAD_BYTES}-byte cap"
            ));
        }
        // Report progress only when the whole-percent figure advances, so the
        // callback (and any channel behind it) isn't spammed every 64 KiB.
        if let Some(total) = total.filter(|t| *t > 0) {
            let pct = ((written.min(total) * 100) / total) as u8;
            if pct != last_pct {
                last_pct = pct;
                on_progress(pct);
            }
        }
        hasher.update(&buf[..n]);
        use std::io::Write;
        file.write_all(&buf[..n])
            .map_err(|e| format!("writing {partial:?}: {e}"))?;
    }
    file.sync_all()
        .map_err(|e| format!("flushing {partial:?}: {e}"))?;
    drop(file);

    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256_hex) {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "checksum mismatch: expected {expected_sha256_hex}, got {actual}"
        ));
    }
    std::fs::rename(&partial, dest).map_err(|e| format!("finalizing download: {e}"))?;
    Ok(())
}

/// Open `url` in the user's default browser. Used when a build can't update
/// itself in place (see [`InstallOutcome::OpenedDownloadPage`]).
pub(crate) fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("/usr/bin/open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        crate::util::HideConsole::hide_console(&mut c);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let mut child = cmd.spawn().map_err(|e| format!("opening {url}: {e}"))?;
    // The opener hands off to the browser and exits; reap it so it doesn't
    // linger as a zombie for the rest of the session.
    std::thread::spawn(move || child.wait());
    Ok(())
}

// ── Once-per-day gate ────────────────────────────────────────────────────────

/// Stamp file recording the last auto-check time (unix seconds).
fn last_check_path() -> Option<std::path::PathBuf> {
    Some(dirs::cache_dir()?.join("macros").join("last-update-check"))
}

/// Minimum interval between launch-time auto-checks.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn due_for_check() -> bool {
    let Some(path) = last_check_path() else {
        return true;
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return true; // never checked
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    modified
        .elapsed()
        .map_or(true, |elapsed| elapsed >= CHECK_INTERVAL) // clock moved backwards → just check
}

fn record_check_now() {
    if let Some(path) = last_check_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Touch: contents don't matter, only the mtime.
        let _ = std::fs::write(&path, current_version().as_bytes());
    }
}

// ── macOS platform ───────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos {
    //! Real in-place self-update via `.app` bundle swap. macOS's BSD layer keeps
    //! the running process's inodes alive after the bundle is renamed, so the
    //! swap-while-running technique is safe (Sparkle and friends do the same).
    //! Ported from sigma's `crates/licensing/src/platform/macos.rs`.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::cache_dir;

    /// Path to the running `Macros.app` bundle. Errors for non-bundled dev
    /// builds (`cargo run`), which therefore never trigger update/relocation.
    pub fn current_app_path() -> Result<PathBuf, String> {
        let mut p = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        // current_exe is .../Macros.app/Contents/MacOS/macros — walk up to `.app`.
        while p.extension().and_then(|s| s.to_str()) != Some("app") {
            if !p.pop() {
                return Err("not running from a .app bundle".to_string());
            }
        }
        Ok(p)
    }

    /// The Team ID a bundle is signed with, via `codesign -dv`; `None` when it
    /// is unsigned, ad-hoc signed, or codesign fails.
    pub fn team_id(app: &Path) -> Option<String> {
        let out = Command::new("/usr/bin/codesign")
            .arg("-dv")
            .arg(app)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // codesign prints its display info on stderr.
        parse_team_id(&String::from_utf8_lossy(&out.stderr))
    }

    /// Pull `TeamIdentifier=<id>` out of `codesign -dv` output (`not set` for
    /// ad-hoc signatures → `None`).
    pub(super) fn parse_team_id(display: &str) -> Option<String> {
        display
            .lines()
            .find_map(|l| l.trim().strip_prefix("TeamIdentifier="))
            .map(str::trim)
            .filter(|id| !id.is_empty() && *id != "not set")
            .map(str::to_string)
    }

    /// Refuse `app` unless its signature verifies (deep, strict) and it is
    /// signed by `expected_team`.
    fn verify_signature(app: &Path, expected_team: &str) -> Result<(), String> {
        let out = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app)
            .output()
            .map_err(|e| format!("codesign: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "the update's code signature is invalid: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        match team_id(app) {
            Some(team) if team == expected_team => Ok(()),
            Some(team) => Err(format!(
                "the update is signed by team {team}, expected {expected_team}"
            )),
            None => Err("the update has no signing team".to_string()),
        }
    }

    /// Extract the verified `downloaded` zip, check the bundle is signed by
    /// `team_id`, and atomically swap its `Macros.app` into the running
    /// bundle's location.
    pub fn stage_in_place(downloaded: &Path, team_id: &str) -> Result<(), String> {
        let app_path = current_app_path()?;
        // A bundle on a read-only volume (DMG / App Translocation mount) can't be
        // swapped in place — fail with an actionable message up front.
        if volume_is_read_only(&app_path) {
            return Err("Macros is running from a read-only location — move it to \
                 /Applications first, then update."
                .to_string());
        }
        let staging = cache_dir()?.join(format!("staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| format!("creating staging dir: {e}"))?;

        // 1. Extract.
        let status = Command::new("/usr/bin/unzip")
            .args(["-q", "-o"])
            .arg(downloaded)
            .arg("-d")
            .arg(&staging)
            .status()
            .map_err(|e| format!("unzip: {e}"))?;
        if !status.success() {
            return Err("failed to extract the update archive".to_string());
        }
        let new_app = staging.join("Macros.app");
        if !new_app.exists() {
            return Err("update archive did not contain Macros.app".to_string());
        }
        // Defense-in-depth against zip-slip: the extracted bundle must
        // canonicalize to a path strictly under the staging directory.
        let staging_canon =
            std::fs::canonicalize(&staging).map_err(|e| format!("canonicalize staging: {e}"))?;
        let new_app_canon =
            std::fs::canonicalize(&new_app).map_err(|e| format!("canonicalize new app: {e}"))?;
        if !new_app_canon.starts_with(&staging_canon) {
            return Err("update archive escaped the staging directory".to_string());
        }

        // 2. Verify the code signature and signing team. The SHA-256 only ties
        //    the archive to the manifest host; this ties it to our identity.
        if let Err(e) = verify_signature(&new_app, team_id) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        // 3. Only now strip quarantine — BEFORE the swap so an early relaunch
        //    from Finder doesn't trip Gatekeeper.
        let _ = Command::new("/usr/bin/xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&new_app)
            .status();

        // 4. Swap the bundle in place. Destroying the old inode after the swap
        //    lets a pinned Dock alias re-bind to the new bundle at the same path
        //    instead of chasing the old inode (the "duplicate Dock icon" bug).
        let needs_admin = app_path.starts_with("/Applications") && !writable_by_me(&app_path);

        if needs_admin {
            // `mv old → bak; mv new → old; rm -rf bak` (rolling `bak` back if
            // the second move fails), quoted by AppleScript's `quoted form of`
            // (shell-injection-safe for any path).
            let bak = app_path.with_extension("app.bak");
            let bak_lit = applescript_string_literal(&bak.to_string_lossy());
            let app_lit = applescript_string_literal(&app_path.to_string_lossy());
            let new_lit = applescript_string_literal(&new_app.to_string_lossy());
            let shell = admin_swap_command(r#"" & b & ""#, r#"" & a & ""#, r#"" & n & ""#);
            let script = format!(
                "set b to quoted form of {bak_lit}\n\
                 set a to quoted form of {app_lit}\n\
                 set n to quoted form of {new_lit}\n\
                 do shell script \"{shell}\" with administrator privileges"
            );
            let status = Command::new("osascript")
                .arg("-e")
                .arg(script)
                .status()
                .map_err(|e| format!("osascript: {e}"))?;
            if !status.success() {
                return Err("failed to install the update (admin step)".to_string());
            }
        } else {
            match renamex_swap(&new_app, &app_path) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&staging);
                }
                Err(_) => {
                    // Fallback if `renamex_np` is unavailable: move the old bundle
                    // aside *into staging* (not a sibling `.app.bak` the Dock alias
                    // would chase), move the new one in, then delete the old.
                    let aside = staging.join("old-Macros.app");
                    let _ = std::fs::remove_dir_all(&aside);
                    std::fs::rename(&app_path, &aside)
                        .map_err(|e| format!("moving old bundle aside: {e}"))?;
                    if let Err(e) = std::fs::rename(&new_app, &app_path) {
                        let _ = std::fs::rename(&aside, &app_path); // roll back
                        return Err(format!("swapping in the new bundle: {e}"));
                    }
                    let _ = std::fs::remove_dir_all(&staging);
                }
            }
        }
        Ok(())
    }

    /// Spawn the detached re-open helper. The swap already happened in
    /// `stage_in_place`, so this just waits for our pid to exit then re-opens.
    pub fn relaunch() -> Result<(), String> {
        let app_path = current_app_path()?;
        spawn_reopen_helper(&app_path)
    }

    /// True if the running bundle is on a read-only volume.
    pub fn needs_move_to_applications() -> bool {
        current_app_path().is_ok_and(|p| volume_is_read_only(&p))
    }

    /// Copy the running bundle into `/Applications` (replacing any existing
    /// copy), strip quarantine, and spawn a helper to relaunch from there.
    pub fn move_to_applications() -> Result<(), String> {
        let src = current_app_path()?;
        let file_name = src.file_name().ok_or("bundle has no file name")?.to_owned();
        let dest = Path::new("/Applications").join(&file_name);

        // Stage a copy first: `src` may be a translocation view *backed by*
        // `dest`, so nothing at the destination may be touched until the running
        // bundle has been fully copied out.
        let staging = cache_dir()?.join(format!("move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| format!("creating staging dir: {e}"))?;
        let staged = staging.join(&file_name);

        // `ditto` preserves the code signature, resource forks, and permissions.
        let status = Command::new("/usr/bin/ditto")
            .arg(&src)
            .arg(&staged)
            .status()
            .map_err(|e| format!("ditto: {e}"))?;
        if !status.success() {
            return Err("failed to copy the app bundle".to_string());
        }
        let _ = Command::new("/usr/bin/xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(&staged)
            .status();

        // Install: replace whatever sits at `dest`. Try as the current user
        // first; fall back to an admin-privileged shell if /Applications isn't
        // ours to touch.
        let direct = (|| -> std::io::Result<()> {
            match dest.symlink_metadata() {
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&dest)?,
                Ok(_) => std::fs::remove_file(&dest)?,
                Err(_) => {}
            }
            let status = Command::new("/usr/bin/ditto")
                .arg(&staged)
                .arg(&dest)
                .status()?;
            if !status.success() {
                return Err(std::io::Error::other("ditto into /Applications failed"));
            }
            Ok(())
        })();
        if direct.is_err() {
            let staged_lit = applescript_string_literal(&staged.to_string_lossy());
            let dest_lit = applescript_string_literal(&dest.to_string_lossy());
            let script = format!(
                "set s to quoted form of {staged_lit}\n\
                 set d to quoted form of {dest_lit}\n\
                 do shell script \"rm -rf \" & d & \" && /usr/bin/ditto \" & s & \" \" & d with administrator privileges"
            );
            let status = Command::new("osascript")
                .arg("-e")
                .arg(script)
                .status()
                .map_err(|e| format!("osascript: {e}"))?;
            if !status.success() {
                return Err("failed to move Macros to /Applications".to_string());
            }
        }
        let _ = std::fs::remove_dir_all(&staging);

        spawn_reopen_helper(&dest)
    }

    /// Spawn a detached helper that waits for *this* process to fully exit, then
    /// `open`s `app_path`. Plain `open` (not `open -n`) lets Launch Services
    /// re-bind the user's existing pinned Dock tile. The poll loop avoids racing
    /// the still-quitting old instance. Reparented to launchd, it outlives us.
    fn spawn_reopen_helper(app_path: &Path) -> Result<(), String> {
        let pid = std::process::id();
        let script = format!(
            "while /bin/kill -0 {pid} 2>/dev/null; do /bin/sleep 0.2; done; exec /usr/bin/open \"$1\""
        );
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            // App path as `$1` (after the `sh` placeholder for `$0`) so it needs
            // no shell quoting regardless of spaces/quotes.
            .arg("sh")
            .arg(app_path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("spawning relaunch helper: {e}"))
    }

    /// The shell command behind the admin bundle swap, over already-quoted
    /// `bak` / `app` / `new` operands: move the old bundle aside, move the new
    /// one in — restoring the old one if that fails, so a failed update never
    /// leaves the app missing — then delete the old one.
    pub(super) fn admin_swap_command(bak: &str, app: &str, new: &str) -> String {
        format!(
            "rm -rf {bak} && mv {app} {bak} && {{ mv {new} {app} || {{ mv {bak} {app}; exit 1; }}; }} && rm -rf {bak}"
        )
    }

    /// Can this process swap the bundle at `path` without admin rights? The
    /// rename needs write permission on the containing directory (and, for a
    /// directory moved by rename, on the bundle itself). `access(2)` checks
    /// against our real uid/gid, so a root-owned bundle with the owner-write
    /// bit set no longer looks writable to a regular user.
    fn writable_by_me(path: &Path) -> bool {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let can_write = |p: &Path| {
            CString::new(p.as_os_str().as_bytes())
                // SAFETY: a valid NUL-terminated path; `access` only reads it.
                .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
        };
        path.parent().is_some_and(can_write) && can_write(path)
    }

    /// True if the filesystem holding `path` is mounted read-only. Mode bits on
    /// DMG / App-Translocation volumes still claim writability, so the mount
    /// flags are the only reliable signal.
    fn volume_is_read_only(path: &Path) -> bool {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `libc::statfs` is a plain C struct of integers and byte
        // arrays, for which the all-zero bit pattern is a valid value.
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is a valid NUL-terminated string and `sfs` is a valid
        // out-pointer; `statfs` only writes into it on success.
        let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut sfs) };
        rc == 0 && (sfs.f_flags & libc::MNT_RDONLY as u32) != 0
    }

    /// Atomically exchange the directory entries at `from` and `to` via
    /// `renamex_np(2)` with `RENAME_SWAP` (macOS 10.12+). No window where either
    /// path is missing.
    fn renamex_swap(from: &Path, to: &Path) -> std::io::Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let from_c = CString::new(from.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL byte in path")
        })?;
        let to_c = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL byte in path")
        })?;

        // SAFETY: both pointers are valid NUL-terminated C strings that outlive
        // the call; `renamex_np` only reads them.
        let rc = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_SWAP) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wrap `s` in AppleScript double quotes, escaping `\` and `"`. AppleScript
    /// string literals recognise only `\\` and `\"` inside `"..."`.
    fn applescript_string_literal(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
}

// ── Windows platform ─────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows {
    //! Staged MSI install. Windows won't overwrite a running exe, so the
    //! verified installer runs from a detached helper once this process has
    //! exited. The MSI is per-machine, so it raises one UAC prompt; it runs
    //! `/passive` (a progress bar, no dialogs). A declined prompt or a failed
    //! install just re-opens the old build — the next check offers it again.

    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Mutex;

    use crate::util::HideConsole;

    /// The verified installer [`super::install`] staged for [`relaunch`].
    static STAGED_MSI: Mutex<Option<PathBuf>> = Mutex::new(None);

    /// Windows PowerShell 5.1, by absolute path so `PATH` can't substitute it.
    /// It ships with every supported Windows and has `Get-AuthenticodeSignature`.
    fn powershell() -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
    }

    /// Prints the signer's subject when `$env:MACROS_SIG_PATH` carries a valid
    /// Authenticode signature chaining to a trusted root; exits 1 otherwise.
    const SIGNER_SCRIPT: &str = "$s = Get-AuthenticodeSignature -LiteralPath $env:MACROS_SIG_PATH; \
         if ($s.Status -ne 'Valid' -or -not $s.SignerCertificate) { exit 1 }; \
         $s.SignerCertificate.Subject";

    /// The certificate subject `path` is validly signed with, or `None` when
    /// it is unsigned, its signature doesn't verify, or PowerShell fails. The
    /// subject (not the thumbprint) is the stable identity: Trusted Signing
    /// issues short-lived certificates, so the thumbprint changes per release.
    pub fn authenticode_signer(path: &Path) -> Option<String> {
        let out = Command::new(powershell())
            .args(["-NoProfile", "-NonInteractive", "-Command", SIGNER_SCRIPT])
            // The path travels in the environment, never the script text, so
            // it needs no PowerShell quoting.
            .env("MACROS_SIG_PATH", path)
            .hide_console()
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!subject.is_empty()).then_some(subject)
    }

    /// Remember the verified `msi` for [`relaunch`] to install.
    pub fn stage(msi: PathBuf) {
        if let Ok(mut staged) = STAGED_MSI.lock() {
            *staged = Some(msi);
        }
    }

    /// Wait for `$env:MACROS_UPDATE_PID` to exit, run the staged installer (if
    /// any) elevated into the running exe's directory, then re-open the exe.
    /// Paths with spaces are quoted for msiexec; Windows paths can't contain
    /// `"`, so that quoting can't be broken out of.
    const APPLY_SCRIPT: &str = r#"
Wait-Process -Id ([int]$env:MACROS_UPDATE_PID) -ErrorAction SilentlyContinue
if ($env:MACROS_UPDATE_MSI) {
  $a = @('/i', ('"{0}"' -f $env:MACROS_UPDATE_MSI), '/passive', '/norestart',
         ('INSTALLDIR="{0}"' -f $env:MACROS_UPDATE_DIR.TrimEnd('\')),
         '/l*v', ('"{0}"' -f $env:MACROS_UPDATE_LOG))
  try {
    Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\msiexec.exe') `
      -ArgumentList $a -Verb RunAs -Wait
  } catch {}
  Remove-Item -LiteralPath $env:MACROS_UPDATE_MSI -ErrorAction SilentlyContinue
}
Start-Process -FilePath $env:MACROS_UPDATE_EXE
"#;

    /// Spawn the detached helper running [`APPLY_SCRIPT`]. It outlives us:
    /// Windows doesn't kill a child when its parent exits.
    pub fn relaunch() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut cmd = Command::new(powershell());
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", APPLY_SCRIPT])
            .env("MACROS_UPDATE_PID", std::process::id().to_string())
            .env("MACROS_UPDATE_EXE", &exe);
        let staged = STAGED_MSI.lock().ok().and_then(|s| s.clone());
        if let Some(msi) = staged {
            // Install over the current location, so a custom install dir
            // survives the upgrade and the relaunch finds the new exe.
            let dir = exe.parent().ok_or("the running exe has no directory")?;
            cmd.env("MACROS_UPDATE_MSI", &msi)
                .env("MACROS_UPDATE_DIR", dir)
                .env("MACROS_UPDATE_LOG", msi.with_extension("log"));
        }
        cmd.hide_console()
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("spawning the update helper: {e}"))
    }
}

// ── Linux platform ───────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    //! Package-manager install. dpkg and rpm replace `/usr/bin/macros` by
    //! rename, so the running process keeps its (now unlinked) binary and the
    //! new one takes effect on relaunch. Installing needs root: `pkexec` asks
    //! through the desktop's polkit agent.

    use std::path::Path;
    use std::process::{Command, Stdio};

    /// Where our `.deb` / `.rpm` installs the binary (see `nfpm.yaml`).
    const INSTALLED_BIN: &str = "/usr/bin/macros";
    /// The package name in `nfpm.yaml`.
    const PACKAGE_NAME: &str = "macros";

    #[derive(Clone, Copy)]
    pub enum Package {
        Deb,
        Rpm,
    }

    impl Package {
        /// The artifact extension, also the manifest key suffix
        /// (`linux-x86_64-deb`).
        pub fn ext(self) -> &'static str {
            match self {
                Package::Deb => "deb",
                Package::Rpm => "rpm",
            }
        }
    }

    /// The package format that installed the running binary, when it is our
    /// own package's `/usr/bin/macros`. `None` for a cargo build, a tarball,
    /// or a binary owned by some other package.
    pub fn installed_package() -> Option<Package> {
        let exe = std::env::current_exe().ok()?;
        if exe != Path::new(INSTALLED_BIN) {
            return None;
        }
        let query = |prog: &str, args: &[&str]| {
            Command::new(prog)
                .args(args)
                .arg(INSTALLED_BIN)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        if query("dpkg-query", &["-S"]).is_some_and(|o| dpkg_owner_is_ours(&o)) {
            Some(Package::Deb)
        } else if query("rpm", &["-qf", "--queryformat", "%{NAME}"])
            .is_some_and(|o| o.trim() == PACKAGE_NAME)
        {
            Some(Package::Rpm)
        } else {
            None
        }
    }

    /// `dpkg-query -S` prints `<pkg>[:<arch>][, <pkg>…]: <path>` per match.
    pub(super) fn dpkg_owner_is_ours(output: &str) -> bool {
        output.lines().any(|line| {
            line.rsplit_once(": ").is_some_and(|(owners, _)| {
                owners
                    .split(", ")
                    .any(|o| o.split(':').next() == Some(PACKAGE_NAME))
            })
        })
    }

    /// Install the verified package at `file` as root. apt-get (when present)
    /// resolves any new dependencies; plain `dpkg -i` / `rpm -U` otherwise.
    pub fn install_package(kind: Package, file: &Path) -> Result<(), String> {
        let pkexec = Path::new("/usr/bin/pkexec");
        if !pkexec.exists() {
            return Err("installing the update needs pkexec (polkit), which isn't \
                 installed — install the package from the downloads page instead"
                .to_string());
        }
        let mut cmd = Command::new(pkexec);
        match kind {
            Package::Deb if Path::new("/usr/bin/apt-get").exists() => {
                cmd.args(["/usr/bin/apt-get", "install", "-y"])
            }
            Package::Deb => cmd.args(["/usr/bin/dpkg", "-i"]),
            Package::Rpm => cmd.args(["/usr/bin/rpm", "-U"]),
        };
        let out = cmd
            .arg(file)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("pkexec: {e}"))?;
        match out.status.code() {
            Some(0) => Ok(()),
            // pkexec's own codes: the auth dialog was dismissed or denied.
            Some(126 | 127) => {
                Err("authorization was cancelled — the update wasn't installed".to_string())
            }
            _ => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let reason = stderr.lines().rev().find(|l| !l.trim().is_empty());
                Err(format!(
                    "installing the update failed: {}",
                    reason.unwrap_or("unknown error").trim()
                ))
            }
        }
    }

    /// Spawn a detached helper that waits for *this* process to exit, then
    /// execs the (now updated) installed binary. Its own process group keeps a
    /// Ctrl-C in the launching terminal from killing it.
    pub fn relaunch() -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        let pid = std::process::id();
        let script = format!("while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; exec \"$1\"");
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .arg(INSTALLED_BIN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("spawning relaunch helper: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one connection on a fresh port with the raw `response` (read the
    /// request head first so the client isn't reset mid-send); returns the URL.
    fn serve_once(response: Vec<u8>) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!(
            "http://127.0.0.1:{}/a",
            listener.local_addr().unwrap().port()
        );
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                line.clear();
            }
            let _ = conn.write_all(&response);
        });
        url
    }

    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn temp_dest(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("macros-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("artifact.zip")
    }

    #[test]
    fn http_get_string_reads_body_and_fails_on_error_status() {
        assert_eq!(
            http_get_string(&serve_once(ok_response(b"{\"v\":1}"))).unwrap(),
            "{\"v\":1}"
        );
        let missing = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope"
                .to_vec(),
        );
        assert!(http_get_string(&missing).is_err());
    }

    #[test]
    fn download_verified_writes_on_match_and_reports_progress() {
        use sha2::{Digest, Sha256};
        let body = vec![7u8; 200 * 1024];
        let sha = hex::encode(Sha256::digest(&body));
        let dest = temp_dest("ok");
        let mut pcts = Vec::new();
        download_verified(&serve_once(ok_response(&body)), &sha, &dest, |p| {
            pcts.push(p)
        })
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!dest.with_extension("partial").exists());
        assert_eq!(
            pcts.last(),
            Some(&100),
            "progress from Content-Length: {pcts:?}"
        );
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_verified_rejects_bad_checksum_error_status_and_oversize() {
        let dest = temp_dest("bad");
        let url = serve_once(ok_response(b"tampered"));
        let err = download_verified(&url, &"0".repeat(64), &dest, |_| {}).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!dest.exists() && !dest.with_extension("partial").exists());

        // A 404 page must not be "downloaded" and checksummed as the artifact.
        let url = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope"
                .to_vec(),
        );
        let err = download_verified(&url, &"0".repeat(64), &dest, |_| {}).unwrap_err();
        assert!(err.starts_with("GET "), "{err}");

        // A declared length over the cap is refused before reading the body.
        let url = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_DOWNLOAD_BYTES + 1
            )
            .into_bytes(),
        );
        let err = download_verified(&url, &"0".repeat(64), &dest, |_| {}).unwrap_err();
        assert!(err.contains("over the"), "{err}");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn manifest_override_only_when_allowed() {
        let default = "https://macros.dev/latest.json";
        let local = Some("http://localhost:3000/latest.json".to_string());
        assert_eq!(manifest_url_from(local.clone(), false), default);
        assert_eq!(
            manifest_url_from(local, true),
            "http://localhost:3000/latest.json"
        );
        assert_eq!(manifest_url_from(Some(String::new()), true), default);
        assert_eq!(manifest_url_from(None, true), default);
    }

    #[test]
    fn artifact_file_name_takes_plain_names_with_the_right_extension() {
        let base = "https://downloads.macros.dev";
        assert_eq!(
            artifact_file_name(&format!("{base}/macros_0.7.0_amd64.deb"), "deb").as_deref(),
            Some("macros_0.7.0_amd64.deb")
        );
        assert_eq!(
            artifact_file_name(
                &format!("{base}/Macros-windows-x86_64-0.7.0.msi?x=1"),
                "msi"
            )
            .as_deref(),
            Some("Macros-windows-x86_64-0.7.0.msi")
        );
        // Wrong extension, odd characters, and dotfiles are refused.
        assert_eq!(artifact_file_name(&format!("{base}/a.rpm"), "deb"), None);
        assert_eq!(
            artifact_file_name(&format!("{base}/a%20b.deb"), "deb"),
            None
        );
        assert_eq!(artifact_file_name(&format!("{base}/.deb"), "deb"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dpkg_owner_matches_only_our_package() {
        assert!(linux::dpkg_owner_is_ours("macros: /usr/bin/macros\n"));
        assert!(linux::dpkg_owner_is_ours("macros:amd64: /usr/bin/macros\n"));
        assert!(linux::dpkg_owner_is_ours(
            "other, macros: /usr/bin/macros\n"
        ));
        assert!(!linux::dpkg_owner_is_ours(
            "macros-extra: /usr/bin/macros\n"
        ));
        assert!(!linux::dpkg_owner_is_ours(""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn admin_swap_rolls_back_when_the_new_bundle_cannot_move_in() {
        let dir = std::env::temp_dir().join(format!("macros-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Macros.app")).unwrap();
        std::fs::write(dir.join("Macros.app/marker"), "old").unwrap();
        let q = |name: &str| format!("'{}'", dir.join(name).display());
        // The new bundle doesn't exist, so its `mv` fails.
        let cmd = macos::admin_swap_command(&q("Macros.app.bak"), &q("Macros.app"), &q("New.app"));
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .status()
            .unwrap();
        assert!(!status.success(), "a failed swap must report failure");
        assert_eq!(
            std::fs::read_to_string(dir.join("Macros.app/marker")).unwrap(),
            "old",
            "the old bundle is restored"
        );
        // And the happy path swaps + cleans up the backup.
        std::fs::create_dir_all(dir.join("New.app")).unwrap();
        std::fs::write(dir.join("New.app/marker"), "new").unwrap();
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(dir.join("Macros.app/marker")).unwrap(),
            "new"
        );
        assert!(!dir.join("Macros.app.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_codesign_team_id() {
        let signed = "Executable=/Applications/Macros.app/Contents/MacOS/macros\n\
                      Identifier=dev.macros\nFormat=app bundle with Mach-O thin (arm64)\n\
                      TeamIdentifier=ABCDE12345\nSealed Resources version=2\n";
        assert_eq!(macos::parse_team_id(signed).as_deref(), Some("ABCDE12345"));
        let adhoc = "Signature=adhoc\nTeamIdentifier=not set\n";
        assert_eq!(macos::parse_team_id(adhoc), None);
        assert_eq!(
            macos::parse_team_id("code object is not signed at all"),
            None
        );
    }
}
