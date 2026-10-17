//! Global file search popup (Quick Open modal).
//!
//! Two halves:
//! - `--file-popup-open`: Invoked as a plugin ACTION (e.g. bound to `ctrl+q` host key).
//!   Resolves the focused pane's cwd and asks herdr to open a session-modal popup pane.
//! - `--file-popup`: The popup process itself: a standalone one-shot Ratatui TUI that
//!   scans and fuzzy-matches files across the workspace/repository. Pressing Enter
//!   opens the selected file in the workspace and exits, closing the popup.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use crate::file_search::{
    QuickFile, QuickIndex, QUICK_OPEN_FILE_LIMIT, collect_quick_files, quick_matches,
    truncate_path_tail,
};
use crate::icons::{self, IconTheme};
use crate::ui::{input_tail, keep_visible_scroll, palette, selection_style, truncate_to};

// ── Action side: request Herdr to spawn popup pane ──────────────────────

/// Request Herdr to open the file search popup for the focused pane's directory.
pub fn open() -> io::Result<()> {
    request_popup_pane("file-search", "70%", "60%")
}

/// Ask Herdr to spawn the popup pane `entrypoint` (a manifest `[[panes]]` id)
/// over the focused pane. Shared by every session-modal search popup: the caller
/// pane and its cwd travel in the spawn env so the popup can act on them.
pub fn request_popup_pane(entrypoint: &str, width: &str, height: &str) -> io::Result<()> {
    let mut env = crate::state::spawn_env();
    let caller_pane_id = focused_pane_id();
    let caller_cwd = focused_cwd();
    if let Some(env) = env.as_object_mut() {
        if let Some(cwd) = &caller_cwd {
            env.insert(
                crate::state::SPAWN_CWD_ENV.to_string(),
                serde_json::Value::String(cwd.clone()),
            );
        }
        if let Some(pane_id) = &caller_pane_id {
            env.insert(
                "HERDR_CALLER_PANE_ID".to_string(),
                serde_json::Value::String(pane_id.clone()),
            );
        }
    }
    crate::ipc::call_text(
        "plugin.pane.open",
        serde_json::json!({
            "plugin_id": "herdr-sidebar",
            "entrypoint": entrypoint,
            "placement": "popup",
            "width": width,
            "height": height,
            "env": env,
        }),
    )
    .map(drop)
}

fn focused_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| {
            let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
            let context: serde_json::Value = serde_json::from_str(&context).ok()?;
            ["focused_pane_id", "pane_id"]
                .iter()
                .find_map(|key| context.get(*key)?.as_str().map(str::to_string))
        })
}

