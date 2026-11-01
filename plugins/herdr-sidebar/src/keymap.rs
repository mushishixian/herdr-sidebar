//! `keymap.json`: the user's key bindings, for the sidebar's own keys and for
//! herdr's global chords (see AGENTS.md, "keymap.json").
//!
//! - [`COMMANDS`] is the registry of sidebar commands and their default keys.
//!   Both apps' `on_key` call [`translate`] once, before dispatch, so a remapped
//!   key reaches the unchanged dispatch code as the command's default key.
//! - Global entries are rendered into a marked block at the end of herdr's
//!   config.toml ([`render_block`]). [`apply_to`] writes that file only after
//!   `herdr config check` accepts the candidate, then reloads herdr. Only the
//!   session reporter applies it, on a worker thread (see [`poll_keymap_file`]).
//! - Validation, the applied cache and the last-good copy mirror settings.json
//!   (see [`crate::settings_file`]).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::settings_file::{FileKey, file_key, json_error_message};
use crate::state::{self, View};

// ---------------------------------------------------------------------------
// The command registry: what each in-pane key does, and where.
// ---------------------------------------------------------------------------

/// The views a command's default key works in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Explorer,
    SourceControl,
    Both,
}

impl Scope {
    fn applies(self, view: View) -> bool {
        match self {
            Scope::Both => true,
            Scope::Explorer => view == View::Explorer,
            Scope::SourceControl => view == View::SourceControl,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Scope::Explorer => "explorer",
            Scope::SourceControl => "source-control",
            Scope::Both => "both",
        }
    }
}

/// One sidebar command. `keys[0]` is the key a remapped binding is translated
/// to; the rest are the other defaults (a removal may name any of them).
pub struct Command {
    pub id: &'static str,
    pub keys: &'static [&'static str],
    pub scope: Scope,
    pub description: &'static str,
}

/// Every sidebar command, with the defaults read from `on_key` in
/// `explorer_app.rs` and `scm_app.rs`.
pub const COMMANDS: &[Command] = &[
    Command {
        id: "view.explorer",
        keys: &["1"],
        scope: Scope::Both,
        description: "Switch to Explorer",
    },
    Command {
        id: "view.search",
        keys: &["2"],
        scope: Scope::Both,
        description: "Switch to Search (find in files)",
    },
    Command {
        id: "view.sourceControl",
        keys: &["3"],
        scope: Scope::Both,
        description: "Switch to Source Control",
    },
    Command {
        id: "sidebar.settings",
        keys: &["s"],
        scope: Scope::Both,
        description: "Open the Settings page",
    },
    Command {
        id: "sidebar.hide",
        keys: &["b"],
        scope: Scope::Both,
        description: "Hide the sidebar (prefix+b brings it back)",
    },
    Command {
        id: "sidebar.quit",
        keys: &["q"],
        scope: Scope::Both,
        description: "Close this sidebar pane",
    },
    Command {
        id: "sidebar.menu",
        keys: &["m"],
        scope: Scope::Both,
        description: "Open the context menu for the selection",
    },
    Command {
        id: "sidebar.refresh",
        keys: &["r"],
        scope: Scope::Both,
        description: "Refresh the view",
    },
    Command {
        id: "explorer.quickOpen",
        keys: &["ctrl+p", "f12"],
        scope: Scope::Both,
        description: "Quick Open a file (Source Control hands it to Explorer)",
    },
    Command {
        id: "explorer.find",
        keys: &["ctrl+f"],
        scope: Scope::Explorer,
        description: "Find in files, focused",
    },
    Command {
        id: "explorer.toggleHidden",
        keys: &["."],
        scope: Scope::Explorer,
        description: "Show or hide dotfiles",
    },
    Command {
        id: "explorer.toggleIcons",
        keys: &["i"],
        scope: Scope::Both,
        description: "Toggle the icon theme",
    },
    Command {
        id: "explorer.changeFolder",
        keys: &["c"],
        scope: Scope::Explorer,
        description: "Change the root folder",
    },
    Command {
        id: "scm.toggleTreeView",
        keys: &["t"],
        scope: Scope::SourceControl,
        description: "Toggle list / tree view",
    },
    Command {
        id: "scm.stageAll",
        keys: &["a"],
        scope: Scope::SourceControl,
        description: "Stage all changes",
    },
    Command {
        id: "scm.unstageAll",
        keys: &["u"],
        scope: Scope::SourceControl,
        description: "Unstage all changes",
    },
    Command {
        id: "scm.undoCommit",
        keys: &["U"],
        scope: Scope::SourceControl,
        description: "Undo the last commit (soft), after a confirm",
    },
    Command {
        id: "scm.focusMessage",
        keys: &["c"],
        scope: Scope::SourceControl,
        description: "Focus the commit message box",
    },
    Command {
        id: "scm.suggestMessage",
        keys: &["A"],
        scope: Scope::SourceControl,
        description: "Suggest a commit message (AI)",
    },
    Command {
        id: "scm.open",
        keys: &["o"],
        scope: Scope::SourceControl,
        description: "Open the selected entry",
    },
    Command {
        id: "scm.sync",
        keys: &["S"],
        scope: Scope::SourceControl,
        description: "Sync changes (pull, then push)",
    },
    Command {
        id: "scm.find",
        keys: &["ctrl+f"],
        scope: Scope::SourceControl,
        description: "Find in files, focused",
    },
];

/// The registry entry for `id`.
pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

fn command_in_view(id: &str, view: View) -> bool {
    command(id).is_some_and(|command| command.scope.applies(view))
}

// ---------------------------------------------------------------------------
// Key syntax: `[mod+]*key`, shared by sidebar bindings and the removals.
// ---------------------------------------------------------------------------

/// A key as the apps see it: the code plus the modifiers that matter. Shift is
/// dropped from characters (an uppercase letter already means shift), so `S`
/// written in the file matches the `S` a terminal delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Chord {
    fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods;
        if matches!(code, KeyCode::Char(_) | KeyCode::BackTab) {
            mods.remove(KeyModifiers::SHIFT);
        }
        Self { code, mods }
    }

    /// The chord a pressed key is.
    pub fn of(key: KeyEvent) -> Self {
        Self::new(key.code, key.modifiers)
    }

    fn to_event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.mods)
    }

    /// Ctrl without Alt, or an F-key: the keys that never type text. Alt+letter
    /// and every plain key can be text (AltGr arrives as Ctrl+Alt on Windows).
    fn is_text_safe(self) -> bool {
        matches!(self.code, KeyCode::F(_))
            || (self.mods.contains(KeyModifiers::CONTROL) && !self.mods.contains(KeyModifiers::ALT))
    }
}

/// Parse a key as written in `keymap.json` (see the module docs).
pub fn parse_chord(text: &str) -> Result<Chord, String> {
    let bad = |why: &str| {
        format!(
            "`{text}` is not a key: {why}. Write one character (s, S, .), or a name (enter, esc, \
             tab, space, backspace, delete, up, down, left, right, home, end, pageup, pagedown, \
             f1-f12), with ctrl+, alt+ or shift+ in front"
        )
    };
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return Err(bad("it is empty or contains a space"));
    }
    let (prefix, name) = if text == "+" {
        ("", "+")
    } else if let Some(prefix) = text.strip_suffix("++") {
        (prefix, "+")
    } else {
        text.rsplit_once('+').unwrap_or(("", text))
    };
    if name.is_empty() {
        return Err(bad("the key name is missing"));
    }
    let mut mods = KeyModifiers::NONE;
    if !prefix.is_empty() {
        for part in prefix.split('+') {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "" => return Err(bad("a modifier is empty")),
                other => return Err(bad(&format!("`{other}` is not a modifier"))),
            };
            if mods.contains(flag) {
                return Err(bad(&format!("`{part}` is given twice")));
            }
            mods.insert(flag);
        }
    } else if text.contains('+') && text != "+" {
        return Err(bad("a modifier is empty"));
    }
    let mut code = key_code(name).ok_or_else(|| bad(&format!("`{name}` is not a key name")))?;
    if mods.contains(KeyModifiers::SHIFT) {
        match code {
            KeyCode::Tab => {
                code = KeyCode::BackTab;
                mods.remove(KeyModifiers::SHIFT);
            }
            KeyCode::Char(c) if c.is_ascii_lowercase() => {
                code = KeyCode::Char(c.to_ascii_uppercase());
            }
            _ => {}
        }
    }
    Ok(Chord::new(code, mods))
}

fn key_code(name: &str) -> Option<KeyCode> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyCode::Char(c));
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        other => {
            let number = other.strip_prefix('f')?.parse::<u8>().ok()?;
            if (1..=12).contains(&number) {
                KeyCode::F(number)
            } else {
                return None;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// The parsed keymap.json.
// ---------------------------------------------------------------------------

/// A user binding of a sidebar command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub chord: Chord,
    pub command: &'static str,
}

/// A `-command` entry: the command's default `chord` is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removal {
    pub chord: Chord,
    pub command: &'static str,
}

/// A `global: true` entry, rendered into herdr's config.toml. `index` is its
/// position in keymap.json, so a herdr diagnostic can name the entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalBinding {
    pub index: usize,
    pub key: String,
    pub command: String,
}

/// The effective user keymap.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keymap {
    pub bindings: Vec<Binding>,
    pub removals: Vec<Removal>,
    pub global: Vec<GlobalBinding>,
}

/// A keymap.json that validated, with its warnings and its canonical text
/// (what the last-good copy holds).
#[derive(Debug)]
pub struct Parsed {
    pub keymap: Keymap,
    pub warnings: Vec<String>,
    pub canonical: String,
}

/// Validate keymap.json text. `Err` names the entry (`keymap.json[2]`) or the
/// line and column of a syntax error. An empty file is an empty keymap.
pub fn validate(text: &str) -> Result<Vec<String>, String> {
    check_keymap(text).map(|parsed| parsed.warnings)
}

