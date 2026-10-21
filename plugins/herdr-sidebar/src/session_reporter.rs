//! Elects ONE sidebar process per herdr session to run the session-wide
//! reporters ([`crate::agent_tokens`] and [`crate::pi_state`]).
//!
//! Those reporters read `pane.list` and write data about every workspace and
//! pi pane in the session, so each sidebar process running them repeats the
//! same server work: N sidebars cost N times the herdr server CPU for
//! identical reports. The election is an OS file lock in the private runtime
//! directory. The first sidebar to take it reports; the kernel releases the
//! lock when that process dies, and the next sidebar's beat takes over.
//!
//! The lock is held process-wide (a static), not by the reporter struct:
//! flock and LockFileEx are per open file, so a second handle in the same
//! process would block against the first. The unified sidebar rebuilds its
//! app on every view switch, and leadership must survive that.
//!
//! The lock file name carries a hash of `HERDR_SOCKET_PATH`, because
//! `rundir` is shared by every herdr session of this user, and each session
//! needs its own reporter.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The lock this process holds, once it has won the election.
static HELD: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// Whether this process should run the session-wide reporters now.
///
/// Winning is sticky: a process that holds the lock keeps it for life. A
/// process that lost retries on each call, so a dead leader is replaced
/// within one reporter interval. Any I/O error counts as leadership (fail
/// open): duplicate reports are harmless, while no reports would blank the
/// rail.
pub fn is_session_reporter() -> bool {
    let held = HELD.get_or_init(|| Mutex::new(None));
    let Ok(mut guard) = held.lock() else {
        return true;
    };
    if guard.is_some() {
        return true;
    }
    let Some(path) = lock_path() else {
        return true;
    };
    match try_acquire(&path) {
        Ok(Some(file)) => {
            *guard = Some(file);
            true
        }
        Ok(None) => false,
        Err(_) => true,
    }
}

/// The per-session lock file: `reporters/session-reporter-<hash>.lock` under
/// the runtime directory.
fn lock_path() -> Option<PathBuf> {
    let dir = crate::rundir::dir("reporters");
    Some(dir.join(format!("session-reporter-{:016x}.lock", session_hash())))
}

/// FNV-1a over the socket path, so the name is stable across processes and
/// Rust releases (`DefaultHasher` makes no such promise). Without a socket
/// path every such process shares the one "default" name.
fn session_hash() -> u64 {
    let key = crate::ipc::socket_path()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Try to take the election lock at `path` without blocking.
///
/// `Ok(Some(file))` means this caller won and must keep `file` alive for as
/// long as it leads. `Ok(None)` means another open handle holds the lock.
/// `Err` means the lock file could not be created.
fn try_acquire(path: &Path) -> std::io::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        crate::rundir::ensure_private(dir)?;
    }
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_lock(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-sidebar-session-reporter-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        crate::rundir::ensure_private(&dir).unwrap();
        dir.join("session-reporter.lock")
    }

    #[test]
    fn election_goes_to_one_handle_until_the_leader_is_dropped() {
        let path = scratch_lock("election");
        let leader = try_acquire(&path).unwrap();
        assert!(leader.is_some(), "the first handle takes the lock");

        let rival = try_acquire(&path).unwrap();
        assert!(rival.is_none(), "a second handle is refused while held");

        drop(leader);
        let successor = try_acquire(&path).unwrap();
        assert!(successor.is_some(), "a dead leader's lock is taken over");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn session_hash_is_stable_for_one_socket_path() {
        assert_eq!(session_hash(), session_hash());
    }
}
