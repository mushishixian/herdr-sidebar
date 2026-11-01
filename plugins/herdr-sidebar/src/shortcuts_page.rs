//! The "Keyboard shortcuts" page: a sub-page of ⚙ Settings that lists every
//! sidebar command with the keys it answers to, and rebinds them by editing
//! keymap.json (see [`keymap::apply_edit`]). herdr's own global chords are read
//! from its config.toml and shown read-only, because herdr takes those keys
//! before any sidebar pane sees them.
//!
//! The page is shared by every host of the Settings page (the in-pane modal of
//! both views and the `--settings-popup` process). It keeps no state beyond its
//! own cursor, filter and capture mode: the keymap is re-read on every draw.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::keymap::{self, BLOCK_BEGIN, BLOCK_END, Chord, Command, Edit, Keymap, Scope};
use crate::ui::{
    self, SelectAll, TextKey, hits, palette, selected_text_style, selection_style, text_input_key,
    truncate_to, wrap_footer_message,
};

/// Two clicks on one row within this window count as Enter.
const DOUBLE_CLICK: Duration = Duration::from_millis(450);
/// Row width from which the command id gets its own column.
const WIDE: usize = 64;
/// Width of the command id column in the wide layout.
const ID_WIDTH: usize = 26;
const FOOTER: &str = "⏎ change · a add · ⌫ reset · / filter · esc back";
const FILTER_FOOTER: &str = "type to filter · ⏎ keep · esc clear";
const CAPTURE_FOOTER: &str = "press a key · esc cancels";
const CONFIRM_FOOTER: &str = "⏎ confirm · esc cancel";
const GLOBAL_HINT: &str = "Global chords: add \"global\": true entries in keymap.json";
const HERDR_HINT: &str =
    "herdr takes its own chords first: a key it claims never reaches the sidebar";
const HERDR_READ_ONLY: &str =
    "herdr's own chords are read-only: edit config.toml, or add a global entry in keymap.json";

/// The Settings row value: how many sidebar commands differ from their defaults.
pub fn value() -> String {
    match changed_count() {
        0 => "defaults".to_string(),
        1 => "1 changed".to_string(),
        n => format!("{n} changed"),
    }
}

/// How many sidebar commands the applied keymap changes.
pub fn changed_count() -> usize {
    let keymap = keymap::current();
    keymap::COMMANDS
        .iter()
        .filter(|command| keymap::is_changed(&keymap, command.id))
        .count()
}

/// What the owning host should do after a key or mouse event on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutsAction {
    None,
    /// Return to the Settings rows, with the Keyboard shortcuts row selected.
    Back,
    /// Dismiss the whole Settings page.
    Close,
}

/// A herdr `[[keys.command]]` entry, as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrKey {
    /// The key as written (a string, or several joined with `, `).
    pub key: String,
    /// What the chord runs: the description, or the command for generated entries.
    pub label: String,
    /// The plugin action it runs.
    pub command: String,
    /// The entry sits inside the generated keymap.json block.
    pub generated: bool,
}

/// Parse herdr's config.toml text into its `[[keys.command]]` entries. An entry
/// is `generated` when it sits between the keymap.json block markers.
pub fn parse_herdr_keys(config: &str) -> Result<Vec<HerdrKey>, String> {
    let text = config.trim_start_matches('\u{feff}');
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("herdr config.toml cannot be parsed: {error}"))?;
    let commands = table
        .get("keys")
        .and_then(toml::Value::as_table)
        .and_then(|keys| keys.get("command"))
        .and_then(toml::Value::as_array);
    let Some(commands) = commands else {
        return Ok(Vec::new());
    };
    // Table headers are read in file order, so header N is `commands[N]`.
    let mut generated = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line == BLOCK_BEGIN {
            inside = true;
        } else if line == BLOCK_END {
            inside = false;
        } else if line == "[[keys.command]]" {
            generated.push(inside);
        }
    }
    Ok(commands
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let table = value.as_table()?;
            let command = table
                .get("command")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_string();
            let is_generated = generated.get(index).copied().unwrap_or(false);
            let description = table
                .get("description")
                .and_then(toml::Value::as_str)
                .filter(|text| !text.is_empty());
            // Generated entries all carry the description "keymap.json", which
            // the marker already says, so they show the command instead.
            let label = match description {
                Some(description) if !is_generated => description.to_string(),
                _ => command.clone(),
            };
            Some(HerdrKey {
                key: key_text(table.get("key")),
                label,
                command,
                generated: is_generated,
            })
        })
        .collect())
}

fn key_text(value: Option<&toml::Value>) -> String {
    match value {
        Some(toml::Value::String(text)) => text.clone(),
        Some(toml::Value::Array(keys)) => keys
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => "?".to_string(),
    }
}

/// Whether herdr's key text names `chord` (`ctrl+alt+o`, `alt+f12`, …).
pub fn herdr_key_is(text: &str, chord: Chord) -> bool {
    keymap::parse_chord(text).is_ok_and(|parsed| parsed == chord)
        || keymap::chord_to_string(chord).is_some_and(|written| written.eq_ignore_ascii_case(text))
}

/// The filter matches when any field contains the query, ignoring case. An
/// empty query matches everything.
pub fn matches_filter(query: &str, fields: &[&str]) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || fields
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
}

