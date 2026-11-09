//! Focus changes reach the sidebars as one stamp file instead of per-pane IPC.
//!
//! herdr delivers no per-pane focus events to a TUI's stdin, so both sidebar
//! apps used to ask `pane.get` on a fixed cadence just to learn whether they
//! were focused — the Source Control view every 400 ms, the Explorer every 2 s,
//! in every sidebar pane of the session. Measured 2026-11-09 (18 sidebar
//! panes): ~27 such probes per second, the largest single item of
//! plugin-driven herdr server CPU.
//!
//! The ensure hook ALREADY runs on every focus-ish herdr event (`pane.focused`,
//! `tab.focused`, `workspace.focused`, plus the creation events), so
//! [`crate::ensure`] now stamps the file first — before the auto-open gate and
//! the launcher lock, so even a hook invocation that skips its launcher work
//! still reports the focus change. A sidebar that sees the stamp move probes
//! its own focus ONCE, asking herdr for the LIVE state: two events coalescing
//! into the same stamp value are harmless, because the probe answers with
//! whatever is focused now. A long idle stretch — the user typing inside one
//! pane for an hour — costs zero probes.
//!
//! [`FALLBACK`] keeps the apps correct where the hooks are not (a non-standard
//! install, a dropped burst): a probe at least every 5 s, the same cadence as
//! the sidebar heartbeat, instead of a hard dependency on the hook.
//!
//! The file name carries a hash of `HERDR_SOCKET_PATH`, because the runtime
//! directory is shared by every herdr session of this user (see
//! [`crate::session_reporter`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Probe at least this often even when no stamp moves (hooks absent, a burst
/// coalesced into one stamp, or a stamp that moved before this process read
/// it). Matches the sidebar heartbeat cadence, so worst-case focus detection
/// stays within one beat.
pub const FALLBACK: Duration = Duration::from_secs(5);

/// `<runtime dir>/focus/focus-events-<session hash>.stamp`.
pub fn stamp_path() -> Option<PathBuf> {
    let dir = crate::rundir::dir("focus");
    Some(dir.join(format!(
        "focus-events-{:016x}.stamp",
        crate::session_reporter::session_hash()
    )))
}

/// Unix milliseconds.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// Write `now_ms` as this session's focus stamp. Atomic (temp file + rename)
/// and silent: the stamp is an optimization signal — a failed write only means
/// the apps fall back to their periodic probe.
pub fn write_stamp(now_ms: u64) {
    let Some(path) = stamp_path() else {
        return;
    };
    write_stamp_at(&path, now_ms);
}

/// [`write_stamp`] against an explicit path, for tests.
pub fn write_stamp_at(path: &Path, now_ms: u64) {
    let Some(dir) = path.parent() else {
        return;
    };
    if crate::rundir::ensure_private(dir).is_err() {
        return;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if std::fs::write(&tmp, format!("{now_ms}\n")).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

/// The latest stamp when it moved past `seen`. `None` covers "unchanged",
/// "unreadable" and "absent" — every case where the caller keeps its cache.
pub fn changed_since(seen: u64) -> Option<u64> {
    let path = stamp_path()?;
    changed_since_at(&path, seen)
}

/// [`changed_since`] against an explicit path, for tests.
pub fn changed_since_at(path: &Path, seen: u64) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_stamp(&text).filter(|stamp| *stamp > seen)
}

/// First line, decimal unix milliseconds.
fn parse_stamp(text: &str) -> Option<u64> {
    text.lines().next()?.trim().parse().ok()
}

/// One cached-focus decision for the sidebar apps. Pure and unit-tested:
/// `true` means "run the live probe now and store the answer"; the watcher
/// fields (`seen` stamp, `last` probe time) are updated in place so the next
/// call skips until the stamp moves again or [`FALLBACK`] elapses.
pub fn probe_due(
    seen: &mut u64,
    last: &mut Option<Instant>,
    stamp: Option<u64>,
    now: Instant,
) -> bool {
    let due = last.is_none()
        || stamp.is_some()
        || last.is_some_and(|at| now.duration_since(at) >= FALLBACK);
    if due {
        if let Some(value) = stamp {
            *seen = value;
        }
        *last = Some(now);
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-focus-signal-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn parse_stamp_reads_the_first_line_only() {
        assert_eq!(parse_stamp("123\nrest"), Some(123));
        assert_eq!(parse_stamp(" 456 \n"), Some(456));
        assert_eq!(parse_stamp(""), None);
        assert_eq!(parse_stamp("abc\n"), None);
    }

    #[test]
    fn stamp_round_trips_and_reports_only_newer_values() {
        let path = scratch("roundtrip").join("focus-events.stamp");
        write_stamp_at(&path, 100);
        assert_eq!(changed_since_at(&path, 0), Some(100));
        assert_eq!(changed_since_at(&path, 100), None, "unchanged stamp");
        assert_eq!(changed_since_at(&path, 50), Some(100), "older seen value");

        // A second write replaces the file atomically; the reader sees one or
        // the other, never a partial file.
        write_stamp_at(&path, 200);
        assert_eq!(changed_since_at(&path, 100), Some(200));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_or_corrupt_stamp_is_unchanged() {
        let missing = scratch("missing").join("focus-events.stamp");
        assert_eq!(changed_since_at(&missing, 0), None);

        let corrupt = scratch("corrupt").join("focus-events.stamp");
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        std::fs::write(&corrupt, "not a number\n").unwrap();
        assert_eq!(changed_since_at(&corrupt, 0), None);

        let _ = std::fs::remove_dir_all(corrupt.parent().unwrap());
    }

    #[test]
    fn probe_due_on_the_first_call_only() {
        let mut seen = 0;
        let mut last = None;
        let now = Instant::now();
        assert!(probe_due(&mut seen, &mut last, None, now));
        // No stamp, fallback not elapsed: cache it is.
        assert!(!probe_due(&mut seen, &mut last, None, now));
        // A moved stamp probes once and is consumed (the caller passes the
        // ALREADY filtered value: None means "no stamp newer than seen").
        assert!(probe_due(&mut seen, &mut last, Some(7), now));
        assert_eq!(seen, 7);
        assert!(!probe_due(&mut seen, &mut last, None, now));
    }

    #[test]
    fn probe_due_falls_back_after_the_interval() {
        let mut seen = 0;
        let mut last = None;
        let now = Instant::now();
        assert!(probe_due(&mut seen, &mut last, Some(1), now));
        let later = now + FALLBACK - Duration::from_millis(1);
        assert!(!probe_due(&mut seen, &mut last, None, later));
        let due = now + FALLBACK;
        assert!(probe_due(&mut seen, &mut last, None, due));
    }
}
