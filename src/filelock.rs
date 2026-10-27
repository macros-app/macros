//! Emacs-style lock files (`create-lockfiles`): while a buffer has unsaved
//! changes to `dir/name`, `dir/.#name` records who is editing it, so another
//! editor (Emacs included — the format is the same) can warn before two
//! sessions edit one file.
//!
//! The lock is a symlink whose target is `user@host.pid` (a regular file
//! holding that text where symlinks aren't available). A lock left behind by a
//! process on this host that is no longer running is stale and is taken over.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

thread_local! {
    /// The `create-lockfiles` option, set on the UI thread (so a test editor
    /// turning it on doesn't make every other test's saves take locks).
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// Turn locking on or off for this (the UI) thread.
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

/// Whether `create-lockfiles` is on.
pub fn enabled() -> bool {
    ENABLED.with(Cell::get)
}

/// `true` the first time it's asked about `path`: someone else's lock on a
/// file is reported once, not on every keystroke.
pub fn first_warning(path: &Path) -> bool {
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    WARNED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(path.to_path_buf())
}

/// Who holds a lock, as recorded in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockOwner {
    pub user: String,
    pub host: String,
    pub pid: u32,
}

impl LockOwner {
    /// This process.
    pub fn me() -> Self {
        LockOwner {
            user: user_name(),
            host: host_name(),
            pid: std::process::id(),
        }
    }

    /// Parse a lock's contents: `user@host.pid`, optionally followed by
    /// `:boot-time` (Emacs adds it).
    fn parse(text: &str) -> Option<Self> {
        let text = text.split(':').next()?;
        let (user, rest) = text.split_once('@')?;
        let (host, pid) = rest.rsplit_once('.')?;
        Some(LockOwner {
            user: user.to_string(),
            host: host.to_string(),
            pid: pid.parse().ok()?,
        })
    }

    fn render(&self) -> String {
        format!("{}@{}.{}", self.user, self.host, self.pid)
    }

    /// Whether the lock still means something: the process is ours, or still
    /// running. A process on another host can't be checked, so it counts as
    /// alive (as in Emacs).
    fn is_live(&self) -> bool {
        self.host != host_name() || self.pid == std::process::id() || process_alive(self.pid)
    }
}

impl std::fmt::Display for LockOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{} (pid {})", self.user, self.host, self.pid)
    }
}

/// The lock file guarding `path`: `.#name` beside it.
pub fn lock_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let mut lock = std::ffi::OsString::from(".#");
    lock.push(name);
    Some(path.with_file_name(lock))
}

/// Who holds `path`'s lock, if anyone does (stale locks included).
pub fn owner(path: &Path) -> Option<LockOwner> {
    let lock = lock_path(path)?;
    let text = match fs::read_link(&lock) {
        Ok(target) => target.to_string_lossy().into_owned(),
        Err(_) => fs::read_to_string(&lock).ok()?,
    };
    LockOwner::parse(text.trim())
}

/// Another live process's lock on `path`, if there is one.
pub fn locked_by_other(path: &Path) -> Option<LockOwner> {
    owner(path)
        .filter(|o| o.pid != std::process::id() || o.host != host_name())
        .filter(LockOwner::is_live)
}

/// The locks this process holds, by the path they guard, with the id of the
/// editor window that took each (so one window releases only its own).
fn held() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// Whether `owner` holds `path`'s lock.
#[cfg(test)]
pub fn holds(owner: u64, path: &Path) -> bool {
    held().lock().unwrap().get(path) == Some(&owner)
}

/// The paths `owner` holds locks on.
pub fn held_by(owner: u64) -> Vec<PathBuf> {
    held()
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, o)| **o == owner)
        .map(|(p, _)| p.clone())
        .collect()
}