/// [`validate`] that also returns the parsed keymap.
pub fn check_keymap(text: &str) -> Result<Parsed, String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Parsed {
            keymap: Keymap::default(),
            warnings: Vec::new(),
            canonical: "[]\n".into(),
        });
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("keymap.json: {}", json_error_message(&error)))?;
    let Value::Array(items) = &value else {
        return Err("keymap.json must hold one JSON array: [ … ]".into());
    };
    let mut keymap = Keymap::default();
    let mut warnings = Vec::new();
    for (index, item) in items.iter().enumerate() {
        parse_entry(index, item, &mut keymap, &mut warnings)?;
    }
    let canonical = format!(
        "{}\n",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".into())
    );
    Ok(Parsed {
        keymap,
        warnings,
        canonical,
    })
}

fn parse_entry(
    index: usize,
    item: &Value,
    keymap: &mut Keymap,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let at = |message: String| format!("keymap.json[{index}]: {message}");
    let Value::Object(fields) = item else {
        return Err(at(
            "each entry is an object: { \"key\": …, \"command\": … }".into(),
        ));
    };
    for name in fields.keys() {
        if !matches!(name.as_str(), "key" | "command" | "global") {
            warnings.push(at(format!("unknown field `{name}`")));
        }
    }
    let key = fields
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| at("`key` must be a string".into()))?;
    let command = fields
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| at("`command` must be a string".into()))?;
    let global = match fields.get("global") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(at("`global` must be true or false".into())),
    };

    if global {
        if command.starts_with('-') {
            return Err(at(
                "global bindings can only be added; remove a herdr chord in config.toml".into(),
            ));
        }
        check_global_key(key).map_err(&at)?;
        if !is_plugin_action(command) {
            return Err(at(format!(
                "`{command}` is not a herdr plugin action; it must look like <plugin-id>.<action-id>"
            )));
        }
        keymap.global.push(GlobalBinding {
            index,
            key: key.to_string(),
            command: command.to_string(),
        });
        return Ok(());
    }

    if let Some(id) = command.strip_prefix('-') {
        let entry = command_or_error(id).map_err(&at)?;
        let chord = parse_chord(key).map_err(&at)?;
        let is_default = entry
            .keys
            .iter()
            .any(|default| parse_chord(default) == Ok(chord));
        if is_default {
            keymap.removals.push(Removal {
                chord,
                command: entry.id,
            });
        } else {
            warnings.push(at(format!(
                "`-{}` has no default `{key}`, so there is nothing to remove",
                entry.id
            )));
        }
        return Ok(());
    }

    let entry = command_or_error(command).map_err(&at)?;
    let chord = parse_chord(key).map_err(&at)?;
    keymap.bindings.push(Binding {
        chord,
        command: entry.id,
    });
    Ok(())
}

fn command_or_error(id: &str) -> Result<&'static Command, String> {
    command(id).ok_or_else(|| {
        format!("unknown command `{id}`; keymap.defaults.json lists every sidebar command id")
    })
}

/// A global key goes to herdr verbatim, so only the characters that would break
/// the generated TOML (or the key itself) are refused here. herdr checks the rest.
fn check_global_key(key: &str) -> Result<(), String> {
    let bad = key.is_empty()
        || key
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'));
    if bad {
        return Err(format!(
            "`{key}` is not a herdr key such as \"ctrl+alt+o\" (no spaces, quotes or backslashes)"
        ));
    }
    Ok(())
}

/// `<plugin-id>.<action-id>`, the shape `[a-z0-9._-]+\.[a-z0-9_-]+`.
fn is_plugin_action(command: &str) -> bool {
    let Some((plugin, action)) = command.rsplit_once('.') else {
        return false;
    };
    let plugin_ok = !plugin.is_empty()
        && plugin
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    let action_ok = !action.is_empty()
        && action
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    plugin_ok && action_ok
}

// ---------------------------------------------------------------------------
// Translation: the one step at the top of each app's `on_key`.
// ---------------------------------------------------------------------------

/// What [`translate`] decided for one pressed key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Translated {
    /// Dispatch the key as pressed.
    Unchanged,
    /// Dispatch this key instead (a command's default key).
    Key(KeyEvent),
    /// The user removed this key's default, so it does nothing.
    Swallow,
}

/// Translate one pressed key for `view`, using the applied keymap.
/// `in_text` is true while an overlay is open or a text field has focus; then
/// only Ctrl and F-keys are translated, so typing is never hijacked.
pub fn translate(view: View, key: KeyEvent, in_text: bool) -> Translated {
    translate_with(&current(), view, key, in_text)
}

/// [`translate`] over an explicit keymap.
pub fn translate_with(keymap: &Keymap, view: View, key: KeyEvent, in_text: bool) -> Translated {
    let pressed = Chord::of(key);
    if in_text && !pressed.is_text_safe() {
        return Translated::Unchanged;
    }
    if let Some(binding) = keymap
        .bindings
        .iter()
        .find(|binding| binding.chord == pressed && command_in_view(binding.command, view))
    {
        let target = command(binding.command)
            .and_then(|command| command.keys.first())
            .and_then(|key| parse_chord(key).ok());
        return match target {
            // A plain-key target would type into the field, so leave the key be.
            Some(target) if in_text && !target.is_text_safe() => Translated::Unchanged,
            Some(target) => Translated::Key(target.to_event()),
            None => Translated::Unchanged,
        };
    }
    if keymap
        .removals
        .iter()
        .any(|removal| removal.chord == pressed && command_in_view(removal.command, view))
    {
        return Translated::Swallow;
    }
    Translated::Unchanged
}

// ---------------------------------------------------------------------------
// The applied keymap, cached by file identity.
// ---------------------------------------------------------------------------

/// What this process currently knows about keymap.json.
#[derive(Clone, Default)]
pub struct KeymapSnapshot {
    /// The file exists.
    pub present: bool,
    /// The last VALID keymap in this process (or the last-good copy).
    pub good: Option<Arc<Keymap>>,
    /// The validation error of the file as it is now, if it is invalid.
    pub error: Option<String>,
    /// Warnings of the current valid file.
    pub warnings: Vec<String>,
}

