//! Shortcut reminders: when a plugin action runs from a herdr key binding, toast
//! its name together with the key bound to it, e.g. "Switch branch" / "Ctrl+Alt+C".
//!
//! Only acts when `HERDR_PLUGIN_ACTION_ID` is set, i.e. the process was started
//! as a plugin action. Hooks and internal re-invocations never toast. The key is
//! read from herdr's config.toml (`[[keys.command]]` entries of type
//! `plugin_action`), so the lookup is one file read plus a TOML parse.

use std::path::PathBuf;

use serde_json::json;

/// Env var herdr sets for plugin actions (absent for hooks and terminal runs).
const ACTION_ID_ENV: &str = "HERDR_PLUGIN_ACTION_ID";

/// Toast the action's title, with the bound key as the body when one is found.
/// Errors are ignored: a missing herdr socket must never break the action.
pub fn show(action_id: &str, title: &str) {
    if !invoked_as_action() {
        return;
    }
    let mut params = json!({ "title": title });
    if let Some(key) = key_for(action_id) {
        params["body"] = json!(key);
    }
    let _ = crate::ipc::call_text("notification.show", params);
}

/// Suffix such as `" · Ctrl+Alt+P"` for a title, or empty when the action was
/// not invoked as a plugin action or no key is bound to it.
pub fn suffix_for(action_id: &str) -> String {
    if !invoked_as_action() {
        return String::new();
    }
    key_for(action_id)
        .map(|key| format!(" · {key}"))
        .unwrap_or_default()
}

/// Announce a CLI flag that maps onto a manifest action. Called from both the
/// main binary and the Windows sidecar so the two stay in step. `arg` is the
/// argument following the flag (only `--git-popup-open` uses it).
pub fn announce_flag(flag: &str, arg: Option<&str>) {
    let (action_id, title) = match (flag, arg) {
        ("--toggle", _) => ("open-sidebar", "Toggle sidebar"),
        ("--toggle-git", _) => ("open-git", "Source Control pane"),
        ("--show-explorer", _) => ("show-explorer", "Explorer"),
        ("--show-search", _) => ("show-search", "Search in files"),
        ("--show-git", _) => ("show-git", "Source Control"),
        ("--quick-open", _) => ("quick-open", "Quick Open"),
        ("--file-popup-open", _) => ("file-search", "File search"),
        ("--search-popup-open", _) => ("search-popup", "Search in files"),
        ("--replace-popup-open", _) => ("replace-popup", "Replace in files"),
        ("--symbol-popup-open", _) => ("symbol-popup", "Go to symbol"),
        ("--merge-popup-open", _) => ("merge-editor", "Resolve merge conflicts"),
        ("--git-popup-open", Some("switch")) => ("git-switch-branch", "Switch branch"),
        ("--git-popup-open", Some("rename")) => ("git-rename-branch", "Rename branch"),
        ("--git-popup-open", Some("merge")) => ("git-merge-branch", "Merge into current branch"),
        _ => return,
    };
    show(action_id, title);
}

fn invoked_as_action() -> bool {
    std::env::var(ACTION_ID_ENV).is_ok_and(|value| !value.is_empty())
}

/// The pretty-printed key bound to `herdr-sidebar.<action_id>` (or its
/// `-windows` twin) in herdr's config, if any.
pub fn key_for(action_id: &str) -> Option<String> {
    let path: PathBuf = crate::viewer::herdr_config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    key_in_config(&text, action_id)
}

/// Pure core of [`key_for`]: parse herdr's config text and find the first
/// `plugin_action` binding for the action. `None` on invalid TOML.
pub fn key_in_config(text: &str, action_id: &str) -> Option<String> {
    let table = text.replace('\u{feff}', "").parse::<toml::Table>().ok()?;
    let keys = table.get("keys").and_then(toml::Value::as_table);
    let prefix = keys
        .and_then(|keys| keys.get("prefix"))
        .and_then(toml::Value::as_str)
        .unwrap_or("ctrl+b");
    let commands = keys?.get("command")?.as_array()?;

    let unix = format!("herdr-sidebar.{action_id}");
    let windows = format!("herdr-sidebar.{action_id}-windows");
    for entry in commands {
        let Some(entry) = entry.as_table() else {
            continue;
        };
        if entry.get("type").and_then(toml::Value::as_str) != Some("plugin_action") {
            continue;
        }
        let Some(command) = entry.get("command").and_then(toml::Value::as_str) else {
            continue;
        };
        if command != unix && command != windows {
            continue;
        }
        if let Some(key) = entry.get("key").and_then(toml::Value::as_str) {
            return Some(pretty_key(key, prefix));
        }
    }
    None
}

/// Render a herdr key string for display: `"prefix+i"` with prefix `ctrl+b`
/// becomes `"Ctrl+B I"`, `"ctrl+alt+c"` becomes `"Ctrl+Alt+C"`.
pub fn pretty_key(key: &str, prefix: &str) -> String {
    match key.strip_prefix("prefix+") {
        Some(rest) => format!("{} {}", pretty_combo(prefix), pretty_name(rest)),
        None => pretty_combo(key),
    }
}

