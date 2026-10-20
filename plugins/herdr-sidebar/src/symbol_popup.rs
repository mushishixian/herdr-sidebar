//! "Go to symbol" popup: a session-modal symbol list over the project.
//!
//! Two halves, mirroring the file search popup:
//! - `--symbol-popup-open`: a plugin ACTION (bound to a host key) that resolves
//!   the focused pane and asks herdr to spawn the popup over it.
//! - `--symbol-popup`: the popup process itself — one worker indexes the
//!   project (see [`crate::symbol_search`]), typing filters it in memory, and
//!   Enter opens the definition line through the SAME routing Quick Open and
//!   the search popup use (`file_popup::open_file_in_workspace`).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
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

use crate::file_popup::open_file_in_workspace;
use crate::file_search::truncate_path_tail;
use crate::icons::{self, IconTheme};
use crate::symbol_search::{self, SYMBOL_LIMIT, Symbol};
use crate::ui::{input_tail, keep_visible_scroll, palette, selection_style, truncate_to};

const IDLE_POLL: Duration = Duration::from_millis(250);
const LOAD_POLL: Duration = Duration::from_millis(50);

// ── Action side: request Herdr to spawn the popup pane ──────────────────

/// Request Herdr to open the symbol popup for the focused pane's directory.
pub fn open() -> io::Result<()> {
    crate::file_popup::request_popup_pane("symbol-popup", "80%", "70%")
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
        .map(|git| git.root().to_path_buf())
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
            let timeout = if app.loading { LOAD_POLL } else { IDLE_POLL };
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
    symbols: Arc<Vec<Symbol>>,
    matches: Vec<usize>,
    query: String,
    selected: usize,
    scroll: usize,
    truncated: bool,
    loading: bool,
    done: bool,
    error: Option<String>,
    rx: Option<mpsc::Receiver<(Vec<Symbol>, bool)>>,
    theme: IconTheme,
    list_area: Rect,
}

impl App {
    fn new(root: PathBuf, caller_pane_id: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let (symbols, truncated) =
                symbol_search::collect_symbols(&worker_root, false, SYMBOL_LIMIT);
            let _ = tx.send((symbols, truncated));
        });
        let state = crate::state::load_state();
        let theme = icons::IconTheme::resolve(None, state.icons);
        Self {
            root,
            caller_pane_id,
            symbols: Arc::new(Vec::new()),
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
            Ok((symbols, truncated)) => {
                self.rx = None;
                self.symbols = Arc::new(symbols);
                self.truncated = truncated;
                self.loading = false;
                self.matches = symbol_search::match_symbols(&self.symbols, &self.query);
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
        self.matches = symbol_search::match_symbols(&self.symbols, &self.query);
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
            KeyCode::End if !self.matches.is_empty() => {
                self.selected = self.matches.len().saturating_sub(1);
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
        let Some(&symbol_index) = self.matches.get(self.selected) else {
            return;
        };
        let Some(symbol) = self.symbols.get(symbol_index) else {
            return;
        };
        let path = symbol.path.clone();
        let line = symbol.line;
        let root = self.root.clone();
        let caller = self.caller_pane_id.clone();
        match open_file_in_workspace(caller.as_deref(), &root, &path, Some(line)) {
            Ok(()) => self.done = true,
            Err(err) => self.error = Some(err),
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
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        // Row 0: Header with the title and the project name.
        let title = "Go to symbol";
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

        // Row 1: Query input.
        let prompt = " › ";
        let room = width.saturating_sub(prompt.len() + 2);
        let mut input_spans = vec![
            Span::styled(prompt, accent),
            Span::raw(input_tail(&self.query, room)),
            Span::styled("█", dim),
        ];
        if self.query.is_empty() {
            input_spans.push(Span::styled(" type a symbol name", dim));
        }
        frame.render_widget(
            Paragraph::new(Line::from(input_spans)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );

        // Row 2: Divider.
        frame.render_widget(
            Paragraph::new(Span::styled("─".repeat(width), dim)),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );

        // Rows 3 .. height - 1: the list. Windowed manually so the selection
        // stays put while the mouse wheel scrolls the VIEW only.
        let list_area = Rect::new(
            area.x,
            area.y + 3,
            area.width,
            area.height.saturating_sub(4),
        );
        self.list_area = list_area;

        let visible = usize::from(list_area.height);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.scroll = keep_visible_scroll(self.selected, visible, self.matches.len());

        let query_lower = self.query.to_lowercase();
        let items: Vec<ListItem> = self
            .matches
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .filter_map(|(match_index, &symbol_index)| {
                let symbol = self.symbols.get(symbol_index)?;
                let line = symbol_row(
                    symbol,
                    &query_lower,
                    usize::from(list_area.width),
                    dim,
                    self.theme,
                );
                if match_index == self.selected {
                    Some(ListItem::new(line).style(selection_style(true)))
                } else {
                    Some(ListItem::new(line))
                }
            })
            .collect();
        frame.render_widget(List::new(items), list_area);

        // Last row: hints / error + match count.
        let footer_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_to(format!(" {err}"), width),
                    Style::default().fg(palette().deleted),
                )),
                footer_area,
            );
            return;
        }
        let hints = Span::styled(" ↑↓ select · ⏎ open · esc/ctrl+q cancel", dim);
        let count_text = if self.loading {
            "indexing symbols…".to_string()
        } else if self.matches.is_empty() {
            "no matches".to_string()
        } else if self.truncated {
            format!(
                "{} matches · index capped at {SYMBOL_LIMIT} symbols",
                self.matches.len()
            )
        } else {
            format!("{} symbols", self.matches.len())
        };
        frame.render_widget(Paragraph::new(hints), footer_area);
        frame.render_widget(
            Paragraph::new(Span::styled(format!("{count_text} "), dim)).alignment(Alignment::Right),
            footer_area,
        );
    }
}

