//! Project-wide content search popup (`Ctrl+Alt+F`).
//!
//! VS Code's "Find in Files" as a session-modal popup: a query input with the
//! Match Case / Whole Word / Regex toggles on the left, a results list grouped
//! by file, and a live, syntax-highlighted preview of the selected match on the
//! right. The engine is the one the sidebar's Search view uses
//! ([`crate::content_search`]); the popup adds the worker/debounce loop, the
//! preview window, and opening the match at its line.
//!
//! Two halves, like `file_popup`:
//! - `--search-popup-open`: the plugin ACTION. Captures the focused pane and asks
//!   Herdr for the popup pane ([`open`]).
//! - `--search-popup`: the popup process ([`run`]), a one-shot TUI.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use regex::Regex;

use crate::content_search::{
    CONTENT_SEARCH_MATCH_LIMIT, ContentHit, SearchOptions, collect_content_hits, compile_pattern,
    content_line_match_ranges, highlighted_search_context,
};
use crate::file_popup;
use crate::file_search::truncate_path_tail;
use crate::icons::{self, IconTheme};
use crate::ui::{input_tail, palette, selection_style, truncate_to};

/// Typing settles for this long before a search starts.
const DEBOUNCE: Duration = Duration::from_millis(300);
const DOUBLE_CLICK: Duration = Duration::from_millis(450);
/// Narrower popups hide the preview and list results only.
const PREVIEW_MIN_WIDTH: u16 = 90;
/// Lines of context shown on each side of the hit.
const PREVIEW_CONTEXT_LINES: usize = 300;
/// Files above this are not previewed (matches the engine's search cap).
const PREVIEW_MAX_BYTES: u64 = 1024 * 1024;
const WHEEL_STEP: usize = 3;
/// Width of one toggle chip (" Aa ") and the gap that follows the group.
const CHIP_WIDTH: u16 = 4;
const CHIPS_WIDTH: u16 = CHIP_WIDTH * 3;

// ── Action side: request Herdr to spawn popup pane ──────────────────────

/// Request Herdr to open the content search popup over the focused pane.
pub fn open() -> io::Result<()> {
    file_popup::request_popup_pane("search-popup", "90%", "85%")
}

// ── Popup TUI process side ──────────────────────────────────────────────

