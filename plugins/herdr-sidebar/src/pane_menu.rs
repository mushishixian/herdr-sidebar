//! herdr's own pane right-click items, offered inside the sidebar's context
//! menus. The sidebar takes plain right-clicks in its pane (see
//! `ipc::route_right_click_to_pane`), so herdr's pane menu no longer opens
//! there; these entries bring its actions back, in herdr's order.
//!
//! Requests follow herdr's socket schema (`herdr api schema --json`):
//! `pane.split` names its target `target_pane_id`, while `pane.rename`,
//! `pane.zoom` and `pane.input.set` take `pane_id`.

use serde_json::{Value, json};

use crate::launch;

/// One herdr pane action. "Swap with focused pane" is left out: the sidebar
/// is the focused pane whenever its menu is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneAction {
    Rename,
    ClearName,
    SplitRight,
    SplitDown,
    Zoom,
    /// Hand plain right-clicks back to herdr's pane menu.
    RightClickHerdr,
    Close,
}

impl PaneAction {
    /// herdr's wording for the item.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rename => "Rename pane…",
            Self::ClearName => "Clear pane name",
            Self::SplitRight => "Split right",
            Self::SplitDown => "Split down",
            Self::Zoom => "Zoom",
            Self::RightClickHerdr => "Use Herdr right-click menu",
            Self::Close => "Close pane",
        }
    }
}

/// The section's items in herdr's order. "Clear pane name" is offered only
/// while the pane carries a label.
pub fn items(labelled: bool) -> Vec<PaneAction> {
    let mut items = vec![PaneAction::Rename];
    if labelled {
        items.push(PaneAction::ClearName);
    }
    items.extend([
        PaneAction::SplitRight,
        PaneAction::SplitDown,
        PaneAction::Zoom,
        PaneAction::RightClickHerdr,
        PaneAction::Close,
    ]);
    items
}

/// `pane.rename` for `pane_id`; `None` clears the label.
pub fn rename(pane_id: &str, label: Option<&str>) -> (&'static str, Value) {
    ("pane.rename", json!({ "pane_id": pane_id, "label": label }))
}

/// The socket request for an item that needs no prompt. `Rename` (prompts
/// for a name) and `Close` (the view's own hide path) have none.
pub fn request(action: PaneAction, pane_id: &str) -> Option<(&'static str, Value)> {
    match action {
        PaneAction::ClearName => Some(rename(pane_id, None)),
        PaneAction::SplitRight => Some((
            "pane.split",
            json!({ "target_pane_id": pane_id, "direction": "right" }),
        )),
        PaneAction::SplitDown => Some((
            "pane.split",
            json!({ "target_pane_id": pane_id, "direction": "down" }),
        )),
        PaneAction::Zoom => Some(("pane.zoom", json!({ "pane_id": pane_id }))),
        PaneAction::RightClickHerdr => Some((
            "pane.input.set",
            json!({ "pane_id": pane_id, "right_click": "herdr" }),
        )),
        PaneAction::Rename | PaneAction::Close => None,
    }
}

/// Send one request over the socket. herdr's error object, when present,
/// becomes the message shown to the user.
pub fn run(method: &str, params: Value) -> Result<(), String> {
    let response = crate::ipc::call_text(method, params).map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_str(launch::strip_bom(&response)).map_err(|error| error.to_string())?;
    match value.get("error") {
        Some(error) => Err(error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_string)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_follow_herdr_order() {
        assert_eq!(
            items(true),
            [
                PaneAction::Rename,
                PaneAction::ClearName,
                PaneAction::SplitRight,
                PaneAction::SplitDown,
                PaneAction::Zoom,
                PaneAction::RightClickHerdr,
                PaneAction::Close,
            ]
        );
    }

    #[test]
    fn clear_name_needs_a_label() {
        let unlabelled = items(false);
        assert!(!unlabelled.contains(&PaneAction::ClearName));
        assert_eq!(unlabelled.first(), Some(&PaneAction::Rename));
        assert_eq!(unlabelled.last(), Some(&PaneAction::Close));
        assert_eq!(unlabelled.len(), items(true).len() - 1);
    }

    #[test]
    fn labels_match_herdr_wording() {
        let labels: Vec<&str> = items(true).into_iter().map(PaneAction::label).collect();
        assert_eq!(
            labels,
            [
                "Rename pane…",
                "Clear pane name",
                "Split right",
                "Split down",
                "Zoom",
                "Use Herdr right-click menu",
                "Close pane",
            ]
        );
    }

    #[test]
    fn requests_use_schema_field_names() {
        assert_eq!(
            request(PaneAction::SplitRight, "w1:p2"),
            Some((
                "pane.split",
                json!({ "target_pane_id": "w1:p2", "direction": "right" })
            ))
        );
        assert_eq!(
            request(PaneAction::SplitDown, "w1:p2"),
            Some((
                "pane.split",
                json!({ "target_pane_id": "w1:p2", "direction": "down" })
            ))
        );
        assert_eq!(
            request(PaneAction::Zoom, "w1:p2"),
            Some(("pane.zoom", json!({ "pane_id": "w1:p2" })))
        );
        assert_eq!(
            request(PaneAction::RightClickHerdr, "w1:p2"),
            Some((
                "pane.input.set",
                json!({ "pane_id": "w1:p2", "right_click": "herdr" })
            ))
        );
        assert_eq!(
            request(PaneAction::ClearName, "w1:p2"),
            Some(("pane.rename", json!({ "pane_id": "w1:p2", "label": null })))
        );
        assert_eq!(request(PaneAction::Rename, "w1:p2"), None);
        assert_eq!(request(PaneAction::Close, "w1:p2"), None);
    }

    #[test]
    fn rename_carries_the_typed_label() {
        assert_eq!(
            rename("w1:p2", Some("build")),
            (
                "pane.rename",
                json!({ "pane_id": "w1:p2", "label": "build" })
            )
        );
    }
}