/// The keys as keymap.json writes them, space separated.
fn keys_text(keys: &[Chord]) -> String {
    keys.iter()
        .filter_map(|chord| keymap::chord_to_string(*chord))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Everything the page shows, read fresh.
struct Model {
    keymap: Arc<Keymap>,
    /// keymap.json does not validate; its edits are refused.
    invalid: bool,
    herdr: Result<Vec<HerdrKey>, String>,
}

enum Entry {
    Header(&'static str),
    Command(&'static Command),
    Herdr(HerdrKey),
    Note(String),
}

/// A change waiting for Enter: the key is used by another command or by herdr.
#[derive(Clone, Debug)]
struct Pending {
    id: &'static str,
    edit: Edit,
    takeover: Vec<&'static str>,
    message: String,
}

#[derive(Clone, Debug)]
enum Mode {
    Browse,
    /// The next key press is the new binding (`add` keeps the old keys).
    Capture { add: bool },
    Confirm(Pending),
}

pub struct ShortcutsPage {
    /// Index into the selectable rows (sidebar commands and herdr chords).
    selected: usize,
    /// First rendered line of the list.
    scroll: usize,
    /// The next draw moves the view onto the selection (keyboard moves do;
    /// the wheel does not, so the view can be read while the cursor stays).
    snap: bool,
    filter: String,
    filter_select: SelectAll,
    filtering: bool,
    mode: Mode,
    /// `(text, is_error)` shown above the footer.
    notice: Option<(String, bool)>,
    last_click: Option<(usize, Instant)>,
    /// The list box and its line→row map from the last draw, for the mouse.
    list: Rect,
    list_lines: Vec<Option<usize>>,
    /// Rows per page, from the last draw.
    page: usize,
    /// herdr's config.toml, read on every draw. `None` = not known.
    herdr_path: Option<PathBuf>,
}

impl ShortcutsPage {
    /// The page for a fresh open of the Settings page.
    pub fn open() -> Self {
        // Tests never read the user's herdr config.
        #[cfg(test)]
        let path = None;
        #[cfg(not(test))]
        let path = crate::viewer::herdr_config_path();
        Self::new(path)
    }

    fn new(herdr_path: Option<PathBuf>) -> Self {
        Self {
            selected: 0,
            scroll: 0,
            snap: true,
            filter: String::new(),
            filter_select: SelectAll::default(),
            filtering: false,
            mode: Mode::Browse,
            notice: None,
            last_click: None,
            list: Rect::default(),
            list_lines: Vec::new(),
            page: 1,
            herdr_path,
        }
    }

    /// A key capture is running: the host must hand the next key straight to the
    /// page, before its own global handlers and before translation.
    pub fn captures_keys(&self) -> bool {
        matches!(self.mode, Mode::Capture { .. })
    }

    fn model(&self) -> Model {
        let snapshot = keymap::keymap_snapshot();
        let herdr = match &self.herdr_path {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|error| format!("herdr config.toml cannot be read: {error}"))
                .and_then(|text| parse_herdr_keys(&text)),
            None => Err("herdr config.toml cannot be found".to_string()),
        };
        Model {
            keymap: snapshot.good.unwrap_or_default(),
            invalid: snapshot.error.is_some(),
            herdr,
        }
    }

    fn entries(&self, model: &Model) -> Vec<Entry> {
        let mut out = vec![Entry::Header("Sidebar")];
        let mut shown = 0;
        for command in keymap::COMMANDS {
            let keys = keymap::effective_keys(&model.keymap, command.id);
            let text = keys_text(&keys);
            if matches_filter(&self.filter, &[command.description, command.id, &text]) {
                out.push(Entry::Command(command));
                shown += 1;
            }
        }
        if shown == 0 {
            out.push(Entry::Note("no sidebar shortcut matches the filter".into()));
        }
        out.push(Entry::Header("herdr (global)"));
        match &model.herdr {
            Err(error) => out.push(Entry::Note(error.clone())),
            Ok(keys) => {
                let mut shown = 0;
                for key in keys {
                    if matches_filter(&self.filter, &[&key.key, &key.label, &key.command]) {
                        out.push(Entry::Herdr(key.clone()));
                        shown += 1;
                    }
                }
                if shown == 0 {
                    out.push(Entry::Note(if self.filter.trim().is_empty() {
                        "no herdr chords in config.toml".into()
                    } else {
                        "no herdr chord matches the filter".into()
                    }));
                }
            }
        }
        out.push(Entry::Note(GLOBAL_HINT.into()));
        out.push(Entry::Note(HERDR_HINT.into()));
        out
    }

    /// Everything a key or mouse event needs: the model, its entries, and the
    /// entry index of each selectable row.
    fn snapshot(&self) -> (Model, Vec<Entry>, Vec<usize>) {
        let model = self.model();
        let entries = self.entries(&model);
        let items = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry, Entry::Command(_) | Entry::Herdr(_)))
            .map(|(index, _)| index)
            .collect();
        (model, entries, items)
    }

    fn selected_entry<'a>(&self, entries: &'a [Entry], items: &[usize]) -> Option<&'a Entry> {
        items.get(self.selected).map(|&index| &entries[index])
    }

    fn step(&mut self, delta: isize, count: usize) {
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.snap = true;
    }

    fn set_filter(&mut self, text: String) {
        self.filter = text;
        self.filter_select.disarm();
        self.selected = 0;
        self.scroll = 0;
        self.snap = true;
    }

    /// A key press. Keys are raw: the host does not translate them here.
    pub fn key(&mut self, key: KeyEvent) -> ShortcutsAction {
        if self.filtering {
            let (_, _, items) = self.snapshot();
            self.filter_key(key, items.len());
            return ShortcutsAction::None;
        }
        match self.mode.clone() {
            Mode::Capture { add } => {
                self.capture_key(key, add);
                ShortcutsAction::None
            }
            Mode::Confirm(pending) => {
                self.confirm_key(key, &pending);
                ShortcutsAction::None
            }
            Mode::Browse => self.browse_key(key),
        }
    }

    fn browse_key(&mut self, key: KeyEvent) -> ShortcutsAction {
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let (_, _, items) = self.snapshot();
        let count = items.len();
        match key.code {
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    return ShortcutsAction::Back;
                }
                self.set_filter(String::new());
            }
            KeyCode::Char('q') if plain => return ShortcutsAction::Close,
            KeyCode::Up | KeyCode::Char('k') => self.step(-1, count),
            KeyCode::Down | KeyCode::Char('j') => self.step(1, count),
            KeyCode::PageUp => self.step(-(self.page as isize), count),
            KeyCode::PageDown => self.step(self.page as isize, count),
            KeyCode::Home | KeyCode::Char('g') => self.step(-(count as isize), count),
            KeyCode::End | KeyCode::Char('G') => self.step(count as isize, count),
            KeyCode::Enter => self.activate(),
            KeyCode::Char('a') if plain => self.start_capture(true),
            KeyCode::Backspace | KeyCode::Delete => self.reset(),
            KeyCode::Char('/') if plain => {
                self.filtering = true;
                self.filter_select.disarm();
            }
            _ => {}
        }
        ShortcutsAction::None
    }

    fn filter_key(&mut self, key: KeyEvent, count: usize) {
        let mut text = std::mem::take(&mut self.filter);
        let mut select = self.filter_select;
        let result = text_input_key(key, &mut text, &mut select);
        self.filter = text;
        self.filter_select = select;
        match result {
            TextKey::Edited => {
                self.selected = 0;
                self.scroll = 0;
                self.snap = true;
            }
            TextKey::Consumed => {}
            TextKey::Other => match key.code {
                // The first Esc clears the filter, the next one leaves the field.
                KeyCode::Esc => {
                    if self.filter.is_empty() {
                        self.filtering = false;
                    } else {
                        self.set_filter(String::new());
                    }
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Up => self.step(-1, count),
                KeyCode::Down => self.step(1, count),
                _ => {}
            },
        }
    }

    /// Enter (or a double click) on the selected row: a sidebar command takes a
    /// new key, a herdr chord is read-only.
    fn activate(&mut self) {
        self.start_capture(false);
    }

    fn start_capture(&mut self, add: bool) {
        let (model, entries, items) = self.snapshot();
        match self.selected_entry(&entries, &items) {
            Some(Entry::Command(_)) => {
                if model.invalid {
                    self.notice = Some((keymap::INVALID_EDIT.into(), true));
                    return;
                }
                self.mode = Mode::Capture { add };
                self.notice = None;
            }
            Some(Entry::Herdr(_)) => self.notice = Some((HERDR_READ_ONLY.into(), false)),
            _ => {}
        }
    }

    /// Backspace/Delete: the selected command goes back to its defaults.
    fn reset(&mut self) {
        let (model, entries, items) = self.snapshot();
        match self.selected_entry(&entries, &items) {
            Some(Entry::Command(command)) => {
                if model.invalid {
                    self.notice = Some((keymap::INVALID_EDIT.into(), true));
                } else if !keymap::is_changed(&model.keymap, command.id) {
                    self.notice = Some((
                        format!("{} is already its default", command.description),
                        false,
                    ));
                } else {
                    self.commit(command.id, Edit::Reset, &[]);
                }
            }
            Some(Entry::Herdr(_)) => self.notice = Some((HERDR_READ_ONLY.into(), false)),
            _ => {}
        }
    }

    fn capture_key(&mut self, key: KeyEvent, add: bool) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Browse;
            self.notice = None;
            return;
        }
        // Keys the list uses, and Ctrl+C (which closes the settings popup).
        let reserved = matches!(key.code, KeyCode::Enter | KeyCode::Up | KeyCode::Down)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if reserved {
            self.notice = Some(("that key is reserved".into(), true));
            return;
        }
        let (model, entries, items) = self.snapshot();
        let Some(Entry::Command(command)) = self.selected_entry(&entries, &items) else {
            self.mode = Mode::Browse;
            return;
        };
        let chord = Chord::of(key);
        let Some(text) = keymap::chord_to_string(chord) else {
            self.notice = Some(("that key cannot be bound".into(), true));
            return;
        };
        let keys = keymap::effective_keys(&model.keymap, command.id);
        if add && keys.contains(&chord) {
            self.mode = Mode::Browse;
            self.notice = Some((format!("`{text}` already works here"), false));
            return;
        }
        if !add && keys.len() == 1 && keys[0] == chord {
            self.mode = Mode::Browse;
            self.notice = Some((format!("`{text}` is already its key"), false));
            return;
        }
        let edit = if add {
            Edit::Add(chord)
        } else {
            Edit::Replace(chord)
        };
        if model.invalid {
            self.mode = Mode::Browse;
            self.notice = Some((keymap::INVALID_EDIT.into(), true));
            return;
        }
        let conflicts = keymap::conflicts(&model.keymap, command.id, chord);
        let global = global_use(&model.herdr, chord);
        if conflicts.is_empty() && global.is_none() {
            self.commit(command.id, edit, &[]);
            return;
        }
        let mut parts = Vec::new();
        if !conflicts.is_empty() {
            let names: Vec<&str> = conflicts.iter().map(|other| other.description).collect();
            parts.push(format!("`{text}` is used by {}", names.join(", ")));
        }
        if global.is_some() {
            parts.push(format!(
                "herdr uses `{text}` globally; the sidebar will never see it"
            ));
        }
        let verb = if conflicts.is_empty() {
            "keep it anyway"
        } else {
            "take it over"
        };
        let message = format!("{} — ⏎ {verb}, esc cancel", parts.join("; "));
        let takeover = conflicts.iter().map(|other| other.id).collect();
        self.mode = Mode::Confirm(Pending {
            id: command.id,
            edit,
            takeover,
            message,
        });
    }

    fn confirm_key(&mut self, key: KeyEvent, pending: &Pending) {
        match key.code {
            KeyCode::Enter => self.commit(pending.id, pending.edit, &pending.takeover),
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.notice = Some(("cancelled".into(), false));
            }
            _ => {}
        }
    }

    /// Write one change to keymap.json and say what it became.
    fn commit(&mut self, id: &'static str, edit: Edit, takeover: &[&str]) {
        self.mode = Mode::Browse;
        match keymap::apply_edit(id, edit, takeover) {
            Ok(()) => {
                let keymap = keymap::current();
                let keys = keys_text(&keymap::effective_keys(&keymap, id));
                let name = keymap::command(id).map_or(id, |command| command.description);
                let keys = if keys.is_empty() {
                    "no key".to_string()
                } else {
                    keys
                };
                self.notice = Some((format!("saved: {name} → {keys}"), false));
            }
            Err(error) => self.notice = Some((error, true)),
        }
    }

    /// Mouse input: hover selects, a click selects, a double click is Enter, the
    /// wheel scrolls the view without moving the selection.
    pub fn mouse(&mut self, mouse: MouseEvent) -> ShortcutsAction {
        let max_scroll = self.list_lines.len().saturating_sub(self.page);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => self.scroll = (self.scroll + 3).min(max_scroll),
            MouseEventKind::Moved if !self.captures_keys() => {
                if let Some(item) = self.item_at(mouse.column, mouse.row) {
                    self.selected = item;
                }
            }
            MouseEventKind::Down(MouseButton::Left) if !self.captures_keys() => {
                let Some(item) = self.item_at(mouse.column, mouse.row) else {
                    return ShortcutsAction::None;
                };
                self.selected = item;
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(last, at)| {
                    last == item && now.duration_since(at) < DOUBLE_CLICK
                });
                self.last_click = if double { None } else { Some((item, now)) };
                if double {
                    self.activate();
                }
            }
            _ => {}
        }
        ShortcutsAction::None
    }

    fn item_at(&self, column: u16, row: u16) -> Option<usize> {
        if !hits(self.list, column, row) {
            return None;
        }
        let line = self.scroll + usize::from(row - self.list.y);
        self.list_lines.get(line).copied().flatten()
    }

    /// Draw the page over all of `area`.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let (model, entries, items) = self.snapshot();
        if self.selected >= items.len() {
            self.selected = items.len().saturating_sub(1);
        }
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(" Keyboard shortcuts ")
            .border_style(Style::default().dim());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let width = inner.width;
        let mut bottom: Vec<Line<'static>> = Vec::new();
        if model.invalid {
            bottom.extend(wrapped(keymap::INVALID_EDIT, width, error_style()));
        }
        match (&self.mode, &self.notice) {
            (Mode::Confirm(pending), _) => {
                bottom.extend(wrapped(&pending.message, width, warn_style()));
            }
            (_, Some((text, error))) => {
                let style = if *error {
                    error_style()
                } else {
                    Style::default().dim()
                };
                bottom.extend(wrapped(text, width, style));
            }
            _ => {}
        }
        let footer = match &self.mode {
            _ if self.filtering => FILTER_FOOTER,
            Mode::Browse => FOOTER,
            Mode::Capture { .. } => CAPTURE_FOOTER,
            Mode::Confirm(_) => CONFIRM_FOOTER,
        };
        bottom.extend(wrapped(footer, width, Style::default().dim()));

        let bottom_h = (bottom.len() as u16).min(inner.height.saturating_sub(1));
        let list_h = inner.height.saturating_sub(1 + bottom_h);
        let top = Rect::new(inner.x, inner.y, inner.width, 1);
        let list = Rect::new(inner.x, inner.y + 1, inner.width, list_h);
        let bottom_rect = Rect::new(inner.x, inner.y + 1 + list_h, inner.width, bottom_h);

        let mut filter_line = vec![Span::styled(" / ", Style::default().dim())];
        if self.filtering {
            let style = if self.filter_select.armed() {
                selected_text_style()
            } else {
                Style::default()
            };
            filter_line.push(Span::styled(self.filter.clone(), style));
            filter_line.push(Span::styled("█", Style::default().dim()));
        } else if self.filter.is_empty() {
            filter_line.push(Span::styled("to filter", Style::default().dim()));
        } else {
            filter_line.push(Span::styled(self.filter.clone(), Style::default().dim()));
        }
        frame.render_widget(Paragraph::new(Line::from(filter_line)), top);

        let selected = (!items.is_empty()).then_some(self.selected);
        let row_width = usize::from(width.saturating_sub(1));
        let capturing = matches!(self.mode, Mode::Capture { .. });
        let (lines, line_items) = render_lines(&entries, &model, row_width, selected, capturing);
        let total = lines.len();
        let view = usize::from(list_h);
        if self.snap {
            let at = line_items.iter().position(|item| *item == selected);
            if let (Some(line), true) = (at, view > 0) {
                self.scroll = ui::keep_visible_scroll(line, view, total);
            }
            self.snap = false;
        }
        self.scroll = self.scroll.min(total.saturating_sub(view));
        self.list = list;
        self.list_lines = line_items;
        self.page = view.max(1);
        let visible: Vec<Line<'static>> = lines.into_iter().skip(self.scroll).take(view).collect();
        frame.render_widget(Paragraph::new(visible), list);
        ui::draw_scrollbar(frame, list, total, view, self.scroll);
        frame.render_widget(Paragraph::new(bottom), bottom_rect);
    }
}