/// Lock `path` for `owner`. `Err` with the other holder when another live
/// process has it; a stale lock is replaced. Failing to write the lock (a
/// read-only directory) is not an error — the edit just goes unguarded.
pub fn acquire(owner: u64, path: &Path) -> Result<(), LockOwner> {
    if let Some(other) = locked_by_other(path) {
        return Err(other);
    }
    let Some(lock) = lock_path(path) else {
        return Ok(());
    };
    let _ = fs::remove_file(&lock); // ours, or stale
    if write_lock(&lock, &LockOwner::me().render()).is_ok() {
        held().lock().unwrap().insert(path.to_path_buf(), owner);
    }
    Ok(())
}

/// Release `path`'s lock if this process holds it.
pub fn release(path: &Path) {
    if held().lock().unwrap().remove(path).is_none() {
        return;
    }
    // Only remove what is still ours: another process may have taken over a
    // lock it thought stale.
    if owner(path).is_some_and(|o| o == LockOwner::me())
        && let Some(lock) = lock_path(path)
    {
        let _ = fs::remove_file(lock);
    }
}

#[cfg(unix)]
fn write_lock(lock: &Path, text: &str) -> io::Result<()> {
    std::os::unix::fs::symlink(text, lock)
}

#[cfg(not(unix))]
fn write_lock(lock: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock)?
        .write_all(text.as_bytes())
}

fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
}

#[cfg(unix)]
fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes for the call's duration.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if ok && len > 0 {
        String::from_utf8_lossy(&buf[..len]).into_owned()
    } else {
        "localhost".to_string()
    }
}

#[cfg(not(unix))]
fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".to_string())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // It exists but belongs to someone else.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true // can't tell cheaply: err on the side of warning
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("macros-lock-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn owners_round_trip_and_emacs_boot_times_parse() {
        let me = LockOwner::me();
        assert_eq!(LockOwner::parse(&me.render()), Some(me));
        let emacs = LockOwner::parse("kyle@box.local.4242:1700000000").unwrap();
        assert_eq!(emacs.host, "box.local");
        assert_eq!(emacs.pid, 4242);
        assert_eq!(LockOwner::parse("garbage"), None);
    }

    #[test]
    fn acquire_and_release_manage_the_dot_hash_file() {
        let dir = scratch("own");
        let file = dir.join("a.txt");
        let lock = dir.join(".#a.txt");
        assert_eq!(acquire(1, &file), Ok(()));
        assert!(fs::symlink_metadata(&lock).is_ok());
        assert!(holds(1, &file));
        assert_eq!(held_by(1), vec![file.clone()]);
        assert_eq!(owner(&file), Some(LockOwner::me()));
        // Our own lock never counts as someone else's.
        assert_eq!(locked_by_other(&file), None);
        release(&file);
        assert!(fs::symlink_metadata(&lock).is_err());
        assert!(!holds(1, &file));
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_live_foreign_lock_is_reported_and_a_stale_one_taken_over() {
        let dir = scratch("foreign");
        let file = dir.join("b.txt");
        let lock = dir.join(".#b.txt");
        // pid 1 (init/launchd) is always running.
        let live = format!("someone@{}.1", host_name());
        std::os::unix::fs::symlink(&live, &lock).unwrap();
        let other = acquire(2, &file).unwrap_err();
        assert_eq!(other.pid, 1);
        assert_eq!(other.user, "someone");
        assert!(!holds(2, &file));
        // A process that has exited left a stale lock: take it over.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        fs::remove_file(&lock).unwrap();
        std::os::unix::fs::symlink(format!("someone@{}.{dead}", host_name()), &lock).unwrap();
        assert_eq!(acquire(2, &file), Ok(()));
        assert_eq!(owner(&file), Some(LockOwner::me()));
        release(&file);
        // Another host's lock can't be checked, so it counts as live.
        std::os::unix::fs::symlink("someone@elsewhere.99", &lock).unwrap();
        assert!(locked_by_other(&file).is_some());
        fs::remove_dir_all(&dir).ok();
    }
}