pub fn run() -> io::Result<()> {
    if let Some(cwd) = std::env::var_os(crate::state::SPAWN_CWD_ENV).filter(|cwd| !cwd.is_empty()) {
        let _ = std::env::set_current_dir(cwd);
    }
    let state = crate::state::load_state();
    crate::ui::set_color_theme(state.color_theme);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let caller_pane_id = std::env::var("HERDR_CALLER_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let root = crate::git::Git::discover(&cwd)
        .map(|g| g.root().to_path_buf())
        .unwrap_or_else(|_| cwd.clone());
    let theme = icons::IconTheme::resolve(None, state.icons);

    let mut app = App::new(root, caller_pane_id, theme);

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let mut needs_draw = true;
    let result = (|| -> io::Result<()> {
        while !app.done {
            if app.tick() {
                needs_draw = true;
            }
            if needs_draw {
                terminal.draw(|frame| app.draw(frame))?;
                needs_draw = false;
            }
            let timeout = if app.busy() {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(250)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind != crossterm::event::KeyEventKind::Release => {
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

// ── Pure model ──────────────────────────────────────────────────────────

/// One list row: a file header, or one match inside a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Header { group: usize },
    Match { hit: usize },
}

/// A file's header: its label, match count, and the index of its first hit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Group {
    label: String,
    path: PathBuf,
    count: usize,
    first_hit: usize,
}

/// Group consecutive hits by file (the engine yields each file's hits
/// contiguously) and lay out one header row before each file's match rows.
fn build_rows(hits: &[ContentHit]) -> (Vec<Group>, Vec<Row>) {
    let mut groups = Vec::new();
    let mut rows = Vec::new();
    let mut start = 0;
    while start < hits.len() {
        let label = &hits[start].label;
        let end = hits[start..]
            .iter()
            .position(|hit| hit.label != *label)
            .map_or(hits.len(), |offset| start + offset);
        groups.push(Group {
            label: label.clone(),
            path: hits[start].path.clone(),
            count: end - start,
            first_hit: start,
        });
        rows.push(Row::Header {
            group: groups.len() - 1,
        });
        rows.extend((start..end).map(|hit| Row::Match { hit }));
        start = end;
    }
    (groups, rows)
}

/// The hit a row stands for: a match is itself, a header resolves to its
/// file's first match.
fn hit_for_row(rows: &[Row], groups: &[Group], row: usize) -> Option<usize> {
    match *rows.get(row)? {
        Row::Header { group } => groups.get(group).map(|group| group.first_hit),
        Row::Match { hit } => Some(hit),
    }
}

/// Move the selection by `delta` rows, clamped to the list (headers included).
fn step_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let selected = selected.min(len - 1);
    if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected.saturating_add(delta.unsigned_abs()).min(len - 1)
    }
}

/// The slice of a file shown around a hit (0-based, half-open), clamped to
/// the file.
fn preview_window(hit0: usize, total: usize) -> (usize, usize) {
    let start = hit0.saturating_sub(PREVIEW_CONTEXT_LINES).min(total);
    let end = hit0
        .saturating_add(PREVIEW_CONTEXT_LINES + 1)
        .min(total)
        .max(start);
    (start, end)
}

/// Scroll offset that puts the hit roughly in the middle of the viewport.
fn centered_offset(hit_in_window: usize, viewport: usize, len: usize) -> usize {
    hit_in_window
        .saturating_sub(viewport / 2)
        .min(len.saturating_sub(viewport))
}

/// Typing has settled: `DEBOUNCE` has passed since the last edit.
fn debounce_elapsed(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= DEBOUNCE
}

/// A worker result is applied only when it answers the newest search. Older
/// searches keep running to completion (they cannot be cancelled), but their
/// results are dropped here.
fn is_current_result(current_generation: u64, result_generation: u64) -> bool {
    current_generation == result_generation
}

/// A file's text as the preview sees it: lines with tabs widened to single
/// spaces, so byte offsets from the engine match the rendered text.
enum FileText {
    Text(Vec<String>),
    Binary,
    TooLarge,
    Unreadable(String),
}

fn file_text_from_bytes(bytes: &[u8]) -> FileText {
    if bytes.contains(&0) {
        return FileText::Binary;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => FileText::Text(text.lines().map(|line| line.replace('\t', " ")).collect()),
        Err(_) => FileText::Binary,
    }
}

fn load_file_text(path: &Path) -> FileText {
    match std::fs::metadata(path) {
        Err(error) => FileText::Unreadable(error.to_string()),
        Ok(metadata) if metadata.len() > PREVIEW_MAX_BYTES => FileText::TooLarge,
        Ok(_) => match std::fs::read(path) {
            Ok(bytes) => file_text_from_bytes(&bytes),
            Err(error) => FileText::Unreadable(error.to_string()),
        },
    }
}

/// The preview of one hit: either a window of highlighted lines, or a reason
/// there is no text to show.
enum Preview {
    Message(String),
    Code(PreviewCode),
}

struct PreviewCode {
    /// 1-based line number of `lines[0]`.
    first_line: usize,
    /// Highlighted lines of the window.
    lines: Vec<Line<'static>>,
    /// Index of the hit line within `lines`.
    hit_index: usize,
    /// Byte ranges of the query matches on the hit line.
    hit_ranges: Vec<(usize, usize)>,
    /// Line count of the whole file, for the gutter width.
    total_lines: usize,
}

fn build_preview(
    name: &str,
    text: &FileText,
    hit_line: usize,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> Preview {
    let lines = match text {
        FileText::Text(lines) => lines,
        FileText::Binary => return Preview::Message("binary file, no preview".into()),
        FileText::TooLarge => {
            return Preview::Message("file over 1 MiB, no preview".into());
        }
        FileText::Unreadable(error) => return Preview::Message(format!("cannot read: {error}")),
    };
    if hit_line == 0 || hit_line > lines.len() {
        return Preview::Message("file changed since the search".into());
    }
    let hit0 = hit_line - 1;
    let (start, end) = preview_window(hit0, lines.len());
    let window = &lines[start..end];
    let highlighted =
        crate::syntax::highlight_lines(name, lines.first().map(String::as_str), window);
    Preview::Code(PreviewCode {
        first_line: start + 1,
        lines: highlighted,
        hit_index: hit0 - start,
        hit_ranges: content_line_match_ranges(&lines[hit0], query, options, pattern),
        total_lines: lines.len(),
    })
}

/// Results of the newest search, plus what the preview needs to re-match.
struct Results {
    generation: u64,
    query: String,
    options: SearchOptions,
    pattern: Option<Regex>,
    hits: Vec<ContentHit>,
    truncated: bool,
    groups: Vec<Group>,
    rows: Vec<Row>,
}

struct SearchMessage {
    generation: u64,
    result: Result<(Vec<ContentHit>, bool), String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Toggle {
    MatchCase,
    WholeWord,
    Regex,
}

// ── Popup state and event handling ──────────────────────────────────────

struct App {
    root: PathBuf,
    caller_pane_id: Option<String>,
    theme: IconTheme,
    query: String,
    /// Byte offset of the caret in `query`, always on a char boundary.
    cursor: usize,
    options: SearchOptions,
    /// Set by an edit; the search starts once `DEBOUNCE` has passed.
    dirty_since: Option<Instant>,
    /// Bumped whenever a search starts; results from older ones are dropped.
    generation: u64,
    in_flight: bool,
    tx: mpsc::Sender<SearchMessage>,
    rx: mpsc::Receiver<SearchMessage>,
    results: Option<Results>,
    error: Option<String>,
    selected: usize,
    /// List view offset in rows. The wheel moves this alone.
    scroll: usize,
    /// Bring the selection into view on the next draw (keyboard only).
    snap: bool,
    list_area: Rect,
    preview_area: Rect,
    toggle_zones: Vec<(Rect, Toggle)>,
    last_click: Option<(usize, Instant)>,
    /// The last file read for the preview. Moving between matches of one
    /// file reuses it instead of reading the file again.
    file_cache: Option<(PathBuf, Arc<FileText>)>,
    /// What the preview currently shows: (path, hit line, results generation).
    preview_key: Option<(PathBuf, usize, u64)>,
    preview: Option<Preview>,
    preview_offset: usize,
    preview_recenter: bool,
    done: bool,
}

impl App {
    fn new(root: PathBuf, caller_pane_id: Option<String>, theme: IconTheme) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            root,
            caller_pane_id,
            theme,
            query: String::new(),
            cursor: 0,
            options: SearchOptions::default(),
            dirty_since: None,
            generation: 0,
            in_flight: false,
            tx,
            rx,
            results: None,
            error: None,
            selected: 0,
            scroll: 0,
            snap: false,
            list_area: Rect::default(),
            preview_area: Rect::default(),
            toggle_zones: Vec::new(),
            last_click: None,
            file_cache: None,
            preview_key: None,
            preview: None,
            preview_offset: 0,
            preview_recenter: false,
            done: false,
        }
    }

    /// Something is pending that needs the loop to wake up soon.
    fn busy(&self) -> bool {
        self.in_flight || self.dirty_since.is_some()
    }

    /// Collect finished searches and start a debounced one. Returns true when
    /// the screen needs a redraw.
    fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.rx.try_recv() {
            if is_current_result(self.generation, message.generation) {
                self.apply(message);
                changed = true;
            }
        }
        if self
            .dirty_since
            .is_some_and(|since| debounce_elapsed(since, Instant::now()))
        {
            self.dirty_since = None;
            self.start_search();
            changed = true;
        }
        changed
    }

    fn start_search(&mut self) {
        self.generation += 1;
        if self.query.is_empty() {
            self.in_flight = false;
            self.clear_results();
            return;
        }
        self.in_flight = true;
        let generation = self.generation;
        let query = self.query.clone();
        let options = self.options;
        let root = self.root.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Hidden files follow the Explorer's default (shown); `.git` and
            // ignored paths are always skipped by the engine.
            let result = collect_content_hits(
                &root,
                true,
                &query,
                "",
                "",
                options,
                CONTENT_SEARCH_MATCH_LIMIT,
            );
            let _ = tx.send(SearchMessage { generation, result });
        });
    }

    fn apply(&mut self, message: SearchMessage) {
        self.in_flight = false;
        match message.result {
            Ok((hits, truncated)) => {
                let (groups, rows) = build_rows(&hits);
                let query = self.query.clone();
                let options = self.options;
                let pattern = compile_pattern(&query, options).ok().flatten();
                self.results = Some(Results {
                    generation: message.generation,
                    query,
                    options,
                    pattern,
                    hits,
                    truncated,
                    groups,
                    rows,
                });
                self.error = None;
            }
            Err(error) => {
                self.results = None;
                self.error = Some(error);
            }
        }
        self.selected = 0;
        self.scroll = 0;
    }

    fn clear_results(&mut self) {
        self.results = None;
        self.error = None;
        self.selected = 0;
        self.scroll = 0;
        self.preview_key = None;
        self.preview = None;
    }

    fn rows_len(&self) -> usize {
        self.results
            .as_ref()
            .map_or(0, |results| results.rows.len())
    }

    fn page(&self) -> usize {
        usize::from(self.list_area.height).max(1)
    }

    fn move_selection(&mut self, delta: isize) {
        self.selected = step_selection(self.selected, self.rows_len(), delta);
        self.snap = true;
    }

    /// Any edit to the query or the options: search again after the debounce.
    fn mark_edited(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn toggle(&mut self, toggle: Toggle) {
        match toggle {
            Toggle::MatchCase => self.options.match_case = !self.options.match_case,
            Toggle::WholeWord => self.options.whole_word = !self.options.whole_word,
            Toggle::Regex => self.options.regex = !self.options.regex,
        }
        self.mark_edited();
    }

    fn insert_char(&mut self, character: char) {
        self.query.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.mark_edited();
    }

    fn delete_before_cursor(&mut self) {
        if let Some((index, _)) = self.query[..self.cursor].char_indices().next_back() {
            self.query.remove(index);
            self.cursor = index;
            self.mark_edited();
        }
    }

    fn delete_at_cursor(&mut self) {
        if self.cursor < self.query.len() {
            self.query.remove(self.cursor);
            self.mark_edited();
        }
    }

    fn cursor_left(&mut self) {
        if let Some((index, _)) = self.query[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn cursor_right(&mut self) {
        if let Some(character) = self.query[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.page() as isize;
        match key.code {
            KeyCode::Esc => self.done = true,
            KeyCode::Enter => self.activate(),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Left => self.cursor_left(),
            KeyCode::Right => self.cursor_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.len(),
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Char('u') if ctrl && !alt => {
                self.query.clear();
                self.cursor = 0;
                self.mark_edited();
            }
            // Alt+C / Alt+W / Alt+R are the VS Code toggles; other Alt chords
            // do nothing.
            KeyCode::Char(character) if alt && !ctrl => match character.to_ascii_lowercase() {
                'c' => self.toggle(Toggle::MatchCase),
                'w' => self.toggle(Toggle::WholeWord),
                'r' => self.toggle(Toggle::Regex),
                _ => {}
            },
            // Ctrl+letter is a shortcut we do not bind; AltGr arrives as
            // Ctrl+Alt and is text, so it is inserted.
            KeyCode::Char(character) if !ctrl || alt => self.insert_char(character),
            _ => {}
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        let (column, row) = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                if contains(self.preview_area, column, row) {
                    let delta = if up {
                        -(WHEEL_STEP as isize)
                    } else {
                        WHEEL_STEP as isize
                    };
                    self.scroll_preview(delta);
                } else if contains(self.list_area, column, row) {
                    let max = self
                        .rows_len()
                        .saturating_sub(usize::from(self.list_area.height));
                    self.scroll = if up {
                        self.scroll.saturating_sub(WHEEL_STEP)
                    } else {
                        (self.scroll + WHEEL_STEP).min(max)
                    };
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, toggle)) = self
                    .toggle_zones
                    .iter()
                    .find(|(zone, _)| contains(*zone, column, row))
                {
                    self.toggle(toggle);
                    return;
                }
                if !contains(self.list_area, column, row) {
                    return;
                }
                let index = self.scroll + usize::from(row - self.list_area.y);
                if index >= self.rows_len() {
                    return;
                }
                let now = Instant::now();
                let double = matches!(
                    self.last_click,
                    Some((last, at)) if last == index && now.duration_since(at) <= DOUBLE_CLICK
                );
                self.selected = index;
                if double {
                    self.last_click = None;
                    self.activate();
                } else {
                    self.last_click = Some((index, now));
                }
            }
            _ => {}
        }
    }

    fn scroll_preview(&mut self, delta: isize) {
        let max = self.preview_max_offset();
        self.preview_offset = if delta < 0 {
            self.preview_offset.saturating_sub(delta.unsigned_abs())
        } else {
            (self.preview_offset + delta as usize).min(max)
        };
    }

    fn preview_max_offset(&self) -> usize {
        match &self.preview {
            Some(Preview::Code(code)) => code
                .lines
                .len()
                .saturating_sub(usize::from(self.preview_area.height)),
            _ => 0,
        }
    }

    /// Open the selected hit (a header opens its first match) at its line and
    /// close the popup.
    fn activate(&mut self) {
        let Some(results) = &self.results else {
            return;
        };
        let Some(hit_index) = hit_for_row(&results.rows, &results.groups, self.selected) else {
            return;
        };
        let Some(hit) = results.hits.get(hit_index) else {
            return;
        };
        let path = hit.path.clone();
        let line = hit.line;
        match file_popup::open_file_in_workspace(
            self.caller_pane_id.as_deref(),
            &self.root,
            &path,
            Some(line),
        ) {
            Ok(()) => self.done = true,
            Err(error) => self.error = Some(error),
        }
    }

    /// Load the preview for the selected hit, reusing the cached file text and
    /// skipping the work entirely when the selection is unchanged.
    fn ensure_preview(&mut self) {
        let Some(results) = &self.results else {
            self.preview = None;
            self.preview_key = None;
            return;
        };
        let Some(hit_index) = hit_for_row(&results.rows, &results.groups, self.selected) else {
            self.preview = None;
            self.preview_key = None;
            return;
        };
        let Some(hit) = results.hits.get(hit_index) else {
            return;
        };
        let key = (hit.path.clone(), hit.line, results.generation);
        if self.preview_key.as_ref() == Some(&key) {
            return;
        }
        let name = hit
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let query = results.query.clone();
        let options = results.options;
        let pattern = results.pattern.clone();
        let hit_line = hit.line;

        let text = match &self.file_cache {
            Some((cached, text)) if *cached == key.0 => Arc::clone(text),
            _ => {
                let text = Arc::new(load_file_text(&key.0));
                self.file_cache = Some((key.0.clone(), Arc::clone(&text)));
                text
            }
        };
        self.preview = Some(build_preview(
            &name,
            &text,
            hit_line,
            &query,
            options,
            pattern.as_ref(),
        ));
        self.preview_key = Some(key);
        self.preview_recenter = true;
    }

    fn status(&self) -> (String, bool) {
        if let Some(error) = &self.error {
            return (error.clone(), true);
        }
        if self.query.is_empty() {
            return ("type to search file contents".into(), false);
        }
        let searching = self.busy();
        let text = match &self.results {
            None if searching => "searching…".to_string(),
            None => String::new(),
            Some(results) if results.hits.is_empty() && !searching => "no results".to_string(),
            Some(results) => {
                let mut text = format!(
                    "{} results in {} files",
                    results.hits.len(),
                    results.groups.len()
                );
                if results.truncated {
                    text.push_str(&format!(" · capped at {CONTENT_SEARCH_MATCH_LIMIT}"));
                }
                if searching {
                    text.push_str(" · searching…");
                }
                text
            }
        };
        (text, false)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 20 || area.height < 6 {
            return;
        }
        let width = area.width;
        let preview_visible = width >= PREVIEW_MIN_WIDTH;
        let left_width = if preview_visible {
            (width * 2 / 5).clamp(36, width - 30)
        } else {
            width
        };
        let accent = Style::default().fg(palette().accent);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let list_y = area.y + 4;
        let list_height = area.height.saturating_sub(5);
        self.list_area = Rect::new(area.x, list_y, left_width, list_height);

        // Row 0: title and the searched root.
        let root_name = self
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let title = " Search in files";
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(title, accent.add_modifier(Modifier::BOLD)),
                Span::styled(
                    truncate_to(
                        format!("  {root_name}"),
                        usize::from(left_width).saturating_sub(title.len() + 2),
                    ),
                    dim,
                ),
            ])),
            Rect::new(area.x, area.y, left_width, 1),
        );

        // Row 1: query input with the three toggle chips at its right end.
        self.draw_input(frame, area.x, area.y + 1, left_width, accent, dim);

        // Row 2: status (or error).
        let (status, is_error) = self.status();
        let status_style = if is_error {
            Style::default().fg(palette().deleted)
        } else {
            dim
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to(format!(" {status}"), usize::from(left_width)),
                status_style,
            )),
            Rect::new(area.x, area.y + 2, left_width, 1),
        );

        // Row 3: divider.
        frame.render_widget(
            Paragraph::new(Span::styled("─".repeat(usize::from(left_width)), dim)),
            Rect::new(area.x, area.y + 3, left_width, 1),
        );

        self.draw_list(frame, left_width, list_height);

        if preview_visible {
            self.draw_preview(frame, area, left_width, dim);
        }

        // Last row: key hints.
        let footer = Rect::new(area.x, area.y + area.height - 1, width, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to(
                    " ↑↓ move · pgup/pgdn · ⏎ open · alt+c/w/r options · esc cancel".to_string(),
                    usize::from(width),
                ),
                dim,
            )),
            footer,
        );
    }

    fn draw_input(
        &mut self,
        frame: &mut Frame,
        x: u16,
        y: u16,
        left_width: u16,
        accent: Style,
        dim: Style,
    ) {
        let prompt = " › ";
        let chips_x = x + left_width.saturating_sub(CHIPS_WIDTH + 1);
        let room = usize::from(left_width.saturating_sub(CHIPS_WIDTH + 2))
            .saturating_sub(prompt.len() + 1);
        let before = input_tail(&self.query[..self.cursor], room);
        let used = before.chars().count();
        let after = truncate_to(
            self.query[self.cursor..].to_string(),
            room.saturating_sub(used),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prompt, accent),
                Span::raw(before),
                Span::styled("█", accent),
                Span::raw(after),
            ])),
            Rect::new(x, y, left_width.saturating_sub(CHIPS_WIDTH + 1), 1),
        );

        self.toggle_zones.clear();
        let chips = [
            ("Aa", Toggle::MatchCase, self.options.match_case),
            ("ab", Toggle::WholeWord, self.options.whole_word),
            (".*", Toggle::Regex, self.options.regex),
        ];
        for (index, (label, toggle, on)) in chips.into_iter().enumerate() {
            let chip_x = chips_x + CHIP_WIDTH * index as u16;
            let style = if on {
                Style::default()
                    .fg(palette().button_fg)
                    .bg(palette().button_bg)
            } else {
                dim
            };
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {label} "), style)),
                Rect::new(chip_x, y, CHIP_WIDTH, 1),
            );
            self.toggle_zones
                .push((Rect::new(chip_x, y, CHIP_WIDTH, 1), toggle));
        }
    }

    fn draw_list(&mut self, frame: &mut Frame, left_width: u16, list_height: u16) {
        let visible = usize::from(list_height);
        let total = self.rows_len();
        if self.snap && visible > 0 {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + visible {
                self.scroll = self.selected + 1 - visible;
            }
            self.snap = false;
        }
        self.scroll = self.scroll.min(total.saturating_sub(visible));

        let width = usize::from(left_width);
        let mut lines = Vec::new();
        if let Some(results) = &self.results {
            for (row_index, row) in results
                .rows
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(visible)
            {
                let line = match *row {
                    Row::Header { group } => {
                        let group = &results.groups[group];
                        header_line(group, self.theme, width)
                    }
                    Row::Match { hit } => match_line(&results.hits[hit], width),
                };
                lines.push(if row_index == self.selected {
                    line.style(selection_style(true))
                } else {
                    line
                });
            }
        }
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(self.list_area.x, self.list_area.y, left_width, list_height),
        );
    }

    fn draw_preview(&mut self, frame: &mut Frame, area: Rect, left_width: u16, dim: Style) {
        // Divider between the columns.
        for y in area.y..area.y + area.height.saturating_sub(1) {
            frame.render_widget(
                Paragraph::new(Span::styled("│", dim)),
                Rect::new(area.x + left_width, y, 1, 1),
            );
        }
        let x = area.x + left_width + 1;
        let width = area.width.saturating_sub(left_width + 1);
        let body_height = area.height.saturating_sub(3);
        self.preview_area = Rect::new(x, area.y + 2, width, body_height);

        self.ensure_preview();
        if self.preview_recenter {
            if let Some(Preview::Code(code)) = &self.preview {
                self.preview_offset =
                    centered_offset(code.hit_index, usize::from(body_height), code.lines.len());
            }
            self.preview_recenter = false;
        }
        self.preview_offset = self.preview_offset.min(self.preview_max_offset());

        let header = match (&self.results, &self.preview_key) {
            (Some(results), Some((_, line, _))) => {
                let label = hit_for_row(&results.rows, &results.groups, self.selected)
                    .and_then(|hit| results.hits.get(hit))
                    .map(|hit| hit.label.clone())
                    .unwrap_or_default();
                format!(" {label} · line {line}")
            }
            _ => String::new(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to(header, usize::from(width)),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Rect::new(x, area.y, width, 1),
        );
        frame.render_widget(
            Paragraph::new(Span::styled("─".repeat(usize::from(width)), dim)),
            Rect::new(x, area.y + 1, width, 1),
        );

        match &self.preview {
            None => {}
            Some(Preview::Message(message)) => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        truncate_to(format!(" {message}"), usize::from(width)),
                        dim,
                    )),
                    self.preview_area,
                );
            }
            Some(Preview::Code(code)) => {
                let gutter_width = (code.first_line + code.lines.len())
                    .max(code.total_lines)
                    .to_string()
                    .len();
                let visible = usize::from(body_height);
                let lines: Vec<Line> = code
                    .lines
                    .iter()
                    .enumerate()
                    .skip(self.preview_offset)
                    .take(visible)
                    .map(|(index, line)| {
                        let number = code.first_line + index;
                        let is_hit = index == code.hit_index;
                        let gutter = format!("{number:>gutter_width$} ");
                        code_line(
                            gutter,
                            line.spans.clone(),
                            is_hit.then_some(code.hit_ranges.as_slice()),
                            usize::from(width),
                            dim,
                        )
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines), self.preview_area);
            }
        }
    }

    #[cfg(test)]
    fn set_results_for_test(&mut self, hits: Vec<ContentHit>, truncated: bool) {
        self.apply(SearchMessage {
            generation: self.generation,
            result: Ok((hits, truncated)),
        });
    }
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// A file header row: icon, path tail, and match count. Matches the Search
/// view's header.
fn header_line(group: &Group, theme: IconTheme, width: usize) -> Line<'static> {
    let filename = group
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&group.label);
    let icon = icons::icon(theme, filename, false, false);
    let count = format!(" {}", group.count);
    let label_width = width.saturating_sub(3 + count.len());
    Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{} ", icon.glyph), crate::ui::icon_style(icon.rgb)),
        Span::styled(
            truncate_path_tail(&group.label, label_width),
            Style::default().bold(),
        ),
        Span::styled(count, Style::default().dim()),
    ])
}