fn error_style() -> Style {
    Style::default().fg(palette().deleted)
}

fn warn_style() -> Style {
    Style::default().fg(palette().modified)
}

/// `text` wrapped to `width`, every line in `style`.
fn wrapped(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    wrap_footer_message(text, width, 0)
        .into_iter()
        .map(|line| Line::styled(line, style))
        .collect()
}

/// The herdr chord that already uses `chord`, if any.
fn global_use(herdr: &Result<Vec<HerdrKey>, String>, chord: Chord) -> Option<String> {
    let keys = herdr.as_ref().ok()?;
    keys.iter()
        .find(|key| herdr_key_is(&key.key, chord))
        .map(|key| key.key.clone())
}

/// Widest key text the wide layout gives its key column. Longer key lists are
/// elided with `…`.
const MAX_KEY_COLUMN: usize = 16;
const CAPTURE_PROMPT: &str = "press a key… (esc cancels)";

/// The key column of the wide layout: the widest effective key text of the
/// list (measured in cells), capped. Every row shares it, so the id column
/// starts at the same x on every row.
fn key_column(entries: &[Entry], model: &Model) -> usize {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Command(command) => {
                let keys = keymap::effective_keys(&model.keymap, command.id);
                let text = if keys.is_empty() {
                    "—".to_string()
                } else {
                    keys_text(&keys)
                };
                Some(Span::raw(text).width())
            }
            Entry::Herdr(key) => Some(Span::raw(key.key.as_str()).width()),
            _ => None,
        })
        .max()
        .unwrap_or(1)
        .clamp(1, MAX_KEY_COLUMN)
}