#[derive(Clone, Default)]
struct Cached {
    key: Option<FileKey>,
    good: Option<Arc<Keymap>>,
    error: Option<String>,
    warnings: Vec<String>,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `<state dir>/keymap.json`.
pub fn keymap_path() -> Option<PathBuf> {
    Some(state::state_dir()?.join("keymap.json"))
}

/// `<state dir>/keymap.last-good.json`: a copy of the last valid keymap.json.
/// Read only while keymap.json is invalid and this process has no good parse.
pub fn last_good_path() -> Option<PathBuf> {
    Some(state::state_dir()?.join("keymap.last-good.json"))
}

/// The current keymap.json as this process may apply it. Re-validates only when
/// (mtime, len) changed, so [`translate`] can call it on every key press.
pub fn keymap_snapshot() -> KeymapSnapshot {
    let Some(path) = keymap_path() else {
        return KeymapSnapshot::default();
    };
    let Some(key) = file_key(&path) else {
        // No file: no user bindings. A deleted file forgets its last good parse.
        lock(cache()).remove(&path);
        return KeymapSnapshot::default();
    };
    if let Some(cached) = lock(cache()).get(&path).filter(|c| c.key == Some(key)) {
        return cached.snapshot();
    }
    let checked = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read keymap.json: {error}"))
        .and_then(|text| check_keymap(&text));
    let mut map = lock(cache());
    let entry = map.entry(path).or_default();
    entry.key = Some(key);
    match checked {
        Ok(parsed) => {
            store_last_good(&parsed.canonical);
            entry.good = Some(Arc::new(parsed.keymap));
            entry.error = None;
            entry.warnings = parsed.warnings;
        }
        Err(error) => {
            // A broken file keeps the bindings this process last applied, or,
            // in a fresh process, the copy saved when the file last validated.
            if entry.good.is_none() {
                entry.good = last_good_keymap();
            }
            entry.error = Some(error);
            entry.warnings.clear();
        }
    }
    entry.snapshot()
}

impl Cached {
    fn snapshot(&self) -> KeymapSnapshot {
        KeymapSnapshot {
            present: true,
            good: self.good.clone(),
            error: self.error.clone(),
            warnings: self.warnings.clone(),
        }
    }
}

fn store_last_good(text: &str) {
    let Some(path) = last_good_path() else {
        return;
    };
    if std::fs::read_to_string(&path).is_ok_and(|stored| stored == text) {
        return;
    }
    let _ = state::write_atomic(&path, text);
}

fn last_good_keymap() -> Option<Arc<Keymap>> {
    let path = last_good_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    check_keymap(&text)
        .ok()
        .map(|parsed| Arc::new(parsed.keymap))
}

/// The keymap the apps translate with: the file's valid parse, else the last
/// good one. No file means no user bindings.
pub fn current() -> Arc<Keymap> {
    let snapshot = keymap_snapshot();
    if !snapshot.present {
        return Arc::new(Keymap::default());
    }
    snapshot.good.unwrap_or_else(|| Arc::new(Keymap::default()))
}

/// The validation error of keymap.json, or `None` when it is missing or valid.
/// The Settings page shows it.
pub fn keymap_error() -> Option<String> {
    keymap_snapshot().error
}

// ---------------------------------------------------------------------------
// Files written at startup: keymap.json itself and the command reference.
// ---------------------------------------------------------------------------

/// The read-only reference of every sidebar command, as JSON.
pub fn defaults_text() -> String {
    let entries: Vec<Value> = COMMANDS
        .iter()
        .map(|command| {
            json!({
                "command": command.id,
                "keys": command.keys,
                "views": command.scope.label(),
                "description": command.description,
            })
        })
        .collect();
    let mut text =
        serde_json::to_string_pretty(&Value::Array(entries)).unwrap_or_else(|_| "[]".into());
    text.push('\n');
    text
}

/// Called once per sidebar start, next to the settings migration: create
/// keymap.json (`[]`) when it is missing, and (re)write keymap.defaults.json
/// when its content would change. Best effort.
pub fn ensure_files() {
    let Some(dir) = state::state_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("keymap.json");
    if !path.exists() {
        let _ = state::write_atomic(&path, "[]\n");
    }
    write_defaults(&dir.join("keymap.defaults.json"));
}

// The read-only flag is cleared only for the rewrite (Windows refuses to replace
// a read-only file) and set again right after; the window is one write long.
#[allow(clippy::permissions_set_readonly_false)]
fn write_defaults(path: &Path) {
    let text = defaults_text();
    if std::fs::read_to_string(path).is_ok_and(|stored| stored == text) {
        return;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let mut permissions = meta.permissions();
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(path, permissions);
    }
    if std::fs::write(path, &text).is_ok()
        && let Ok(meta) = std::fs::metadata(path)
    {
        let mut permissions = meta.permissions();
        permissions.set_readonly(true);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

// ---------------------------------------------------------------------------
// The generated block in herdr's config.toml.
// ---------------------------------------------------------------------------

pub const BLOCK_BEGIN: &str =
    "# >>> herdr-sidebar keymap.json (generated; edit keymap.json, not this block) >>>";
pub const BLOCK_END: &str = "# <<< herdr-sidebar keymap.json <<<";

/// The block for `globals`, or an empty string when there are none (which
/// removes the block).
pub fn render_block(globals: &[GlobalBinding]) -> String {
    if globals.is_empty() {
        return String::new();
    }
    let mut out = format!("{BLOCK_BEGIN}\n");
    for global in globals {
        out.push_str("[[keys.command]]\n");
        out.push_str(&format!("key = {}\n", toml_string(&global.key)));
        out.push_str("type = \"plugin_action\"\n");
        out.push_str(&format!("command = {}\n", toml_string(&global.command)));
        out.push_str("description = \"keymap.json\"\n\n");
    }
    out.push_str(BLOCK_END);
    out.push('\n');
    out
}

/// A TOML basic string.
fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `config` with its generated block replaced by `block`. An empty `block`
/// removes the block; with no block and nothing to add, `config` is returned
/// unchanged. A block whose only one marker is present is an error, because
/// replacing it would guess at what the user kept.
pub fn apply_block(config: &str, block: &str) -> Result<String, String> {
    let lines: Vec<&str> = config.split_inclusive('\n').collect();
    let marker = |line: &str, wanted: &str| line.trim_end_matches(['\r', '\n']) == wanted;
    let begin = lines.iter().position(|line| marker(line, BLOCK_BEGIN));
    let end = lines.iter().position(|line| marker(line, BLOCK_END));
    match (begin, end) {
        (Some(begin), Some(end)) if end >= begin => {
            let mut from = begin;
            // Removing the block also takes the blank line that separated it.
            if block.is_empty() && from > 0 && lines[from - 1].trim().is_empty() {
                from -= 1;
            }
            let mut out = String::new();
            out.extend(lines[..from].iter().copied());
            out.push_str(block);
            out.extend(lines[end + 1..].iter().copied());
            Ok(out)
        }
        (None, None) => {
            if block.is_empty() {
                return Ok(config.to_string());
            }
            let mut out = config.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(block);
            Ok(out)
        }
        _ => Err(
            "the keymap.json block in config.toml is incomplete (one marker is missing); \
             restore both marker lines or delete the block by hand"
                .into(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Checking the candidate with herdr before anything is written.
// ---------------------------------------------------------------------------

/// The herdr operations the apply needs, behind a trait so tests never run the
/// real binary.
pub trait Herdr {
    /// `herdr config check` on `config`: whether it passed, and its output.
    fn check(&self, config: &Path) -> Result<(bool, String), String>;
    /// `herdr server reload-config`.
    fn reload(&self) -> Result<(), String>;
}

/// The real herdr CLI: `HERDR_BIN_PATH` when set, else `herdr` on PATH.
pub struct Cli;

impl Herdr for Cli {
    fn check(&self, config: &Path) -> Result<(bool, String), String> {
        run_herdr(&["config", "check"], Some(config))
    }

    fn reload(&self) -> Result<(), String> {
        match run_herdr(&["server", "reload-config"], None)? {
            (true, _) => Ok(()),
            (false, text) => Err(text.trim().to_string()),
        }
    }
}

const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

fn run_herdr(args: &[&str], config: Option<&Path>) -> Result<(bool, String), String> {
    let program = std::env::var_os("HERDR_BIN_PATH")
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "herdr".into());
    let mut command = process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(config) = config {
        command.env("HERDR_CONFIG_PATH", config);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run herdr: {error}"))?;
    let stdout = child.stdout.take().map(read_all);
    let stderr = child.stderr.take().map(read_all);
    let deadline = Instant::now() + HERDR_TIMEOUT;
    let status: process::ExitStatus = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "herdr did not answer within {} s",
                    HERDR_TIMEOUT.as_secs()
                ));
            }
            Err(error) => return Err(format!("herdr: {error}")),
        }
    };
    let mut text = collected(stdout);
    text.push_str(&collected(stderr));
    Ok((status.success(), text))
}

/// Drain a pipe on its own thread, so a chatty child cannot block on a full pipe.
fn read_all<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    rx
}

fn collected(pipe: Option<mpsc::Receiver<String>>) -> String {
    pipe.and_then(|rx| rx.recv_timeout(Duration::from_millis(500)).ok())
        .unwrap_or_default()
}

/// The diagnostic lines of `candidate` that `current` does not have. Status
/// headers (`config: ok`) are not diagnostics.
fn new_diagnostics(current: &str, candidate: &str) -> Vec<String> {
    let diagnostic = |line: &str| {
        let line = line.trim();
        (!line.is_empty() && !line.starts_with("config:")).then(|| line.to_string())
    };
    let before: HashSet<String> = current.lines().filter_map(diagnostic).collect();
    let mut seen = HashSet::new();
    candidate
        .lines()
        .filter_map(diagnostic)
        .filter(|line| !before.contains(line) && seen.insert(line.clone()))
        .collect()
}

/// herdr's index of the first generated `[[keys.command]]` table: the number
/// of table headers before the block's begin marker.
fn first_generated_entry(config: &str) -> usize {
    match config
        .lines()
        .position(|line| line.trim_end() == BLOCK_BEGIN)
    {
        Some(begin) => config
            .lines()
            .take(begin)
            .filter(|line| line.trim() == "[[keys.command]]")
            .count(),
        None => 0,
    }
}

/// Rewrite a herdr diagnostic that points into the generated block so it names
/// the keymap.json entry instead (`keys.command[N]` → `keymap.json[i]`).
fn map_diagnostic(line: &str, first: usize, globals: &[GlobalBinding]) -> String {
    let mut text = line.to_string();
    let mut named = None;
    for (position, global) in globals.iter().enumerate() {
        let needle = format!("keys.command[{}]", first + position);
        if text.contains(&needle) {
            text = text.replace(&needle, &format!("keymap.json[{}]", global.index));
            named.get_or_insert(global.index);
        }
    }
    match named {
        Some(index) => format!("keymap.json[{index}]: {text}"),
        None => text,
    }
}

/// What an apply did.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// config.toml already holds what keymap.json implies.
    Unchanged,
    /// The block was written and herdr reloaded.
    Written,
}

/// Why an apply did not finish.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// herdr rejects the candidate because of keymap.json; the lines name the
    /// entries. Nothing was written.
    Rejected(Vec<String>),
    /// Anything else (herdr missing, timeout, I/O). Nothing was written, unless
    /// the message says the file was written and only the reload failed.
    Failed(String),
}

/// Bring the generated block of `config` in line with `keymap`.
///
/// The candidate is written to a temp file beside the TARGET (a symlink is
/// followed, so the link itself is never replaced), checked with
/// `herdr config check`, and compared with the check of the current file: only
/// diagnostics the candidate adds count. A clean candidate is renamed over the
/// target and herdr is reloaded.
pub fn apply_to(
    config: &Path,
    herdr: &dyn Herdr,
    keymap: &Keymap,
) -> Result<ApplyOutcome, ApplyError> {
    let is_link = std::fs::symlink_metadata(config).is_ok_and(|meta| meta.file_type().is_symlink());
    let target = if is_link {
        std::fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf())
    } else {
        config.to_path_buf()
    };
    let existed = target.exists();
    let current = if existed {
        std::fs::read_to_string(&target).map_err(|error| {
            ApplyError::Failed(format!("cannot read {}: {error}", target.display()))
        })?
    } else {
        String::new()
    };
    let block = render_block(&keymap.global);
    let candidate = apply_block(&current, &block).map_err(ApplyError::Failed)?;
    if candidate == current {
        return Ok(ApplyOutcome::Unchanged);
    }

    let (current_ok, current_out) = if existed {
        herdr.check(&target).map_err(ApplyError::Failed)?
    } else {
        (true, String::new())
    };
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let tmp = dir.join(format!(".{name}.keymap-{}.tmp", std::process::id()));
    std::fs::write(&tmp, &candidate)
        .map_err(|error| ApplyError::Failed(format!("cannot write {}: {error}", tmp.display())))?;
    let checked = herdr.check(&tmp);
    let (candidate_ok, candidate_out) = match checked {
        Ok(result) => result,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(ApplyError::Failed(error));
        }
    };

    let new_lines = new_diagnostics(&current_out, &candidate_out);
    let first = first_generated_entry(&candidate);
    let problems: Vec<String> = if new_lines.is_empty() {
        if current_ok && !candidate_ok {
            vec![format!(
                "herdr config check failed: {}",
                candidate_out.trim()
            )]
        } else {
            Vec::new()
        }
    } else {
        new_lines
            .iter()
            .map(|line| map_diagnostic(line, first, &keymap.global))
            .collect()
    };
    if !problems.is_empty() {
        let _ = std::fs::remove_file(&tmp);
        return Err(ApplyError::Rejected(problems));
    }
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ApplyError::Failed(format!(
            "cannot replace {}: {error}",
            target.display()
        )));
    }
    herdr.reload().map_err(|error| {
        ApplyError::Failed(format!(
            "keymap.json was written, but herdr did not reload: {error}"
        ))
    })?;
    Ok(ApplyOutcome::Written)
}