/// `ctrl+shift+a` -> `Ctrl+Shift+A`: modifiers capitalised, last part is the key.
fn pretty_combo(combo: &str) -> String {
    let parts: Vec<&str> = combo.split('+').collect();
    let Some((last, modifiers)) = parts.split_last() else {
        return String::new();
    };
    let mut out: Vec<String> = modifiers.iter().map(|m| pretty_modifier(m)).collect();
    out.push(pretty_name(last));
    out.join("+")
}

fn pretty_modifier(modifier: &str) -> String {
    match modifier.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Ctrl".to_string(),
        "alt" | "option" => "Alt".to_string(),
        "shift" => "Shift".to_string(),
        "cmd" | "command" => "Cmd".to_string(),
        "super" | "win" | "meta" => "Super".to_string(),
        other => capitalise(other),
    }
}

/// A single key name: single characters upper-cased, punctuation words mapped
/// to their glyph, everything else capitalised ("tab" -> "Tab").
fn pretty_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let glyph = match lower.as_str() {
        "backtick" | "grave" => Some("`"),
        "slash" => Some("/"),
        "backslash" => Some("\\"),
        "minus" | "dash" => Some("-"),
        "equal" | "equals" => Some("="),
        "comma" => Some(","),
        "period" | "dot" => Some("."),
        "semicolon" => Some(";"),
        "quote" | "apostrophe" => Some("'"),
        "leftbracket" | "lbracket" => Some("["),
        "rightbracket" | "rbracket" => Some("]"),
        _ => None,
    };
    if let Some(glyph) = glyph {
        return glyph.to_string();
    }
    match lower.as_str() {
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        _ if name.chars().count() == 1 => name.to_uppercase(),
        _ => capitalise(&lower),
    }
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_key_renders_prefix_chords_and_modifiers() {
        assert_eq!(pretty_key("prefix+i", "ctrl+b"), "Ctrl+B I");
        assert_eq!(pretty_key("ctrl+alt+c", "ctrl+b"), "Ctrl+Alt+C");
        assert_eq!(pretty_key("ctrl+shift+a", "ctrl+b"), "Ctrl+Shift+A");
        assert_eq!(pretty_key("prefix+/", "ctrl+b"), "Ctrl+B /");
        assert_eq!(pretty_key("prefix+backtick", "ctrl+b"), "Ctrl+B `");
        assert_eq!(pretty_key("ctrl+q", "ctrl+b"), "Ctrl+Q");
        assert_eq!(pretty_key("tab", "ctrl+b"), "Tab");
        assert_eq!(pretty_key("prefix+enter", "ctrl+a"), "Ctrl+A Enter");
        assert_eq!(pretty_key("prefix+space", "ctrl+b"), "Ctrl+B Space");
        assert_eq!(pretty_key("ctrl+f5", "ctrl+b"), "Ctrl+F5");
    }

    const CONFIG: &str = r#"
[keys]
prefix = "ctrl+b"

[[keys.command]]
key = "prefix+x"
type = "popup"
command = "herdr-sidebar.open-sidebar"

[[keys.command]]
key = "ctrl+alt+c"
type = "plugin_action"
command = "herdr-sidebar.git-switch-branch"

[[keys.command]]
key = "ctrl+alt+w"
type = "plugin_action"
command = "herdr-sidebar.open-sidebar-windows"

[[keys.command]]
key = "prefix+i"
type = "plugin_action"
command = "herdr-sidebar.open-sidebar"

[[keys.command]]
key = "ctrl+alt+s"
type = "plugin_action"
command = "herdr-sidebar.open-sidebar-windows"
"#;

    #[test]
    fn key_in_config_finds_plugin_action_bindings() {
        assert_eq!(
            key_in_config(CONFIG, "git-switch-branch").as_deref(),
            Some("Ctrl+Alt+C")
        );
        // The popup-typed binding for open-sidebar is skipped; the first
        // plugin_action entry (the -windows variant) wins.
        assert_eq!(
            key_in_config(CONFIG, "open-sidebar").as_deref(),
            Some("Ctrl+Alt+W")
        );
    }

    #[test]
    fn key_in_config_matches_windows_variant_only_action() {
        let text = r#"
[[keys.command]]
key = "ctrl+alt+s"
type = "plugin_action"
command = "herdr-sidebar.show-search-windows"
"#;
        assert_eq!(
            key_in_config(text, "show-search").as_deref(),
            Some("Ctrl+Alt+S")
        );
    }

    #[test]
    fn key_in_config_uses_configured_prefix() {
        let text = r#"
[keys]
prefix = "ctrl+a"

[[keys.command]]
key = "prefix+i"
type = "plugin_action"
command = "herdr-sidebar.open-sidebar"
"#;
        assert_eq!(
            key_in_config(text, "open-sidebar").as_deref(),
            Some("Ctrl+A I")
        );
    }

    #[test]
    fn key_in_config_missing_action_is_none() {
        assert_eq!(key_in_config(CONFIG, "quick-open"), None);
        assert_eq!(key_in_config("", "open-sidebar"), None);
    }

    #[test]
    fn key_in_config_ignores_bom() {
        let text = format!("\u{feff}{CONFIG}");
        assert_eq!(
            key_in_config(&text, "git-switch-branch").as_deref(),
            Some("Ctrl+Alt+C")
        );
    }

    #[test]
    fn key_in_config_invalid_toml_is_none() {
        assert_eq!(
            key_in_config("[keys\ncommand = [", "git-switch-branch"),
            None
        );
    }
}