fn focused_cwd() -> Option<String> {
    let pane_id = focused_pane_id()?;
    let reply =
        crate::ipc::call_text("pane.get", serde_json::json!({ "pane_id": pane_id })).ok()?;
    let reply: serde_json::Value = serde_json::from_str(crate::launch::strip_bom(&reply)).ok()?;
    let pane = &reply["result"]["pane"];
    pane["foreground_cwd"]
        .as_str()
        .or(pane["cwd"].as_str())
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

// ── Popup TUI process side ──────────────────────────────────────────────

pub fn run() -> io::Result<()> {
    if let Some(cwd) = std::env::var_os(crate::state::SPAWN_CWD_ENV).filter(|cwd| !cwd.is_empty()) {
        let _ = std::env::set_current_dir(cwd);
    }
    crate::ui::set_color_theme(crate::state::load_state().color_theme);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let caller_pane_id = std::env::var("HERDR_CALLER_PANE_ID").ok();
    let root = crate::git::Git::discover(&cwd)
        .map(|g| g.root().to_path_buf())
        .unwrap_or_else(|_| cwd.clone());

    let mut app = App::new(root, caller_pane_id);

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let mut needs_draw = true;
    let result = (|| -> io::Result<()> {
        while !app.done {
            if app.poll_index() {
                needs_draw = true;
            }
            if needs_draw {
                terminal.draw(|frame| app.draw(frame))?;
                needs_draw = false;
            }
            let timeout = if app.loading {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(250)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        app.key(key);
                        needs_draw = true;
                    }
                    Event::Mouse(mouse) => {
                        app.mouse(mouse);
                        needs_draw = true;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

struct App {
    root: PathBuf,
    caller_pane_id: Option<String>,
    files: Arc<Vec<QuickFile>>,
    matches: Vec<usize>,
    query: String,
    selected: usize,
    scroll: usize,
    truncated: bool,
    loading: bool,
    done: bool,
    error: Option<String>,
    rx: Option<mpsc::Receiver<QuickIndex>>,
    theme: IconTheme,
    list_area: Rect,
}

impl App {
    fn new(root: PathBuf, caller_pane_id: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let (files, truncated) =
                collect_quick_files(&worker_root, false, QUICK_OPEN_FILE_LIMIT);
            let _ = tx.send(QuickIndex {
                root: worker_root,
                show_hidden: false,
                files: Arc::new(files),
                truncated,
            });
        });
        let state = crate::state::load_state();
        let theme = icons::IconTheme::resolve(None, state.icons);
        Self {
            root,
            caller_pane_id,
            files: Arc::new(Vec::new()),
            matches: Vec::new(),
            query: String::new(),
            selected: 0,
            scroll: 0,
            truncated: false,
            loading: true,
            done: false,
            error: None,
            rx: Some(rx),
            theme,
            list_area: Rect::default(),
        }
    }

    fn poll_index(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(index) => {
                self.rx = None;
                self.files = Arc::clone(&index.files);
                self.truncated = index.truncated;
                self.loading = false;
                self.matches = quick_matches(&self.files, &self.query);
                self.selected = self.selected.min(self.matches.len().saturating_sub(1));
                true
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.loading = false;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn on_query_changed(&mut self) {
        self.matches = quick_matches(&self.files, &self.query);
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
    }

    fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
        }
    }

    fn select_page_up(&mut self) {
        let step = usize::from(self.list_area.height).max(1);
        self.selected = self.selected.saturating_sub(step);
    }

    fn select_page_down(&mut self) {
        let step = usize::from(self.list_area.height).max(1);
        if !self.matches.is_empty() {
            self.selected = (self.selected + step).min(self.matches.len().saturating_sub(1));
        }
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.done = true,
            KeyCode::Char('q') | KeyCode::Char('c') if ctrl => self.done = true,
            KeyCode::Up => self.select_prev(),
            KeyCode::Down => self.select_next(),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => self.select_prev(),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.select_next(),
            KeyCode::PageUp => self.select_page_up(),
            KeyCode::PageDown => self.select_page_down(),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => {
                if !self.matches.is_empty() {
                    self.selected = self.matches.len().saturating_sub(1);
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.on_query_changed();
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.on_query_changed();
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Char(c) if !ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(c);
                self.on_query_changed();
            }
            _ => {}
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        let total = self.matches.len();
        match mouse.kind {
            MouseEventKind::ScrollUp => self.select_prev(),
            MouseEventKind::ScrollDown => self.select_next(),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                if mouse.row >= area.y
                    && mouse.row < area.y + area.height
                    && mouse.column >= area.x
                    && mouse.column < area.x + area.width
                {
                    let clicked = self.scroll + usize::from(mouse.row - area.y);
                    if clicked < total {
                        self.selected = clicked;
                        self.activate();
                    }
                }
            }
            _ => {}
        }
    }

    fn activate(&mut self) {
        let Some(&file_idx) = self.matches.get(self.selected) else {
            return;
        };
        let Some(file) = self.files.get(file_idx) else {
            return;
        };
        let path = file.path.clone();
        let root = self.root.clone();
        let caller = self.caller_pane_id.clone();
        match open_file_in_workspace(caller.as_deref(), &root, &path, None) {
            Ok(()) => {
                self.done = true;
            }
            Err(err) => {
                self.error = Some(err);
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 10 || area.height < 4 {
            return;
        }

        let width = usize::from(area.width);
        let accent = Style::default().fg(palette().accent);
        let dim = Style::default().add_modifier(Modifier::DIM);

        let root_name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        // Row 0: Header with title and repository name
        let title = "Quick Open";
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {title}"), accent.add_modifier(Modifier::BOLD)),
                Span::styled(
                    truncate_to(
                        format!("  {root_name}"),
                        width.saturating_sub(title.len() + 2),
                    ),
                    dim,
                ),
            ])),
            Rect::new(area.x, area.y, area.width, 1),
        );

        // Row 1: Search input bar
        let prompt = " › ";
        let room = width.saturating_sub(prompt.len() + 2);
        let mut input_spans = vec![
            Span::styled(prompt, accent),
            Span::raw(input_tail(&self.query, room)),
            Span::styled("█", dim),
        ];
        if self.query.is_empty() {
            input_spans.push(Span::styled(" type to search files", dim));
        }
        frame.render_widget(
            Paragraph::new(Line::from(input_spans)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );

        // Row 2: Divider line
        frame.render_widget(
            Paragraph::new(Span::styled("─".repeat(width), dim)),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );

        // Row 3 .. height - 1: List area
        let list_y = area.y + 3;
        let list_h = area.height.saturating_sub(4);
        let list_area = Rect::new(area.x, list_y, area.width, list_h);
        self.list_area = list_area;

        let visible = usize::from(list_area.height);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.scroll = keep_visible_scroll(self.selected, visible, self.matches.len());

        let items: Vec<ListItem> = self
            .matches
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .filter_map(|(match_idx, &file_idx)| {
                let file = self.files.get(file_idx)?;
                let filename = file
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file.label);
                let icon = icons::icon(self.theme, filename, false, false);
                let available = usize::from(list_area.width).saturating_sub(6);
                let label = truncate_path_tail(&file.label, available);

                let line = Line::from(vec![
                    Span::raw(" "),
                    Span::styled(format!("{} ", icon.glyph), crate::ui::icon_style(icon.rgb)),
                    Span::raw(label),
                ]);

                if match_idx == self.selected {
                    Some(ListItem::new(line).style(selection_style(true)))
                } else {
                    Some(ListItem::new(line))
                }
            })
            .collect();
        frame.render_widget(List::new(items), list_area);

        // Bottom row: Status / error / hints
        let footer_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        if let Some(err) = &self.error {
            let error_span = Span::styled(
                truncate_to(format!(" {err}"), width),
                Style::default().fg(palette().deleted),
            );
            frame.render_widget(Paragraph::new(error_span), footer_area);
        } else {
            let hints = Span::styled(" ↑↓ select · ⏎ open · esc/ctrl+q cancel", dim);
            let count_text = if self.loading {
                "indexing files…".to_string()
            } else if self.matches.is_empty() {
                "no matches".to_string()
            } else if self.truncated {
                format!(
                    "{} matches · first {QUICK_OPEN_FILE_LIMIT} files",
                    self.matches.len()
                )
            } else {
                format!("{} matches", self.matches.len())
            };
            let count_span = Span::styled(format!("{count_text} "), dim);

            frame.render_widget(Paragraph::new(hints), footer_area);
            frame.render_widget(
                Paragraph::new(count_span).alignment(Alignment::Right),
                footer_area,
            );
        }
    }
}

/// Open `file` in the caller's workspace: the custom editor when opted in,
/// otherwise the preview pane. `line` (1-based) positions the view on a search
/// hit; the custom editor tab mode cannot address a line and ignores it.
pub fn open_file_in_workspace(
    caller_pane_id: Option<&str>,
    root: &Path,
    file: &Path,
    line: Option<usize>,
) -> Result<(), String> {
    let state = crate::state::load_state();
    let panes_str = crate::ipc::call_text("pane.list", serde_json::json!({}))
        .map_err(|e| format!("cannot list panes: {e}"))?;
    let panes: serde_json::Value = serde_json::from_str(crate::launch::strip_bom(&panes_str))
        .map_err(|e| format!("invalid pane list: {e}"))?;

    let caller_pane_id = caller_pane_id
        .filter(|id| !id.is_empty())
        .map(String::from)
        .or_else(|| {
            let panes_arr = panes.get("result")?.get("panes")?.as_array()?;
            panes_arr
                .iter()
                .find(|p| p.get("focused") == Some(&serde_json::Value::Bool(true)))
                .and_then(|p| p.get("pane_id")?.as_str().map(String::from))
                .or_else(|| {
                    panes_arr
                        .iter()
                        .find(|p| p.get("placement") != Some(&serde_json::json!("popup")))
                        .and_then(|p| p.get("pane_id")?.as_str().map(String::from))
                })
        });

    let Some(pane_id) = caller_pane_id else {
        return Err("No active pane found to open file".into());
    };

    if state.custom_editor_on_click {
        if state.custom_editor_inline {
            // The popup pane belongs to no tab, so the editor must be given the
            // CALLER pane's tab/workspace or it cannot find the current tab.
            let tab = crate::launch::tab_of(&panes_str, &pane_id);
            let ws = crate::launch::workspace_of(&panes_str, &pane_id);
            let mut env = vec![("HERDR_PANE_ID", pane_id.as_str())];
            if !tab.is_empty() {
                env.push(("HERDR_TAB_ID", tab.as_str()));
            }
            if !ws.is_empty() {
                env.push(("HERDR_WORKSPACE_ID", ws.as_str()));
            }
            match line {
                Some(line) => crate::actions::open_in_editor_inline_at(root, file, line, &env),
                None => crate::actions::open_in_editor_inline_env(root, file, &env),
            }
            .map_err(|e| e.to_string())
        } else {
            crate::actions::open_in_editor_tab(&pane_id, root, file).map_err(|e| e.to_string())
        }
    } else {
        let doc_key = crate::viewer::doc_key_for_file(file);
        let payload = match line {
            Some(line) => crate::viewer::file_request_at(file, line),
            None => crate::viewer::file_request(file),
        };
        crate::viewer::open_in_pane(&pane_id, root, &doc_key, &payload).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn file_popup_key_navigation_and_cancellation() {
        let mut app = App {
            root: PathBuf::from("/test"),
            caller_pane_id: None,
            files: Arc::new(vec![
                QuickFile {
                    path: PathBuf::from("/test/src/main.rs"),
                    label: "src/main.rs".to_string(),
                    label_lower: "src/main.rs".to_string(),
                },
                QuickFile {
                    path: PathBuf::from("/test/src/lib.rs"),
                    label: "src/lib.rs".to_string(),
                    label_lower: "src/lib.rs".to_string(),
                },
            ]),
            matches: vec![0, 1],
            query: String::new(),
            selected: 0,
            scroll: 0,
            truncated: false,
            loading: false,
            done: false,
            error: None,
            rx: None,
            theme: IconTheme::Emoji,
            list_area: Rect::new(0, 0, 80, 20),
        };

        // Navigation
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected, 0);

        // Typing
        app.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(app.query, "lib");
        assert_eq!(app.matches, vec![1]);
        assert_eq!(app.selected, 0);

        // Ctrl+U clears query
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "");
        assert_eq!(app.matches, vec![0, 1]);

        // Ctrl+Q closes
        assert!(!app.done);
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.done);

        // Esc closes
        app.done = false;
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.done);
    }

    #[test]
    fn file_popup_draws_without_panicking() {
        let mut app = App {
            root: PathBuf::from("/test/project"),
            caller_pane_id: None,
            files: Arc::new(vec![QuickFile {
                path: PathBuf::from("/test/project/src/main.rs"),
                label: "src/main.rs".to_string(),
                label_lower: "src/main.rs".to_string(),
            }]),
            matches: vec![0],
            query: "main".to_string(),
            selected: 0,
            scroll: 0,
            truncated: false,
            loading: false,
            done: false,
            error: None,
            rx: None,
            theme: IconTheme::Emoji,
            list_area: Rect::default(),
        };

        for (width, height) in [(100, 30), (70, 20), (20, 6)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
    }
}