/// One list row: ` <file icon> name  kind  …path/file.rs:12`.
///
/// The name's matched characters are drawn with the accent so the list reads
/// like the search popup's hits; the location keeps its TAIL (file name, then
/// line) when the popup is narrow, because that is the part that tells symbols
/// with the same name apart.
fn symbol_row(
    symbol: &Symbol,
    query_lower: &str,
    width: usize,
    dim: Style,
    theme: IconTheme,
) -> Line<'static> {
    let prefix = 3; // " " + icon + " "
    let kind = symbol.kind.label();
    let kind_width = kind.chars().count() + 2; // kind + two spaces
    let location = format!("{}:{}", symbol.label, symbol.line);
    let location_budget = width
        .saturating_sub(prefix + kind_width + 8)
        .min(width / 2)
        .max(8);
    let location = truncate_path_tail(&location, location_budget);
    let name_budget = width.saturating_sub(prefix + kind_width + location.len() + 1);

    let file_name = symbol
        .label
        .rsplit('/')
        .next()
        .unwrap_or(symbol.label.as_str());
    let icon = icons::icon(theme, file_name, false, false);

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(format!("{} ", icon.glyph), crate::ui::icon_style(icon.rgb)),
    ];
    spans.extend(name_spans(&symbol.name, query_lower, name_budget));
    spans.push(Span::styled(format!("  {kind} "), dim));
    spans.push(Span::styled(location, dim));
    Line::from(spans)
}

/// Highlight the characters `query_lower` matches inside `name`, clipped to
/// `budget` cells (an ellipsis marks a clipped name).
fn name_spans(name: &str, query_lower: &str, budget: usize) -> Vec<Span<'static>> {
    let positions = symbol_search::match_positions(query_lower, name);
    let accent = Style::default()
        .fg(palette().header_accent)
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut used = 0usize;
    for (index, character) in name.chars().enumerate() {
        let character_width = ratatui::text::Span::raw(character.to_string()).width();
        if used + character_width > budget {
            if !plain.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut plain)));
            }
            spans.push(Span::raw("…"));
            return spans;
        }
        used += character_width;
        if positions.contains(&index) {
            if !plain.is_empty() {
                spans.push(Span::raw(std::mem::take(&mut plain)));
            }
            spans.push(Span::styled(character.to_string(), accent));
        } else {
            plain.push(character);
        }
    }
    if !plain.is_empty() {
        spans.push(Span::raw(plain));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn symbol(name: &str, label: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            name_lower: name.to_lowercase(),
            label: label.to_string(),
            label_lower: label.to_lowercase(),
            path: PathBuf::from(label),
            line,
            kind: symbol_search::SymbolKind::Function,
        }
    }

    fn app_with(symbols: Vec<Symbol>) -> App {
        let symbols = Arc::new(symbols);
        let matches = symbol_search::match_symbols(&symbols, "");
        App {
            root: PathBuf::from("/test/project"),
            caller_pane_id: None,
            symbols,
            matches,
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
        }
    }

    #[test]
    fn key_navigation_typing_and_cancellation() {
        let mut app = app_with(vec![
            symbol("render", "src/app.rs", 10),
            symbol("refresh", "src/app.rs", 20),
        ]);

        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected, 0);

        for character in "refr".chars() {
            app.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.query, "refr");
        assert_eq!(app.selected, 0);
        assert_eq!(app.matches, vec![1]);

        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.query, "ref");
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "");
        assert_eq!(app.matches, vec![0, 1]);

        assert!(!app.done);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.done);
        app.done = false;
        app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.done);
    }

    #[test]
    fn alt_gr_characters_type_instead_of_acting_as_shortcuts() {
        // German layouts deliver `[` as CONTROL|ALT on Windows; it must be text.
        let mut app = app_with(vec![symbol("a", "src/a.rs", 1)]);
        app.key(KeyEvent::new(
            KeyCode::Char('['),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.query, "[");
        assert!(!app.done);
    }

    #[test]
    fn empty_index_reports_no_matches_and_ignores_enter() {
        let mut app = app_with(Vec::new());
        assert!(app.matches.is_empty());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.done);
        assert!(app.error.is_none());
    }

    #[test]
    fn draws_without_panicking_at_any_size() {
        for (width, height) in [(120, 30), (60, 12), (20, 6), (12, 4)] {
            let mut app = app_with(vec![
                symbol(
                    "a_very_long_symbol_name_for_clipping",
                    "src/deep/nested/module.rs",
                    3,
                ),
                symbol("b", "x.rs", 1),
            ]);
            app.query = "a".to_string();
            app.matches = symbol_search::match_symbols(&app.symbols, &app.query);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
    }

    #[test]
    fn name_spans_highlight_the_match_and_clip_long_names() {
        let spans = name_spans("parse_token", "pt", 40);
        let text = spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "parse_token");

        let clipped = name_spans("parse_token", "", 5);
        let text = clipped
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "parse…");
    }

    #[test]
    fn rows_keep_the_file_name_when_the_popup_is_narrow() {
        let row = symbol_row(
            &symbol("render", "src/components/deeply/nested/popup.rs", 1234),
            "",
            60,
            Style::default(),
            IconTheme::Emoji,
        );
        let text = row
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("render"), "{text}");
        assert!(text.contains("popup.rs:1234"), "{text}");
    }
}
