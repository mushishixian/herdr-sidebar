//! Toast the user when an agent starts needing them.
//!
//! The rail shows each agent's state, but a rail you are not looking at is not
//! a notification: an agent that blocks on a permission prompt in another
//! workspace stays `×` until you happen to glance at it, and one that finishes
//! while you are elsewhere goes quiet. herdr itself has no agent-state
//! notification (verified against 0.9.5: `notification.show` is the only toast
//! API, and nothing calls it for agents).
//!
//! The elected session reporter ([crate::session_reporter]) already fetches
//! `pane.list` every 5 s for the `$agents` token ([crate::agent_tokens]), so
//! that same reply is diffed here: one toast per beat at most, with the sound
//! herdr reserves for the case (`request` for blocked, `done` for finished).
//!
//! What counts:
//! - `blocked` — a permission prompt or a question — is "needs input";
//! - `done`, or `working` -> `idle`, is "finished". herdr's `done` means "idle
//!   but not looked at yet" and IS reported for a pane (observed live: a
//!   CodeBuddy pane that ran `Stop` in the background read `done`), but the same
//!   pane can also just read `idle`, so both transitions count. `done` right
//!   after `idle` is suppressed, so one finish never toasts twice.
//!
//! The name comes from `display_agent`, else `agent` — the same rule the rail
//! uses. Agents herdr does not detect (CodeBuddy, `codebuddy-code`, …) are
//! therefore named correctly as long as their hook reports one of the two
//! fields, and a pane with neither is skipped (there is nothing to say).
//!
//! Suppressed:
//! - the first beat after a sidebar (re)starts: replaying every existing state
//!   as a toast would greet a restart with a storm;
//! - the FOCUSED pane: the prompt is in front of you, and the finish is
//!   something you watched;
//! - a state that did not change: `blocked` stays blocked until the tool runs,
//!   so a toast per beat would be a pager.

use std::collections::BTreeMap;

/// What the agent did that the user should hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Blocked on a permission prompt or a question.
    NeedsInput,
    /// The turn settled and the agent waits for the next prompt.
    Finished,
}

/// One agent transition worth a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Pane that changed; only used to key the state map.
    pub pane_id: String,
    /// `display_agent` else `agent`.
    pub agent: String,
    /// Last path component of the pane's cwd, so the body says WHERE.
    pub project: String,
    pub kind: Kind,
}

/// The `notification.show` request, built from one beat's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub title: String,
    pub body: String,
    /// `request` (blocked) or `done` (finished), the two herdr sounds.
    pub sound: &'static str,
}

impl Toast {
    /// Show it. Failure is ignored like every other heartbeat report: a lost
    /// toast is not worth a retry loop on the event-loop thread.
    pub fn show(&self) {
        let _ = crate::ipc::call_text(
            "notification.show",
            serde_json::json!({
                "title": self.title,
                "body": self.body,
                "sound": self.sound,
            }),
        );
    }
}

/// Per-sidebar diff of `pane.list` agent states.
///
/// Holds the previous beat's state per pane and the priming flag; every
/// `report_if_due` beat feeds it the reply it already fetched.
#[derive(Debug, Default)]
pub struct AgentNotify {
    /// pane id -> `agent_status` seen last beat.
    states: BTreeMap<String, String>,
    /// False until the first beat has been recorded (see "Suppressed" above).
    primed: bool,
}

impl AgentNotify {
    /// Diff this `pane.list` reply against the last beat and return the toast
    /// to show, if any. Always records the new states, even when the toast is
    /// suppressed.
    pub fn observe(&mut self, pane_list_json: &str) -> Option<Toast> {
        let parsed = parse(pane_list_json)?;
        let mut changes = Vec::new();
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for pane in panes(&parsed) {
            let Some(pane_id) = non_empty(pane.get("pane_id")) else {
                continue;
            };
            let state = pane
                .get("agent_status")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let previous = self.states.get(pane_id).map(String::as_str);
            seen.insert(pane_id.to_string(), state.to_string());

            if !self.primed {
                continue;
            }
            // You are looking at it: no toast for the pane in front of you.
            if pane.get("focused").and_then(|value| value.as_bool()) == Some(true) {
                continue;
            }
            let Some(agent) =
                non_empty(pane.get("display_agent")).or_else(|| non_empty(pane.get("agent")))
            else {
                continue;
            };
            let kind = match state {
                // Already reported, and blocked stays blocked until the tool
                // runs: never repeat it.
                "blocked" if previous == Some("blocked") => continue,
                "blocked" => Kind::NeedsInput,
                // herdr promotes an idle-but-unseen pane to `done` itself, so
                // a finish usually arrives as `done`. The `idle` arm below
                // catches the beat that lands before the promotion, and `done`
                // must not fire again on top of it: one finish, one toast.
                "done" if previous == Some("done") || previous == Some("idle") => continue,
                "done" => Kind::Finished,
                "idle" if previous == Some("working") => Kind::Finished,
                _ => continue,
            };
            changes.push(Change {
                pane_id: pane_id.to_string(),
                agent: agent.to_string(),
                project: project_of(pane),
                kind,
            });
        }
        self.states = seen;
        self.primed = true;
        toast_for(&changes)
    }
}