// ---------------------------------------------------------------------------
// Telling the user, and applying from the elected session reporter.
// ---------------------------------------------------------------------------

/// What this process has seen of keymap.json.
#[derive(Default)]
struct Watch {
    /// The first poll has run.
    seen: bool,
    last: Option<FileKey>,
    /// The last content this process observed was invalid.
    invalid: bool,
    /// The generated block should be brought in line with the keymap.
    pending: bool,
    /// While pending and not elected, do not re-check the election before this.
    next_election: Option<Instant>,
}

/// How often a non-elected sidebar re-checks the election while it has work.
const ELECTION_RETRY: Duration = Duration::from_secs(2);

fn watches() -> &'static Mutex<HashMap<PathBuf, Watch>> {
    static WATCHES: OnceLock<Mutex<HashMap<PathBuf, Watch>>> = OnceLock::new();
    WATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

type ApplyResult = Result<ApplyOutcome, ApplyError>;

/// The one apply in flight, if any.
fn apply_slot() -> &'static Mutex<Option<mpsc::Receiver<ApplyResult>>> {
    static SLOT: OnceLock<Mutex<Option<mpsc::Receiver<ApplyResult>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// The toast a changed keymap.json earns, if any (same rules as settings.json).
fn toast(was_invalid: bool, snapshot: &KeymapSnapshot) -> Option<(&'static str, String)> {
    match &snapshot.error {
        Some(error) => Some(("keymap.json not applied", error.clone())),
        None if was_invalid => Some(("keymap.json applied", snapshot.warnings.join("; "))),
        None => None,
    }
}

fn set_pending(path: &Path, pending: bool, next_election: Option<Instant>) {
    let mut watches = lock(watches());
    let watch = watches.entry(path.to_path_buf()).or_default();
    watch.pending = pending;
    watch.next_election = next_election;
}

/// Event-loop hook, called on every turn of both sidebar loops. One `metadata()`
/// call; a changed file is validated and toasted (by the session reporter only),
/// and a valid keymap queues an apply of the generated herdr block. The first
/// poll queues one too, so a block that drifted while no sidebar ran is fixed.
pub fn poll_keymap_file() {
    collect_apply();
    let Some(path) = keymap_path() else {
        return;
    };
    let key = file_key(&path);
    let changed = {
        let mut watches = lock(watches());
        let watch = watches.entry(path.clone()).or_default();
        if watch.seen && watch.last == key {
            false
        } else {
            watch.seen = true;
            watch.last = key;
            true
        }
    };
    if changed {
        let snapshot = keymap_snapshot();
        let was_invalid = {
            let mut watches = lock(watches());
            let watch = watches.entry(path.clone()).or_default();
            if snapshot.error.is_none() {
                watch.pending = true;
                watch.next_election = None;
            }
            std::mem::replace(&mut watch.invalid, snapshot.error.is_some())
        };
        if let Some((title, body)) = toast(was_invalid, &snapshot)
            && crate::session_reporter::is_session_reporter()
        {
            crate::git_action::notify(title, &body);
        }
    }

    let due = {
        let watches = lock(watches());
        watches.get(&path).is_some_and(|watch| {
            watch.pending && watch.next_election.is_none_or(|at| Instant::now() >= at)
        })
    };
    if !due {
        return;
    }
    if !crate::session_reporter::is_session_reporter() {
        set_pending(&path, true, Some(Instant::now() + ELECTION_RETRY));
        return;
    }
    if lock(apply_slot()).is_some() {
        return;
    }
    let snapshot = keymap_snapshot();
    // A missing file means no global bindings, so its block is removed.
    let keymap = if !snapshot.present {
        Some(Arc::new(Keymap::default()))
    } else if snapshot.error.is_none() {
        snapshot.good.clone()
    } else {
        None
    };
    let config = crate::viewer::herdr_config_path();
    set_pending(&path, false, None);
    if let (Some(keymap), Some(config)) = (keymap, config) {
        spawn_apply(config, keymap);
    }
}

fn spawn_apply(config: PathBuf, keymap: Arc<Keymap>) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = apply_to(&config, &Cli, &keymap);
        let _ = tx.send(result);
    });
    *lock(apply_slot()) = Some(rx);
}

/// Deliver a finished apply, toasting a failure. Never blocks.
fn collect_apply() {
    let result = {
        let mut slot = lock(apply_slot());
        let Some(rx) = slot.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                ApplyResult::Err(ApplyError::Failed("the apply worker stopped".into()))
            }
        };
        *slot = None;
        result
    };
    match result {
        Ok(_) => {}
        Err(ApplyError::Rejected(lines)) => {
            crate::git_action::notify("keymap.json: herdr rejected it", &lines.join("; "));
        }
        Err(ApplyError::Failed(message)) => {
            crate::git_action::notify("keymap.json: could not update herdr", &message);
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the effective keys and editing keymap.json (the Keyboard shortcuts page).
// ---------------------------------------------------------------------------

/// The key as keymap.json writes it: the inverse of [`parse_chord`]. `None` for
/// a key the sidebar cannot bind (Insert, media keys, a bare modifier, …).
pub fn chord_to_string(chord: Chord) -> Option<String> {
    let name: String = match chord.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) if !c.is_control() && !c.is_whitespace() => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab | KeyCode::BackTab => "tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(n) if (1..=12).contains(&n) => format!("f{n}"),
        _ => return None,
    };
    let mut mods = chord.mods;
    if chord.code == KeyCode::BackTab {
        // Shift+Tab is the only way to name BackTab, and Chord::new drops the bit.
        mods.insert(KeyModifiers::SHIFT);
    }
    let mut out = String::new();
    for (flag, word) in [
        (KeyModifiers::CONTROL, "ctrl"),
        (KeyModifiers::ALT, "alt"),
        (KeyModifiers::SHIFT, "shift"),
    ] {
        if mods.contains(flag) {
            out.push_str(word);
            out.push('+');
        }
    }
    out.push_str(&name);
    Some(out)
}

