//! Keep each workspace's first herdr tab named after its checked-out branch.
//!
//! herdr names a generated first tab with its position ("1") and has no
//! template for the tab label, so the sidebar renames it through `tab.rename`.
//! The creation hooks do that once (`ensure::label_first_tab`); this module
//! keeps it true afterwards: a checkout, a branch rename or a `git switch`
//! typed in a terminal rewrites the git dir's `HEAD`, and the 5 s heartbeat
//! compares it with the label on screen.
//!
//! Only herdr-sidebar's own labels are overwritten: the generated position, the
//! [`FIRST_TAB_LABEL`] fallback, and whatever this reporter set last. A tab a
//! human named is left alone. The sync is elected session-wide
//! ([`crate::session_reporter`]) like the other reporters, so N sidebars do not
//! repeat the same socket calls.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::launch::FIRST_TAB_LABEL;

/// How often the first-tab labels are compared with `HEAD`. Half the time a
/// branch popup needs to feel immediate would be enough; this rides the
/// existing 5 s sidebar heartbeat, which also keeps the rail tokens fresh.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    last: Option<Instant>,
    /// tab id → the label this reporter applied last. A change means the user
    /// renamed the tab; it is then theirs and is never touched again.
    applied: BTreeMap<String, String>,
}

/// Process-wide, like the reporter election: the unified sidebar rebuilds its
/// app on every view switch, and the tab labels must survive that rebuild.
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// A first tab: `number == 1` in its workspace.
#[derive(Debug, PartialEq, Eq)]
struct FirstTab {
    tab_id: String,
    workspace_id: String,
    label: String,
}

/// Rename every first tab whose branch moved, at most once per
/// [`SYNC_INTERVAL`]. Never panics and ignores every failure: the tab bar is
/// decoration, and the next beat fixes a transient error.
pub fn sync_if_due() {
    let Ok(mut state) = STATE.get_or_init(Default::default).lock() else {
        return;
    };
    // The lock is held across the socket calls, but the two sidebars of one
    // process never run at the same time; the throttle below is what limits
    // work, not the lock.
    if state
        .last
        .is_some_and(|last| last.elapsed() < SYNC_INTERVAL)
    {
        return;
    }
    state.last = Some(Instant::now());
    if !crate::session_reporter::is_session_reporter() {
        return;
    }
    let Ok(panes) = crate::ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    let Ok(tabs) = crate::ipc::call_text("tab.list", serde_json::json!({})) else {
        return;
    };
    let cwds = workspace_cwds(&panes);
    for tab in first_tabs(&tabs) {
        let Some(cwd) = cwds.get(&tab.workspace_id) else {
            continue;
        };
        let Some(desired) = crate::git::branch_label_at(std::path::Path::new(cwd)) else {
            continue;
        };
        let applied = state.applied.get(&tab.tab_id);
        if !ours(&tab.label, &desired, applied.map(String::as_str)) {
            state.applied.remove(&tab.tab_id);
            continue;
        }
        if tab.label == desired {
            state.applied.insert(tab.tab_id, desired);
            continue;
        }
        if crate::ipc::call_text(
            "tab.rename",
            serde_json::json!({ "tab_id": tab.tab_id, "label": desired }),
        )
        .is_err()
        {
            // One failure means the socket is in trouble: stop instead of
            // stacking another bounded-timeout round trip per workspace.
            return;
        }
        state.applied.insert(tab.tab_id, desired);
    }
}

/// The label to give a freshly generated tab, from the tab's own pane cwd.
///
/// Called by the creation hooks before the sidebar for this workspace exists:
/// one `pane.list` plus a `HEAD` read, no git process. `None` means "not in a
/// repository" and lets the caller keep [`FIRST_TAB_LABEL`].
pub fn branch_label_for_tab(tab_id: &str) -> Option<String> {
    let panes = crate::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    let cwd = cwd_of_tab(&panes, tab_id)?;
    crate::git::branch_label_at(std::path::Path::new(&cwd))
}

/// Is `current` a label this reporter is allowed to replace? The generated
/// position ("1") is still herdr's, not a human's.
fn ours(current: &str, desired: &str, applied: Option<&str>) -> bool {
    current == desired
        || current == "1"
        || current == FIRST_TAB_LABEL
        || applied == Some(current)
}

/// `tab.list` JSON → every first tab (`number == 1`) with its id, workspace and
/// label. Malformed replies yield nothing; a later numbered tab is never ours.
fn first_tabs(tab_list_json: &str) -> Vec<FirstTab> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(tab_list_json) else {
        return Vec::new();
    };
    let Some(tabs) = parsed
        .pointer("/result/tabs")
        .and_then(|tabs| tabs.as_array())
    else {
        return Vec::new();
    };
    tabs.iter()
        .filter_map(|tab| {
            let tab_id = tab.get("tab_id")?.as_str().filter(|id| !id.is_empty())?;
            let workspace_id = tab
                .get("workspace_id")?
                .as_str()
                .filter(|id| !id.is_empty())?;
            let number = tab.get("number")?.as_u64()?;
            let label = tab.get("label")?.as_str()?;
            (number == 1).then(|| FirstTab {
                tab_id: tab_id.to_string(),
                workspace_id: workspace_id.to_string(),
                label: label.to_string(),
            })
        })
        .collect()
}

