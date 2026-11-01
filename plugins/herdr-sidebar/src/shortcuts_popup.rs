//! The Keyboard shortcuts page as its own floating popup (`--shortcuts-popup`).
//!
//! herdr cannot resize an open popup, and the Settings popup must stay narrow
//! (its rows put the label and value apart), so the shortcuts page gets a
//! popup of its own at [`WIDTH`] × [`HEIGHT`]. It is reached two ways:
//!
//! - the `keyboard-shortcuts` action (`--shortcuts-popup-open`), which opens it
//!   directly: Esc and `q` close it;
//! - the Settings popup's `Keyboard shortcuts…` row, which closes the Settings
//!   popup and hands off to a detached `--shortcuts-popup-open` child. That
//!   popup carries the Settings context, so Esc spawns a detached
//!   `--settings-popup-open` child that reopens Settings on that row.
//!
//! herdr allows one popup per session. Both hand-offs therefore spawn a child
//! and exit first; the child retries `ui_busy` until the slot is free (see
//! [`crate::file_popup::retry_while_busy`]).
//!
//! When the popup cannot be opened (Windows, no pane, a refused request), the
//! Settings page keeps its in-pane Keyboard shortcuts sub-page instead.

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::file_popup;
use crate::keymap;
use crate::settings_popup;
use crate::shortcuts_page::{ShortcutsAction, ShortcutsPage};
use crate::state;
use crate::ui;

/// The `[[panes]]` id in `herdr-plugin.toml`.
pub const ENTRYPOINT: &str = "shortcuts";
/// The flag that opens the popup: the action, and the Settings hand-off child.
pub const OPEN_FLAG: &str = "--shortcuts-popup-open";
/// The popup's size, as a share of the terminal.
pub const WIDTH: &str = "86%";
pub const HEIGHT: &str = "88%";
/// `"1"` when a Settings popup opened this one, so Esc returns to Settings.
pub const ENV_RETURN: &str = "HERDR_SIDEBAR_SHORTCUTS_RETURN";

/// Idle redraw interval. Keys and mouse events wake the loop at once.
const POLL: Duration = Duration::from_millis(250);

/// The env this process forwards to the popup it opens, read through `read`:
/// the Settings context (when present) and the return marker. Empty for a
/// popup opened straight from the action. Pure.
pub fn forwarded_env(read: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut env = settings_popup::context_env(&read);
    env.extend(file_popup::env_from(&read, &[ENV_RETURN]));
    env
}

/// The env of the shortcuts child a Settings popup spawns: its context plus
/// the return marker, so this popup knows to go back. Pure.
pub fn from_settings_env(context: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut env = context;
    env.push((ENV_RETURN.to_string(), "1".to_string()));
    env
}

/// Whether Esc returns to Settings: this popup was opened from there. Pure.
pub fn returns_to_settings(read: impl Fn(&str) -> Option<String>) -> bool {
    read(ENV_RETURN).is_some_and(|value| value == "1")
}

// ── Action side ──────────────────────────────────────────────────────────

/// `--shortcuts-popup-open`: request the popup. Forwards the Settings context
/// and the return marker when this process carries them (it is the Settings
/// hand-off child); otherwise opens a plain popup.
pub fn open() -> io::Result<()> {
    let env = forwarded_env(|key| std::env::var(key).ok());
    let refs = file_popup::env_refs(&env);
    file_popup::retry_while_busy(
        || file_popup::request_popup_pane_with_env(ENTRYPOINT, WIDTH, HEIGHT, &refs),
        file_popup::BUSY_STEP,
        file_popup::BUSY_LIMIT,
    )
}

