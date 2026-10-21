//! Keep a herdr space's rail showing `working` while a `pi` agent's BACKGROUND
//! subagent still runs.
//!
//! herdr's spaces rail takes each space's `agent_status` from its panes, and pi
//! feeds that through herdr's managed hook (`herdr integration install pi`,
//! `pane.report_agent` with `source = "herdr:pi"`). The hook reports `idle` the
//! moment pi's main turn settles — so an agent that dispatches a background
//! subagent (`Agent` tool with `run_in_background: true`) makes the space look
//! idle while the child keeps working for minutes (measured: `working` at 1.5 s,
//! `idle` from 4.5 s to 52.7 s while the child ran).
//!
//! pi exposes no cross-process signal for subagents, but it DOES persist its
//! transcript as JSONL under `~/.pi/agent/sessions/…`, and herdr's pane list
//! carries each pi pane's session ref (`agent_session {kind, value}`). So the
//! sidebar — already a long-lived process in every IDE tab with a 5 s heartbeat
//! and a socket client — infers "a child is still running" from that file and
//! re-asserts `working` with the SAME identity the official hook uses
//! ([`SOURCE`]/[`AGENT`]; herdr accepts it because that pair is a full-lifecycle
//! hook identity).
//!
//! This is an INFERENCE over a file another process is writing, so it is built
//! to fail towards today's behaviour: an unreadable, missing, truncated or
//! mis-parsed transcript means "not busy", i.e. we report `idle` and the rail
//! looks exactly as it does without this module. There is no path that reports
//! `working` forever — every `working` needs a currently-live dispatched child
//! (see the liveness rule below) or an in-flight turn in the transcript tail.
//!
//! Public surface:
//!
//! - [`pi_panes`] — pure: `pane.list` JSON → the pi panes with a session ref.
//! - [`summarise`] — pure: transcript JSONL → [`Transcript`].
//! - [`read_transcript`] — file → [`Transcript`], tail-bounded.
//! - [`next_seq`] — the only legal way to build a report sequence (see below).
//! - [`report_params`] — pure: the `pane.report_agent` params object.
//! - [`PiState::report_if_due`] — the reporter both sidebar apps call from their
//!   heartbeat. Only the one sidebar elected per herdr session reports
//!   ([`crate::session_reporter`]), so N sidebars do not repeat the same
//!   `pane.list` and `pane.report_agent` traffic.
//!
//! ## Transcript rules (pi session format v3)
//!
//! `started`: ids from any `message` whose role is `toolResult` and whose joined
//! text content contains `Agent started in background` — the id follows
//! `Agent ID: ` up to the next whitespace. A FOREGROUND run's result reads
//! `Agent completed in 25.4s (…)` instead, so the phrase (not the tool name)
//! is what distinguishes a background child.
//!
//! `done`: ids from `custom` / `subagents:record` (`data.id`),
//! `custom_message` / `subagent-notification` (`details.id`), and any
//! `toolResult` text containing `Agent not found: "<id>"` (a `steer_subagent`
//! against a child that no longer exists), deduped — both record kinds appear
//! once per finished agent. `running = started − done`, each entry carrying the
//! child's `Output file:` path from the start result (when present).
//!
//! The record rule alone is not enough: when pi's session shuts down
//! (`/reload`, a session switch, quit) the pi-subagents extension aborts every
//! live child WITHOUT writing a `subagents:record` or notification, so those
//! ids would stay `started − done` forever and pin the pane at `working`.
//!
//! `turn_active`: the LAST activity marker wins, scanning in order:
//! - `message` role `user`/`system`/`toolResult` → active (pi is about to, or is
//!   currently, taking a turn);
//! - `message` role `assistant` → active iff `stopReason == "toolUse"`, or the
//!   stop reason is absent/null and a content part is a `toolCall` (a turn that
//!   ends in a tool call has not finished);
//! - `custom_message` / `subagent-notification` and `custom` with a
//!   `subagents:*` customType → active (a completion notification starts a turn);
//! - everything else (the `session` header, `model_change`,
//!   `thinking_level_change`, `usage`, other extension `custom` records) →
//!   no marker, the previous one stands. These are exactly the records pi
//!   appends after a settled turn, so treating them as activity would pin a
//!   space on `working` forever.
//!
//! ## Liveness rule
//!
//! A `running` child only counts as busy while its output file is fresh. pi's
//! subagents append to the child's `Output file` after every child turn, so a
//! live child keeps that file's mtime moving. [`PiState`] stats each running
//! child's file on every beat (never cached) and calls [`child_alive`]: a file
//! modified within [`CHILD_STALE`] is alive; a missing file, a file without an
//! `Output file:` path, or a stat error is NOT alive. The cost of this
//! heuristic is deliberate and fails towards idle: a child silent for longer
//! than [`CHILD_STALE`] (e.g. one tool call running over five minutes) reads
//! as idle until it writes again.
//!
//! ## Reporting rules
//!
//! - A pane is reported `working` when [`PiState`] finds it busy (the turn is
//!   open, or a running child is alive per the liveness rule), else `idle`.
//! - A report goes out when the pane's state CHANGED since our last report, or
//!   when [`REPORT_INTERVAL`] has elapsed for that pane. The periodic re-assert
//!   is what heals a dropped report: herdr silently discards a
//!   `pane.report_agent` whose `seq` is not strictly greater than the last one
//!   it accepted, and pi's own hook overrides our state whenever it reports, so
//!   the re-assert also wins the race back after the turn-settled `idle`.
//! - A file is re-read only when its length or mtime changed.
//! - Every socket/IO failure is ignored; the next beat re-reports.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// `pane.report_agent` source id. Identical to herdr's own pi integration on
/// purpose: herdr only accepts a report for a pi pane from the pi identity.
pub const SOURCE: &str = "herdr:pi";