/// `pane.list` JSON → workspace id → the cwd of its first pane that reports
/// one. The same rule the `$pad` reporter uses: `foreground_cwd` (where the
/// user actually is) falls back to `cwd`.
fn workspace_cwds(pane_list_json: &str) -> BTreeMap<String, String> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(pane_list_json) else {
        return BTreeMap::new();
    };
    let Some(panes) = parsed.pointer("/result/panes").and_then(|p| p.as_array()) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for pane in panes {
        let Some(workspace_id) = pane
            .get("workspace_id")
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let cwd = pane
            .get("foreground_cwd")
            .and_then(|value| value.as_str())
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| {
                pane.get("cwd")
                    .and_then(|value| value.as_str())
                    .filter(|cwd| !cwd.is_empty())
            });
        if let Some(cwd) = cwd {
            out.entry(workspace_id.to_string())
                .or_insert_with(|| cwd.to_string());
        }
    }
    out
}

/// The cwd of the first pane in `tab_id`, from a `pane.list` reply.
fn cwd_of_tab(pane_list_json: &str, tab_id: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(pane_list_json).ok()?;
    let panes = parsed.pointer("/result/panes")?.as_array()?;
    panes
        .iter()
        .filter(|pane| pane.get("tab_id").and_then(|id| id.as_str()) == Some(tab_id))
        .find_map(|pane| {
            pane.get("foreground_cwd")
                .and_then(|value| value.as_str())
                .filter(|cwd| !cwd.is_empty())
                .or_else(|| {
                    pane.get("cwd")
                        .and_then(|value| value.as_str())
                        .filter(|cwd| !cwd.is_empty())
                })
                .map(str::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_tab_is_considered() {
        let list = r#"{"result":{"tabs":[
            {"tab_id":"wD:t1","workspace_id":"wD","label":"1","number":1},
            {"tab_id":"wD:t2","workspace_id":"wD","label":"terminal","number":2},
            {"tab_id":"wE:t1","workspace_id":"wE","label":"feat/x","number":1},
            {"tab_id":"","workspace_id":"wF","label":"1","number":1},
            {"tab_id":"wG:t1","workspace_id":"","label":"1","number":1}
        ]}}"#;
        assert_eq!(
            first_tabs(list),
            vec![
                FirstTab {
                    tab_id: "wD:t1".into(),
                    workspace_id: "wD".into(),
                    label: "1".into()
                },
                FirstTab {
                    tab_id: "wE:t1".into(),
                    workspace_id: "wE".into(),
                    label: "feat/x".into()
                },
            ]
        );
        assert!(first_tabs("garbage").is_empty());
        assert!(first_tabs(r#"{"result":{}}"#).is_empty());
    }

    #[test]
    fn workspaces_take_the_first_pane_cwd() {
        let list = r#"{"result":{"panes":[
            {"workspace_id":"wD","foreground_cwd":"/repo/src","cwd":"/repo"},
            {"workspace_id":"wD","foreground_cwd":"/other"},
            {"workspace_id":"wE","cwd":"/elsewhere"},
            {"workspace_id":"wF"}
        ]}}"#;
        let cwds = workspace_cwds(list);
        assert_eq!(cwds.get("wD").map(String::as_str), Some("/repo/src"));
        assert_eq!(cwds.get("wE").map(String::as_str), Some("/elsewhere"));
        assert!(!cwds.contains_key("wF"), "no cwd is not a cwd");
    }

    #[test]
    fn cwd_of_tab_ignores_other_tabs() {
        let list = r#"{"result":{"panes":[
            {"tab_id":"wD:t1","cwd":"/one"},
            {"tab_id":"wD:t2","cwd":"/two"},
            {"tab_id":"wD:t1","foreground_cwd":"/one/src"}
        ]}}"#;
        assert_eq!(cwd_of_tab(list, "wD:t1").as_deref(), Some("/one"));
        assert_eq!(cwd_of_tab(list, "wD:t2").as_deref(), Some("/two"));
        assert_eq!(cwd_of_tab(list, "wD:t9"), None);
    }

    #[test]
    fn only_our_labels_are_replaced() {
        assert!(ours("1", "feat/x", None));
        assert!(ours("main", "feat/x", None));
        assert!(ours("feat/x", "feat/x", None));
        assert!(ours("feat/x", "feat/y", Some("feat/x")));
        assert!(
            !ours("my stuff", "feat/x", Some("feat/x")),
            "a human renamed it after us"
        );
        assert!(!ours("my stuff", "feat/x", None));
    }
}
