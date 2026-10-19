//! Report *which* coding agent runs in each herdr space, for herdr's spaces
//! rail.
//!
//! herdr's rail renders `[ui.sidebar.spaces] rows` from a fixed set of built-in
//! tokens (`state_icon`, `state_text`, `workspace`, `branch`, `git_status`) and
//! has no "agent name" token at all. It does, however, accept ARBITRARY
//! display-only tokens per workspace (`workspace.report_metadata`) and renders
//! `$name` in those rows — so the sidebar, which is already a long-lived
//! process in every IDE tab with a 5 s heartbeat and a socket client, reports
//! one `agents` token per space. The dotfiles config renders it as
//! `["$agents", <styled state_text>]`, i.e. `claude` (idle) or `pi · working`.
//!
//! The same heartbeat also reports a `pad` token, rendered as `$pad` on the
//! `branch`/`git_status` row. herdr drops a row whose tokens are all empty, so
//! a Space whose project is not a git repository would otherwise be one line
//! shorter than its neighbours; `$pad` holds the row open with an invisible
//! zero-width space and is cleared (`null`) wherever a repository exists.
//!
//! Public surface:
//!
//! - [`describe_agents`] — pure: `pane.list` JSON → workspace id → value.
//! - [`describe_pads`] — pure: `pane.list` JSON → workspace id → `$pad` value.
//! - [`report_params`] — pure: the `workspace.report_metadata` params object (both tokens).
//! - [`AgentTokens::report_if_due`] — the 5 s-throttled reporter both sidebar
//!   apps call from their heartbeat. Only the sidebar apps report; the ensure
//!   sidecar and the preview viewer never do.
//!
//! Contract (also documented in the root `AGENTS.md`): token key `agents`,
//! `source = "herdr-sidebar"`, `ttl_ms` 20 s, value format `label · label` with
//! busy agents first, and [`NO_AGENT`] (`-`) for a space with no agent pane.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Metadata token key the rail renders as `$agents`.
pub const TOKEN: &str = "agents";

/// Metadata token key the rail renders as `$pad`.
///
/// herdr drops a rail row when none of its tokens have a value ("a row
/// disappears when none of its tokens have a value", configuration.mdx), so a
/// Space whose project is not a git repository loses the `branch`/`git_status`
/// row and renders one line shorter than its neighbours. `$pad` gives that row
/// a value that paints nothing: a ZERO WIDTH SPACE. It is reported only for
/// workspaces with no repository above their cwd, and cleared (`null`) for the
/// rest, so a git Space never grows a trailing ` · ` separator.
pub const PAD_TOKEN: &str = "pad";

/// The value of [`PAD_TOKEN`]: invisible, non-whitespace (Rust's `trim` does
/// not strip U+200B), and non-empty, which is all herdr needs to keep the row.
pub const PAD_VALUE: &str = "\u{200B}";

/// The `agents` value for a space with no agent pane at all.
///
/// Reporting `null` (the pre-2026-10 behaviour) leaves the slot after the
/// project name empty, which reads as a rendering glitch rather than as "no
/// agent here"; a lone dash says it on purpose.
pub const NO_AGENT: &str = "-";

/// `workspace.report_metadata` source id (herdr shows it nowhere, but tokens
/// from different sources merge into the same workspace map).
pub const SOURCE: &str = "herdr-sidebar";

/// Token lifetime. Re-reported every [`REPORT_INTERVAL`], so a killed sidebar's
/// value expires on its own instead of pinning a stale agent name forever.
pub const TTL_MS: u64 = 20_000;

/// How often a heartbeat re-reports. Half the TTL: one lost beat never lets a
/// live sidebar's value expire.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Cap on the rendered value. The rail ellipsizes anything wider than its
/// column, so there is no point sending more; truncation keeps the head, where
/// the busy agents are.
pub const MAX_VALUE_CHARS: usize = 48;

/// Separator between agent labels inside one workspace's value.
const SEPARATOR: &str = " · ";

/// Pane states that make an agent's label sort first.
const WORKING: &str = "working";