/// `pane.report_agent` agent label.
pub const AGENT: &str = "pi";

/// How often a heartbeat may re-assert state. The apps call this from their 5 s
/// heartbeat, so the rail follows a change within ~5 s.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(3);

/// Transcripts up to this size are read whole.
pub const MAX_READ_BYTES: usize = 2 * 1024 * 1024;

/// Larger transcripts are read from the end: only the tail can say what is
/// running now, and a session file grows to hundreds of megabytes.
pub const TAIL_BYTES: usize = 512 * 1024;

/// Reported state for a busy pane.
const WORKING: &str = "working";

/// Reported state for an idle pane.
const IDLE: &str = "idle";

/// Marker pi writes into a background `Agent` tool's result.
const STARTED_MARKER: &str = "Agent started in background";

/// Marker preceding the child id inside that result.
const AGENT_ID_MARKER: &str = "Agent ID: ";

/// Marker preceding the child's live output file inside that result.
const OUTPUT_FILE_MARKER: &str = "Output file: ";

/// Marker preceding a child id in a `steer_subagent` "not found" result.
const NOT_FOUND_MARKER: &str = "Agent not found: \"";

/// A running child whose output file has not moved for this long is treated as
/// gone (see the liveness rule in the module docs).
pub const CHILD_STALE: Duration = Duration::from_secs(5 * 60);

/// A `pi` pane with a session ref, i.e. one whose transcript we can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiPane {
    pub pane_id: String,
    /// Set when the session ref is a file path (`kind == "path"`).
    pub session_path: Option<String>,
    /// Set when the session ref is an id (any other kind).
    pub session_id: Option<String>,
}

impl PiPane {
    /// The session ref key/value pair a report must carry.
    fn session_field(&self) -> Option<(&'static str, &str)> {
        if let Some(path) = self.session_path.as_deref() {
            return Some(("agent_session_path", path));
        }
        self.session_id
            .as_deref()
            .map(|id| ("agent_session_id", id))
    }
}

/// A dispatched background child that has started and not (yet) reported done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    /// The child's agent id (`Agent ID: <id>`).
    pub id: String,
    /// The child's live output file (`Output file: <path>`), when the start
    /// result named one. Liveness is judged from this file's mtime.
    pub output: Option<String>,
}

/// What a transcript tail says about the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    /// Started background children (in id order) that have not reported done.
    pub running: Vec<Child>,
    /// Whether the last activity marker says a turn is in flight.
    pub turn_active: bool,
}

impl Transcript {
    /// Pi is mid-turn, or a running child is alive according to `child_alive`.
    pub fn busy(&self, child_alive: impl Fn(&Child) -> bool) -> bool {
        self.turn_active || self.running.iter().any(child_alive)
    }
}

/// Whether a running child's output file, last modified at `output_modified`,
/// still counts as live at `now`. `None` (no file, or a stat error) is not
/// alive. A modification time in the future also counts as alive: a clock skew
/// must not read as a dead child.
pub fn child_alive(output_modified: Option<SystemTime>, now: SystemTime) -> bool {
    output_modified.is_some_and(|modified| match now.duration_since(modified) {
        Ok(age) => age < CHILD_STALE,
        Err(_) => true,
    })
}