/// A match row: `  <line>: <context>` with the matched text emphasized, the
/// same look as the Search view.
fn match_line(hit: &ContentHit, width: usize) -> Line<'static> {
    let prefix = format!("   {}: ", hit.line);
    let context_width = width.saturating_sub(prefix.len());
    let mut spans = vec![Span::styled(
        prefix,
        Style::default().fg(palette().accent_focus),
    )];
    spans.extend(highlighted_search_context(
        &hit.context,
        &hit.matches,
        context_width,
        Style::default().fg(palette().header_accent).bold(),
    ));
    Line::from(spans)
}

/// One preview line: the gutter, then the highlighted text. The hit line gets
/// the subtle band and emphasized match ranges.
fn code_line(
    gutter: String,
    spans: Vec<Span<'static>>,
    hit_ranges: Option<&[(usize, usize)]>,
    width: usize,
    dim: Style,
) -> Line<'static> {
    let Some(ranges) = hit_ranges else {
        let mut out = vec![Span::styled(gutter, dim)];
        out.extend(spans);
        return Line::from(out);
    };
    let band = Style::default().bg(palette().hover_bg);
    let match_style = Style::default().fg(palette().header_accent).bold();
    let mut out = vec![Span::styled(gutter, dim.patch(band))];
    let mut offset = 0usize;
    for span in spans {
        let text = span.content.to_string();
        let start = offset;
        let end = offset + text.len();
        offset = end;
        let mut cuts = vec![start, end];
        for &(range_start, range_end) in ranges {
            for point in [range_start, range_end] {
                if point > start && point < end {
                    cuts.push(point);
                }
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let Some(piece) = text.get(from - start..to - start) else {
                continue;
            };
            let in_match = ranges.iter().any(|&(s, e)| from >= s && to <= e);
            let mut style = span.style.patch(band);
            if in_match {
                style = style.patch(match_style);
            }
            out.push(Span::styled(piece.to_string(), style));
        }
    }
    let used: usize = out.iter().map(|span| span.width()).sum();
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), band));
    }
    Line::from(out)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn hit(path: &str, label: &str, line: usize, context: &str) -> ContentHit {
        ContentHit {
            path: PathBuf::from(path),
            label: label.to_string(),
            line,
            context: context.to_string(),
            matches: vec![(0, 3)],
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn rows_group_matches_under_their_file_header() {
        let hits = vec![
            hit("/r/a.rs", "a.rs", 1, "one"),
            hit("/r/a.rs", "a.rs", 4, "two"),
            hit("/r/b.rs", "b.rs", 9, "three"),
        ];
        let (groups, rows) = build_rows(&hits);
        assert_eq!(
            groups,
            vec![
                Group {
                    label: "a.rs".into(),
                    path: PathBuf::from("/r/a.rs"),
                    count: 2,
                    first_hit: 0,
                },
                Group {
                    label: "b.rs".into(),
                    path: PathBuf::from("/r/b.rs"),
                    count: 1,
                    first_hit: 2,
                },
            ]
        );
        assert_eq!(
            rows,
            vec![
                Row::Header { group: 0 },
                Row::Match { hit: 0 },
                Row::Match { hit: 1 },
                Row::Header { group: 1 },
                Row::Match { hit: 2 },
            ]
        );
        assert_eq!(build_rows(&[]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn selection_moves_across_headers_and_matches_and_clamps() {
        assert_eq!(step_selection(0, 5, 1), 1);
        assert_eq!(step_selection(4, 5, 1), 4);
        assert_eq!(step_selection(0, 5, -1), 0);
        assert_eq!(step_selection(2, 5, 10), 4);
        assert_eq!(step_selection(9, 5, 0), 4, "stale selection is clamped");
        assert_eq!(step_selection(3, 0, 1), 0);
    }

    #[test]
    fn header_resolves_to_its_first_match_and_match_to_itself() {
        let hits = vec![
            hit("/r/a.rs", "a.rs", 1, "one"),
            hit("/r/a.rs", "a.rs", 4, "two"),
            hit("/r/b.rs", "b.rs", 9, "three"),
        ];
        let (groups, rows) = build_rows(&hits);
        assert_eq!(hit_for_row(&rows, &groups, 0), Some(0));
        assert_eq!(hit_for_row(&rows, &groups, 2), Some(1));
        assert_eq!(hit_for_row(&rows, &groups, 3), Some(2));
        assert_eq!(hit_for_row(&rows, &groups, 99), None);
    }

    #[test]
    fn preview_window_clamps_to_the_file_edges() {
        assert_eq!(preview_window(0, 10), (0, 10));
        assert_eq!(preview_window(5, 1000), (0, 306));
        assert_eq!(preview_window(500, 1000), (200, 801));
        assert_eq!(preview_window(999, 1000), (699, 1000));
        assert_eq!(preview_window(3, 0), (0, 0));
    }

    #[test]
    fn preview_scroll_centers_the_hit_and_stays_in_range() {
        assert_eq!(centered_offset(10, 10, 100), 5);
        assert_eq!(centered_offset(0, 10, 100), 0, "hit near the top");
        assert_eq!(centered_offset(99, 10, 100), 90, "hit near the bottom");
        assert_eq!(
            centered_offset(5, 30, 10),
            0,
            "window shorter than viewport"
        );
    }

    #[test]
    fn debounce_and_generation_drop_stale_work() {
        let since = Instant::now();
        assert!(!debounce_elapsed(since, since + Duration::from_millis(299)));
        assert!(debounce_elapsed(since, since + Duration::from_millis(300)));
        assert!(is_current_result(4, 4));
        assert!(!is_current_result(5, 4));
    }

    #[test]
    fn file_text_marks_binary_and_widens_tabs() {
        assert!(matches!(file_text_from_bytes(b"a\0b"), FileText::Binary));
        assert!(matches!(
            file_text_from_bytes(&[0xff, 0xfe]),
            FileText::Binary
        ));
        match file_text_from_bytes(b"x\ty\nz\n") {
            FileText::Text(lines) => assert_eq!(lines, vec!["x y", "z"]),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn enter_on_a_header_resolves_before_opening() {
        let mut app = App::new(PathBuf::from("/r"), None, IconTheme::Emoji);
        app.set_results_for_test(
            vec![
                hit("/r/a.rs", "a.rs", 1, "one"),
                hit("/r/a.rs", "a.rs", 4, "two"),
            ],
            false,
        );
        app.selected = 0;
        app.list_area = Rect::new(0, 0, 40, 2);
        let results = app.results.as_ref().unwrap();
        assert_eq!(
            hit_for_row(&results.rows, &results.groups, app.selected),
            Some(0)
        );
        app.move_selection(1);
        assert_eq!(app.selected, 1);
        app.move_selection(1);
        assert_eq!(app.selected, 2);
        app.key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn query_editing_and_toggles_follow_the_keymap() {
        let mut app = App::new(PathBuf::from("/r"), None, IconTheme::Emoji);
        for character in "héllo".chars() {
            app.key(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.query, "héllo");
        app.key(key(KeyCode::Left, KeyModifiers::NONE));
        app.key(key(KeyCode::Left, KeyModifiers::NONE));
        app.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.query, "hélo");
        app.key(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.query, "héo");
        app.key(key(KeyCode::Home, KeyModifiers::NONE));
        app.key(key(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.query, "éo");
        app.key(key(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.cursor, app.query.len());

        app.key(key(KeyCode::Char('c'), KeyModifiers::ALT));
        app.key(key(KeyCode::Char('w'), KeyModifiers::ALT));
        app.key(key(KeyCode::Char('r'), KeyModifiers::ALT));
        assert!(app.options.match_case && app.options.whole_word && app.options.regex);
        assert_eq!(app.query, "éo", "alt chords are not text");
        app.key(key(KeyCode::Char('c'), KeyModifiers::ALT));
        assert!(!app.options.match_case);

        assert!(
            app.dirty_since.is_some(),
            "edits schedule a debounced search"
        );
        app.key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "");
        app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.done);
    }

    #[test]
    fn code_line_emphasizes_only_the_match_ranges() {
        let spans = vec![Span::raw("let needle = 1")];
        let line = code_line(
            "3 ".into(),
            spans,
            Some(&[(4, 10)][..]),
            20,
            Style::default(),
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "3 let needle = 1    ");
        let emphasized: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(emphasized, vec!["needle"]);
    }

    /// A temp project with a match at line 2 of `src/needle.rs`, plus the
    /// engine hits for it, so draw tests run against real search output.
    fn sample_project(tag: &str) -> (PathBuf, Vec<ContentHit>) {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-search-popup-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/needle.rs"),
            "use crate::prelude;\nfn needle_body() {}\n// tail\n",
        )
        .unwrap();
        let (hits, _) =
            collect_content_hits(&root, false, "needle", "", "", SearchOptions::default(), 20)
                .unwrap();
        (root, hits)
    }

    fn drawn_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn draw_shows_preview_when_wide_and_hides_it_when_narrow() {
        let (root, hits) = sample_project("draw");
        let mut app = App::new(root.clone(), None, IconTheme::Emoji);
        app.query = "needle".into();
        app.cursor = app.query.len();
        app.set_results_for_test(hits, false);
        // Select the match (row 1) so the preview shows line 2.
        app.selected = 1;

        let wide = drawn_text(&mut app, 120, 30);
        assert!(wide.contains("needle"), "query text is drawn");
        assert!(wide.contains("src/needle.rs"), "result path is drawn");
        assert!(wide.contains("line 2"), "preview header shows the hit line");
        assert!(
            wide.contains("use crate::prelude"),
            "preview shows context around the hit"
        );

        let narrow = drawn_text(&mut app, 70, 20);
        assert!(narrow.contains("needle"));
        assert!(narrow.contains("src/needle.rs"));
        assert!(
            !narrow.contains("line 2"),
            "preview is hidden below 90 columns"
        );
        assert!(!narrow.contains("use crate::prelude"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_preview_colors_a_window_that_ends_in_a_blank_line() {
        let lines: Vec<String> = ["fn main() {", "    let needle = 1;", "}", "", ""]
            .iter()
            .map(|line| line.to_string())
            .collect();
        let preview = build_preview(
            "main.rs",
            &FileText::Text(lines),
            2,
            "needle",
            SearchOptions::default(),
            None,
        );
        let Preview::Code(code) = preview else {
            panic!("expected a code preview");
        };
        assert_eq!(code.first_line, 1);
        assert_eq!(code.lines.len(), 5, "one output line per window line");
        assert!(
            code.lines
                .iter()
                .any(|line| line.spans.iter().any(|span| span.style.fg.is_some())),
            "preview must be colored despite the trailing blank line"
        );
    }

    #[test]
    fn content_search_engine_runs_from_the_popup_root() {
        let (root, hits) = sample_project("engine");
        assert_eq!(hits.len(), 1);
        assert!(hits.iter().all(|hit| hit.label == "src/needle.rs"));
        assert_eq!(hits[0].line, 2);
        let _ = std::fs::remove_dir_all(root);
    }
}
