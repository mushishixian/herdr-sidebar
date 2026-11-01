//! `settings.json`: the one file that holds the user's sidebar settings (see
//! [`crate::state`] for the resolution rules and the migration from
//! state.json).
//!
//! The file is user-edited, so nothing here trusts it:
//! - [`validate_settings`] is the single rule set. A file is applied only when
//!   it validates; an invalid one leaves the last good parse of this process in
//!   force (see [`settings_snapshot`]) and never gets overwritten by a save.
//! - [`poll_settings_file`] runs on every event-loop turn. It costs one
//!   `metadata()` call, and it validates and reports only when the file's
//!   (mtime, len) changed since the last poll. Only the elected session reporter
//!   toasts, so one error is shown once per session, not once per sidebar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::icons::IconTheme;
use crate::state::{self, ColorTheme, PreviewPlacement, USER_KEYS};

type Settings = Map<String, Value>;

/// A cheap identity of the file's current content: (mtime, length).
pub(crate) type FileKey = (SystemTime, u64);

pub(crate) fn file_key(path: &Path) -> Option<FileKey> {
    let meta = std::fs::metadata(path).ok()?;
    Some((
        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        meta.len(),
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Validate settings.json text: `Ok(warnings)` when the file is applied,
/// `Err(message)` when it is not. The message names the key and the accepted
/// values, or the line and column of a syntax error. Unknown keys are warnings.
/// A leading UTF-8 BOM is accepted.
pub fn validate_settings(text: &str) -> Result<Vec<String>, String> {
    check_settings(text).map(|(_, warnings)| warnings)
}

/// [`validate_settings`] that also returns the parsed object, for the cache and
/// for the save path, which keeps the file's unknown keys.
pub(crate) fn check_settings(text: &str) -> Result<(Settings, Vec<String>), String> {
    let text = text.trim_start_matches('\u{feff}');
    let value: Value = serde_json::from_str(text).map_err(|error| json_error_message(&error))?;
    let Value::Object(map) = value else {
        return Err("settings.json must hold one JSON object: { … }".into());
    };
    let mut warnings = Vec::new();
    for (key, value) in &map {
        if USER_KEYS.contains(&key.as_str()) {
            check_value(key, value)?;
        } else {
            warnings.push(format!("unknown setting `{key}`"));
        }
    }
    Ok((map, warnings))
}

/// A JSON syntax error in the house format: `line L, column C: message`. The
/// location is spelled out once, in front; serde_json's own " at line" suffix
/// is dropped. Shared with keymap.json.
pub(crate) fn json_error_message(error: &serde_json::Error) -> String {
    let full = error.to_string();
    let message = match full.rsplit_once(" at line ") {
        Some((message, _)) => message.to_string(),
        None => full.clone(),
    };
    format!(
        "line {}, column {}: {message}",
        error.line(),
        error.column()
    )
}

/// One user setting's value: its type, and for the enums and ranges the names
/// and bounds the sidebar's own parser accepts.
fn check_value(key: &str, value: &Value) -> Result<(), String> {
    let ok = match key {
        "icons" => {
            value.is_null()
                || value
                    .as_str()
                    .is_some_and(|name| IconTheme::from_state_name(name).is_some())
        }
        "colors" => value
            .as_str()
            .is_some_and(|name| ColorTheme::from_state_name(name).is_some()),
        "preview_placement" => value
            .as_str()
            .is_some_and(|name| PreviewPlacement::from_state_name(name).is_some()),
        "sidebar_width" => in_range(value, 24, 80),
        "terminal_height" => value.is_null() || in_range(value, 4, 40),
        "agent_width" => value.is_null() || in_range(value, 24, 320),
        _ => value.is_boolean(),
    };
    if ok {
        return Ok(());
    }
    Err(match key {
        "icons" => "`icons` must be \"emoji\", \"material\", or null for auto".into(),
        "colors" => "`colors` must be one of \"vscode\", \"light\", \"terminal\"".into(),
        "preview_placement" => {
            "`preview_placement` must be one of \"tab\", \"pane\", \"replace\"".into()
        }
        "sidebar_width" => "`sidebar_width` must be a number from 24 to 80".into(),
        "terminal_height" => {
            "`terminal_height` must be a number from 4 to 40, or null for auto".into()
        }
        "agent_width" => "`agent_width` must be a number from 24 to 320, or null for auto".into(),
        _ => format!("`{key}` must be true or false"),
    })
}

fn in_range(value: &Value, min: u64, max: u64) -> bool {
    value
        .as_u64()
        .is_some_and(|number| (min..=max).contains(&number))
}

// ---------------------------------------------------------------------------
// The applied view, cached by file identity.
// ---------------------------------------------------------------------------

/// What this process currently knows about settings.json.
#[derive(Clone, Default)]
pub struct SettingsSnapshot {
    /// The file exists.
    pub present: bool,
    /// The last VALID parse in this process, or `None` when there never was one.
    /// An invalid file never replaces it.
    pub good: Option<Arc<Settings>>,
    /// The validation error of the file as it is now, if it is invalid.
    pub error: Option<String>,
    /// Warnings of the current valid file (unknown keys).
    pub warnings: Vec<String>,
}

#[derive(Clone, Default)]
struct Cached {
    key: Option<FileKey>,
    good: Option<Arc<Settings>>,
    error: Option<String>,
    warnings: Vec<String>,
}

impl Cached {
    fn snapshot(&self) -> SettingsSnapshot {
        SettingsSnapshot {
            present: true,
            good: self.good.clone(),
            error: self.error.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The current settings.json as this process may apply it. Re-validates only
/// when (mtime, len) changed, so [`crate::state::load_state`] can call it on
/// every draw.
pub fn settings_snapshot() -> SettingsSnapshot {
    let Some(path) = state::settings_path() else {
        return SettingsSnapshot::default();
    };
    let Some(key) = file_key(&path) else {
        // No file: no settings layer. A deleted file forgets its last good parse.
        lock(cache()).remove(&path);
        return SettingsSnapshot::default();
    };
    if let Some(cached) = lock(cache()).get(&path).filter(|c| c.key == Some(key)) {
        return cached.snapshot();
    }
    let checked = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read settings.json: {error}"))
        .and_then(|text| check_settings(&text));
    let mut map = lock(cache());
    let entry = map.entry(path).or_default();
    entry.key = Some(key);
    match checked {
        Ok((settings, warnings)) => {
            store_last_good_text(&state::pretty_object(&canonical_entries(&settings)));
            entry.good = Some(Arc::new(settings));
            entry.error = None;
            entry.warnings = warnings;
        }
        Err(error) => {
            entry.error = Some(error);
            entry.warnings.clear();
        }
    }
    entry.snapshot()
}

/// The settings in the canonical order of the written file: the user keys in
/// [`USER_KEYS`] order, then every other key in the file's order.
fn canonical_entries(settings: &Settings) -> Vec<(String, Value)> {
    let mut entries: Vec<(String, Value)> = USER_KEYS
        .iter()
        .filter_map(|key| {
            settings
                .get(*key)
                .map(|value| (key.to_string(), value.clone()))
        })
        .collect();
    entries.extend(
        settings
            .iter()
            .filter(|(key, _)| !USER_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    entries
}

// ---------------------------------------------------------------------------
// The last-good copy: settings.last-good.json.
//
// A process that starts while settings.json is broken has no in-memory parse
// to fall back on, and state.json no longer holds the user keys. So every
// VALID parse or save also leaves a copy beside the file, and `load_state`
// reads it only while settings.json is invalid.
// ---------------------------------------------------------------------------

/// Save `text` (a canonical settings file) as the last-good copy, unless the
/// copy already holds exactly that. Best effort.
pub(crate) fn store_last_good_text(text: &str) {
    let Some(path) = state::last_good_path() else {
        return;
    };
    if std::fs::read_to_string(&path).is_ok_and(|stored| stored == text) {
        return;
    }
    let _ = state::write_atomic(&path, text);
}

type LastGood = HashMap<PathBuf, (FileKey, Arc<Settings>)>;

fn last_good_cache() -> &'static Mutex<LastGood> {
    static LAST_GOOD: OnceLock<Mutex<LastGood>> = OnceLock::new();
    LAST_GOOD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The user settings saved in the last-good copy, or `None` when it is missing
/// or unreadable. Read and parsed only on the invalid path, cached by
/// (mtime, len).
pub(crate) fn last_good_values() -> Option<Arc<Settings>> {
    let path = state::last_good_path()?;
    let key = file_key(&path)?;
    if let Some((cached_key, values)) = lock(last_good_cache()).get(&path)
        && *cached_key == key
    {
        return Some(values.clone());
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let (map, _warnings) = check_settings(&text).ok()?;
    let values = Arc::new(map);
    lock(last_good_cache()).insert(path, (key, values.clone()));
    Some(values)
}

/// Forget every cached parse, so the next read goes to the files (tests only).
#[cfg(test)]
pub(crate) fn reset_caches() {
    lock(cache()).clear();
    lock(last_good_cache()).clear();
}

/// The current validation error of settings.json, or `None` when the file is
/// missing or valid. The Settings page shows it.
pub fn settings_error() -> Option<String> {
    settings_snapshot().error
}

// ---------------------------------------------------------------------------
// Telling the user when the file changes underneath them.
// ---------------------------------------------------------------------------

/// What one sidebar process has seen of settings.json.
#[derive(Default)]
struct Watch {
    /// The first poll has run (or this process has written the file).
    seen: bool,
    last: Option<FileKey>,
    /// The key of this process's most recent write. Its own writes never toast.
    own: Option<FileKey>,
    /// The last content this process observed was invalid.
    invalid: bool,
}

fn watches() -> &'static Mutex<HashMap<PathBuf, Watch>> {
    static WATCHES: OnceLock<Mutex<HashMap<PathBuf, Watch>>> = OnceLock::new();
    WATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that THIS process just wrote settings.json, so the poll does not
/// report its own write. A save only writes a file that validates (or a file
/// that was missing), so the watch is marked valid.
pub(crate) fn note_own_write(path: &Path) {
    let Some(key) = file_key(path) else {
        return;
    };
    let mut watches = lock(watches());
    let watch = watches.entry(path.to_path_buf()).or_default();
    watch.seen = true;
    watch.last = Some(key);
    watch.own = Some(key);
    watch.invalid = false;
}

/// The toast a changed settings.json earns, if any. An invalid file is always
/// reported. A file that became valid again is announced, with its warnings.
pub(crate) fn toast_for(
    was_invalid: bool,
    snapshot: &SettingsSnapshot,
) -> Option<(&'static str, String)> {
    match &snapshot.error {
        Some(error) => Some(("settings.json not applied", error.clone())),
        None if was_invalid => Some(("settings.json applied", snapshot.warnings.join("; "))),
        None => None,
    }
}

/// Event-loop hook, called on every turn of both sidebar loops. One
/// `metadata()` call; the file is validated only when its (mtime, len) changed
/// since the last poll in this process. The first poll records the baseline and
/// reports only an invalid file.
pub fn poll_settings_file() {
    let Some(path) = state::settings_path() else {
        return;
    };
    let key = file_key(&path);
    let own = {
        let mut watches = lock(watches());
        if !watches.contains_key(&path) {
            watches.insert(path.clone(), Watch::default());
        }
        let Some(watch) = watches.get_mut(&path) else {
            return;
        };
        if watch.seen && watch.last == key {
            return;
        }
        watch.seen = true;
        watch.last = key;
        key.is_some() && watch.own == key
    };
    let snapshot = settings_snapshot();
    let was_invalid = {
        let mut watches = lock(watches());
        let Some(watch) = watches.get_mut(&path) else {
            return;
        };
        std::mem::replace(&mut watch.invalid, snapshot.error.is_some())
    };
    if own {
        return;
    }
    let Some((title, body)) = toast_for(was_invalid, &snapshot) else {
        return;
    };
    // Only one sidebar per session speaks, so one error is one toast.
    if !crate::session_reporter::is_session_reporter() {
        return;
    }
    crate::git_action::notify(title, &body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{State, TEST_STATE_DIR, load_state, parse_state};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh state dir for this test thread (see [`TEST_STATE_DIR`]).
    fn isolated() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "hs-settings-file-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch state dir");
        TEST_STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
        dir
    }

    fn settings_text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("settings.json")).expect("settings.json")
    }

    fn keys_of(text: &str) -> Vec<String> {
        let value: Value = serde_json::from_str(text).expect("json");
        value.as_object().expect("object").keys().cloned().collect()
    }

    /// A state.json as written before the split: every setting in one file.
    const LEGACY: &str = r#"{
        "merged": true, "active": "source-control", "search_active": true,
        "hotkeys": true, "git_footer": false, "confirm_delete": false,
        "font_prompt": true, "icons": "emoji", "colors": "terminal",
        "auto_open": false, "strict_toggle": true, "focus_on_open": false,
        "follow_cwd": false, "git_deco": false, "dock_right": true,
        "sidebar_width": 44, "terminal_height": 22, "agent_width": 96,
        "preview_placement": "pane", "custom_editor_on_click": true,
        "custom_editor_inline": true, "scm_tree_view": true, "show_hidden": false
    }"#;

    #[test]
    fn migration_moves_user_keys_into_settings_and_strips_state() {
        let dir = isolated();
        std::fs::write(dir.join("state.json"), LEGACY).unwrap();

        state::migrate_settings();

        let settings = settings_text(&dir);
        assert_eq!(
            keys_of(&settings),
            {
                let mut expected: Vec<String> = USER_KEYS.iter().map(|k| k.to_string()).collect();
                expected.sort();
                expected
            },
            "every user key, and nothing else"
        );
        assert!(
            settings.starts_with("{\n  \"merged\": true,\n"),
            "{settings}"
        );
        assert!(settings.ends_with("}\n"));
        let state_json = std::fs::read_to_string(dir.join("state.json")).unwrap();
        let mut internal = keys_of(&state_json);
        internal.sort();
        assert_eq!(internal, ["active", "font_prompt", "search_active"]);
        // The effective state is unchanged by the move.
        let loaded = load_state();
        assert_eq!(loaded.color_theme, ColorTheme::Terminal);
        assert_eq!(loaded.sidebar_width, 44);
        assert_eq!(loaded.terminal_height, Some(22));
        assert_eq!(loaded.preview_placement, PreviewPlacement::Pane);
        assert!(loaded.font_prompt_done);
        assert_eq!(loaded.icons, Some(IconTheme::Emoji));
        assert!(!loaded.show_hidden);
    }

    #[test]
    fn a_settings_key_wins_and_a_missing_key_falls_back() {
        let dir = isolated();
        std::fs::write(
            dir.join("state.json"),
            r#"{"colors":"light","show_hidden":false,"sidebar_width":40}"#,
        )
        .unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"colors":"terminal"}"#).unwrap();
        let loaded = load_state();
        assert_eq!(loaded.color_theme, ColorTheme::Terminal, "settings wins");
        assert!(!loaded.show_hidden, "missing key falls back to state.json");
        assert_eq!(loaded.sidebar_width, 40);
        assert!(!loaded.dock_right, "then to the default");
    }

    #[test]
    fn an_invalid_settings_file_keeps_the_last_good_parse_and_is_not_overwritten() {
        let dir = isolated();
        std::fs::write(dir.join("settings.json"), r#"{"colors":"light"}"#).unwrap();
        assert_eq!(load_state().color_theme, ColorTheme::Light);

        let broken = r#"{"colors": "solarized""#;
        std::fs::write(dir.join("settings.json"), broken).unwrap();
        assert_eq!(
            load_state().color_theme,
            ColorTheme::Light,
            "the last good value stays in force"
        );
        assert!(settings_error().is_some());

        let (state, blocked) = state::update_state_reporting(|s| s.dock_right = true);
        assert!(blocked, "a save must not overwrite the broken file");
        assert!(state::settings_write_blocked());
        assert!(state.dock_right, "the change is in memory for this save");
        assert_eq!(settings_text(&dir), broken, "the user's text is untouched");
        let state_json = std::fs::read_to_string(dir.join("state.json")).unwrap();
        assert!(
            !state_json.contains("dock_right"),
            "state.json: internal keys only"
        );
    }

    #[test]
    fn a_valid_save_clears_the_blocked_flag() {
        let dir = isolated();
        std::fs::write(dir.join("settings.json"), "{ nope").unwrap();
        state::update_state(|s| s.dock_right = true);
        assert!(state::settings_write_blocked());
        std::fs::write(dir.join("settings.json"), "{}").unwrap();
        state::update_state(|s| s.dock_right = true);
        assert!(!state::settings_write_blocked());
        assert!(settings_text(&dir).contains("\"dock_right\": true"));
    }

    #[test]
    fn syntax_errors_name_the_line_and_column() {
        let error = validate_settings("{\n  \"merged\" true\n}").unwrap_err();
        assert!(error.contains("line 2"), "{error}");
        assert!(error.contains("column"), "{error}");
    }

    #[test]
    fn a_wrong_type_names_the_key() {
        let error = validate_settings(r#"{"hotkeys": "yes"}"#).unwrap_err();
        assert!(error.contains("`hotkeys`"), "{error}");
        assert!(error.contains("true or false"), "{error}");
    }

    #[test]
    fn an_unknown_enum_value_lists_the_accepted_names() {
        let error = validate_settings(r#"{"colors": "solarized"}"#).unwrap_err();
        assert!(error.contains("`colors`"), "{error}");
        for name in ["vscode", "light", "terminal"] {
            assert!(error.contains(name), "{error}");
        }
        assert!(validate_settings(r#"{"preview_placement": "split"}"#).is_err());
        assert!(validate_settings(r#"{"icons": "fancy"}"#).is_err());
    }

    #[test]
    fn numeric_settings_are_range_checked_and_null_is_auto_where_allowed() {
        let error = validate_settings(r#"{"sidebar_width": 99}"#).unwrap_err();
        assert!(error.contains("from 24 to 80"), "{error}");
        assert!(validate_settings(r#"{"terminal_height": null}"#).is_ok());
        assert!(validate_settings(r#"{"terminal_height": 2}"#).is_err());
        assert!(validate_settings(r#"{"agent_width": 400}"#).is_err());
        assert!(validate_settings(r#"{"sidebar_width": "32"}"#).is_err());
        assert!(validate_settings(r#"{"icons": null}"#).is_ok());
    }

    #[test]
    fn an_unknown_key_is_a_warning_not_an_error() {
        let warnings = validate_settings(r#"{"colours": "light"}"#).expect("applied");
        assert_eq!(warnings, vec!["unknown setting `colours`".to_string()]);
        assert!(validate_settings("[1, 2]").is_err(), "not an object");
    }

    #[test]
    fn a_byte_order_mark_is_accepted() {
        assert!(validate_settings("\u{feff}{\"merged\": false}").is_ok());
        assert!(!parse_state("\u{feff}{\"merged\": false}").merged);
    }

    #[test]
    fn unknown_keys_survive_a_rewrite_after_the_user_keys() {
        let dir = isolated();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"colors": "light", "my_typo": 42, "hotkeys": false}"#,
        )
        .unwrap();
        state::update_state(|s| s.dock_right = true);
        let text = settings_text(&dir);
        assert!(text.contains("\"my_typo\": 42"), "{text}");
        // File order, not the sorted key list: unknown keys come after ours.
        let my_typo = text.find("\"my_typo\"").unwrap();
        let show_hidden = text.find("\"show_hidden\"").unwrap();
        assert!(
            my_typo > show_hidden,
            "unknown keys come after ours: {text}"
        );
        assert_eq!(load_state().color_theme, ColorTheme::Light);
    }

    #[test]
    fn a_fresh_install_lists_every_key_with_its_default() {
        let dir = isolated();
        state::migrate_settings();
        let text = settings_text(&dir);
        let value: Value = serde_json::from_str(&text).unwrap();
        for key in USER_KEYS {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert_eq!(value["icons"], Value::Null);
        assert_eq!(value["terminal_height"], Value::Null);
        assert_eq!(value["colors"], "vscode");
        assert_eq!(state::load_state(), State::default());
    }

    #[test]
    fn the_written_order_is_the_canonical_order() {
        let dir = isolated();
        state::update_state(|_| {});
        let order: Vec<String> = {
            let text = settings_text(&dir);
            text.lines()
                .filter_map(|line| line.strip_prefix("  \""))
                .filter_map(|rest| rest.split('"').next())
                .map(str::to_string)
                .collect()
        };
        assert_eq!(order, USER_KEYS, "one key per line, canonical order");
    }

    /// Migrate, break settings.json, then start a "fresh process" (every cache
    /// cleared): the migrated values must still apply, from the last-good copy.
    #[test]
    fn a_broken_settings_file_keeps_the_migrated_values_in_a_fresh_process() {
        let dir = isolated();
        std::fs::write(dir.join("state.json"), LEGACY).unwrap();
        state::migrate_settings();
        assert!(dir.join("settings.last-good.json").exists());

        // A dropped comma: invalid, and no in-process parse survives the reset.
        std::fs::write(
            dir.join("settings.json"),
            r#"{"merged": true "dock_right": true}"#,
        )
        .unwrap();
        reset_caches();

        let loaded = load_state();
        assert_eq!(loaded.sidebar_width, 44, "not the default 32");
        assert!(loaded.dock_right, "not the default false");
        assert_eq!(loaded.color_theme, ColorTheme::Terminal);
        assert!(!loaded.show_hidden);
        assert_eq!(loaded.terminal_height, Some(22));
    }

    /// A valid file refreshes the last-good copy, and it is canonical text.
    #[test]
    fn the_last_good_copy_follows_every_valid_file() {
        let dir = isolated();
        std::fs::write(dir.join("settings.json"), r#"{"colors":"light"}"#).unwrap();
        assert_eq!(load_state().color_theme, ColorTheme::Light);
        let saved = std::fs::read_to_string(dir.join("settings.last-good.json")).unwrap();
        // The copy holds exactly the keys the valid file had, in canonical form.
        assert_eq!(saved, "{\n  \"colors\": \"light\"\n}\n");
    }

    #[test]
    fn toasts_follow_validity_transitions() {
        let bad = SettingsSnapshot {
            present: true,
            error: Some("line 1, column 2: x".into()),
            ..SettingsSnapshot::default()
        };
        assert_eq!(
            toast_for(false, &bad),
            Some((
                "settings.json not applied",
                "line 1, column 2: x".to_string()
            ))
        );
        let fixed = SettingsSnapshot {
            present: true,
            warnings: vec!["unknown setting `x`".into()],
            ..SettingsSnapshot::default()
        };
        assert_eq!(
            toast_for(true, &fixed),
            Some(("settings.json applied", "unknown setting `x`".to_string()))
        );
        assert_eq!(
            toast_for(false, &fixed),
            None,
            "an unchanged valid file is silent"
        );
    }
}