/// One beat's changes -> at most one toast. Blocked wins over finished (it is
/// the one that stops work); everything else is aggregated so a beat with three
/// transitions still makes one toast instead of three.
pub fn toast_for(changes: &[Change]) -> Option<Toast> {
    if changes.is_empty() {
        return None;
    }
    let (kind, sound, verb) = if changes.iter().any(|change| change.kind == Kind::NeedsInput) {
        (Kind::NeedsInput, "request", "needs input")
    } else {
        (Kind::Finished, "done", "finished")
    };
    let relevant: Vec<&Change> = changes
        .iter()
        .filter(|change| change.kind == kind)
        .collect();

    let title = if relevant.len() == 1 {
        format!("{} {verb}", relevant[0].agent)
    } else {
        format!("{} agents {verb}", relevant.len())
    };
    Some(Toast {
        title,
        body: body_of(&relevant),
        sound,
    })
}

/// `agent · project` per change, deduped, at most `MAX_BODY_ITEMS`, with the
/// remainder summarized. One change says just where it happened.
fn body_of(changes: &[&Change]) -> String {
    if changes.len() == 1 {
        return truncate(&changes[0].project, MAX_BODY_CHARS);
    }
    let mut items: Vec<String> = Vec::new();
    for change in changes {
        let item = format!("{} · {}", change.agent, change.project);
        if !items.contains(&item) {
            items.push(item);
        }
    }
    let extra = items.len().saturating_sub(MAX_BODY_ITEMS);
    items.truncate(MAX_BODY_ITEMS);
    let mut body = items.join(", ");
    if extra > 0 {
        body.push_str(&format!(" +{extra} more"));
    }
    truncate(&body, MAX_BODY_CHARS)
}

/// How many `agent · project` items a multi-agent body lists.
const MAX_BODY_ITEMS: usize = 3;

/// Cap on the body, so a wall of long directory names cannot make a toast
/// unreadable (herdr wraps, but only so far).
const MAX_BODY_CHARS: usize = 120;

/// Last path component of the pane's cwd (`foreground_cwd` first, where the
/// user actually is), or the workspace id when no cwd was reported.
fn project_of(pane: &serde_json::Value) -> String {
    let cwd = non_empty(pane.get("foreground_cwd")).or_else(|| non_empty(pane.get("cwd")));
    cwd.and_then(|cwd| {
        std::path::Path::new(cwd)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    })
    .or_else(|| non_empty(pane.get("workspace_id")).map(str::to_string))
    .unwrap_or_default()
}

/// Truncate on a char boundary without appending anything.
fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Parse the CLI's printed response line (`{"id":..,"result":{..}}`).
fn parse(pane_list_json: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(pane_list_json).ok()
}

