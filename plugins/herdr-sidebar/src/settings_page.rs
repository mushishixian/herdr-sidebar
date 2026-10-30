//! The ⚙ Settings modal, shared by the Explorer and Source Control views (and
//! by the full-screen settings popup). The page owns its rows, the key and
//! mouse handling and the drawing. Everything it changes that lives in the
//! host app — the layout, the icon theme, the folder, the unified mode — goes
//! through [`SettingsHost`], so the page depends on no sidebar app.
//!
//! Settings that persist are written to `state.json` here (see
//! [`crate::state::update_state`]); the host then gets [`SettingsHost::state_changed`]
//! with the fresh value and runs whatever side effect that setting needs.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::ai_settings::{self, AiPageAction, AiSettingsPage};
use crate::icons::IconTheme;
use crate::state::{self, State};
use crate::ui::{
    SelectAll, TextKey, draw_input_dialog, hits, keep_visible_scroll, selection_style,
    set_color_theme, text_input_key, truncate_to, wrap_hints,
};
use crate::{actions, pane_layout, updates};

/// One row of the Settings modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Version,
    Update,
    UnifiedSidebar,
    DockRight,
    SidebarWidth,
    /// Default height, in rows, of the bottom pane of the IDE layout's
    /// editor/terminal column.
    TerminalHeight,
    /// Default width, in columns, of the IDE layout's rightmost Agent column.
    AgentWidth,
    IconTheme,
    ColorTheme,
    PreviewPlacement,
    CustomEditorCommand,
    CustomEditorClick,
    CustomEditorInline,
    AutoOpen,
    StrictToggle,
    FocusOnOpen,
    FollowCwd,
    HiddenFiles,
    Hotkeys,
    GitDecorations,
    GitFooter,
    /// Source Control's list/tree mode (`scm_tree_view`).
    ScmViewMode,
    /// Ask before deleting a file or folder from the Explorer.
    ConfirmDelete,
    /// Opens the AI commit page; the value is the configured model.
    AiCommit,
    Folder,
}

/// (setting, label, current value, enabled) — disabled rows render dimmed and
/// don't toggle.
pub type SettingRow = (Setting, &'static str, String, bool);

/// What the host app must answer for the page.
pub trait SettingsHost {
    /// The unified sidebar can be switched on or off (the other view's
    /// executable is present).
    fn unified_available(&self) -> bool;
    /// Flip unified mode. The page is already closed when this runs (the
    /// layout changes underneath it).
    fn set_unified(&mut self, on: bool);
    /// Open the change-folder flow. The page is already closed when this runs.
    fn change_folder(&mut self);
    /// The name of the folder the sidebar is rooted at.
    fn root_name(&self) -> String;
    fn icon_theme(&self) -> IconTheme;
    /// Switch icon themes and remember the choice.
    fn set_icon_theme(&mut self, theme: IconTheme);
    /// `state.json` was just written for `setting`; `state` is the value
    /// `update_state` returned. The host adopts it and runs the side effect
    /// that setting has in that view.
    fn state_changed(&mut self, setting: Setting, state: &State);
    /// The sidebar pane the layout rows act on; `None` when there is no herdr
    /// pane (the layout rows then only store their value).
    fn pane_id(&self) -> Option<String>;
    /// A one-line message for the host's footer. `error` marks a failure.
    fn notice(&mut self, text: String, error: bool);
    /// Hotkey chips shown under the rows.
    fn hints(&self) -> Vec<(&'static str, &'static str)>;
}

/// What the owner should do with the page after a key or mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Keep the page open.
    Stay,
    /// The page is dismissed.
    Close,
}

/// What is drawn over the rows.
enum Sub {
    /// The rows themselves.
    Rows,
    /// The AI commit page, opened from the AI commit row.
    Ai(AiSettingsPage),
    /// The one-line custom editor command, opened from its row.
    Editor {
        input: String,
        select_all: SelectAll,
    },
}

/// The ⚙ Settings modal. The host keeps it in its overlay slot and passes
/// itself in as the [`SettingsHost`] for every key, mouse event and draw.
pub struct SettingsPage {
    selected: usize,
    scroll: usize,
    /// Outer box from the last draw, for mouse hit-testing.
    rect: Rect,
    sub: Sub,
}