/// The rendered lines of the list, and for each line the selectable row it
/// belongs to (`None` for headers, notes and spacing).
fn render_lines(
    entries: &[Entry],
    model: &Model,
    width: usize,
    selected: Option<usize>,
    capturing: bool,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let wide = width >= WIDE;
    // Wide layout: one key column for the whole list. Narrow: each key keeps its own width.
    let key_col = wide.then(|| key_column(entries, model));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut owners: Vec<Option<usize>> = Vec::new();
    let mut item = 0usize;
    let dim = Style::default().dim();
    for entry in entries {
        match entry {
            Entry::Header(title) => {
                if !lines.is_empty() {
                    lines.push(Line::default());
                    owners.push(None);
                }
                let style = Style::default().fg(palette().header_accent).bold();
                lines.push(Line::styled(format!(" {title}"), style));
                owners.push(None);
            }
            Entry::Command(command) => {
                let is_selected = selected == Some(item);
                let keys = keymap::effective_keys(&model.keymap, command.id);
                let changed = keymap::is_changed(&model.keymap, command.id);
                lines.push(command_row(
                    command,
                    &keys,
                    changed,
                    is_selected && capturing,
                    width,
                    key_col,
                    is_selected,
                ));
                owners.push(Some(item));
                item += 1;
            }
            Entry::Herdr(key) => {
                let is_selected = selected == Some(item);
                let detail = if key.generated { " · keymap.json" } else { "" };
                lines.push(row(
                    Span::raw("  "),
                    key.label.clone(),
                    detail.to_string(),
                    key.command.clone(),
                    key.key.clone(),
                    dim,
                    width,
                    key_col,
                    is_selected,
                ));
                owners.push(Some(item));
                item += 1;
            }
            Entry::Note(text) => {
                for line in wrap_footer_message(text, width as u16, 0) {
                    lines.push(Line::styled(line, dim));
                    owners.push(None);
                }
            }
        }
    }
    (lines, owners)
}