/// `pane.list` JSON → workspace id → the `$agents` value for that workspace.
///
/// Rules:
/// - only panes carrying a non-empty `display_agent`/`agent` count, labelled by
///   `display_agent` when set and `agent` otherwise;
/// - repeated labels inside one workspace are deduped (two `pi` panes → `pi`),
///   and a label with ANY working pane counts as working;
/// - busy agents come first, then alphabetical, joined with `" · "`;
/// - the value is capped at [`MAX_VALUE_CHARS`] on a char boundary;
/// - a workspace with no agent pane is ABSENT from the map (so the caller
///   clears it);
/// - malformed or agent-less JSON yields an empty map — this never panics.
pub fn describe_agents(pane_list_json: &str) -> BTreeMap<String, String> {
    let Some(parsed) = parse(pane_list_json) else {
        return BTreeMap::new();
    };
    // workspace → (label → any pane working?)
    let mut by_workspace: BTreeMap<String, BTreeMap<String, bool>> = BTreeMap::new();
    for pane in panes(&parsed) {
        let Some(workspace_id) = non_empty(pane.get("workspace_id")) else {
            continue;
        };
        let label = non_empty(pane.get("display_agent"))
            .or_else(|| non_empty(pane.get("agent")))
            .map(str::to_string);
        let Some(label) = label else {
            continue;
        };
        let working = pane
            .get("agent_status")
            .and_then(|value| value.as_str())
            .is_some_and(|state| state == WORKING);
        let entry = by_workspace
            .entry(workspace_id.to_string())
            .or_default()
            .entry(label)
            .or_insert(false);
        *entry = *entry || working;
    }

    by_workspace
        .into_iter()
        .map(|(workspace, labels)| {
            let mut labels: Vec<(String, bool)> = labels.into_iter().collect();
            // Busy first, then alphabetical: `false` sorts before `true`, so the
            // working flag is compared inverted.
            labels.sort_by(|(a_label, a_working), (b_label, b_working)| {
                b_working.cmp(a_working).then_with(|| a_label.cmp(b_label))
            });
            let joined = labels
                .into_iter()
                .map(|(label, _)| label)
                .collect::<Vec<_>>()
                .join(SEPARATOR);
            (workspace, truncate(&joined, MAX_VALUE_CHARS))
        })
        .collect()
}

/// `pane.list` JSON → workspace id → the `$pad` value for that workspace.
///
/// Only workspaces with NO git repository above their cwd are included; the
/// caller reports `null` for the others. The cwd is the first pane's
/// `foreground_cwd` (where the user actually is) falling back to `cwd`, and the
/// repository test is the same upward `.git` walk git itself does — a directory
/// or the `gitdir:` file a worktree checkout uses both count.
pub fn describe_pads(pane_list_json: &str) -> BTreeMap<String, String> {
    let Some(parsed) = parse(pane_list_json) else {
        return BTreeMap::new();
    };
    // workspace → cwd of its first pane (None when no pane reported one)
    let mut by_workspace: BTreeMap<String, Option<String>> = BTreeMap::new();
    for pane in panes(&parsed) {
        let Some(workspace_id) = non_empty(pane.get("workspace_id")) else {
            continue;
        };
        let cwd = non_empty(pane.get("foreground_cwd"))
            .or_else(|| non_empty(pane.get("cwd")))
            .map(str::to_string);
        by_workspace.entry(workspace_id.to_string()).or_insert(cwd);
    }

    by_workspace
        .into_iter()
        .filter(|(_, cwd)| cwd.as_deref().is_none_or(|cwd| !in_git_repo(cwd)))
        .map(|(workspace, _)| (workspace, PAD_VALUE.to_string()))
        .collect()
}

/// Does `cwd` or any ancestor hold a `.git` entry?
fn in_git_repo(cwd: &str) -> bool {
    let mut path = std::path::Path::new(cwd);
    loop {
        if path.join(".git").exists() {
            return true;
        }
        match path.parent() {
            Some(parent) => path = parent,
            None => return false,
        }
    }
}

/// The `agents` token value for `workspace`: its agent labels, or [`NO_AGENT`].
fn agents_or_dash<'a>(agents: &'a BTreeMap<String, String>, workspace: &str) -> &'a str {
    agents.get(workspace).map_or(NO_AGENT, String::as_str)
}

/// The exact `workspace.report_metadata` params object for one workspace.
/// Both tokens are always sent: `report_metadata` MERGES the map, so a stale
/// value only goes away when it is re-sent as `null`.
pub fn report_params(
    workspace_id: &str,
    agents: Option<&str>,
    pad: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "workspace_id": workspace_id,
        "source": SOURCE,
        "tokens": { TOKEN: agents, PAD_TOKEN: pad },
        "ttl_ms": TTL_MS,
    })
}