/// The pi panes of a `pane.list` reply that carry a session ref.
///
/// Identity is `agent_session.source == "herdr:pi"` OR `agent == "pi"`; a pane
/// with no session ref is dropped, because there would be no transcript to
/// read. Malformed JSON yields an empty list — this never panics.
pub fn pi_panes(pane_list_json: &str) -> Vec<PiPane> {
    let Ok(parsed) =
        serde_json::from_str::<serde_json::Value>(crate::launch::strip_bom(pane_list_json))
    else {
        return Vec::new();
    };
    let panes = parsed
        .pointer("/result/panes")
        .and_then(|panes| panes.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    panes.iter().filter_map(pi_pane).collect()
}

/// One `pane.list` entry → a [`PiPane`], when it is a pi pane with a ref.
fn pi_pane(pane: &serde_json::Value) -> Option<PiPane> {
    let pane_id = non_empty(pane.get("pane_id"))?.to_string();
    let session = pane.get("agent_session");
    let is_pi = session
        .and_then(|session| non_empty(session.get("source")))
        .is_some_and(|source| source == SOURCE)
        || non_empty(pane.get("agent")).is_some_and(|agent| agent == AGENT);
    if !is_pi {
        return None;
    }
    let value = session.and_then(|session| non_empty(session.get("value")));
    let kind = session.and_then(|session| non_empty(session.get("kind")));
    let (session_path, session_id) = match (value, kind) {
        (Some(value), Some("path")) => (Some(value.to_string()), None),
        (Some(value), _) => (None, Some(value.to_string())),
        (None, _) => (None, None),
    };
    if session_path.is_none() && session_id.is_none() {
        return None;
    }
    Some(PiPane {
        pane_id,
        session_path,
        session_id,
    })
}

/// Parse a transcript (one JSON object per line) into a [`Transcript`].
///
/// Empty lines and lines that do not parse are skipped: the file can end
/// mid-write, and a torn last line says nothing. See the module docs for the
/// rule set.
pub fn summarise(jsonl: &str) -> Transcript {
    let mut started: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut turn_active = false;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match record.get("type").and_then(|kind| kind.as_str()) {
            Some("message") => {
                let Some(message) = record.get("message") else {
                    continue;
                };
                match message.get("role").and_then(|role| role.as_str()) {
                    Some("user") | Some("system") => turn_active = true,
                    Some("toolResult") => {
                        turn_active = true;
                        let text = joined_text(message.get("content"));
                        if let Some(index) = text.find(STARTED_MARKER)
                            && let Some(id) = agent_id_after(&text[index..])
                        {
                            started.insert(id.to_string(), output_file_after(&text[index..]));
                        }
                        for id in not_found_ids(&text) {
                            done.insert(id.to_string());
                        }
                    }
                    Some("assistant") => {
                        turn_active =
                            match message.get("stopReason").and_then(|reason| reason.as_str()) {
                                Some("toolUse") => true,
                                Some(_) => false,
                                None => has_tool_call(message.get("content")),
                            };
                    }
                    _ => {}
                }
            }
            Some("custom_message")
                if record.get("customType").and_then(|kind| kind.as_str())
                    == Some("subagent-notification") =>
            {
                if let Some(id) = record
                    .pointer("/details/id")
                    .and_then(|id| id.as_str())
                    .filter(|id| !id.is_empty())
                {
                    done.insert(id.to_string());
                }
                turn_active = true;
            }
            Some("custom") => {
                let custom_type = record
                    .get("customType")
                    .and_then(|kind| kind.as_str())
                    .unwrap_or_default();
                if custom_type.starts_with("subagents:") {
                    if custom_type == "subagents:record"
                        && let Some(id) = record
                            .pointer("/data/id")
                            .and_then(|id| id.as_str())
                            .filter(|id| !id.is_empty())
                    {
                        done.insert(id.to_string());
                    }
                    turn_active = true;
                }
            }
            _ => {}
        }
    }
    let running = started
        .into_iter()
        .filter(|(id, _)| !done.contains(id))
        .map(|(id, output)| Child { id, output })
        .collect();
    Transcript {
        running,
        turn_active,
    }
}