/// Esc in a popup opened from Settings: start the Settings popup again on the
/// Keyboard shortcuts row. The child is detached; this popup exits right after.
fn return_to_settings() -> io::Result<()> {
    let env = settings_popup::reopen_env(
        settings_popup::context_env(|key| std::env::var(key).ok()),
        settings_popup::SELECT_KEYBOARD_SHORTCUTS,
    );
    let caller = std::env::var("HERDR_CALLER_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty());
    file_popup::spawn_detached(settings_popup::REOPEN_FLAG, &env, caller.as_deref())
}

// ── Popup process ────────────────────────────────────────────────────────

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn draw(frame: &mut Frame, page: &mut ShortcutsPage, back: bool, notice: Option<&(String, bool)>) {
    let area = frame.area();
    let body_height = area.height.saturating_sub(1);
    let body = Rect::new(area.x, area.y, area.width, body_height);
    let footer = Rect::new(
        area.x,
        area.y + body_height,
        area.width,
        area.height - body_height,
    );
    page.draw(frame, body);
    let line = match notice {
        Some((text, true)) => {
            Span::styled(text.clone(), Style::default().fg(ui::palette().deleted))
        }
        Some((text, false)) => Span::styled(text.clone(), Style::default().dim()),
        None if back => Span::styled("esc settings · q close", Style::default().dim()),
        None => Span::styled("esc · q close", Style::default().dim()),
    };
    frame.render_widget(Paragraph::new(line), footer);
}

/// The `--shortcuts-popup` process: the Keyboard shortcuts page, full size,
/// until it closes, Ctrl+C, or Esc returns to a Settings popup.
pub fn run() -> io::Result<()> {
    state::migrate_settings();
    keymap::ensure_files();
    ui::set_color_theme(state::load_state().color_theme);
    let back = returns_to_settings(|key| std::env::var(key).ok());
    let mut page = ShortcutsPage::open();
    let mut notice: Option<(String, bool)> = None;

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let result = (|| -> io::Result<()> {
        loop {
            terminal.draw(|frame| draw(frame, &mut page, back, notice.as_ref()))?;
            if !event::poll(POLL)? {
                continue;
            }
            let action = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if is_ctrl_c(key) {
                        break;
                    }
                    notice = None;
                    page.key(key)
                }
                Event::Mouse(mouse) => {
                    notice = None;
                    page.mouse(mouse)
                }
                Event::Resize(..) => {
                    ui::repaint_after_resize(&mut terminal)?;
                    ShortcutsAction::None
                }
                _ => ShortcutsAction::None,
            };
            match action {
                ShortcutsAction::None => {}
                ShortcutsAction::Close => break,
                ShortcutsAction::Back if !back => break,
                ShortcutsAction::Back => match return_to_settings() {
                    Ok(()) => break,
                    // Stay open with the reason in the footer: the page is
                    // still usable, and Esc can be tried again.
                    Err(error) => {
                        notice = Some((format!("could not return to Settings: {error}"), true));
                    }
                },
            }
        }
        Ok(())
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn reader(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    const CONTEXT: [(&str, &str); 4] = [
        (settings_popup::ENV_UNIFIED_OK, "1"),
        (settings_popup::ENV_ROOT, "acme"),
        (settings_popup::ENV_ICONS, "material"),
        (settings_popup::ENV_HINTS, "[]"),
    ];

    #[test]
    fn a_direct_open_forwards_nothing() {
        // Only the unrelated variables are set, as in a plain action run.
        let read = reader(&[("HERDR_PANE_ID", "w1-p1"), ("PATH", "/bin")]);
        assert!(forwarded_env(&read).is_empty());
        assert!(!returns_to_settings(&read));
    }

    #[test]
    fn a_settings_child_forwards_context_and_the_return_marker() {
        let mut pairs = CONTEXT.to_vec();
        pairs.push((ENV_RETURN, "1"));
        pairs.push(("HERDR_PLUGIN_STATE_DIR", "/state"));
        let env = forwarded_env(reader(&pairs));
        let mut expected: Vec<(String, String)> = CONTEXT
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        expected.push((ENV_RETURN.to_string(), "1".to_string()));
        assert_eq!(env, expected);
    }

    #[test]
    fn empty_context_values_are_not_forwarded() {
        let env = forwarded_env(reader(&[(settings_popup::ENV_ROOT, "")]));
        assert!(env.is_empty());
    }

    #[test]
    fn the_settings_hand_off_marks_the_return() {
        let context = vec![(settings_popup::ENV_ROOT.to_string(), "acme".to_string())];
        let env = from_settings_env(context.clone());
        assert_eq!(&env[..1], context.as_slice());
        assert_eq!(env.last(), Some(&(ENV_RETURN.to_string(), "1".to_string())));
        // The marker reads back as "return to Settings".
        let read = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert!(returns_to_settings(read));
    }

    #[test]
    fn only_the_settings_marker_value_returns() {
        assert!(returns_to_settings(reader(&[(ENV_RETURN, "1")])));
        assert!(!returns_to_settings(reader(&[(ENV_RETURN, "0")])));
        assert!(!returns_to_settings(reader(&[])));
    }

    #[test]
    fn the_reopened_settings_popup_selects_the_shortcuts_row() {
        let env = settings_popup::reopen_env(
            settings_popup::context_env(reader(&CONTEXT)),
            settings_popup::SELECT_KEYBOARD_SHORTCUTS,
        );
        let select = env
            .iter()
            .find(|(k, _)| k == settings_popup::ENV_SELECT)
            .map(|(_, v)| v.as_str());
        assert_eq!(select, Some("keyboard-shortcuts"));
        assert_eq!(
            settings_popup::setting_named(select.unwrap_or_default()),
            Some(crate::settings_page::Setting::KeyboardShortcuts)
        );
    }
}