/// The defaults of `id`, before any keymap.json entry.
pub fn default_keys(id: &str) -> Vec<Chord> {
    command(id)
        .map(|command| {
            command
                .keys
                .iter()
                .filter_map(|key| parse_chord(key).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The keys `id` answers to under `keymap`: its defaults minus the `-id`
/// removals, plus its bindings (a binding wins over a removal of the same key).
pub fn effective_keys(keymap: &Keymap, id: &str) -> Vec<Chord> {
    let Some(command) = command(id) else {
        return Vec::new();
    };
    let mut keys: Vec<Chord> = default_keys(command.id)
        .into_iter()
        .filter(|chord| {
            !keymap
                .removals
                .iter()
                .any(|removal| removal.command == command.id && removal.chord == *chord)
        })
        .collect();
    for binding in keymap
        .bindings
        .iter()
        .filter(|binding| binding.command == command.id)
    {
        if !keys.contains(&binding.chord) {
            keys.push(binding.chord);
        }
    }
    keys
}

/// Whether `id` answers to other keys than its defaults under `keymap`.
pub fn is_changed(keymap: &Keymap, id: &str) -> bool {
    let effective = effective_keys(keymap, id);
    let defaults = default_keys(id);
    effective.len() != defaults.len() || defaults.iter().any(|key| !effective.contains(key))
}

/// Two views overlap when they share at least one of them (`Both` meets all).
pub fn scopes_overlap(a: Scope, b: Scope) -> bool {
    a == Scope::Both || b == Scope::Both || a == b
}

/// The other sidebar commands that answer `chord` in a view where `id` also
/// works. These are the commands a new binding for `chord` would take it from.
pub fn conflicts(keymap: &Keymap, id: &str, chord: Chord) -> Vec<&'static Command> {
    let Some(me) = command(id) else {
        return Vec::new();
    };
    COMMANDS
        .iter()
        .filter(|other| {
            other.id != me.id
                && scopes_overlap(me.scope, other.scope)
                && effective_keys(keymap, other.id).contains(&chord)
        })
        .collect()
}

/// A change to one command's keymap.json entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// The command answers to this key only: its defaults are removed.
    Replace(Chord),
    /// The command also answers to this key.
    Add(Chord),
    /// The command's own entries are dropped, so it has its defaults again.
    Reset,
}

/// One keymap.json entry with its fields in file order. `serde_json::Value`
/// maps are sorted, so entries kept as `Value`s would come back alphabetised.
#[derive(Clone, Debug, PartialEq)]
pub struct RawEntry(Vec<(String, Value)>);

impl RawEntry {
    /// An entry of plain string fields, in the order given.
    fn new(fields: &[(&str, &str)]) -> Self {
        RawEntry(
            fields
                .iter()
                .map(|(name, text)| ((*name).to_string(), Value::String((*text).to_string())))
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    /// The fields in write order: `key`, `command`, `global` when present, then
    /// every other field in the order the file had it.
    fn ordered(&self) -> Vec<(&str, &Value)> {
        let mut out = Vec::new();
        for lead in ["key", "command", "global"] {
            if let Some(value) = self.get(lead) {
                out.push((lead, value));
            }
        }
        for (name, value) in &self.0 {
            if !matches!(name.as_str(), "key" | "command" | "global") {
                out.push((name.as_str(), value));
            }
        }
        out
    }
}

impl<'de> serde::Deserialize<'de> for RawEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = RawEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a keymap.json entry object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<RawEntry, A::Error> {
                let mut fields = Vec::new();
                while let Some((name, value)) = map.next_entry::<String, Value>()? {
                    fields.push((name, value));
                }
                Ok(RawEntry(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// The text of keymap.json for `entries`: two-space pretty, one field per line,
/// key/command/global first, and a trailing newline. `[]` when there are none.
fn render_entries(entries: &[RawEntry]) -> String {
    if entries.is_empty() {
        return "[]\n".into();
    }
    let mut out = String::from("[\n");
    for (index, entry) in entries.iter().enumerate() {
        out.push_str("  {\n");
        let fields = entry.ordered();
        for (position, (name, value)) in fields.iter().enumerate() {
            let value = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into());
            let value = value.replace('\n', "\n    ");
            let name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into());
            out.push_str(&format!("    {name}: {value}"));
            if position + 1 < fields.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  }");
        if index + 1 < entries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    out
}

/// The parsed `key` of a keymap.json entry, if it is one.
fn entry_chord(entry: &RawEntry) -> Option<Chord> {
    entry
        .get("key")
        .and_then(Value::as_str)
        .and_then(|text| parse_chord(text).ok())
}

/// The `command` of a sidebar (non-global) entry.
fn entry_command(entry: &RawEntry) -> Option<&str> {
    if entry.get("global") == Some(&Value::Bool(true)) {
        return None;
    }
    entry.get("command").and_then(Value::as_str)
}

fn text_of(chord: Chord) -> Result<String, String> {
    chord_to_string(chord).ok_or_else(|| "that key cannot be bound in keymap.json".to_string())
}

fn take_over(entries: &mut Vec<RawEntry>, other_id: &str, chord: Chord) -> Result<(), String> {
    let Some(other) = command(other_id) else {
        return Ok(());
    };
    entries.retain(|entry| {
        !(entry_command(entry) == Some(other.id) && entry_chord(entry) == Some(chord))
    });
    let removal = format!("-{}", other.id);
    let already_removed = entries.iter().any(|entry| {
        entry_command(entry) == Some(removal.as_str()) && entry_chord(entry) == Some(chord)
    });
    if default_keys(other.id).contains(&chord) && !already_removed {
        let text = text_of(chord)?;
        entries.push(RawEntry::new(&[("key", &text), ("command", &removal)]));
    }
    Ok(())
}

/// Apply `edit` to the parsed keymap.json array `entries`. `takeover` names the
/// commands that lose the key to `id` (from [`conflicts`]); their user binding
/// for it is removed, and a `-other` removal is added when it was their default.
/// Global entries and unknown fields are left as they are.
pub fn edit_entries(
    entries: &mut Vec<RawEntry>,
    id: &str,
    edit: Edit,
    takeover: &[&str],
) -> Result<(), String> {
    let Some(command) = command(id) else {
        return Err(format!("unknown command `{id}`"));
    };
    let id = command.id;
    let removal = format!("-{id}");
    match edit {
        Edit::Reset => entries.retain(|entry| {
            !matches!(entry_command(entry), Some(name) if name == id || name == removal)
        }),
        Edit::Add(chord) => {
            let text = text_of(chord)?;
            entries.retain(|entry| {
                !(entry_command(entry) == Some(removal.as_str())
                    && entry_chord(entry) == Some(chord))
            });
            let bound = entries.iter().any(|entry| {
                entry_command(entry) == Some(id) && entry_chord(entry) == Some(chord)
            });
            if !default_keys(id).contains(&chord) && !bound {
                entries.push(RawEntry::new(&[("key", &text), ("command", id)]));
            }
        }
        Edit::Replace(chord) => {
            let text = text_of(chord)?;
            for other in takeover {
                take_over(entries, other, chord)?;
            }
            entries.retain(|entry| {
                !matches!(entry_command(entry), Some(name) if name == id || name == removal)
            });
            if !default_keys(id).contains(&chord) {
                entries.push(RawEntry::new(&[("key", &text), ("command", id)]));
            }
            for default in default_keys(id) {
                if default != chord {
                    let removed = text_of(default)?;
                    entries.push(RawEntry::new(&[("key", &removed), ("command", &removal)]));
                }
            }
        }
    }
    Ok(())
}

/// What the Keyboard shortcuts page says when keymap.json does not validate.
pub const INVALID_EDIT: &str = "keymap.json has errors: fix it first (Edit keymap.json…)";

/// The parsed array of keymap.json text (`[]` when it is empty).
fn parse_entries(text: &str) -> Result<Vec<RawEntry>, String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<RawEntry>>(text).map_err(|_| INVALID_EDIT.to_string())
}

/// Change one command in `<state dir>/keymap.json`. Under the file's lock the
/// current text is re-read and must still validate, the edited array is written
/// atomically, and this process's cached copy is reloaded at once (other
/// sidebars see the new file through their mtime poll).
pub fn apply_edit(id: &str, edit: Edit, takeover: &[&str]) -> Result<(), String> {
    let path = keymap_path().ok_or_else(|| "keymap.json: no state directory".to_string())?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _lock = state::StateWriteLock::acquire(&path)
        .ok_or_else(|| "keymap.json: cannot lock the file".to_string())?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "[]".to_string(),
        Err(error) => return Err(format!("cannot read keymap.json: {error}")),
    };
    check_keymap(&text).map_err(|_| INVALID_EDIT.to_string())?;
    let mut entries = parse_entries(&text)?;
    edit_entries(&mut entries, id, edit, takeover)?;
    if !state::write_atomic(&path, &render_entries(&entries)) {
        return Err("keymap.json: cannot write the file".into());
    }
    lock(cache()).remove(&path);
    // Validate the new text now, so the last-good copy follows it as an external save would.
    let _ = keymap_snapshot();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers as M;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    /// A fresh state dir for this test thread (see [`state::TEST_STATE_DIR`]).
    fn isolated() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "hs-keymap-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch state dir");
        state::TEST_STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
        dir
    }

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn plain(c: char) -> KeyEvent {
        ev(KeyCode::Char(c), M::NONE)
    }

    fn chord(text: &str) -> Chord {
        parse_chord(text).unwrap_or_else(|error| panic!("{error}"))
    }

    fn map(text: &str) -> Keymap {
        check_keymap(text)
            .unwrap_or_else(|error| panic!("{error}"))
            .keymap
    }

    #[test]
    fn key_syntax_accepts_the_documented_forms() {
        assert_eq!(chord("s"), Chord::of(plain('s')));
        assert_eq!(chord("S"), Chord::of(plain('S')));
        assert_eq!(
            chord("shift+s"),
            chord("S"),
            "shift on a letter means the capital"
        );
        assert_eq!(chord("."), Chord::of(plain('.')));
        assert_eq!(
            chord("ctrl+o"),
            Chord::of(ev(KeyCode::Char('o'), M::CONTROL))
        );
        assert_eq!(
            chord("ctrl+alt+o"),
            Chord::of(ev(KeyCode::Char('o'), M::CONTROL | M::ALT))
        );
        assert_eq!(chord("shift+tab"), Chord::of(ev(KeyCode::BackTab, M::NONE)));
        assert_eq!(chord("f12"), Chord::of(ev(KeyCode::F(12), M::NONE)));
        assert_eq!(chord("Enter"), Chord::of(ev(KeyCode::Enter, M::NONE)));
        assert_eq!(chord("pagedown"), Chord::of(ev(KeyCode::PageDown, M::NONE)));
        assert_eq!(
            chord("ctrl++"),
            Chord::of(ev(KeyCode::Char('+'), M::CONTROL))
        );
        assert_eq!(chord("+"), Chord::of(plain('+')));
    }

    #[test]
    fn key_syntax_rejects_bad_keys_and_says_why() {
        for bad in [
            "",
            "ctrl+",
            "hyper+s",
            "ctrl+ctrl+s",
            "ctrl+zz",
            "f13",
            "+s",
            "ctrl s",
            "\"s\"",
        ] {
            assert!(parse_chord(bad).is_err(), "`{bad}` should be refused");
        }
        let error = parse_chord("ctrl+zz").unwrap_err();
        assert!(error.contains("`zz`"), "{error}");
    }

    #[test]
    fn every_default_key_parses_and_every_id_is_unique() {
        let mut ids = HashSet::new();
        for command in COMMANDS {
            assert!(ids.insert(command.id), "duplicate id {}", command.id);
            assert!(!command.keys.is_empty(), "{}", command.id);
            for key in command.keys {
                parse_chord(key).unwrap_or_else(|error| panic!("{}: {error}", command.id));
            }
        }
    }

    #[test]
    fn an_empty_file_or_an_empty_array_has_no_bindings() {
        for text in ["", "  \n", "[]", "\u{feff}[]"] {
            assert_eq!(map(text), Keymap::default(), "{text:?}");
        }
    }

    #[test]
    fn validation_names_the_entry_and_the_problem() {
        let error = validate(
            r#"[{"key":"s","command":"sidebar.settings"},{"key":"ctrl+zz","command":"sidebar.settings"}]"#,
        )
        .unwrap_err();
        assert!(error.starts_with("keymap.json[1]:"), "{error}");
        assert!(error.contains("`zz`"), "{error}");

        let error = validate(r#"[{"key":"s","command":"sidebar.nope"}]"#).unwrap_err();
        assert!(error.starts_with("keymap.json[0]:"), "{error}");
        assert!(error.contains("sidebar.nope"), "{error}");

        let error =
            validate(r#"[{"key":"s","command":"-sidebar.settings","global":true}]"#).unwrap_err();
        assert_eq!(
            error,
            "keymap.json[0]: global bindings can only be added; remove a herdr chord in config.toml"
        );

        let error =
            validate(r#"[{"key":"ctrl+o","command":"settings","global":true}]"#).unwrap_err();
        assert!(error.contains("not a herdr plugin action"), "{error}");

        assert!(
            validate(
                r#"[{"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true}]"#
            )
            .is_ok()
        );
        assert!(validate(r#"{"key":"s"}"#).unwrap_err().contains("array"));
        assert!(
            validate(r#"[{"command":"sidebar.settings"}]"#)
                .unwrap_err()
                .contains("`key` must be a string")
        );
        assert!(
            validate(r#"[{"key":"s","command":"sidebar.settings","global":"yes"}]"#)
                .unwrap_err()
                .contains("`global` must be true or false")
        );
        assert!(
            validate(r#"[{"key":"a b","command":"herdr-sidebar.x","global":true}]"#).is_err(),
            "a global key with a space is refused"
        );
    }

    #[test]
    fn syntax_errors_name_the_line_and_unknown_fields_are_warnings() {
        let error = validate("[\n  {\"key\" \"s\"}\n]").unwrap_err();
        assert!(error.contains("line 2"), "{error}");
        let warnings =
            validate(r#"[{"key":"s","command":"sidebar.settings","extra":1}]"#).expect("applied");
        assert_eq!(
            warnings,
            vec!["keymap.json[0]: unknown field `extra`".to_string()]
        );
        let warnings =
            validate(r#"[{"key":"ctrl+o","command":"-sidebar.settings"}]"#).expect("applied");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nothing to remove"), "{warnings:?}");
        assert!(validate("\u{feff}[]").is_ok(), "a BOM is accepted");
    }

    #[test]
    fn a_user_binding_translates_to_the_commands_default_key() {
        let keymap = map(r#"[{"key":"ctrl+o","command":"sidebar.settings"}]"#);
        assert_eq!(
            translate_with(
                &keymap,
                View::Explorer,
                ev(KeyCode::Char('o'), M::CONTROL),
                false
            ),
            Translated::Key(KeyEvent::new(KeyCode::Char('s'), M::NONE))
        );
        // The default key still works: defaults are kept unless removed.
        assert_eq!(
            translate_with(&keymap, View::Explorer, plain('s'), false),
            Translated::Unchanged
        );
    }

    #[test]
    fn a_removal_swallows_only_its_default_key() {
        let keymap = map(r#"[{"key":"s","command":"-sidebar.settings"}]"#);
        assert_eq!(
            translate_with(&keymap, View::Explorer, plain('s'), false),
            Translated::Swallow
        );
        assert_eq!(
            translate_with(&keymap, View::Explorer, plain('b'), false),
            Translated::Unchanged
        );
    }

    #[test]
    fn in_text_only_ctrl_and_f_keys_are_translated_or_removed() {
        let keymap = map(r#"[
                {"key":"s","command":"-sidebar.settings"},
                {"key":"ctrl+o","command":"sidebar.settings"},
                {"key":"ctrl+o","command":"explorer.quickOpen"},
                {"key":"f5","command":"sidebar.hide"},
                {"key":"x","command":"sidebar.settings"}
            ]"#);
        // A plain-key removal does not apply while typing.
        assert_eq!(
            translate_with(&keymap, View::Explorer, plain('s'), true),
            Translated::Unchanged
        );
        // A plain-key binding does not apply while typing.
        assert_eq!(
            translate_with(&keymap, View::Explorer, plain('x'), true),
            Translated::Unchanged
        );
        // ctrl+o → sidebar.settings targets plain `s`, which would type: left alone.
        assert_eq!(
            translate_with(
                &keymap,
                View::Explorer,
                ev(KeyCode::Char('o'), M::CONTROL),
                true
            ),
            Translated::Unchanged
        );
        // The ctrl binding whose target is ctrl+p is still translated while typing.
        let quick = map(r#"[{"key":"ctrl+o","command":"explorer.quickOpen"}]"#);
        assert_eq!(
            translate_with(
                &quick,
                View::Explorer,
                ev(KeyCode::Char('o'), M::CONTROL),
                true
            ),
            Translated::Key(ev(KeyCode::Char('p'), M::CONTROL))
        );
        // F-keys count as keys, not text.
        assert_eq!(
            translate_with(&keymap, View::Explorer, ev(KeyCode::F(5), M::NONE), true),
            Translated::Unchanged,
            "f5 → plain b is a plain target, left alone while typing"
        );
        assert_eq!(
            translate_with(&keymap, View::Explorer, ev(KeyCode::F(5), M::NONE), false),
            Translated::Key(plain('b'))
        );
    }

    #[test]
    fn bindings_and_removals_follow_the_view_scope() {
        let bound = map(r#"[{"key":"ctrl+o","command":"scm.sync"}]"#);
        assert_eq!(
            translate_with(
                &bound,
                View::Explorer,
                ev(KeyCode::Char('o'), M::CONTROL),
                false
            ),
            Translated::Unchanged,
            "scm.sync is not an Explorer command"
        );
        assert_eq!(
            translate_with(
                &bound,
                View::SourceControl,
                ev(KeyCode::Char('o'), M::CONTROL),
                false
            ),
            Translated::Key(plain('S'))
        );
        let removed = map(r#"[{"key":"t","command":"-scm.toggleTreeView"}]"#);
        assert_eq!(
            translate_with(&removed, View::Explorer, plain('t'), false),
            Translated::Unchanged
        );
        assert_eq!(
            translate_with(&removed, View::SourceControl, plain('t'), false),
            Translated::Swallow
        );
    }

    #[test]
    fn a_translated_key_is_not_translated_again() {
        // ctrl+o → `s`, and `s` is itself bound to view.explorer. The first
        // translation is the answer; nothing re-translates the synthesized key.
        let keymap = map(
            r#"[{"key":"ctrl+o","command":"sidebar.settings"},{"key":"s","command":"view.explorer"}]"#,
        );
        assert_eq!(
            translate_with(
                &keymap,
                View::Explorer,
                ev(KeyCode::Char('o'), M::CONTROL),
                false
            ),
            Translated::Key(plain('s'))
        );
    }

    #[test]
    fn every_command_has_a_registry_entry_it_can_be_bound_to() {
        assert!(command("sidebar.settings").is_some());
        assert!(command("scm.toggleTreeView").is_some());
        assert!(command("herdr-sidebar.file-search").is_none());
    }

    fn entry(index: usize, key: &str) -> GlobalBinding {
        GlobalBinding {
            index,
            key: key.into(),
            command: "herdr-sidebar.file-search".into(),
        }
    }

    #[test]
    fn the_block_renders_one_table_per_entry_with_escaped_strings() {
        let block = render_block(&[entry(0, "ctrl+alt+o")]);
        assert!(
            block.starts_with(&format!(
                "{BLOCK_BEGIN}\n[[keys.command]]\nkey = \"ctrl+alt+o\"\ntype = \"plugin_action\"\n"
            )),
            "{block}"
        );
        assert!(
            block.contains(
                "command = \"herdr-sidebar.file-search\"\ndescription = \"keymap.json\"\n"
            )
        );
        assert!(block.ends_with(&format!("{BLOCK_END}\n")));
        assert_eq!(render_block(&[]), "");
        assert_eq!(toml_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(toml_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn the_block_is_replaced_and_removed_in_place() {
        let config = "[ui]\nx = 1\n";
        let a = render_block(&[entry(0, "ctrl+alt+o")]);
        let b = render_block(&[entry(0, "ctrl+alt+p")]);

        let added = apply_block(config, &a).unwrap();
        assert_eq!(added, format!("[ui]\nx = 1\n\n{a}"));
        assert_eq!(apply_block(&added, &a).unwrap(), added, "idempotent");

        let replaced = apply_block(&added, &b).unwrap();
        assert_eq!(
            replaced.matches(BLOCK_BEGIN).count(),
            1,
            "no duplicate block"
        );
        assert!(replaced.contains("ctrl+alt+p") && !replaced.contains("ctrl+alt+o"));

        assert_eq!(
            apply_block(&replaced, "").unwrap(),
            config,
            "removed with its blank line"
        );
        assert_eq!(apply_block(config, "").unwrap(), config);
    }

    #[test]
    fn a_half_block_is_refused_rather_than_guessed_at() {
        let error = apply_block(&format!("x\n{BLOCK_BEGIN}\n"), "").unwrap_err();
        assert!(error.contains("incomplete"), "{error}");
    }

    #[test]
    fn only_new_diagnostics_count_and_block_entries_are_named() {
        let candidate = "config: issues found\nctrl+q: kept keys.command[0].key, disabled keys.command[29].key\n";
        let new = new_diagnostics("config: ok\n", candidate);
        assert_eq!(
            new,
            vec!["ctrl+q: kept keys.command[0].key, disabled keys.command[29].key".to_string()]
        );
        assert!(
            new_diagnostics(candidate, candidate).is_empty(),
            "already in the current file"
        );

        let globals = vec![GlobalBinding {
            index: 2,
            key: "ctrl+q".into(),
            command: "a.b".into(),
        }];
        assert_eq!(
            map_diagnostic(&new[0], 29, &globals),
            "keymap.json[2]: ctrl+q: kept keys.command[0].key, disabled keymap.json[2].key"
        );
        // A diagnostic that points outside the block is passed through.
        assert_eq!(
            map_diagnostic("bad: keys.command[3].key", 29, &globals),
            "bad: keys.command[3].key"
        );
    }

    #[test]
    fn the_generated_tables_are_counted_from_the_block_start() {
        let config = format!(
            "[[keys.command]]\nkey = \"a\"\n[[keys.command]]\nkey = \"b\"\n{}",
            render_block(&[entry(0, "ctrl+alt+o")])
        );
        assert_eq!(first_generated_entry(&config), 2);
        assert_eq!(first_generated_entry("[ui]\n"), 0);
    }

    const CONFIG: &str = "[keys]\ntoggle_sidebar = \"prefix+b\"\n\n[[keys.command]]\nkey = \"ctrl+q\"\ntype = \"plugin_action\"\ncommand = \"herdr-sidebar.file-search\"\ndescription = \"x\"\n";

    /// A herdr stand-in: a key bound by two `[[keys.command]]` tables is a conflict
    /// (herdr's own wording), and every reload is counted.
    struct Fake {
        reloads: Cell<u32>,
    }

    impl Herdr for Fake {
        fn check(&self, config: &Path) -> Result<(bool, String), String> {
            let text = std::fs::read_to_string(config).map_err(|error| error.to_string())?;
            let mut keys: Vec<(usize, String)> = Vec::new();
            let mut table = None;
            let mut count = 0;
            for line in text.lines() {
                let line = line.trim();
                if line == "[[keys.command]]" {
                    table = Some(count);
                    count += 1;
                } else if let (Some(index), Some(value)) = (table, line.strip_prefix("key = ")) {
                    keys.push((index, value.trim_matches('"').to_string()));
                    table = None;
                }
            }
            let mut out = String::new();
            for (i, (first, key)) in keys.iter().enumerate() {
                if let Some((second, _)) = keys[i + 1..].iter().find(|(_, other)| other == key) {
                    out.push_str(&format!(
                        "{key}: kept keys.command[{first}].key, disabled keys.command[{second}].key\n"
                    ));
                }
            }
            let ok = out.is_empty();
            Ok((
                ok,
                format!("config: {}\n{out}", if ok { "ok" } else { "issues found" }),
            ))
        }

        fn reload(&self) -> Result<(), String> {
            self.reloads.set(self.reloads.get() + 1);
            Ok(())
        }
    }

    fn scratch_config(dir: &Path) -> PathBuf {
        let config = dir.join("config.toml");
        std::fs::write(&config, CONFIG).unwrap();
        config
    }

    #[test]
    fn a_clean_entry_is_written_and_herdr_reloaded_once() {
        let dir = isolated().join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let config = scratch_config(&dir);
        let keymap =
            map(r#"[{"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true}]"#);
        let fake = Fake {
            reloads: Cell::new(0),
        };

        assert_eq!(apply_to(&config, &fake, &keymap), Ok(ApplyOutcome::Written));
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with(CONFIG), "the user's config is kept");
        assert!(text.contains("key = \"ctrl+alt+o\""));
        assert_eq!(fake.reloads.get(), 1);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp file left behind"
        );

        assert_eq!(
            apply_to(&config, &fake, &keymap),
            Ok(ApplyOutcome::Unchanged)
        );
        assert_eq!(fake.reloads.get(), 1, "an unchanged block reloads nothing");
    }

    #[test]
    fn a_conflicting_entry_is_rejected_and_nothing_is_written() {
        let dir = isolated().join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let config = scratch_config(&dir);
        let keymap =
            map(r#"[{"key":"ctrl+q","command":"herdr-sidebar.file-search","global":true}]"#);
        let fake = Fake {
            reloads: Cell::new(0),
        };

        let error = apply_to(&config, &fake, &keymap).unwrap_err();
        match error {
            ApplyError::Rejected(lines) => {
                assert_eq!(lines.len(), 1, "{lines:?}");
                assert!(lines[0].starts_with("keymap.json[0]:"), "{lines:?}");
                assert!(lines[0].contains("keymap.json[0].key"), "{lines:?}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert_eq!(fake.reloads.get(), 0);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), CONFIG);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "temp file removed"
        );
    }

    #[test]
    fn removing_the_last_global_entry_removes_the_block() {
        let dir = isolated().join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let config = scratch_config(&dir);
        let fake = Fake {
            reloads: Cell::new(0),
        };
        let with =
            map(r#"[{"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true}]"#);
        apply_to(&config, &fake, &with).unwrap();
        assert_eq!(
            apply_to(&config, &fake, &Keymap::default()),
            Ok(ApplyOutcome::Written)
        );
        assert_eq!(std::fs::read_to_string(&config).unwrap(), CONFIG);
    }

    #[test]
    fn no_global_entries_never_touch_a_config_without_a_block() {
        let dir = isolated().join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let config = scratch_config(&dir);
        let fake = Fake {
            reloads: Cell::new(0),
        };
        assert_eq!(
            apply_to(&config, &fake, &Keymap::default()),
            Ok(ApplyOutcome::Unchanged)
        );
        assert_eq!(fake.reloads.get(), 0);
        let missing = dir.join("absent.toml");
        assert_eq!(
            apply_to(&missing, &fake, &Keymap::default()),
            Ok(ApplyOutcome::Unchanged)
        );
        assert!(
            !missing.exists(),
            "no config is created for an empty keymap"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_keeps_its_link_and_the_target_is_written() {
        let dir = isolated().join("herdr");
        std::fs::create_dir_all(&dir).unwrap();
        let target = scratch_config(&dir);
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let keymap =
            map(r#"[{"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true}]"#);
        let fake = Fake {
            reloads: Cell::new(0),
        };

        assert_eq!(apply_to(&link, &fake, &keymap), Ok(ApplyOutcome::Written));
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains(BLOCK_BEGIN)
        );
    }

    #[test]
    fn ensure_files_creates_the_empty_keymap_and_the_reference_once() {
        let dir = isolated();
        ensure_files();
        assert_eq!(
            std::fs::read_to_string(dir.join("keymap.json")).unwrap(),
            "[]\n"
        );
        let defaults = std::fs::read_to_string(dir.join("keymap.defaults.json")).unwrap();
        assert!(defaults.contains("\"sidebar.settings\""), "{defaults}");
        assert!(defaults.contains("\"ctrl+p\""), "{defaults}");
        assert!(
            std::fs::metadata(dir.join("keymap.defaults.json"))
                .unwrap()
                .permissions()
                .readonly()
        );

        std::fs::write(
            dir.join("keymap.json"),
            r#"[{"key":"ctrl+o","command":"sidebar.settings"}]"#,
        )
        .unwrap();
        ensure_files();
        assert!(
            std::fs::read_to_string(dir.join("keymap.json"))
                .unwrap()
                .contains("ctrl+o")
        );
    }

    #[test]
    fn a_broken_keymap_keeps_the_last_good_bindings_in_a_fresh_process() {
        let dir = isolated();
        std::fs::write(
            dir.join("keymap.json"),
            r#"[{"key":"ctrl+o","command":"sidebar.settings"}]"#,
        )
        .unwrap();
        assert_eq!(current().bindings.len(), 1);
        assert!(dir.join("keymap.last-good.json").exists());

        std::fs::write(dir.join("keymap.json"), r#"[{"key":"ctrl+o" "command"}]"#).unwrap();
        assert_eq!(
            current().bindings.len(),
            1,
            "the last good keymap stays in force"
        );
        assert!(keymap_error().is_some());

        // A fresh process has no in-process parse; the saved copy still applies.
        lock(cache()).clear();
        assert_eq!(current().bindings.len(), 1, "from keymap.last-good.json");
    }

    #[test]
    fn toasts_follow_validity_transitions() {
        let bad = KeymapSnapshot {
            present: true,
            error: Some("line 1, column 2: x".into()),
            ..KeymapSnapshot::default()
        };
        assert_eq!(
            toast(false, &bad),
            Some(("keymap.json not applied", "line 1, column 2: x".to_string()))
        );
        let fixed = KeymapSnapshot {
            present: true,
            warnings: vec!["keymap.json[0]: unknown field `x`".into()],
            ..KeymapSnapshot::default()
        };
        assert_eq!(
            toast(true, &fixed),
            Some((
                "keymap.json applied",
                "keymap.json[0]: unknown field `x`".to_string()
            ))
        );
        assert_eq!(toast(false, &fixed), None);
    }

    /// The real herdr, run against a COPY of a config (never the live one), with
    /// a no-op reload. Run by hand:
    /// `HS_KEYMAP_E2E_CONFIG=<copy> HS_KEYMAP_E2E_JSON='[…]' cargo test --lib -- --ignored e2e_check_copy --nocapture`
    #[test]
    #[ignore]
    fn e2e_check_copy() {
        struct CheckOnly;
        impl Herdr for CheckOnly {
            fn check(&self, config: &Path) -> Result<(bool, String), String> {
                Cli.check(config)
            }
            fn reload(&self) -> Result<(), String> {
                Ok(())
            }
        }
        let config = PathBuf::from(std::env::var("HS_KEYMAP_E2E_CONFIG").expect("config copy"));
        let json = std::env::var("HS_KEYMAP_E2E_JSON").expect("keymap json");
        let keymap = map(&json);
        let outcome = apply_to(&config, &CheckOnly, &keymap);
        println!("apply: {outcome:?}");
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;
    use crossterm::event::KeyModifiers as M;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn chord(text: &str) -> Chord {
        parse_chord(text).unwrap_or_else(|error| panic!("{error}"))
    }

    fn entries(text: &str) -> Vec<RawEntry> {
        serde_json::from_str(text).expect("sample json")
    }

    fn keymap_of(entries: &[RawEntry]) -> Keymap {
        let text = render_entries(entries);
        check_keymap(&text).unwrap_or_else(|error| panic!("{error}")).keymap
    }

    fn isolated() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "hs-keymap-edit-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch state dir");
        state::TEST_STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
        dir
    }

    #[test]
    fn chord_text_round_trips() {
        for text in [
            "s", "S", ".", "+", "ctrl++", "ctrl+o", "alt+x", "alt+S", "ctrl+S", "shift+up",
            "ctrl+space", "space", "f12", "pagedown", "shift+tab", "enter", "backspace",
            "delete", "esc", "home", "end", "ctrl+alt+o",
        ] {
            let parsed = chord(text);
            let written = chord_to_string(parsed).unwrap_or_else(|| panic!("{text} has no text"));
            assert_eq!(chord(&written), parsed, "{text} wrote as {written}");
        }
        assert_eq!(
            chord_to_string(Chord::of(KeyEvent::new(KeyCode::BackTab, M::SHIFT))).as_deref(),
            Some("shift+tab")
        );
        assert_eq!(
            chord_to_string(Chord::of(KeyEvent::new(KeyCode::Insert, M::NONE))),
            None
        );
    }

    #[test]
    fn effective_keys_defaults_removal_add_and_replace() {
        let default = Keymap::default();
        assert_eq!(effective_keys(&default, "sidebar.settings"), vec![chord("s")]);
        assert_eq!(
            effective_keys(&default, "explorer.quickOpen"),
            vec![chord("ctrl+p"), chord("f12")]
        );
        assert!(!is_changed(&default, "sidebar.settings"));

        let removed = keymap_of(&entries(r#"[{"key":"s","command":"-sidebar.settings"}]"#));
        assert!(effective_keys(&removed, "sidebar.settings").is_empty());
        assert!(is_changed(&removed, "sidebar.settings"));

        let added = keymap_of(&entries(r#"[{"key":"x","command":"sidebar.settings"}]"#));
        assert_eq!(
            effective_keys(&added, "sidebar.settings"),
            vec![chord("s"), chord("x")]
        );

        let replaced = keymap_of(&entries(
            r#"[{"key":"x","command":"sidebar.settings"},{"key":"s","command":"-sidebar.settings"}]"#,
        ));
        assert_eq!(effective_keys(&replaced, "sidebar.settings"), vec![chord("x")]);
        assert!(is_changed(&replaced, "sidebar.settings"));
        // A binding for a key that is also removed wins.
        let both = keymap_of(&entries(
            r#"[{"key":"s","command":"-sidebar.settings"},{"key":"s","command":"sidebar.settings"}]"#,
        ));
        assert_eq!(effective_keys(&both, "sidebar.settings"), vec![chord("s")]);
    }

    #[test]
    fn conflicts_respect_view_overlap() {
        let default = Keymap::default();
        // ctrl+p is Both-scope Quick Open: a Both command takes it from it.
        let found: Vec<&str> = conflicts(&default, "sidebar.settings", chord("ctrl+p"))
            .iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(found, vec!["explorer.quickOpen"]);
        // `U` is Source Control only, so an Explorer command does not conflict with it.
        assert!(conflicts(&default, "explorer.changeFolder", chord("U")).is_empty());
        // `c` is Explorer's change folder AND SCM's focus message: no overlap, no conflict.
        assert!(conflicts(&default, "explorer.changeFolder", chord("c")).is_empty());
        // `s` is Both-scope Settings, which overlaps SCM's Stage: the key conflicts.
        assert!(conflicts(&default, "scm.stageAll", chord("s"))
            .iter()
            .any(|c| c.id == "sidebar.settings"));
    }

    #[test]
    fn replace_writes_the_binding_and_removes_the_old_default() {
        let mut list = entries("[]");
        edit_entries(&mut list, "sidebar.settings", Edit::Replace(chord("x")), &[]).unwrap();
        assert_eq!(
            list,
            entries(r#"[{"key":"x","command":"sidebar.settings"},{"key":"s","command":"-sidebar.settings"}]"#)
        );
    }

    #[test]
    fn replace_with_the_default_key_drops_the_extra_binding() {
        let mut list = entries(r#"[{"key":"x","command":"sidebar.settings"}]"#);
        edit_entries(&mut list, "sidebar.settings", Edit::Replace(chord("s")), &[]).unwrap();
        assert!(list.is_empty(), "the default needs no entry: {list:?}");
    }

    #[test]
    fn add_writes_only_the_binding_and_keeps_the_default() {
        let mut list = entries("[]");
        edit_entries(&mut list, "sidebar.settings", Edit::Add(chord("x")), &[]).unwrap();
        assert_eq!(list, entries(r#"[{"key":"x","command":"sidebar.settings"}]"#));
        // Adding it again writes nothing.
        edit_entries(&mut list, "sidebar.settings", Edit::Add(chord("x")), &[]).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn add_of_a_removed_default_restores_it() {
        let mut list = entries(r#"[{"key":"s","command":"-sidebar.settings"}]"#);
        edit_entries(&mut list, "sidebar.settings", Edit::Add(chord("s")), &[]).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn reset_removes_own_entries_and_keeps_globals_and_unknown_fields() {
        let mut list = entries(
            r#"[
              {"key":"x","command":"sidebar.settings"},
              {"key":"s","command":"-sidebar.settings"},
              {"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true},
              {"key":"q","command":"sidebar.quit","note":"kept"}
            ]"#,
        );
        edit_entries(&mut list, "sidebar.settings", Edit::Reset, &[]).unwrap();
        assert_eq!(
            list,
            entries(
                r#"[
                  {"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true},
                  {"key":"q","command":"sidebar.quit","note":"kept"}
                ]"#
            )
        );
    }

    #[test]
    fn take_over_moves_a_user_binding_and_removes_a_default() {
        // Stealing a key another command was bound to by the user.
        let mut list = entries(r#"[{"key":"x","command":"sidebar.hide"}]"#);
        edit_entries(
            &mut list,
            "sidebar.settings",
            Edit::Replace(chord("x")),
            &["sidebar.hide"],
        )
        .unwrap();
        assert_eq!(
            list,
            entries(r#"[{"key":"x","command":"sidebar.settings"},{"key":"s","command":"-sidebar.settings"}]"#)
        );

        // Stealing a DEFAULT key: its owner gets a removal, and the result validates.
        let mut list = entries("[]");
        edit_entries(
            &mut list,
            "sidebar.settings",
            Edit::Replace(chord("ctrl+p")),
            &["explorer.quickOpen"],
        )
        .unwrap();
        let text = render_entries(&list);
        let keymap = check_keymap(&text).expect("valid").keymap;
        assert_eq!(effective_keys(&keymap, "explorer.quickOpen"), vec![chord("f12")]);
        assert_eq!(effective_keys(&keymap, "sidebar.settings"), vec![chord("ctrl+p")]);
    }

    #[test]
    fn unknown_commands_are_refused() {
        let mut list = entries("[]");
        assert!(edit_entries(&mut list, "no.such", Edit::Add(chord("x")), &[]).is_err());
        assert!(list.is_empty());
    }

    #[test]
    fn apply_edit_writes_reloads_and_refuses_an_invalid_file() {
        let dir = isolated();
        let path = dir.join("keymap.json");
        std::fs::write(&path, "{ broken").unwrap();
        let error = apply_edit("sidebar.settings", Edit::Add(chord("x")), &[]).unwrap_err();
        assert_eq!(error, INVALID_EDIT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");

        std::fs::write(&path, "[]\n").unwrap();
        apply_edit("sidebar.settings", Edit::Replace(chord("x")), &[]).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.ends_with('\n'));
        assert!(written.contains("\"command\": \"sidebar.settings\""), "{written}");
        assert!(written.contains("\"-sidebar.settings\""), "{written}");
        // The applied keymap in this process follows the write at once.
        let live = current();
        assert_eq!(effective_keys(&live, "sidebar.settings"), vec![chord("x")]);
        assert_eq!(
            translate_with(
                &live,
                View::Explorer,
                KeyEvent::new(KeyCode::Char('x'), M::NONE),
                false
            ),
            Translated::Key(KeyEvent::new(KeyCode::Char('s'), M::NONE))
        );
        assert_eq!(
            translate_with(
                &live,
                View::Explorer,
                KeyEvent::new(KeyCode::Char('s'), M::NONE),
                false
            ),
            Translated::Swallow
        );
    }

    #[test]
    fn written_entries_lead_with_key_then_command_and_keep_unknown_fields() {
        let dir = isolated();
        let path = dir.join("keymap.json");
        std::fs::write(
            &path,
            r#"[{"command":"sidebar.quit","note":"kept","key":"q"},{"global":true,"extra":{"n":1},"command":"herdr-sidebar.file-search","key":"ctrl+alt+o"}]"#,
        )
        .unwrap();
        apply_edit("sidebar.settings", Edit::Add(chord("x")), &[]).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let at = |needle: &str| written.find(needle).unwrap_or_else(|| panic!("{needle} missing: {written}"));
        assert!(at("\"key\": \"q\"") < at("\"command\": \"sidebar.quit\""), "{written}");
        assert!(at("\"command\": \"sidebar.quit\"") < at("\"note\": \"kept\""), "{written}");
        assert!(at("\"key\": \"ctrl+alt+o\"") < at("\"command\": \"herdr-sidebar.file-search\""), "{written}");
        assert!(at("\"command\": \"herdr-sidebar.file-search\"") < at("\"global\": true"), "{written}");
        assert!(at("\"global\": true") < at("\"extra\""), "{written}");
        assert!(written.contains("\"note\": \"kept\""), "{written}");
        assert!(written.ends_with("]\n"), "{written}");
        assert!(written.contains("\n  {\n    \"key\": \"x\",\n    \"command\": \"sidebar.settings\"\n  }"), "{written}");
        // The file still validates, and the unknown field is a warning as before.
        let parsed = check_keymap(&written).expect("valid");
        assert!(parsed.warnings.iter().any(|w| w.contains("note")), "{:?}", parsed.warnings);
    }

    #[test]
    fn apply_edit_creates_a_missing_file() {
        let dir = isolated();
        apply_edit("sidebar.settings", Edit::Add(chord("ctrl+o")), &[]).unwrap();
        assert!(dir.join("keymap.json").exists());
    }
}