/// Throttled reporter for one sidebar pane's heartbeat.
///
/// First beat reports immediately (`last` starts at `None`), then at most once
/// per [`REPORT_INTERVAL`].
#[derive(Debug, Default)]
pub struct AgentTokens {
    last: Option<Instant>,
}

impl AgentTokens {
    /// Report every workspace's `agents` token if due. Never panics, never
    /// retries, and ignores every failure: the rail is decoration, and the next
    /// beat (or any other sidebar's) fixes a transient error.
    ///
    /// EVERY workspace seen in `pane.list` is (re)reported each beat, with
    /// [`NO_AGENT`] for the ones that have no agent pane. Reporting instead of diffing
    /// keeps the 20 s TTL alive, so a killed sidebar's value can never outlive
    /// the TTL; and duplicate reports from the several sidebar processes of one
    /// session are harmless because the values are identical and the API merges
    /// them.
    pub fn report_if_due(&mut self) {
        if self
            .last
            .is_some_and(|last| last.elapsed() < REPORT_INTERVAL)
        {
            return;
        }
        self.last = Some(Instant::now());
        let Ok(json) = crate::ipc::call_text("pane.list", serde_json::json!({})) else {
            return;
        };
        let agents = describe_agents(&json);
        let pads = describe_pads(&json);
        for workspace_id in workspace_ids(&json) {
            let pad = pads.get(&workspace_id);
            // One failure means the socket is in trouble: stop here instead of
            // stacking another bounded-timeout round trip per workspace on the
            // event loop. The next beat re-reports every workspace anyway.
            if crate::ipc::call_text(
                "workspace.report_metadata",
                report_params(
                    &workspace_id,
                    Some(agents_or_dash(&agents, &workspace_id)),
                    pad.map(String::as_str),
                ),
            )
            .is_err()
            {
                break;
            }
        }
    }
}