impl SettingsPage {
    /// The page for a fresh open. Also starts the background update check the
    /// Update row reports on.
    pub fn open() -> Self {
        updates::check();
        Self::new()
    }

    fn new() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            rect: Rect::default(),
            sub: Sub::Rows,
        }
    }

    /// The modal's rows for the current state.
    pub fn rows(&self, host: &dyn SettingsHost) -> Vec<SettingRow> {
        let state = state::load_state();
        let update = updates::row();
        let editor = actions::configured_editor();
        let on_off = |on: bool| if on { "on" } else { "off" }.to_string();
        let shown = |on: bool| if on { "shown" } else { "hidden" }.to_string();
        let unified = host.unified_available();
        vec![
            (
                Setting::Version,
                "Sidebar version",
                updates::VERSION.into(),
                false,
            ),
            (Setting::Update, update.0, update.1, update.2),
            (
                Setting::UnifiedSidebar,
                "Unified sidebar",
                on_off(state.merged && unified),
                unified,
            ),
            (
                Setting::DockRight,
                "Dock on the right",
                on_off(state.dock_right),
                true,
            ),
            (
                Setting::SidebarWidth,
                "Sidebar width",
                format!("{} cols", state.sidebar_width),
                true,
            ),
            (
                Setting::TerminalHeight,
                "Terminal height",
                match state.terminal_height {
                    Some(rows) => format!("{rows} rows"),
                    None => "auto".to_string(),
                },
                true,
            ),
            (
                Setting::AgentWidth,
                "Agent width",
                match state.agent_width {
                    Some(cols) => format!("{cols} cols"),
                    None => "auto".to_string(),
                },
                true,
            ),
            (
                Setting::IconTheme,
                "Icon theme",
                match host.icon_theme() {
                    IconTheme::Material => "material",
                    IconTheme::Emoji => "emoji",
                }
                .to_string(),
                true,
            ),
            (
                Setting::ColorTheme,
                "Color theme",
                state.color_theme.label().to_string(),
                true,
            ),
            (
                Setting::PreviewPlacement,
                "Preview opens in",
                state.preview_placement.label().to_string(),
                true,
            ),
            (
                Setting::CustomEditorCommand,
                "Custom editor...",
                editor
                    .clone()
                    .map(|command| truncate_to(command, 24))
                    .unwrap_or_else(|| "not set".into()),
                true,
            ),
            (
                Setting::CustomEditorClick,
                "Use editor on click",
                on_off(state.custom_editor_on_click),
                editor.is_some(),
            ),
            (
                Setting::CustomEditorInline,
                "Editor opens",
                if state.custom_editor_inline {
                    "inline"
                } else {
                    "tab"
                }
                .to_string(),
                editor.is_some(),
            ),
            (
                Setting::AutoOpen,
                "Auto-open sidebar",
                on_off(state.auto_open),
                true,
            ),
            (
                Setting::StrictToggle,
                "Strict toggle",
                on_off(state.strict_toggle),
                true,
            ),
            (
                Setting::FocusOnOpen,
                "Focus on open",
                on_off(state.focus_on_open),
                true,
            ),
            (
                Setting::FollowCwd,
                "Follow pane folder",
                state::follow_cwd_setting_value(state.follow_cwd),
                true,
            ),
            (
                Setting::HiddenFiles,
                "Hidden files",
                shown(state.show_hidden),
                true,
            ),
            (
                Setting::Hotkeys,
                "Footer hotkeys",
                shown(state.show_hotkeys),
                true,
            ),
            (
                Setting::GitDecorations,
                "Git decorations",
                on_off(state.git_deco),
                true,
            ),
            (
                Setting::GitFooter,
                "Git footer",
                shown(state.show_git_footer),
                true,
            ),
            (
                Setting::ScmViewMode,
                "View as",
                if state.scm_tree_view { "tree" } else { "list" }.to_string(),
                true,
            ),
            (
                Setting::ConfirmDelete,
                "Confirm delete",
                on_off(state.confirm_delete),
                true,
            ),
            (
                Setting::AiCommit,
                "AI commit…",
                ai_settings::model_value(),
                true,
            ),
            (Setting::Folder, "Change folder…", host.root_name(), true),
        ]
    }

    /// Keyboard input. Esc/q/s close; ↑↓ move; ←→ step the layout rows;
    /// Enter/Space activate the selected row.
    pub fn key(&mut self, key: KeyEvent, host: &mut dyn SettingsHost) -> Outcome {
        match &mut self.sub {
            Sub::Ai(page) => {
                let action = page.key(key);
                return self.ai_done(action, host);
            }
            Sub::Editor { .. } => return self.editor_key(key, host),
            Sub::Rows => {}
        }
        let row_count = self.rows(host).len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Outcome::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(row_count.saturating_sub(1));
                Outcome::Stay
            }
            KeyCode::Left | KeyCode::Char('h') => self.step(false, host),
            KeyCode::Right | KeyCode::Char('l') => self.step(true, host),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(self.selected, host),
            _ => Outcome::Stay,
        }
    }

    /// Mouse input. Hover selects a row; a click on a row activates it; a click
    /// inside the box on no row does nothing; a click outside closes.
    pub fn mouse(&mut self, mouse: MouseEvent, host: &mut dyn SettingsHost) -> Outcome {
        match &mut self.sub {
            Sub::Ai(page) => {
                let action = page.mouse(mouse);
                return self.ai_done(action, host);
            }
            // The editor dialog is keyboard-driven; clicks do nothing.
            Sub::Editor { .. } => return Outcome::Stay,
            Sub::Rows => {}
        }
        let row_count = self.rows(host).len();
        let rect = self.rect;
        let scroll = self.scroll;
        // Rows start just inside the top border (the title renders ON the
        // border, not on its own line).
        let row_at = |row: u16, col: u16| -> Option<usize> {
            let index = usize::from(row.saturating_sub(rect.y + 1)) + scroll;
            (col > rect.x
                && col < rect.x + rect.width.saturating_sub(1)
                && row > rect.y
                && row < rect.y + rect.height.saturating_sub(1)
                && index < row_count)
                .then_some(index)
        };
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(i) = row_at(mouse.row, mouse.column) {
                    self.selected = i;
                }
                Outcome::Stay
            }
            MouseEventKind::Down(MouseButton::Left) => match row_at(mouse.row, mouse.column) {
                Some(i) => {
                    self.selected = i;
                    self.activate(i, host)
                }
                None if hits(rect, mouse.column, mouse.row) => Outcome::Stay,
                None => Outcome::Close,
            },
            _ => Outcome::Stay,
        }
    }

    /// Draw the page inside `area`. `fill = false` draws the centered box
    /// (the in-pane modal); `fill = true` uses all of `area`.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, fill: bool, host: &dyn SettingsHost) {
        if let Sub::Ai(page) = &mut self.sub {
            page.draw(frame);
            return;
        }
        let rows = self.rows(host);
        let desired_width = rows
            .iter()
            .map(|(_, label, value, _)| label.chars().count() + value.chars().count() + 5)
            .max()
            .unwrap_or(30)
            .max(30) as u16;
        let width = if fill {
            area.width
        } else {
            desired_width.min(area.width)
        };
        // The hotkey reference lives here; the footer chips are opt-in.
        let hint_lines = wrap_hints(&host.hints(), width.saturating_sub(2), 0);
        let height = if fill {
            area.height
        } else {
            (rows.len() as u16 + 5 + hint_lines.len() as u16).min(area.height)
        };
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        self.rect = popup;
        let inner_height = usize::from(height.saturating_sub(2));
        let content_height = rows.len() + 3 + hint_lines.len();
        self.scroll = keep_visible_scroll(self.selected, inner_height, content_height);

        let inner_w = usize::from(width.saturating_sub(2));
        let mut lines: Vec<Line> = Vec::new();
        for (i, (_, label, value, enabled)) in rows.iter().enumerate() {
            let pad = inner_w.saturating_sub(label.chars().count() + value.chars().count() + 2);
            let text = format!(" {label}{}{value} ", " ".repeat(pad.max(1)));
            let style = if !enabled {
                Style::default().dim()
            } else if i == self.selected {
                selection_style(true)
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Hotkeys",
            Style::default().bold(),
        )));
        lines.extend(hint_lines);
        lines.push(Line::from(" click/⏎ · ←/→ adjust · esc".dim()));

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).scroll((self.scroll as u16, 0)).block(
                Block::bordered()
                    .title(" Settings ")
                    .border_style(Style::default().dim()),
            ),
            popup,
        );
        if let Sub::Editor { input, select_all } = &self.sub {
            draw_input_dialog(frame, "Custom editor", input, select_all.armed());
        }
    }

    /// The AI page asked for something: Back returns to the rows with the AI
    /// commit row selected, Close dismisses the whole page.
    fn ai_done(&mut self, action: AiPageAction, host: &dyn SettingsHost) -> Outcome {
        match action {
            AiPageAction::None => Outcome::Stay,
            AiPageAction::Close => Outcome::Close,
            AiPageAction::Back => {
                self.selected = self
                    .rows(host)
                    .iter()
                    .position(|row| row.0 == Setting::AiCommit)
                    .unwrap_or(0);
                self.scroll = 0;
                self.sub = Sub::Rows;
                Outcome::Stay
            }
        }
    }

    /// The custom editor dialog: Enter saves the command and returns to the
    /// rows, Esc returns without saving.
    fn editor_key(&mut self, key: KeyEvent, host: &mut dyn SettingsHost) -> Outcome {
        let Sub::Editor { input, select_all } = &mut self.sub else {
            return Outcome::Stay;
        };
        if text_input_key(key, input, select_all) != TextKey::Other {
            return Outcome::Stay;
        }
        match key.code {
            KeyCode::Enter => {
                let saved = state::save_editor_command(input.as_str());
                self.sub = Sub::Rows;
                if saved {
                    host.notice(
                        "custom editor saved; Enter still previews files".into(),
                        false,
                    );
                } else {
                    host.notice(
                        "custom editor command must be one non-empty line".into(),
                        true,
                    );
                }
            }
            KeyCode::Esc => self.sub = Sub::Rows,
            _ => {}
        }
        Outcome::Stay
    }

    /// `←`/`→` on the selected row: only the three layout rows step.
    fn step(&mut self, wider: bool, host: &mut dyn SettingsHost) -> Outcome {
        let rows = self.rows(host);
        if let Some(&(setting, ..)) = rows.get(self.selected)
            && matches!(
                setting,
                Setting::SidebarWidth | Setting::TerminalHeight | Setting::AgentWidth
            )
        {
            adjust(setting, wider, host);
        }
        Outcome::Stay
    }

    /// Enter/Space/click on row `index`.
    fn activate(&mut self, index: usize, host: &mut dyn SettingsHost) -> Outcome {
        let rows = self.rows(host);
        let Some(&(setting, _, _, enabled)) = rows.get(index) else {
            return Outcome::Stay;
        };
        if !enabled {
            return Outcome::Stay;
        }
        match setting {
            Setting::Version => {}
            Setting::Update => match updates::activate() {
                Ok(()) => host.notice("Update requested".into(), false),
                Err(error) => host.notice(error, true),
            },
            Setting::UnifiedSidebar => {
                let on = !state::load_state().merged;
                self.sub = Sub::Rows;
                host.set_unified(on);
                return Outcome::Close;
            }
            Setting::Folder => {
                host.change_folder();
                return Outcome::Close;
            }
            Setting::IconTheme => host.set_icon_theme(host.icon_theme().toggled()),
            Setting::ColorTheme => {
                let state = state::update_state(|s| s.color_theme = s.color_theme.next());
                set_color_theme(state.color_theme);
                host.state_changed(setting, &state);
            }
            Setting::SidebarWidth => write(host, setting, |s| {
                s.sidebar_width = state::step_sidebar_width(s.sidebar_width, true);
            }),
            Setting::TerminalHeight => write(host, setting, |s| s.terminal_height = None),
            Setting::AgentWidth => write(host, setting, |s| s.agent_width = None),
            Setting::DockRight => write(host, setting, |s| s.dock_right = !s.dock_right),
            Setting::PreviewPlacement => write(host, setting, |s| {
                s.preview_placement = s.preview_placement.other();
            }),
            Setting::CustomEditorCommand => {
                self.sub = Sub::Editor {
                    input: actions::configured_editor().unwrap_or_default(),
                    select_all: SelectAll::default(),
                };
            }
            Setting::CustomEditorClick => write(host, setting, |s| {
                s.custom_editor_on_click = !s.custom_editor_on_click;
            }),
            Setting::CustomEditorInline => write(host, setting, |s| {
                s.custom_editor_inline = !s.custom_editor_inline;
            }),
            Setting::AutoOpen => write(host, setting, |s| s.auto_open = !s.auto_open),
            Setting::StrictToggle => write(host, setting, |s| s.strict_toggle = !s.strict_toggle),
            Setting::FocusOnOpen => write(host, setting, |s| s.focus_on_open = !s.focus_on_open),
            Setting::FollowCwd => write(host, setting, |s| s.follow_cwd = !s.follow_cwd),
            Setting::HiddenFiles => write(host, setting, |s| s.show_hidden = !s.show_hidden),
            Setting::Hotkeys => write(host, setting, |s| s.show_hotkeys = !s.show_hotkeys),
            Setting::GitDecorations => write(host, setting, |s| s.git_deco = !s.git_deco),
            Setting::GitFooter => write(host, setting, |s| {
                s.show_git_footer = !s.show_git_footer;
            }),
            Setting::ScmViewMode => write(host, setting, |s| s.scm_tree_view = !s.scm_tree_view),
            Setting::ConfirmDelete => {
                write(host, setting, |s| s.confirm_delete = !s.confirm_delete)
            }
            Setting::AiCommit => self.sub = Sub::Ai(AiSettingsPage::open()),
        }
        Outcome::Stay
    }
}