fn command_row(
    command: &Command,
    keys: &[Chord],
    changed: bool,
    capturing: bool,
    width: usize,
    key_col: Option<usize>,
    selected: bool,
) -> Line<'static> {
    let (key, key_style) = if capturing {
        // The full prompt needs its own column in the wide layout; otherwise the short one.
        let fits = width >= 40
            && key_col.is_none_or(|col| col >= Span::raw(CAPTURE_PROMPT).width());
        let text = if fits {
            CAPTURE_PROMPT
        } else {
            "press a key…"
        };
        (text.to_string(), Style::default().bold())
    } else if keys.is_empty() {
        ("—".to_string(), Style::default().dim())
    } else {
        let style = if changed {
            Style::default().fg(palette().header_accent).bold()
        } else {
            Style::default()
        };
        (keys_text(keys), style)
    };
    let detail = match command.scope {
        Scope::Both => String::new(),
        Scope::Explorer => " · Explorer".to_string(),
        Scope::SourceControl => " · Source Control".to_string(),
    };
    let marker = if changed {
        Span::styled("• ", Style::default().fg(palette().header_accent))
    } else {
        Span::raw("  ")
    };
    row(
        marker,
        command.description.to_string(),
        detail,
        command.id.to_string(),
        key,
        key_style,
        width,
        key_col,
        selected,
    )
}

