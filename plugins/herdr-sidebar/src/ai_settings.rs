//! The "AI commit" page: a modal opened from ⚙ Settings that edits the API
//! URL, API key and model in [`ai_config`], and tests the connection through
//! [`ai_client::test_connection`]. Shared by both sidebar apps, like
//! `branch_ui::BranchPicker`: the apps own the overlay, this module owns the
//! rows, the field editing and the worker that runs the blocking test.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::ai_client;
use crate::ai_config::{self, AiConfig};
use crate::ui::{
    SelectAll, draw_input_dialog, hits, palette, selection_style, truncate_to, wrap_footer_message,
};

/// The Settings row value for the AI commit page: the configured model, or
/// `not set`.
pub fn model_value() -> String {
    let model = ai_config::load().model;
    let model = model.trim();
    if model.is_empty() {
        "not set".to_string()
    } else {
        model.to_string()
    }
}

/// What the owning app should do after a key or mouse event on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiPageAction {
    None,
    /// Return to the main Settings modal, on the AI commit row.
    Back,
    /// Dismiss the whole modal.
    Close,
}

/// The rows, top to bottom. The three text rows edit a field; the Thinking
/// row cycles the reasoning level and saves at once; the last one runs the
/// connection test.
const ROW_COUNT: usize = 5;
const THINKING_ROW: usize = 3;
const TEST_ROW: usize = 4;

const HELP: &str = "Any OpenAI-compatible chat-completions API (OpenAI, DeepSeek, OpenRouter, a local gateway). Saved owner-only in ai.json.";
const MIN_WIDTH: u16 = 44;

/// One editable value of [`AiConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Url,
    Key,
    Model,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Url => "API URL",
            Field::Key => "API key",
            Field::Model => "Model",
        }
    }

    fn get(self, config: &AiConfig) -> &str {
        match self {
            Field::Url => &config.api_url,
            Field::Key => &config.api_key,
            Field::Model => &config.model,
        }
    }

    fn set(self, config: &mut AiConfig, value: String) {
        match self {
            Field::Url => config.api_url = value,
            Field::Key => config.api_key = value,
            Field::Model => config.model = value,
        }
    }
}

fn field_of(row: usize) -> Option<Field> {
    match row {
        0 => Some(Field::Url),
        1 => Some(Field::Key),
        2 => Some(Field::Model),
        _ => None,
    }
}

/// The API key as the edit dialog shows it: every character a bullet except
/// the last four when the key is longer than eight characters.
pub fn masked_input(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let visible = if chars.len() > 8 { 4 } else { 0 };
    let hidden = chars.len() - visible;
    let mut out = "•".repeat(hidden);
    out.extend(chars[hidden..].iter());
    out
}

/// Persists a saved config. A plain fn pointer so tests can skip the disk.
type SaveFn = fn(&AiConfig) -> Result<(), String>;

pub struct AiSettingsPage {
    config: AiConfig,
    save: SaveFn,
    selected: usize,
    /// Outer box, for mouse hit-testing (set by `draw`).
    rect: Rect,
    /// The edit dialog's box while one is open, for click-outside handling.
    dialog: Rect,
    /// `Some` while a field is being edited; every key belongs to the dialog.
    editing: Option<Field>,
    input: String,
    /// Ctrl+A on the edit dialog: the next edit replaces the whole value.
    select_all: SelectAll,
    /// `(text, is_error)` shown under the rows.
    status: Option<(String, bool)>,
    /// A running connection test; its result arrives on this channel.
    testing: Option<Receiver<Result<String, String>>>,
}

impl AiSettingsPage {
    /// The page for the saved settings.
    pub fn open() -> Self {
        Self::with_config(ai_config::load(), ai_config::save)
    }

    fn with_config(config: AiConfig, save: SaveFn) -> Self {
        Self {
            config,
            save,
            selected: 0,
            rect: Rect::default(),
            dialog: Rect::default(),
            editing: None,
            input: String::new(),
            select_all: SelectAll::default(),
            status: None,
            testing: None,
        }
    }

    /// A connection test is running on its worker thread.
    pub fn is_busy(&self) -> bool {
        self.testing.is_some()
    }