/// Persist one change to `state.json` and tell the host what it became.
fn write(host: &mut dyn SettingsHost, setting: Setting, update: impl FnOnce(&mut State)) {
    let state = state::update_state(update);
    host.state_changed(setting, &state);
}

/// The tab of `pane_id` when the page runs inside that pane (the sidebar
/// itself): the layout helpers then act on the tab the user is looking at.
/// Otherwise the helpers resolve the tab through the pane.
fn tab_of(pane_id: &str) -> Option<String> {
    let own = std::env::var("HERDR_PANE_ID").ok()?;
    if own != pane_id {
        return None;
    }
    std::env::var("HERDR_TAB_ID")
        .ok()
        .filter(|id| !id.is_empty())
}

/// `←`/`→` on a layout row: step the stored default and, when the host has a
/// pane, size THIS tab's split right away. The setting is a default (the IDE
/// layer's remembered per-project ratio still wins when a layout is built);
/// applying it here just makes the row feel immediate.
fn adjust(setting: Setting, wider: bool, host: &mut dyn SettingsHost) {
    let pane = host.pane_id();
    match setting {
        Setting::SidebarWidth => write(host, setting, |s| {
            s.sidebar_width = state::step_sidebar_width(s.sidebar_width, wider);
        }),
        Setting::TerminalHeight => {
            // Step from the STORED value: a failed apply leaves the tab's
            // layout alone, so re-measuring it every press would land on the
            // same two values forever.
            let seed = match state::load_state().terminal_height {
                Some(rows) => rows,
                None => pane
                    .as_deref()
                    .and_then(pane_layout::terminal_rows_now)
                    .unwrap_or(state::SEED_TERMINAL_HEIGHT),
            };
            let rows = state::step_terminal_height(seed, wider);
            let state = state::update_state(|s| s.terminal_height = Some(rows));
            host.state_changed(setting, &state);
            let applied = pane
                .as_deref()
                .is_some_and(|id| pane_layout::set_terminal_rows(id, tab_of(id).as_deref(), rows));
            if !applied {
                // Keep the stored value: it still applies to tabs built later.
                host.notice(
                    "Terminal height: this tab has no editor/terminal split".into(),
                    false,
                );
            }
        }
        Setting::AgentWidth => {
            let seed = match state::load_state().agent_width {
                Some(cols) => cols,
                None => pane
                    .as_deref()
                    .and_then(pane_layout::agent_cols_now)
                    .unwrap_or(state::SEED_AGENT_WIDTH),
            };
            let cols = state::step_agent_width(seed, wider);
            let state = state::update_state(|s| s.agent_width = Some(cols));
            host.state_changed(setting, &state);
            let applied = pane
                .as_deref()
                .is_some_and(|id| pane_layout::set_agent_cols(id, tab_of(id).as_deref(), cols));
            if !applied {
                // Keep the stored value: it still applies to tabs built later.
                host.notice("Agent width: this tab has no Agent column".into(), false);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// Records every host call; the page's state writes go to a scratch
    /// state dir (see [`isolated_state`]).
    struct FakeHost {
        unified: bool,
        pane: Option<String>,
        calls: RefCell<Vec<String>>,
        notices: Vec<(String, bool)>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                unified: true,
                pane: None,
                calls: RefCell::new(Vec::new()),
                notices: Vec::new(),
            }
        }

        fn called(&self, call: &str) -> bool {
            self.calls.borrow().iter().any(|c| c == call)
        }
    }

    impl SettingsHost for FakeHost {
        fn unified_available(&self) -> bool {
            self.unified
        }
        fn set_unified(&mut self, on: bool) {
            self.calls.borrow_mut().push(format!("unified:{on}"));
        }
        fn change_folder(&mut self) {
            self.calls.borrow_mut().push("folder".into());
        }
        fn root_name(&self) -> String {
            "acme".into()
        }
        fn icon_theme(&self) -> IconTheme {
            IconTheme::Material
        }
        fn set_icon_theme(&mut self, theme: IconTheme) {
            self.calls.borrow_mut().push(format!("icons:{theme:?}"));
        }
        fn state_changed(&mut self, setting: Setting, _state: &State) {
            self.calls.borrow_mut().push(format!("state:{setting:?}"));
        }
        fn pane_id(&self) -> Option<String> {
            self.pane.clone()
        }
        fn notice(&mut self, text: String, error: bool) {
            self.notices.push((text, error));
        }
        fn hints(&self) -> Vec<(&'static str, &'static str)> {
            Vec::new()
        }
    }

    /// Point this thread's state dir at a fresh scratch directory (see
    /// [`state::TEST_STATE_DIR`]).
    fn isolated_state() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir: PathBuf =
            std::env::temp_dir().join(format!("hs-settings-page-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch state dir");
        state::TEST_STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn index_of(page: &SettingsPage, host: &FakeHost, setting: Setting) -> usize {
        page.rows(host)
            .iter()
            .position(|row| row.0 == setting)
            .expect("row present")
    }

    /// Move the selection onto `setting`.
    fn select(page: &mut SettingsPage, host: &mut FakeHost, setting: Setting) {
        page.selected = index_of(page, host, setting);
    }

    #[test]
    fn every_setting_has_exactly_one_row_in_order() {
        isolated_state();
        let host = FakeHost::new();
        let page = SettingsPage::new();
        let rows: Vec<Setting> = page.rows(&host).iter().map(|row| row.0).collect();
        let expected = [
            Setting::Version,
            Setting::Update,
            Setting::UnifiedSidebar,
            Setting::DockRight,
            Setting::SidebarWidth,
            Setting::TerminalHeight,
            Setting::AgentWidth,
            Setting::IconTheme,
            Setting::ColorTheme,
            Setting::PreviewPlacement,
            Setting::CustomEditorCommand,
            Setting::CustomEditorClick,
            Setting::CustomEditorInline,
            Setting::AutoOpen,
            Setting::StrictToggle,
            Setting::FocusOnOpen,
            Setting::FollowCwd,
            Setting::HiddenFiles,
            Setting::Hotkeys,
            Setting::GitDecorations,
            Setting::GitFooter,
            Setting::ScmViewMode,
            Setting::ConfirmDelete,
            Setting::AiCommit,
            Setting::Folder,
        ];
        assert_eq!(rows, expected);
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        for _ in 0..100 {
            assert_eq!(page.key(key(KeyCode::Down), &mut host), Outcome::Stay);
        }
        assert_eq!(page.selected, 24);
        for _ in 0..100 {
            page.key(key(KeyCode::Up), &mut host);
        }
        assert_eq!(page.selected, 0);
    }

    #[test]
    fn enter_on_dock_right_flips_the_saved_state() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::DockRight);
        let before = state::load_state().dock_right;
        page.key(key(KeyCode::Enter), &mut host);
        assert_eq!(state::load_state().dock_right, !before);
        assert!(host.called("state:DockRight"));
    }

    #[test]
    fn right_on_sidebar_width_steps_the_width() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::SidebarWidth);
        let before = state::load_state().sidebar_width;
        page.key(key(KeyCode::Right), &mut host);
        assert_eq!(
            state::load_state().sidebar_width,
            state::step_sidebar_width(before, true)
        );
        assert!(host.called("state:SidebarWidth"));
    }

    #[test]
    fn hidden_files_row_toggles_the_persisted_flag() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::HiddenFiles);
        assert!(state::load_state().show_hidden);
        page.key(key(KeyCode::Enter), &mut host);
        assert!(!state::load_state().show_hidden);
        assert!(host.called("state:HiddenFiles"));
    }

    #[test]
    fn esc_and_s_close_the_page() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        assert_eq!(page.key(key(KeyCode::Esc), &mut host), Outcome::Close);
        assert_eq!(page.key(key(KeyCode::Char('s')), &mut host), Outcome::Close);
    }

    #[test]
    fn enter_on_unified_closes_and_flips_the_mode() {
        isolated_state();
        let mut host = FakeHost::new();
        // The default state is unified on, so Enter switches it off.
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::UnifiedSidebar);
        assert_eq!(page.key(key(KeyCode::Enter), &mut host), Outcome::Close);
        assert!(host.called("unified:false"));

        state::update_state(|s| s.merged = false);
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::UnifiedSidebar);
        assert_eq!(page.key(key(KeyCode::Enter), &mut host), Outcome::Close);
        assert!(host.called("unified:true"));
    }

    #[test]
    fn unavailable_unified_row_does_nothing() {
        isolated_state();
        let mut host = FakeHost::new();
        host.unified = false;
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::UnifiedSidebar);
        assert_eq!(page.key(key(KeyCode::Enter), &mut host), Outcome::Stay);
        assert!(host.calls.borrow().is_empty());
    }

    #[test]
    fn ai_page_back_returns_to_the_ai_row() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::AiCommit);
        assert_eq!(page.key(key(KeyCode::Enter), &mut host), Outcome::Stay);
        assert!(matches!(page.sub, Sub::Ai(_)));
        // Esc is the AI page's "Back".
        assert_eq!(page.key(key(KeyCode::Esc), &mut host), Outcome::Stay);
        assert!(matches!(page.sub, Sub::Rows));
        assert_eq!(page.selected, index_of(&page, &host, Setting::AiCommit));
    }

    #[test]
    fn custom_editor_saves_on_enter_and_returns_to_the_rows() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::CustomEditorCommand);
        page.key(key(KeyCode::Enter), &mut host);
        assert!(matches!(page.sub, Sub::Editor { .. }));
        // Ctrl+A arms the prefilled value (if any) so the typed text replaces it.
        page.key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut host,
        );
        for c in "nano".chars() {
            page.key(key(KeyCode::Char(c)), &mut host);
        }
        assert_eq!(page.key(key(KeyCode::Enter), &mut host), Outcome::Stay);
        assert!(matches!(page.sub, Sub::Rows));
        assert_eq!(state::load_editor_command().as_deref(), Some("nano"));
        assert_eq!(
            host.notices.last(),
            Some(&(
                "custom editor saved; Enter still previews files".to_string(),
                false
            ))
        );
    }

    #[test]
    fn custom_editor_esc_leaves_the_saved_command_alone() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::CustomEditorCommand);
        page.key(key(KeyCode::Enter), &mut host);
        page.key(key(KeyCode::Char('x')), &mut host);
        assert_eq!(page.key(key(KeyCode::Esc), &mut host), Outcome::Stay);
        assert!(matches!(page.sub, Sub::Rows));
        assert_eq!(state::load_editor_command(), None);
    }

    #[test]
    fn layout_row_without_a_pane_reports_and_keeps_the_stored_value() {
        isolated_state();
        let mut host = FakeHost::new();
        let mut page = SettingsPage::new();
        select(&mut page, &mut host, Setting::AgentWidth);
        page.key(key(KeyCode::Right), &mut host);
        assert!(state::load_state().agent_width.is_some());
        assert_eq!(
            host.notices.last(),
            Some(&(
                "Agent width: this tab has no Agent column".to_string(),
                false
            ))
        );
    }
}