/// The joined text of a message content value (a string, or an array of parts
/// whose `text` fields are concatenated).
fn joined_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// The child id right after [`AGENT_ID_MARKER`] (up to the next whitespace).
fn agent_id_after(text: &str) -> Option<&str> {
    let start = text.find(AGENT_ID_MARKER)? + AGENT_ID_MARKER.len();
    let rest = &text[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let id = &rest[..end];
    (!id.is_empty()).then_some(id)
}

/// The output file path on the `Output file: ` line after the start marker
/// (value up to end of line, trimmed). `None` when absent or empty.
fn output_file_after(text: &str) -> Option<String> {
    let start = text.find(OUTPUT_FILE_MARKER)? + OUTPUT_FILE_MARKER.len();
    let line = text[start..].lines().next().unwrap_or_default().trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Every id named by `Agent not found: "<id>"` in a tool result's text.
fn not_found_ids(text: &str) -> Vec<&str> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find(NOT_FOUND_MARKER) {
        let after = &rest[index + NOT_FOUND_MARKER.len()..];
        let Some(end) = after.find('"') else {
            break;
        };
        let id = &after[..end];
        if !id.is_empty() {
            ids.push(id);
        }
        rest = &after[end + 1..];
    }
    ids
}

/// Whether a content value holds a `toolCall` part.
fn has_tool_call(content: Option<&serde_json::Value>) -> bool {
    content
        .and_then(|content| content.as_array())
        .is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part.get("type").and_then(|kind| kind.as_str()) == Some("toolCall"))
        })
}

/// Read a transcript, tail-bounded, and summarise it. Any I/O error → `None`
/// (the caller reports idle).
pub fn read_transcript(path: &Path) -> Option<Transcript> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut bytes = Vec::new();
    if len > MAX_READ_BYTES as u64 {
        file.seek(SeekFrom::Start(len - TAIL_BYTES as u64)).ok()?;
        bytes.reserve(TAIL_BYTES);
        file.read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        // The seek landed mid-line: that first partial line is not parseable
        // as a whole record, so everything up to the first newline is dropped.
        let tail = match text.find('\n') {
            Some(index) => &text[index + 1..],
            None => "",
        };
        return Some(summarise(tail));
    }
    file.read_to_end(&mut bytes).ok()?;
    Some(summarise(&String::from_utf8_lossy(&bytes)))
}

/// The next report sequence for `previous`, derived from the wall clock.
///
/// HARD RULE: the sequence must never run ahead of the clock. herdr silently
/// drops a `pane.report_agent` whose `seq` is not strictly greater than the
/// last one it accepted for that (pane, source, agent), and a `seq` far in the
/// future (a constant like `9e15`) blocks even a RESTARTED pi from ever
/// reporting again. pi's own hook uses `Date.now() * 1000 + n`, i.e. the same
/// base, so both writers stay mutually current.
pub fn next_seq(previous: u64, now_ms: u64) -> u64 {
    previous.max(now_ms.saturating_mul(1000)).saturating_add(1)
}

/// The exact `pane.report_agent` params for one pane and state. Exactly one
/// session ref is carried; `message: null` matches what the hook sends.
pub fn report_params(pane: &PiPane, state: &str, seq: u64) -> serde_json::Value {
    let mut params = serde_json::json!({
        "pane_id": pane.pane_id.as_str(),
        "source": SOURCE,
        "agent": AGENT,
        "state": state,
        "message": serde_json::Value::Null,
        "seq": seq,
    });
    if let Some((key, value)) = pane.session_field() {
        params[key] = serde_json::Value::String(value.to_string());
    }
    params
}

/// A cached transcript plus the file stamp it was read at.
#[derive(Debug)]
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    transcript: Transcript,
}

/// Throttled reporter for one sidebar pane's heartbeat.
///
/// `last` holds, per pane id, the busy flag we last reported and when — that is
/// both the change detector and the per-pane re-assert clock.
#[derive(Debug, Default)]
pub struct PiState {
    seq: u64,
    last: BTreeMap<String, (bool, Instant)>,
    /// Per transcript path, so an unchanged 2 MB session file is not re-read
    /// on every beat.
    cache: BTreeMap<String, Cached>,
    /// Global beat gate, like the sibling reporters.
    last_beat: Option<Instant>,
}