/// The `result.panes` array of a parsed reply (empty for any other shape).
fn panes(parsed: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    parsed
        .pointer("/result/panes")
        .and_then(|panes| panes.as_array())
        .map(|panes| panes.as_slice())
        .unwrap_or(&[])
        .iter()
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

    fn pane(
        id: &str,
        agent: Option<&str>,
        display: Option<&str>,
        state: &str,
        focused: bool,
    ) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert("pane_id".to_string(), id.into());
        fields.insert("workspace_id".to_string(), "w4".into());
        fields.insert("agent_status".to_string(), state.into());
        fields.insert("focused".to_string(), focused.into());
        fields.insert("cwd".to_string(), format!("/data/project/{id}").into());
        for (key, value) in [("agent", agent), ("display_agent", display)] {
            fields.extend(value.map(|value| (key.to_string(), value.into())));
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

    /// Feed one beat and return the toast.
    fn beat(notify: &mut AgentNotify, panes: &[serde_json::Value]) -> Option<Toast> {
        notify.observe(&list(panes))
    }

    #[test]
    fn the_first_beat_primes_without_a_toast() {
        let mut notify = AgentNotify::default();
        assert_eq!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", false)]
            ),
            None
        );
        // Already blocked: nothing changed, so still nothing.
        assert_eq!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", false)]
            ),
            None
        );
    }

    #[test]
    fn blocking_toasts_once_with_the_request_sound() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane("p1", Some("codebuddy"), None, "working", false)],
        );
        let toast = beat(
            &mut notify,
            &[pane("p1", Some("codebuddy"), None, "blocked", false)],
        )
        .expect("blocked toast");
        assert_eq!(toast.title, "codebuddy needs input");
        assert_eq!(toast.body, "p1");
        assert_eq!(toast.sound, "request");
        // No repeat while it stays blocked.
        assert_eq!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", false)]
            ),
            None
        );
    }

    #[test]
    fn a_finished_turn_toasts_with_the_done_sound() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane("p1", Some("pi"), None, "working", false)],
        );
        let toast = beat(&mut notify, &[pane("p1", Some("pi"), None, "idle", false)])
            .expect("finished toast");
        assert_eq!(toast.title, "pi finished");
        assert_eq!(toast.sound, "done");
    }

    /// herdr only computes `done` per workspace, but some agents (and future
    /// ones) may report it per pane; it must read as finished.
    #[test]
    fn a_done_pane_toasts_as_finished() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane("p1", Some("claude"), None, "working", false)],
        );
        let toast = beat(
            &mut notify,
            &[pane("p1", Some("claude"), None, "done", false)],
        )
        .expect("finished toast");
        assert_eq!(toast.title, "claude finished");
    }

    #[test]
    fn the_focused_pane_is_never_toasted() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane("p1", Some("codebuddy"), None, "working", true)],
        );
        assert_eq!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", true)]
            ),
            None
        );
        // Switching away afterwards is not a transition either: the state was
        // already recorded while the pane was focused.
        assert_eq!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", false)]
            ),
            None
        );
    }

    #[test]
    fn a_pane_without_a_name_is_skipped() {
        let mut notify = AgentNotify::default();
        beat(&mut notify, &[pane("p1", None, None, "working", false)]);
        assert_eq!(
            beat(&mut notify, &[pane("p1", None, None, "blocked", false)]),
            None
        );
    }

    #[test]
    fn display_agent_names_the_toast() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane(
                "p1",
                Some("codebuddy-code"),
                Some("codebuddy"),
                "working",
                false,
            )],
        );
        let toast = beat(
            &mut notify,
            &[pane(
                "p1",
                Some("codebuddy-code"),
                Some("codebuddy"),
                "blocked",
                false,
            )],
        )
        .expect("toast");
        assert_eq!(toast.title, "codebuddy needs input");
    }

    #[test]
    fn several_transitions_make_one_toast_and_blocked_wins() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[
                pane("p1", Some("pi"), None, "working", false),
                pane("p2", Some("codebuddy"), None, "working", false),
                pane("p3", Some("claude"), None, "working", false),
            ],
        );
        let toast = beat(
            &mut notify,
            &[
                pane("p1", Some("pi"), None, "idle", false),
                pane("p2", Some("codebuddy"), None, "blocked", false),
                pane("p3", Some("claude"), None, "idle", false),
            ],
        )
        .expect("one aggregated toast");
        assert_eq!(toast.title, "codebuddy needs input");
        assert_eq!(toast.sound, "request");
        // The two finished panes were recorded and are not repeated next beat.
        assert_eq!(
            beat(
                &mut notify,
                &[
                    pane("p1", Some("pi"), None, "idle", false),
                    pane("p2", Some("codebuddy"), None, "blocked", false),
                    pane("p3", Some("claude"), None, "idle", false),
                ],
            ),
            None
        );
    }

    #[test]
    fn several_finished_agents_are_aggregated() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[
                pane("p1", Some("pi"), None, "working", false),
                pane("p2", Some("codebuddy"), None, "working", false),
            ],
        );
        let toast = beat(
            &mut notify,
            &[
                pane("p1", Some("pi"), None, "idle", false),
                pane("p2", Some("codebuddy"), None, "idle", false),
            ],
        )
        .expect("aggregated toast");
        assert_eq!(toast.title, "2 agents finished");
        assert_eq!(toast.body, "pi · p1, codebuddy · p2");
        assert_eq!(toast.sound, "done");
    }

    #[test]
    fn a_restarted_agent_is_notified_again() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane("p1", Some("codebuddy"), None, "working", false)],
        );
        assert!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", false)]
            )
            .is_some()
        );
        // SessionEnd removes the pane from pane.list; the next session's
        // prompt is a new transition, not a repeat.
        beat(&mut notify, &[]);
        assert!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "blocked", false)]
            )
            .is_some()
        );
    }

    /// herdr promotes an unfocused idle pane to `done`, so one finish can be
    /// seen as working -> idle -> done. It must not toast twice.
    #[test]
    fn a_finish_is_not_toasted_twice_when_herdr_promotes_it_to_done() {
        let mut notify = AgentNotify::default();
        beat(
            &mut notify,
            &[pane("p1", Some("codebuddy"), None, "working", false)],
        );
        assert!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "idle", false)]
            )
            .is_some()
        );
        assert_eq!(
            beat(
                &mut notify,
                &[pane("p1", Some("codebuddy"), None, "done", false)]
            ),
            None
        );
    }

    #[test]
    fn malformed_json_is_ignored() {
        let mut notify = AgentNotify::default();
        assert_eq!(notify.observe("not json"), None);
        assert_eq!(notify.observe(""), None);
    }

    #[test]
    fn body_is_capped_and_lists_at_most_three_items() {
        let changes: Vec<Change> = (0..5)
            .map(|index| Change {
                pane_id: format!("p{index}"),
                agent: format!("agent{index}"),
                project: format!("project{index}"),
                kind: Kind::Finished,
            })
            .collect();
        let toast = toast_for(&changes).expect("toast");
        assert_eq!(toast.title, "5 agents finished");
        assert_eq!(
            toast.body,
            "agent0 · project0, agent1 · project1, agent2 · project2 +2 more"
        );
        assert!(toast.body.chars().count() <= MAX_BODY_CHARS);
    }
}
