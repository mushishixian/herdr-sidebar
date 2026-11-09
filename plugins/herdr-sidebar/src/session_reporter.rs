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
pub(crate) fn session_hash() -> u64 {
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

/// How far past its stamp a snapshot still serves the session's periodic
/// observers. The leader beats every 5 s and a dead leader is replaced within
/// one beat, so 10 s bridges a leadership transition; beyond that the callers
/// are better off with their own fresh fetch.
const SNAPSHOT_MAX_AGE_MS: u64 = 10_000;

/// `<reporters>/pane-list-<session hash>.json`, the elected reporter's latest
/// `pane.list` reply, shared with the session's other sidebar processes. The
/// cwd follower, pi state and tab titles all read the same session-wide list
/// on the same 5 s beat; before the snapshot each of them fetched its own, so
/// the fleet cost scaled with the sidebar count instead of the session count.
fn snapshot_path() -> Option<PathBuf> {
    let dir = crate::rundir::dir("reporters");
    Some(dir.join(format!("pane-list-{:016x}.json", session_hash())))
}

/// Publish a `pane.list` reply as this session's shared snapshot. Called by
/// the ELECTED reporter right after its beat's fetch — the single writer, so
/// temp-file + rename needs no locking. Failures are silent: readers fall
/// back to their own fetch, which is exactly the pre-snapshot behavior.
pub fn publish_pane_list(json: &str) {
    let Some(path) = snapshot_path() else {
        return;
    };
    publish_pane_list_at(&path, json, crate::focus_signal::unix_ms());
}

/// [`publish_pane_list`] against an explicit path, for tests.
pub fn publish_pane_list_at(path: &Path, json: &str, now_ms: u64) {
    let Some(dir) = path.parent() else {
        return;
    };
    if crate::rundir::ensure_private(dir).is_err() {
        return;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&tmp, format!("{now_ms}\n{json}")).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

/// The latest snapshot when it is fresh enough, else one direct `pane.list`
/// fetch. For PERIODIC observers only (cwd follower, pi state, tab titles):
/// their data was already up to one beat old before the snapshot existed.
/// Correctness-critical paths (launch decisions) always fetch directly.
pub fn observer_pane_list() -> Option<String> {
    observer_sample().map(|sample| sample.body)
}

/// One observer sample of the session's shared `pane.list`.
pub struct ObserverSample {
    /// The fetch's unix-ms stamp. [`ObserverSample::shared`] samples carry the
    /// elected leader's fetch time, which advances once per beat; a direct
    /// fetch carries this process's own fetch time.
    pub stamp: u64,
    /// The raw `pane.list` reply, verbatim.
    pub body: String,
    /// `true` when the body came from the shared snapshot; `false` when this
    /// process had to fetch directly (no fresh snapshot — no elected leader
    /// yet, or it just died). Callers pace the two cases differently.
    pub shared: bool,
}

pub fn observer_sample() -> Option<ObserverSample> {
    if let Some(path) = snapshot_path()
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Some((stamp, body)) =
            parse_snapshot(&text, crate::focus_signal::unix_ms(), SNAPSHOT_MAX_AGE_MS)
    {
        return Some(ObserverSample {
            stamp,
            body,
            shared: true,
        });
    }
    let body = crate::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    Some(ObserverSample {
        stamp: crate::focus_signal::unix_ms(),
        body,
        shared: false,
    })
}

/// Pure: the first line is the fetch's unix milliseconds, the rest the raw
/// reply verbatim. A future stamp (clock moved back), an over-aged one, a
/// truncated or empty body all read as "no snapshot".
fn parse_snapshot(text: &str, now_ms: u64, max_age_ms: u64) -> Option<(u64, String)> {
    let (head, rest) = text.split_once('\n')?;
    let stamp: u64 = head.trim().parse().ok()?;
    if stamp > now_ms || now_ms - stamp > max_age_ms {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some((stamp, rest.to_string()))
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

    #[test]
    fn snapshot_round_trips_verbatim_while_fresh() {
        let path = scratch_lock("snapshot").with_file_name("pane-list.json");
        publish_pane_list_at(&path, r#"{"result":{"panes":[]}}"#, 1_000);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            parse_snapshot(&text, 1_500, SNAPSHOT_MAX_AGE_MS),
            Some((1_000, r#"{"result":{"panes":[]}}"#.to_string()))
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_over_aged_or_future_snapshot_is_rejected() {
        let text = "1000\n{\"panes\":[]}\n";
        // Exactly max_age is still fresh; one millisecond more is not.
        assert_eq!(
            parse_snapshot(text, 1_000 + SNAPSHOT_MAX_AGE_MS, SNAPSHOT_MAX_AGE_MS),
            Some((1_000, "{\"panes\":[]}".to_string()))
        );
        assert_eq!(parse_snapshot(text, 1_000 + SNAPSHOT_MAX_AGE_MS + 1, SNAPSHOT_MAX_AGE_MS), None);
        // A stamp from a clock that moved backwards can never age into range.
        assert_eq!(parse_snapshot(text, 500, SNAPSHOT_MAX_AGE_MS), None);
    }

    #[test]
    fn a_truncated_or_corrupt_snapshot_is_rejected() {
        assert_eq!(parse_snapshot("", 1_000, SNAPSHOT_MAX_AGE_MS), None);
        assert_eq!(parse_snapshot("no number\n{}", 1_000, SNAPSHOT_MAX_AGE_MS), None);
        assert_eq!(parse_snapshot("1000\n", 1_000, SNAPSHOT_MAX_AGE_MS), None);
        assert_eq!(parse_snapshot("1000\n   \n", 1_000, SNAPSHOT_MAX_AGE_MS), None);
    }
}