impl PiState {
    /// Report every pi pane's state if due. Never panics, never retries, and
    /// ignores every failure: the rail is decoration, and the next beat (or
    /// pi's own hook) fixes a transient error. Only the elected session
    /// reporter sends the reports; the throttle still advances on the others,
    /// so they re-check once per interval rather than every loop iteration.
    pub fn report_if_due(&mut self) {
        if self
            .last_beat
            .is_some_and(|beat| beat.elapsed() < REPORT_INTERVAL)
        {
            return;
        }
        self.last_beat = Some(Instant::now());
        if !crate::session_reporter::is_session_reporter() {
            return;
        }
        let Ok(json) = crate::ipc::call_text("pane.list", serde_json::json!({})) else {
            return;
        };
        let panes = pi_panes(&json);
        let now_ms = unix_millis();
        let mut live: BTreeSet<&str> = BTreeSet::new();
        let mut live_paths: BTreeSet<&str> = BTreeSet::new();
        for pane in &panes {
            live.insert(pane.pane_id.as_str());
            if let Some(path) = pane.session_path.as_deref() {
                live_paths.insert(path);
            }
            let busy = self.busy(pane);
            let due = match self.last.get(&pane.pane_id) {
                None => true,
                Some((reported, sent)) => *reported != busy || sent.elapsed() >= REPORT_INTERVAL,
            };
            if !due {
                continue;
            }
            let state = if busy { WORKING } else { IDLE };
            self.seq = next_seq(self.seq, now_ms);
            // One failure means the socket is in trouble: stop here instead of
            // stacking another bounded-timeout round trip per pane on the event
            // loop. The next beat re-reports everything anyway.
            if crate::ipc::call_text("pane.report_agent", report_params(pane, state, self.seq))
                .is_err()
            {
                break;
            }
            self.last
                .insert(pane.pane_id.clone(), (busy, Instant::now()));
        }
        self.last
            .retain(|pane_id, _| live.contains(pane_id.as_str()));
        self.cache
            .retain(|path, _| live_paths.contains(path.as_str()));
    }

    /// Whether a pane's transcript says it is busy. A pane whose ref is an id
    /// (no file), a missing file, or an unreadable file is NOT busy: the
    /// fail-safe direction is today's behaviour, never a stuck `working`.
    fn busy(&mut self, pane: &PiPane) -> bool {
        let Some(path) = pane.session_path.as_deref() else {
            return false;
        };
        let transcript = self.transcript_of(path);
        // Child output files are stat'ed on every call: they move while the
        // child works, so caching them would hide a live child.
        let now = SystemTime::now();
        transcript.busy(|child| {
            let modified = child
                .output
                .as_deref()
                .and_then(|output| std::fs::metadata(output).ok())
                .and_then(|meta| meta.modified().ok());
            child_alive(modified, now)
        })
    }

    /// The transcript at `path`, cached until the file's length or mtime moves.
    fn transcript_of(&mut self, path: &str) -> Transcript {
        let Ok(meta) = std::fs::metadata(path) else {
            self.cache.remove(path);
            return Transcript::default();
        };
        let len = meta.len();
        let modified = meta.modified().ok();
        if let Some(cached) = self.cache.get(path)
            && cached.len == len
            && cached.modified == modified
        {
            return cached.transcript.clone();
        }
        let transcript = read_transcript(Path::new(path)).unwrap_or_default();
        self.cache.insert(
            path.to_string(),
            Cached {
                len,
                modified,
                transcript: transcript.clone(),
            },
        );
        transcript
    }
}

