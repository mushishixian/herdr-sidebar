//! The sidebar's Terminal-height and Agent-width settings act on the IDE
//! layout of ONE tab: the editor/terminal column's bottom pane and the
//! rightmost Agent column. These helpers read that layout from herdr and set
//! a split's ratio through `layout.set_split_ratio`. Both sidebar apps and the
//! Settings page share them, so the sizing rules cannot drift.

use crate::{ipc, launch};

/// Raw `pane.layout` JSON for `pane_id`, or `None` when herdr did not answer.
fn layout(pane_id: &str) -> Option<String> {
    ipc::call_text("pane.layout", serde_json::json!({ "pane_id": pane_id })).ok()
}

/// Rows the bottom pane of this tab's editor/terminal column occupies right
/// now — the seed for the first `←`/`→` on an "auto" Terminal height.
pub fn terminal_rows_now(pane_id: &str) -> Option<u16> {
    let layout = layout(pane_id)?;
    let (_, ratio, height) = launch::foreign_down_split(&layout, pane_id)?;
    let rows = (1.0 - ratio) * height as f64;
    Some(rows.round().max(0.0) as u16)
}

/// Size this tab's editor/terminal split so its BOTTOM pane is `rows` rows
/// tall. False when the tab has no such split (nothing to size), so the caller
/// can say so instead of looking like it worked. `tab_id` names the tab when
/// the caller knows it; without it the split is resolved through `pane_id`.
pub fn set_terminal_rows(pane_id: &str, tab_id: Option<&str>, rows: u16) -> bool {
    let Some(layout) = layout(pane_id) else {
        return false;
    };
    let Some((path, _ratio, height)) = launch::foreign_down_split(&layout, pane_id) else {
        return false;
    };
    apply_split_ratio(pane_id, tab_id, path, launch::ratio_for_rows(rows, height))
}

/// Columns the Agent column of this tab occupies right now — the seed for the
/// first `←`/`→` on an "auto" Agent width.
pub fn agent_cols_now(pane_id: &str) -> Option<u16> {
    let layout = layout(pane_id)?;
    let (_, ratio, width) = launch::foreign_right_split(&layout, pane_id)?;
    let cols = (1.0 - ratio) * width as f64;
    Some(cols.round().max(0.0) as u16)
}

/// Size this tab's Agent column so it is `cols` columns wide. False when the
/// tab has no such split, like [`set_terminal_rows`].
pub fn set_agent_cols(pane_id: &str, tab_id: Option<&str>, cols: u16) -> bool {
    let Some(layout) = layout(pane_id) else {
        return false;
    };
    let Some((path, _ratio, width)) = launch::foreign_right_split(&layout, pane_id) else {
        return false;
    };
    apply_split_ratio(pane_id, tab_id, path, launch::ratio_for_cols(cols, width))
}

/// Set one of this tab's splits to `ratio`. False when the host refused it
/// (`call_text` returns the reply line even for an error response).
fn apply_split_ratio(pane_id: &str, tab_id: Option<&str>, path: Vec<bool>, ratio: f64) -> bool {
    let mut params = serde_json::json!({ "path": path, "ratio": ratio });
    // Prefer the tab form; a launch that did not tell us its tab still
    // resolves the same tab from the pane.
    match tab_id {
        Some(tab) => params["tab_id"] = serde_json::Value::String(tab.to_string()),
        None => params["pane_id"] = serde_json::Value::String(pane_id.to_string()),
    }
    match ipc::call_text("layout.set_split_ratio", params) {
        Ok(reply) => {
            serde_json::from_str::<serde_json::Value>(reply.trim_start_matches('\u{feff}'))
                .map(|value| value.get("error").is_none())
                .unwrap_or(false)
        }
        Err(_) => false,
    }
}