/// One list row: marker, name with its dim detail, then (wide layout) the id in
/// a fixed column and the key right-aligned in the shared key column. The name
/// gives way first; in the narrow layout the key keeps its own width.
#[allow(clippy::too_many_arguments)]
fn row(
    marker: Span<'static>,
    name: String,
    detail: String,
    id: String,
    key: String,
    key_style: Style,
    width: usize,
    key_col: Option<usize>,
    selected: bool,
) -> Line<'static> {
    let dim = Style::default().dim();
    let wide = key_col.is_some();
    let key_room = key_col.unwrap_or((width / 2).max(4));
    let key = truncate_to(key, key_room);
    let key_w = Span::raw(key.as_str()).width();
    let key_cell = if wide { key_room } else { key_w };
    let id_cols = if wide { ID_WIDTH + 1 } else { 0 };
    // Key cell, one trailing space, one gap before the key, one gutter after the
    // name (an elided name must not run into the id or key), then the name.
    let name_w = width.saturating_sub(2 + id_cols + key_cell + 3).max(4);
    let name = truncate_to(name, name_w);
    let used = Span::raw(name.as_str()).width();
    let detail_room = name_w.saturating_sub(used);
    let detail = if detail_room >= 4 {
        truncate_to(detail, detail_room)
    } else {
        String::new()
    };
    let pad = name_w.saturating_sub(used + Span::raw(detail.as_str()).width());
    let mut spans = vec![
        marker,
        Span::raw(name),
        Span::styled(detail, dim),
        Span::raw(" ".repeat(pad + 1)),
    ];
    if wide {
        let id = truncate_to(id, ID_WIDTH);
        let id_w = Span::raw(id.as_str()).width();
        spans.push(Span::styled(id, dim));
        spans.push(Span::raw(" ".repeat(ID_WIDTH - id_w + 1)));
    }
    spans.push(Span::raw(" ".repeat(key_cell - key_w)));
    spans.push(Span::styled(key, key_style));
    spans.push(Span::raw(" "));
    let line = Line::from(spans);
    if selected {
        line.style(selection_style(true))
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers as M;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh state dir for this test thread (see [`crate::state::TEST_STATE_DIR`]).
    fn isolated() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "hs-shortcuts-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch state dir");
        crate::state::TEST_STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));
        dir
    }

    fn plain(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), M::NONE)
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, M::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), M::CONTROL)
    }

    /// Select the sidebar row of `id`.
    fn select(page: &mut ShortcutsPage, id: &str) {
        let (_, entries, items) = page.snapshot();
        page.selected = items
            .iter()
            .position(|&entry| matches!(&entries[entry], Entry::Command(c) if c.id == id))
            .expect("command row");
    }

    fn write_keymap(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("keymap.json");
        std::fs::write(&path, text).expect("keymap.json");
        path
    }

    fn written(path: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(path).expect("keymap.json");
        serde_json::from_str(&text).expect("json")
    }

    #[test]
    fn filter_matches_description_id_and_key_text_ignoring_case() {
        assert!(matches_filter("", &["anything"]));
        assert!(matches_filter("QUICK", &["Quick Open a file", "explorer.quickOpen"]));
        assert!(matches_filter("scm.", &["Stage", "scm.stageAll"]));
        assert!(matches_filter("ctrl+p", &["Quick Open", "ctrl+p f12"]));
        assert!(!matches_filter("zzz", &["Quick Open", "explorer.quickOpen", "ctrl+p"]));
    }

    #[test]
    fn herdr_section_parser_reads_user_and_generated_entries() {
        let config = r#"
[keys]
prefix = "ctrl+b"

[[keys.command]]
key = "ctrl+x"
type = "plugin_action"
command = "dotfiles-ide.toggle-terminal"
description = "Toggle the terminal"

[[keys.command]]
key = ["alt+a", "alt+b"]
type = "plugin_action"
command = "herdr-sidebar.search-popup"

# >>> herdr-sidebar keymap.json (generated; edit keymap.json, not this block) >>>
[[keys.command]]
key = "ctrl+alt+o"
type = "plugin_action"
command = "herdr-sidebar.file-search"
description = "keymap.json"

# <<< herdr-sidebar keymap.json <<<
"#;
        let keys = parse_herdr_keys(config).expect("parses");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].key, "ctrl+x");
        assert_eq!(keys[0].label, "Toggle the terminal");
        assert!(!keys[0].generated);
        assert_eq!(keys[1].key, "alt+a, alt+b");
        assert_eq!(keys[1].label, "herdr-sidebar.search-popup");
        assert!(!keys[1].generated);
        assert_eq!(keys[2].key, "ctrl+alt+o");
        assert_eq!(keys[2].label, "herdr-sidebar.file-search");
        assert!(keys[2].generated);

        // Without a generated block nothing is marked.
        let plain_only = "[[keys.command]]\nkey = \"ctrl+x\"\ncommand = \"a.b\"\n";
        assert!(!parse_herdr_keys(plain_only).expect("parses")[0].generated);
        assert_eq!(parse_herdr_keys("").expect("empty"), Vec::new());
        assert!(parse_herdr_keys("[keys").is_err());
    }

    #[test]
    fn herdr_key_text_matches_a_chord() {
        let chord = keymap::parse_chord("ctrl+alt+o").expect("chord");
        assert!(herdr_key_is("ctrl+alt+o", chord));
        assert!(herdr_key_is("Ctrl+Alt+O", chord));
        assert!(!herdr_key_is("prefix+b", chord));
    }

    #[test]
    fn replace_writes_the_binding_and_removes_the_default() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        assert!(page.captures_keys());
        page.key(plain('x'));
        assert!(!page.captures_keys());
        assert_eq!(
            written(&path),
            serde_json::json!([
                {"key": "x", "command": "sidebar.settings"},
                {"key": "s", "command": "-sidebar.settings"}
            ])
        );
        assert_eq!(changed_count(), 1);
        assert_eq!(value(), "1 changed");
    }

    #[test]
    fn esc_cancels_a_capture_and_leaves_the_file_alone() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        page.key(code(KeyCode::Esc));
        assert!(!page.captures_keys());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");
    }

    #[test]
    fn enter_up_and_down_are_reserved_in_a_capture() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        for reserved in [KeyCode::Enter, KeyCode::Up, KeyCode::Down] {
            page.key(code(reserved));
            assert!(page.captures_keys(), "{reserved:?} stays in the capture");
            assert_eq!(
                page.notice.as_ref().map(|n| n.0.as_str()),
                Some("that key is reserved")
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");
    }

    #[test]
    fn add_keeps_the_default_and_writes_only_the_binding() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(plain('a'));
        page.key(plain('x'));
        assert_eq!(
            written(&path),
            serde_json::json!([{"key": "x", "command": "sidebar.settings"}])
        );
    }

    #[test]
    fn backspace_resets_the_command_and_keeps_globals() {
        let dir = isolated();
        let path = write_keymap(
            &dir,
            r#"[
              {"key":"x","command":"sidebar.settings"},
              {"key":"s","command":"-sidebar.settings"},
              {"key":"ctrl+alt+o","command":"herdr-sidebar.file-search","global":true}
            ]"#,
        );
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        assert_eq!(changed_count(), 1);
        page.key(code(KeyCode::Backspace));
        assert_eq!(
            written(&path),
            serde_json::json!([{
                "key": "ctrl+alt+o",
                "command": "herdr-sidebar.file-search",
                "global": true
            }])
        );
        assert_eq!(changed_count(), 0);
        assert_eq!(value(), "defaults");
    }

    #[test]
    fn a_conflict_waits_for_enter_and_takes_the_key_over() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        page.key(ctrl('p'));
        // Ctrl+P is Quick Open's default: nothing is written until Enter.
        let message = match &page.mode {
            Mode::Confirm(pending) => pending.message.clone(),
            other => panic!("expected a confirmation, got {other:?}"),
        };
        assert!(message.contains("Quick Open"), "{message}");
        assert!(message.contains("take it over"), "{message}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");

        page.key(code(KeyCode::Enter));
        let file = written(&path);
        assert!(
            file.as_array().unwrap().contains(&serde_json::json!({
                "key": "ctrl+p",
                "command": "-explorer.quickOpen"
            })),
            "{file}"
        );
        let live = keymap::current();
        assert_eq!(
            keymap::effective_keys(&live, "explorer.quickOpen"),
            vec![keymap::parse_chord("f12").unwrap()]
        );
        assert_eq!(
            keymap::effective_keys(&live, "sidebar.settings"),
            vec![keymap::parse_chord("ctrl+p").unwrap()]
        );
    }

    #[test]
    fn esc_cancels_a_conflict() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        page.key(ctrl('p'));
        page.key(code(KeyCode::Esc));
        assert!(matches!(page.mode, Mode::Browse));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");
    }

    #[test]
    fn a_herdr_chord_warns_and_then_allows() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[[keys.command]]\nkey = \"ctrl+o\"\ntype = \"plugin_action\"\ncommand = \"x.y\"\n",
        )
        .unwrap();
        let mut page = ShortcutsPage::new(Some(config));
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        page.key(ctrl('o'));
        let message = match &page.mode {
            Mode::Confirm(pending) => pending.message.clone(),
            other => panic!("expected a warning, got {other:?}"),
        };
        assert!(message.contains("herdr uses `ctrl+o` globally"), "{message}");
        page.key(code(KeyCode::Enter));
        assert_eq!(written(&path)[0]["key"], "ctrl+o");
    }

    #[test]
    fn an_invalid_keymap_refuses_every_edit() {
        let dir = isolated();
        let path = write_keymap(&dir, "{ broken");
        let mut page = ShortcutsPage::new(None);
        select(&mut page, "sidebar.settings");
        page.key(code(KeyCode::Enter));
        assert!(!page.captures_keys());
        assert_eq!(
            page.notice.as_ref().map(|n| (n.0.as_str(), n.1)),
            Some((keymap::INVALID_EDIT, true))
        );
        page.key(code(KeyCode::Backspace));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
    }

    #[test]
    fn herdr_rows_are_read_only() {
        let dir = isolated();
        let path = write_keymap(&dir, "[]\n");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[[keys.command]]\nkey = \"ctrl+x\"\ntype = \"plugin_action\"\ncommand = \"x.y\"\n",
        )
        .unwrap();
        let mut page = ShortcutsPage::new(Some(config));
        let (_, entries, items) = page.snapshot();
        page.selected = items
            .iter()
            .position(|&i| matches!(&entries[i], Entry::Herdr(_)))
            .expect("herdr row");
        page.key(code(KeyCode::Enter));
        assert!(!page.captures_keys());
        assert_eq!(
            page.notice.as_ref().map(|n| n.0.as_str()),
            Some(HERDR_READ_ONLY)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]\n");
    }

    #[test]
    fn slash_filters_the_list_and_esc_steps_back() {
        let dir = isolated();
        write_keymap(&dir, "[]\n");
        let mut page = ShortcutsPage::new(None);
        page.key(plain('/'));
        for c in "quick".chars() {
            page.key(plain(c));
        }
        let (_, entries, items) = page.snapshot();
        let commands: Vec<&str> = items
            .iter()
            .filter_map(|&i| match &entries[i] {
                Entry::Command(c) => Some(c.id),
                _ => None,
            })
            .collect();
        assert_eq!(commands, vec!["explorer.quickOpen"]);
        // First Esc clears the filter, the next leaves the field, the next goes back.
        assert_eq!(page.key(code(KeyCode::Esc)), ShortcutsAction::None);
        assert!(page.filter.is_empty());
        assert!(page.filtering);
        assert_eq!(page.key(code(KeyCode::Esc)), ShortcutsAction::None);
        assert!(!page.filtering);
        assert_eq!(page.key(code(KeyCode::Esc)), ShortcutsAction::Back);
    }

    /// The cell column where `needle` starts, read from the buffer's own cells
    /// (each cell's symbol, then the byte offset of the needle inside the row).
    fn cell_of(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, needle: &str) -> Option<u16> {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        for y in area.y..area.y + area.height {
            let mut row = String::new();
            let mut starts = Vec::new();
            for x in area.x..area.x + area.width {
                starts.push((row.len(), x));
                row.push_str(buffer[(x, y)].symbol());
            }
            if let Some(byte) = row.find(needle) {
                return starts
                    .iter()
                    .rev()
                    .find(|(start, _)| *start <= byte)
                    .map(|(_, x)| *x);
            }
        }
        None
    }

    /// In the wide layout every row's command id starts at one x, even when the
    /// key texts differ in width and a herdr description is CJK (2 cells a char).
    #[test]
    fn wide_rows_share_one_id_column() {
        let dir = isolated();
        write_keymap(&dir, "[]\n");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[[keys.command]]\nkey = \"ctrl+alt+s\"\ntype = \"plugin_action\"\ncommand = \"herdr-sidebar.search-popup\"\ndescription = \"全局搜索内容 · 左结果右预览\"\n\n[[keys.command]]\nkey = \"ctrl+q\"\ntype = \"plugin_action\"\ncommand = \"herdr-sidebar.file-search\"\ndescription = \"快速打开文件\"\n",
        )
        .unwrap();
        let mut page = ShortcutsPage::new(Some(config));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 40))
            .expect("terminal");
        terminal
            .draw(|frame| page.draw(frame, frame.area()))
            .expect("draw");
        let columns: Vec<u16> = [
            "view.explorer",
            "explorer.quickOpen",
            "scm.find",
            "explorer.toggleIcons",
            "herdr-sidebar.search-popup",
            "herdr-sidebar.file-search",
        ]
        .iter()
        .map(|id| cell_of(&terminal, id).unwrap_or_else(|| panic!("{id} not drawn")))
        .collect();
        assert!(columns.windows(2).all(|pair| pair[0] == pair[1]), "{columns:?}");
    }

    #[test]
    fn the_page_draws_in_a_wide_and_a_narrow_box() {
        let dir = isolated();
        write_keymap(&dir, "[]\n");
        for (width, height) in [(110u16, 40u16), (34, 30)] {
            let mut page = ShortcutsPage::new(None);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                    .expect("terminal");
            terminal
                .draw(|frame| page.draw(frame, frame.area()))
                .expect("draw");
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Keyboard shortcuts"), "{text}");
            assert!(text.contains("esc back"), "{text}");
            assert!(text.contains("Switch to"), "{text}");
        }
    }
}