/// Wall clock in milliseconds, matching the base pi's hook uses.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// A non-empty string field.
fn non_empty(value: Option<&serde_json::Value>) -> Option<&str> {
    value
        .and_then(|value| value.as_str())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{"type":"session","version":3,"id":"01a14e5d","parentSession":null}"#;
    const DISPATCH: &str = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","name":"Agent","arguments":{"description":"rail test","prompt":"sleep 45","subagent_type":"general-purpose","run_in_background":true}}],"stopReason":"toolUse"}}"#;
    const STARTED: &str = r#"{"type":"message","message":{"role":"toolResult","toolCallId":"call_00","toolName":"Agent","content":[{"type":"text","text":"Agent started in background.\nAgent ID: 1422865d-6a1d-4de\nType: general-purpose\nOutput file: /tmp/x/1422865d-6a1d-4de.output\n\nYou will be notified."}]}}"#;
    const FOREGROUND: &str = r#"{"type":"message","message":{"role":"toolResult","toolCallId":"call_01","toolName":"Agent","content":[{"type":"text","text":"Agent completed in 25.4s (1 tool uses, 8.2k token).\n\n## Done\n- stuff"}]}}"#;
    const RECORD: &str = r#"{"type":"custom","customType":"subagents:record","data":{"id":"1422865d-6a1d-4de","status":"completed","startedAt":1,"completedAt":2}}"#;
    const NOTIFICATION: &str = r#"{"type":"custom_message","customType":"subagent-notification","details":{"id":"1422865d-6a1d-4de","status":"completed"},"content":"<task-notification>done</task-notification>","display":true}"#;
    const SETTLED: &str = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"OK"}],"stopReason":"stop"}}"#;
    const MODEL_CHANGE: &str = r#"{"type":"model_change","provider":"anthropic","model":"x"}"#;
    const NOT_FOUND: &str = r#"{"type":"message","message":{"role":"toolResult","toolCallId":"call_02","toolName":"steer_subagent","content":[{"type":"text","text":"Agent not found: \"1422865d-6a1d-4de\". It may have been cleaned up."}]}}"#;

    fn transcript(lines: &[&str]) -> Transcript {
        summarise(&lines.join("\n"))
    }

    fn child(id: &str, output: Option<&str>) -> Child {
        Child {
            id: id.to_string(),
            output: output.map(str::to_string),
        }
    }

    #[test]
    fn background_dispatch_without_completion_is_busy() {
        let result = transcript(&[HEADER, DISPATCH, STARTED]);
        assert_eq!(result.running.len(), 1);
        assert!(result.busy(|_| true));
    }

    #[test]
    fn start_result_captures_output_file() {
        let result = transcript(&[HEADER, DISPATCH, STARTED]);
        assert_eq!(
            result.running,
            vec![child(
                "1422865d-6a1d-4de",
                Some("/tmp/x/1422865d-6a1d-4de.output")
            )]
        );
    }

    #[test]
    fn start_result_without_output_line_has_no_output() {
        let no_output = r#"{"type":"message","message":{"role":"toolResult","toolName":"Agent","content":[{"type":"text","text":"Agent started in background.\nAgent ID: abc-1\nType: worker\n\nYou will be notified."}]}}"#;
        let result = transcript(&[HEADER, DISPATCH, no_output]);
        assert_eq!(result.running, vec![child("abc-1", None)]);
    }

    #[test]
    fn agent_not_found_marks_child_done() {
        let result = transcript(&[HEADER, DISPATCH, STARTED, NOT_FOUND]);
        assert!(result.running.is_empty());
    }

    #[test]
    fn running_child_busy_only_while_alive() {
        let settled = transcript(&[HEADER, DISPATCH, STARTED, SETTLED]);
        assert!(!settled.busy(|_| false));
        assert!(settled.busy(|_| true));
    }

    #[test]
    fn child_alive_follows_output_mtime() {
        let now = SystemTime::now();
        assert!(!child_alive(None, now));
        assert!(child_alive(Some(now), now));
        assert!(child_alive(Some(now - Duration::from_secs(4 * 60)), now));
        assert!(!child_alive(Some(now - Duration::from_secs(6 * 60)), now));
        assert!(child_alive(Some(now + Duration::from_secs(60)), now));
    }

    #[test]
    fn pi_state_busy_checks_child_output_file() {
        let output = temp_path("child-output");
        std::fs::write(&output, "child turn\n").expect("write output fixture");
        let started = serde_json::json!({
            "type": "message",
            "message": {
                "role": "toolResult",
                "toolName": "Agent",
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Agent started in background.\nAgent ID: live-1\nOutput file: {}\n\nYou will be notified.",
                        output.display()
                    ),
                }],
            },
        })
        .to_string();
        let transcript_file = temp_path("child-transcript");
        std::fs::write(&transcript_file, [HEADER, DISPATCH, &started, SETTLED].join("\n"))
            .expect("write transcript fixture");
        let pane = PiPane {
            pane_id: "p1".to_string(),
            session_path: Some(transcript_file.display().to_string()),
            session_id: None,
        };
        let mut state = PiState::default();
        assert!(state.busy(&pane), "fresh child output keeps the pane busy");

        std::fs::remove_file(&output).ok();
        let mut state = PiState::default();
        assert!(!state.busy(&pane), "a missing output file is not alive");

        std::fs::remove_file(&transcript_file).ok();
    }

    #[test]
    fn completion_record_clears_running() {
        let result = transcript(&[HEADER, DISPATCH, STARTED, RECORD]);
        assert!(result.running.is_empty());
        // The record itself is an activity marker: a turn follows it.
        assert!(result.turn_active);
    }

    #[test]
    fn completion_notification_is_active() {
        let result = transcript(&[HEADER, DISPATCH, STARTED, RECORD, NOTIFICATION]);
        assert!(result.running.is_empty());
        assert!(result.busy(|_| false), "notification starts a turn");
    }

    #[test]
    fn settled_assistant_turn_is_idle() {
        let result = transcript(&[HEADER, DISPATCH, STARTED, RECORD, NOTIFICATION, SETTLED]);
        assert!(result.running.is_empty());
        assert!(!result.busy(|_| false));
    }

    #[test]
    fn assistant_tool_use_is_busy() {
        assert!(transcript(&[HEADER, DISPATCH]).busy(|_| false));
    }

    #[test]
    fn trailing_tool_result_is_busy() {
        let result = transcript(&[HEADER, DISPATCH, STARTED]);
        assert!(result.turn_active);
    }

    #[test]
    fn assistant_without_stop_reason_uses_tool_call() {
        let no_reason = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"working"},{"type":"toolCall","name":"bash","arguments":{}}]}}"#;
        assert!(transcript(&[HEADER, no_reason]).busy(|_| false));
        let text_only = r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        assert!(!transcript(&[HEADER, text_only]).busy(|_| false));
    }

    #[test]
    fn records_after_a_settled_turn_do_not_stick() {
        let usage = r#"{"type":"usage","input":1,"output":2}"#;
        let thinking = r#"{"type":"thinking_level_change","level":"high"}"#;
        let extension = r#"{"type":"custom","customType":"web-search-results","data":{"n":1}}"#;
        let result = transcript(&[HEADER, SETTLED, MODEL_CHANGE, thinking, usage, extension]);
        assert!(!result.busy(|_| false));
    }

    #[test]
    fn garbage_and_partial_lines_are_ignored() {
        assert!(!summarise("").busy(|_| false));
        assert!(!summarise("not json at all").busy(|_| false));
        assert!(!summarise("{\"type\":\"message\"}").busy(|_| false));
        // A file caught mid-write: the final line is torn but the record before
        // it still counts.
        let torn = format!("{HEADER}\n{DISPATCH}\n{{\"type\":\"message\",\"messa");
        let result = summarise(&torn);
        assert_eq!(
            result,
            Transcript {
                running: Vec::new(),
                turn_active: true
            }
        );
    }

    #[test]
    fn foreground_agent_result_is_not_a_start() {
        let result = transcript(&[HEADER, DISPATCH, FOREGROUND]);
        assert!(result.running.is_empty());
    }

    #[test]
    fn completion_without_a_start_is_not_a_count() {
        // Only the tail of a huge session may be visible: a `done` id whose
        // start was cut off must not underflow to a busy state.
        let result = transcript(&[RECORD, SETTLED]);
        assert!(result.running.is_empty());
    }

    fn pane_entry(
        pane_id: &str,
        agent: Option<&str>,
        session: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert("pane_id".to_string(), pane_id.into());
        fields.insert("workspace_id".to_string(), "w1".into());
        fields.insert("agent_status".to_string(), "idle".into());
        if let Some(agent) = agent {
            fields.insert("agent".to_string(), agent.into());
        }
        if let Some(session) = session {
            fields.insert("agent_session".to_string(), session);
        }
        serde_json::Value::Object(fields)
    }

    fn list(panes: &[serde_json::Value]) -> String {
        serde_json::json!({
            "id": "herdr-sidebar:pane.list",
            "result": { "panes": panes },
        })
        .to_string()
    }

    fn session(source: &str, kind: &str, value: &str) -> serde_json::Value {
        serde_json::json!({ "source": source, "agent": "pi", "kind": kind, "value": value })
    }

    #[test]
    fn pi_panes_selects_by_session_source_and_agent() {
        let json = list(&[
            pane_entry(
                "p1",
                Some("pi"),
                Some(session(SOURCE, "path", "/tmp/a.jsonl")),
            ),
            // Identity from `agent` alone: the hook may not have reported its
            // session ref yet, but herdr's own detection already says pi.
            pane_entry(
                "p2",
                Some("pi"),
                Some(serde_json::json!({ "kind": "id", "value": "sess-2" })),
            ),
            pane_entry("p3", None, Some(session(SOURCE, "id", "sess-3"))),
            pane_entry(
                "p4",
                Some("claude"),
                Some(session("herdr:claude", "id", "x")),
            ),
            // A pi pane with no session ref has no transcript to read.
            pane_entry("p5", Some("pi"), None),
        ]);
        let panes = pi_panes(&json);
        let ids: Vec<&str> = panes.iter().map(|pane| pane.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["p1", "p2", "p3"]);
        assert_eq!(
            panes[0].session_path.as_deref(),
            Some("/tmp/a.jsonl"),
            "kind=path fills session_path"
        );
        assert_eq!(panes[0].session_id, None);
        assert_eq!(panes[1].session_id.as_deref(), Some("sess-2"));
        assert_eq!(panes[2].session_id.as_deref(), Some("sess-3"));
        assert_eq!(panes[2].session_path, None);
    }

    #[test]
    fn pi_panes_strips_bom_and_survives_garbage() {
        let json = format!(
            "\u{feff}{}",
            list(&[pane_entry(
                "p1",
                Some("pi"),
                Some(session(SOURCE, "path", "/tmp/a.jsonl"))
            )])
        );
        assert_eq!(pi_panes(&json).len(), 1);
        assert!(pi_panes("not json").is_empty());
        assert!(pi_panes("").is_empty());
        assert!(pi_panes(r#"{"result":{}}"#).is_empty());
        assert!(pi_panes(&list(&[pane_entry("", Some("pi"), None)])).is_empty());
    }

    #[test]
    fn next_seq_is_monotonic_and_never_ahead_of_the_clock() {
        let now = 1_700_000_000_000u64;
        let first = next_seq(0, now);
        let second = next_seq(first, now);
        assert!(second > first);
        // The year-2286 rule: bounded by the clock, so a restart cannot block.
        assert_eq!(first, now * 1000 + 1);
        assert_eq!(second, now * 1000 + 2);
        // A previous value from the future still only steps by one, never
        // multiplying: it is carried, not recomputed from the clock.
        let carried = next_seq(9_000_000_000_000_000, now);
        assert_eq!(carried, 9_000_000_000_000_001);
        for previous in [0, 1, now * 1000, u64::MAX / 2] {
            let seq = next_seq(previous, now);
            assert!(seq <= previous.max(now * 1000) + 1);
        }
    }

    #[test]
    fn next_seq_survives_an_absurd_clock() {
        // Saturating, never a panic: the clock value is untrusted input here.
        assert_eq!(next_seq(0, u64::MAX), u64::MAX);
        assert_eq!(next_seq(7, u64::MAX), u64::MAX);
    }

    #[test]
    fn report_params_carry_one_session_ref() {
        let by_path = PiPane {
            pane_id: "pane-1".to_string(),
            session_path: Some("/tmp/s.jsonl".to_string()),
            session_id: None,
        };
        let params = report_params(&by_path, WORKING, 1_700_000_000_000_001);
        assert_eq!(params["pane_id"], "pane-1");
        assert_eq!(params["source"], "herdr:pi");
        assert_eq!(params["agent"], "pi");
        assert_eq!(params["state"], "working");
        assert_eq!(params["seq"], 1_700_000_000_000_001u64);
        assert!(params["message"].is_null());
        assert_eq!(params["agent_session_path"], "/tmp/s.jsonl");
        assert!(params.get("agent_session_id").is_none());

        let by_id = PiPane {
            pane_id: "pane-2".to_string(),
            session_path: None,
            session_id: Some("sess".to_string()),
        };
        let params = report_params(&by_id, IDLE, 5);
        assert_eq!(params["agent_session_id"], "sess");
        assert!(params.get("agent_session_path").is_none());
        assert_eq!(params["state"], "idle");
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("herdr-pi-state-{}-{tag}.jsonl", std::process::id()))
    }

    #[test]
    fn read_transcript_reads_a_settled_session() {
        let path = temp_path("settled");
        std::fs::write(
            &path,
            [HEADER, DISPATCH, STARTED, RECORD, NOTIFICATION, SETTLED].join("\n"),
        )
        .expect("write fixture");
        let result = read_transcript(&path).expect("read fixture");
        std::fs::remove_file(&path).ok();
        assert!(result.running.is_empty());
        assert!(!result.busy(|_| false));
    }

    #[test]
    fn read_transcript_reads_the_tail_of_a_huge_session() {
        let path = temp_path("tail");
        let filler = format!(
            "{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}],\"stopReason\":\"stop\"}}}}",
            "x".repeat(1000)
        );
        // Comfortably over MAX_READ_BYTES so the tail path is exercised; the
        // line the seek lands in is cut in half and must be skipped.
        let mut body = String::with_capacity(MAX_READ_BYTES + TAIL_BYTES);
        while body.len() <= MAX_READ_BYTES + 1024 {
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str(DISPATCH);
        body.push('\n');
        std::fs::write(&path, &body).expect("write fixture");
        assert!(std::fs::metadata(&path).expect("stat").len() > MAX_READ_BYTES as u64);
        let result = read_transcript(&path).expect("read fixture");
        std::fs::remove_file(&path).ok();
        assert!(result.busy(|_| false), "the tail's last record is a toolUse turn");
        assert!(result.running.is_empty());
    }

    #[test]
    fn read_transcript_missing_file_is_none() {
        assert!(read_transcript(Path::new("/nonexistent/herdr-pi-state.jsonl")).is_none());
    }
}