    pub fn key(&mut self, key: KeyEvent) -> AiPageAction {
        if self.editing.is_some() {
            self.edit_key(key);
            return AiPageAction::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => AiPageAction::Back,
            KeyCode::Char('q') => AiPageAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                AiPageAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(ROW_COUNT - 1);
                AiPageAction::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate(self.selected);
                AiPageAction::None
            }
            _ => AiPageAction::None,
        }
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> AiPageAction {
        let left = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        // While the dialog is open the page behind it is inert: a click outside
        // the dialog only cancels the edit.
        if self.editing.is_some() {
            if left && !hits(self.dialog, mouse.column, mouse.row) {
                self.cancel_edit();
            }
            return AiPageAction::None;
        }
        let row = self.row_at(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(index) = row {
                    self.selected = index;
                }
                AiPageAction::None
            }
            MouseEventKind::Down(MouseButton::Left) => match row {
                Some(index) => {
                    self.selected = index;
                    self.activate(index);
                    AiPageAction::None
                }
                None if hits(self.rect, mouse.column, mouse.row) => AiPageAction::None,
                None => AiPageAction::Back,
            },
            _ => AiPageAction::None,
        }
    }

    /// The row under a mouse position, from the box drawn last frame.
    fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        let rect = self.rect;
        let top = rect.y + 1;
        let inside_x = column > rect.x && column < rect.x + rect.width.saturating_sub(1);
        (inside_x && row >= top && usize::from(row - top) < ROW_COUNT)
            .then(|| usize::from(row - top))
    }

    fn activate(&mut self, row: usize) {
        if row == TEST_ROW {
            self.start_test();
            return;
        }
        if row == THINKING_ROW {
            self.config.thinking = self.config.thinking.next();
            self.status = Some(match (self.save)(&self.config) {
                Ok(()) => ("saved".to_string(), false),
                Err(error) => (error, true),
            });
            return;
        }
        let Some(field) = field_of(row) else { return };
        self.input = field.get(&self.config).to_string();
        self.select_all.disarm();
        self.status = None;
        self.editing = Some(field);
    }

    fn start_test(&mut self) {
        if self.testing.is_some() || !self.config.is_configured() {
            return;
        }
        let config = self.config.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // The receiver is dropped when the page closes; nobody is waiting then.
            let _ = tx.send(ai_client::test_connection(&config));
        });
        self.testing = Some(rx);
        self.status = None;
    }

    /// Collects a finished connection test without blocking.
    fn poll_test(&mut self) {
        let Some(rx) = self.testing.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                Err("connection test stopped unexpectedly".to_string())
            }
        };
        self.testing = None;
        self.status = Some(match result {
            Ok(text) => (text, false),
            Err(error) => (error, true),
        });
    }

    fn edit_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Esc first drops an armed Ctrl+A selection, then the dialog.
            KeyCode::Esc if self.select_all.armed() => self.select_all.disarm(),
            KeyCode::Esc => self.cancel_edit(),
            KeyCode::Enter => self.confirm_edit(),
            KeyCode::Backspace => {
                if self.select_all.armed() {
                    self.select_all.disarm();
                    self.input.clear();
                } else {
                    self.input.pop();
                }
            }
            KeyCode::Char('a') if ctrl && !alt => {
                self.select_all.arm_if_nonempty(!self.input.is_empty());
            }
            KeyCode::Char('u') if ctrl && !alt => {
                self.select_all.disarm();
                self.input.clear();
            }
            // CONTROL+ALT is AltGr on Windows: that is text, not a shortcut.
            KeyCode::Char(c) if !ctrl || alt => {
                if self.select_all.armed() {
                    self.select_all.disarm();
                    self.input.clear();
                }
                self.input.push(c);
            }
            _ => {}
        }
    }

    fn cancel_edit(&mut self) {
        self.editing = None;
        self.input.clear();
        self.select_all.disarm();
    }

    fn confirm_edit(&mut self) {
        let Some(field) = self.editing else { return };
        match ai_config::clean_field(&self.input) {
            Ok(value) => {
                field.set(&mut self.config, value);
                self.cancel_edit();
                self.status = Some(match (self.save)(&self.config) {
                    Ok(()) => ("saved".to_string(), false),
                    Err(error) => (error, true),
                });
            }
            // Keep the dialog open so the typed value can be fixed.
            Err(error) => self.status = Some((error, true)),
        }
    }

    /// The value shown on a row, and whether it is the dimmed default.
    fn row_value(&self, row: usize) -> (String, bool) {
        match row {
            0 if self.config.api_url.trim().is_empty() => {
                (format!("{} (default)", ai_config::DEFAULT_API_URL), true)
            }
            0 => (self.config.api_url.trim().to_string(), false),
            1 => (self.config.masked_key(), false),
            2 if self.config.model.trim().is_empty() => ("not set".to_string(), true),
            2 => (self.config.model.trim().to_string(), false),
            3 => (self.config.thinking.label().to_string(), false),
            TEST_ROW if self.testing.is_some() => ("testing…".to_string(), false),
            _ => (String::new(), false),
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.poll_test();
        let area = frame.area();
        let labels = ["API URL", "API key", "Model", "Thinking", "Test connection"];
        let values: Vec<(String, bool)> = (0..ROW_COUNT).map(|row| self.row_value(row)).collect();

        let widest = labels
            .iter()
            .zip(&values)
            .map(|(label, (value, _))| {
                Span::raw(*label).width() + Span::raw(value.as_str()).width() + 5
            })
            .max()
            .unwrap_or(0) as u16;
        let width = widest.max(MIN_WIDTH).min(area.width);
        let inner = usize::from(width.saturating_sub(2));
        let help = wrap_footer_message(HELP, inner as u16, 0);
        let status = self
            .status
            .as_ref()
            .map(|(text, error)| (wrap_footer_message(text, inner as u16, 0), *error));
        let status_lines = status.as_ref().map_or(0, |(lines, _)| lines.len());
        let height = (2 + ROW_COUNT + 1 + help.len() + status_lines + 1) as u16;
        let height = height.min(area.height);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        self.rect = popup;

        let mut lines: Vec<Line> = Vec::new();
        for (row, (label, (value, dim_value))) in labels.iter().zip(&values).enumerate() {
            let enabled = row != TEST_ROW || self.config.is_configured();
            let label_width = Span::raw(*label).width();
            let value = truncate_to(value.clone(), inner.saturating_sub(label_width + 3));
            let pad = inner.saturating_sub(label_width + Span::raw(value.as_str()).width() + 2);
            let text = format!(" {label}{}{value} ", " ".repeat(pad.max(1)));
            let style = if !enabled {
                Style::default().dim()
            } else if row == self.selected {
                selection_style(true)
            } else if *dim_value {
                Style::default().dim()
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }
        lines.push(Line::default());
        for line in help {
            lines.push(Line::from(Span::styled(
                format!(" {line}"),
                Style::default().dim(),
            )));
        }
        if let Some((text_lines, error)) = status {
            let color = if error {
                palette().deleted
            } else {
                palette().untracked
            };
            for line in text_lines {
                lines.push(Line::from(Span::styled(
                    format!(" {line}"),
                    Style::default().fg(color),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            " ⏎ edit · esc back",
            Style::default().dim(),
        )));

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(" AI commit ")
                    .border_style(Style::default().fg(palette().accent)),
            ),
            popup,
        );

        self.dialog = Rect::default();
        if let Some(field) = self.editing {
            let shown = match field {
                Field::Key => masked_input(&self.input),
                _ => self.input.clone(),
            };
            self.dialog = draw_input_dialog(frame, field.label(), &shown, self.select_all.armed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn page(config: AiConfig) -> AiSettingsPage {
        // The no-op save keeps the tests away from the real ai.json.
        AiSettingsPage::with_config(config, |_| Ok(()))
    }

    #[test]
    fn masked_input_shows_only_the_last_four_of_a_long_key() {
        assert_eq!(masked_input(""), "");
        assert_eq!(masked_input("abc"), "•••");
        assert_eq!(masked_input("12345678"), "••••••••");
        assert_eq!(masked_input("sk-1234567890abcd"), "•••••••••••••abcd");
    }

    #[test]
    fn enter_then_typing_updates_the_model() {
        let mut page = page(AiConfig {
            api_url: String::new(),
            api_key: "k".into(),
            model: "m".into(),
            ..AiConfig::default()
        });
        page.key(key(KeyCode::Down));
        page.key(key(KeyCode::Down));
        assert_eq!(page.key(key(KeyCode::Enter)), AiPageAction::None);
        assert_eq!(page.input, "m");
        page.key(key(KeyCode::Backspace));
        for c in "gpt-4o".chars() {
            page.key(key(KeyCode::Char(c)));
        }
        page.key(key(KeyCode::Enter));
        assert_eq!(page.config.model, "gpt-4o");
        assert!(page.editing.is_none());
        assert_eq!(page.status, Some(("saved".to_string(), false)));
    }

    #[test]
    fn thinking_row_cycles_and_saves() {
        static SAVES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut page = AiSettingsPage::with_config(
            AiConfig {
                api_url: String::new(),
                api_key: "k".into(),
                model: "m".into(),
                ..AiConfig::default()
            },
            |_| {
                SAVES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        );
        assert_eq!(page.config.thinking, ai_config::Thinking::Off);
        page.selected = THINKING_ROW;
        page.key(key(KeyCode::Enter));
        assert_eq!(page.config.thinking, ai_config::Thinking::Low);
        assert_eq!(SAVES.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(page.status, Some(("saved".to_string(), false)));
        assert_eq!(page.row_value(THINKING_ROW).0, "low");
        assert!(page.editing.is_none());
    }

    #[test]
    fn test_connection_is_disabled_until_configured() {
        let mut page = page(AiConfig::default());
        page.selected = TEST_ROW;
        page.key(key(KeyCode::Enter));
        assert!(!page.is_busy());
        assert!(page.testing.is_none());
    }

    #[test]
    fn esc_cancels_an_edit_without_changing_the_field() {
        let mut page = page(AiConfig {
            api_url: "https://a.example/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            ..AiConfig::default()
        });
        page.key(key(KeyCode::Enter));
        page.key(key(KeyCode::Char('x')));
        assert_eq!(page.key(key(KeyCode::Esc)), AiPageAction::None);
        assert!(page.editing.is_none());
        assert_eq!(page.config.api_url, "https://a.example/v1");
        assert_eq!(page.key(key(KeyCode::Esc)), AiPageAction::Back);
    }
}