/// Every distinct workspace id in a `pane.list` reply, sorted.
fn workspace_ids(pane_list_json: &str) -> Vec<String> {
    let Some(parsed) = parse(pane_list_json) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = panes(&parsed)
        .filter_map(|pane| non_empty(pane.get("workspace_id")))
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    ids
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

/// Truncate on a char boundary without appending anything.
fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `pane.list` entry. `pane_id` is irrelevant to `describe_agents`, so
    /// every pane in a workspace shares one.
    fn pane(
        workspace: &str,
        agent: Option<&str>,
        display: Option<&str>,
        state: &str,
    ) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        fields.insert("pane_id".to_string(), format!("{workspace}:p1").into());
        fields.insert("workspace_id".to_string(), workspace.into());
        fields.insert("agent_status".to_string(), state.into());
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

    #[test]
    fn working_agent_sorts_first() {
        let json = list(&[
            pane("w1", Some("codex"), Some("codex"), "idle"),
            pane("w1", Some("claude"), Some("claude"), "working"),
        ]);
        let map = describe_agents(&json);
        assert_eq!(map.get("w1").map(String::as_str), Some("claude · codex"));
    }

    #[test]
    fn repeated_agent_deduped() {
        let json = list(&[
            pane("w1", Some("pi"), Some("pi"), "idle"),
            pane("w1", Some("pi"), Some("pi"), "working"),
        ]);
        let map = describe_agents(&json);
        assert_eq!(map.get("w1").map(String::as_str), Some("pi"));
    }

    #[test]
    fn workspace_without_agents_is_absent() {
        let json = list(&[
            pane("w1", Some("claude"), None, "idle"),
            pane("w2", None, None, "idle"),
        ]);
        let map = describe_agents(&json);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("w1"));
        assert!(!map.contains_key("w2"));
    }

    #[test]
    fn display_agent_wins_over_agent() {
        let json = list(&[pane("w1", Some("claude-code"), Some("claude"), "idle")]);
        let map = describe_agents(&json);
        assert_eq!(map.get("w1").map(String::as_str), Some("claude"));
    }

    #[test]
    fn malformed_json_is_empty() {
        assert!(describe_agents("not json").is_empty());
        assert!(describe_agents("").is_empty());
        assert!(describe_agents(r#"{"result":{}}"#).is_empty());
        assert!(workspace_ids("not json").is_empty());
    }

    /// A space with no agent pane says so with a dash instead of leaving an
    /// empty slot after the project name (user-requested).
    #[test]
    fn an_agent_less_space_reports_a_dash() {
        let with = describe_agents(
            r#"{"result":{"panes":[
                {"pane_id":"w4:p1","workspace_id":"w4","agent":"pi"},
                {"pane_id":"w5:p1","workspace_id":"w5"}
            ]}}"#,
        );
        assert_eq!(agents_or_dash(&with, "w4"), "pi");
        assert_eq!(agents_or_dash(&with, "w5"), NO_AGENT);
        assert_eq!(agents_or_dash(&with, "w9"), NO_AGENT);
        assert_eq!(NO_AGENT, "-");
        // The rail renders whatever string arrives, so the dash is what shows.
        let params = report_params("w5", Some(agents_or_dash(&with, "w5")), None);
        assert_eq!(params["tokens"]["agents"], "-");
    }

    #[test]
    fn report_params_sets_and_clears_both_tokens() {
        let set = report_params("w4", Some("claude · pi"), None);
        assert_eq!(set["workspace_id"], "w4");
        assert_eq!(set["source"], "herdr-sidebar");
        assert_eq!(set["tokens"]["agents"], "claude · pi");
        assert!(set["tokens"]["pad"].is_null());
        assert_eq!(set["ttl_ms"], 20_000);

        // Both keys are always present: report_metadata merges, so only an
        // explicit null removes a value an earlier beat left behind.
        let pad = report_params("w4", None, Some(PAD_VALUE));
        assert!(pad["tokens"]["agents"].is_null());
        assert_eq!(pad["tokens"]["pad"], PAD_VALUE);
        assert_eq!(pad["tokens"].as_object().map(|map| map.len()), Some(2));
    }

    /// A workspace whose panes sit in a directory (or under one) with a `.git`
    /// entry gets no pad; one with no repository above it is padded.
    #[test]
    fn pads_only_workspaces_without_a_repository() {
        let root = std::env::temp_dir().join(format!("herdr-pad-test-{}", std::process::id()));
        let repo = root.join("repo");
        let nested = repo.join("src").join("deep");
        let plain = root.join("plain");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::create_dir_all(&plain).expect("mkdir");
        std::fs::create_dir_all(repo.join(".git")).expect("mkdir");

        let pane_at = |workspace: &str, cwd: &std::path::Path| {
            let mut pane = pane(workspace, None, None, "idle");
            pane["cwd"] = cwd.to_string_lossy().to_string().into();
            pane
        };
        let json = list(&[pane_at("git", &nested), pane_at("plain", &plain)]);
        let pads = describe_pads(&json);
        assert!(!pads.contains_key("git"), "a repository must not be padded");
        assert_eq!(pads.get("plain").map(String::as_str), Some(PAD_VALUE));

        std::fs::remove_dir_all(&root).ok();
    }
    #[test]
    fn value_is_capped_at_48_chars() {
        let names: Vec<String> = (0..12).map(|index| format!("agent-{index:02}")).collect();
        let panes: Vec<serde_json::Value> = names
            .iter()
            .map(|name| pane("w1", Some(name), None, "idle"))
            .collect();
        let map = describe_agents(&list(&panes));
        let value = map.get("w1").expect("workspace present");
        assert_eq!(value.chars().count(), MAX_VALUE_CHARS);
        // Nothing appended: the value is a prefix of the untruncated join.
        let full = names.join(SEPARATOR);
        assert_eq!(
            value,
            &full.chars().take(MAX_VALUE_CHARS).collect::<String>()
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // A 60-char label of multi-byte chars: the cut lands inside a
        // character if it is measured in bytes rather than chars.
        let label = "é".repeat(60);
        let map = describe_agents(&list(&[pane("w1", Some(&label), None, "idle")]));
        let value = map.get("w1").expect("workspace present");
        assert_eq!(value.chars().count(), MAX_VALUE_CHARS);
        assert!(value.chars().all(|c| c == 'é'));
    }


    #[test]
    fn workspace_ids_are_deduped_and_sorted() {
        let json = list(&[
            pane("w2", None, None, "idle"),
            pane("w1", Some("pi"), None, "idle"),
            pane("w2", Some("pi"), None, "idle"),
        ]);
        assert_eq!(
            workspace_ids(&json),
            vec!["w1".to_string(), "w2".to_string()]
        );
    }
}
