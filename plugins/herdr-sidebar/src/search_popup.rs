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
    CONTENT_SEARCH_MATCH_LIMIT, ContentHit, ReplaceReport, SearchOptions, Target,
    apply_replacements, collect_content_hits, compile_pattern, content_line_match_ranges,
    first_match_target, highlighted_search_context,
};
use crate::file_popup;
use crate::file_search::truncate_path_tail;
use crate::icons::{self, IconTheme};
use crate::ui::{
    SelectAll, input_tail, palette, selected_text_style, selection_style, truncate_to,
};

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

/// Environment variable that once started the popup in replace mode. `run()`
/// no longer reads it: the Replace row always starts open. It is kept so old
/// keymaps and actions that set it keep working (they open the same popup).
pub const REPLACE_MODE_ENV: &str = "HERDR_SIDEBAR_SEARCH_MODE";
const REPLACE_MODE_VALUE: &str = "replace";

/// Request Herdr to open the content search popup over the focused pane.
pub fn open() -> io::Result<()> {
    file_popup::request_popup_pane("search-popup", "90%", "85%")
}

/// Request the search popup for Ctrl+Alt+E ("Replace in files"). It is the
/// identical popup to `open()` (the Replace row is always shown), and the env
/// it sends is ignored by `run()`.
pub fn open_replace() -> io::Result<()> {
    file_popup::request_popup_pane_with_env(
        "search-popup",
        "90%",
        "85%",
        &[(REPLACE_MODE_ENV, REPLACE_MODE_VALUE)],
    )
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
    // The Replace row always starts expanded: users could not find it behind
    // the collapsed chevron. The chevron still folds it.
    app.replace_open = true;

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
    /// Replace mode only: the hit line as old text struck through, each
    /// match followed by the replacement. Replaces the highlighted hit line.
    hit_replaced: Option<Vec<Span<'static>>>,
}

fn build_preview(
    name: &str,
    text: &FileText,
    hit_line: usize,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
    replacement: Option<&str>,
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
    let hit_ranges = content_line_match_ranges(&lines[hit0], query, options, pattern);
    let hit_replaced = replacement.map(|replacement| {
        replaced_spans(&lines[hit0], &hit_ranges, replacement, Style::default())
    });
    Preview::Code(PreviewCode {
        first_line: start + 1,
        lines: highlighted,
        hit_index: hit0 - start,
        hit_ranges,
        total_lines: lines.len(),
        hit_replaced,
    })
}

// ── Replace mode: pure helpers ──────────────────────────────────────────

/// The old text of each match struck through, each followed by the
/// replacement. Text between matches keeps `base`. The colors are the diff
/// marks, so they read on both the dark and the light theme.
fn replaced_spans(
    text: &str,
    ranges: &[(usize, usize)],
    replacement: &str,
    base: Style,
) -> Vec<Span<'static>> {
    let old_style = base
        .fg(palette().diff_del_mark)
        .add_modifier(Modifier::CROSSED_OUT);
    let new_style = base
        .fg(palette().diff_add_mark)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut cursor = 0;
    for &(start, end) in ranges {
        if start < cursor || end > text.len() || start > end {
            continue;
        }
        if start > cursor {
            spans.push(Span::styled(text[cursor..start].to_string(), base));
        }
        spans.push(Span::styled(text[start..end].to_string(), old_style));
        if !replacement.is_empty() {
            spans.push(Span::styled(replacement.to_string(), new_style));
        }
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base));
    }
    spans
}

/// [`replaced_spans`] for a list row's context, cut to `max_width` cells with
/// an ellipsis, like [`highlighted_search_context`].
fn replaced_context_spans(
    context: &str,
    matches: &[(usize, usize)],
    replacement: &str,
    max_width: usize,
) -> Vec<Span<'static>> {
    let spans = replaced_spans(context, matches, replacement, Style::default());
    clip_spans(spans, max_width)
}

/// Fit `text` to `width` cells by eliding its middle and keeping the last
/// `keep_tail` characters. Used for confirmation prompts, whose `(y/N)` must
/// stay visible.
fn ellipsize_middle(text: &str, width: usize, keep_tail: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width || width <= keep_tail + 2 {
        return truncate_to(text.to_string(), width);
    }
    let head_len = width - keep_tail - 1;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - keep_tail..].iter().collect();
    format!("{head}…{tail}")
}

/// Cut spans to `max_width` cells, ending with `…` when anything was dropped.
fn clip_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(Span::width).sum();
    if total <= max_width {
        return spans;
    }
    if max_width < 2 {
        return Vec::new();
    }
    let budget = max_width - 1;
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut kept = String::new();
        for character in span.content.chars() {
            let width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            if used + width > budget {
                break;
            }
            used += width;
            kept.push(character);
        }
        let stopped = kept.chars().count() < span.content.chars().count();
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        if stopped {
            break;
        }
    }
    out.push(Span::raw("…"));
    out
}

/// Matches in these hits, summed. A hit's matches are its occurrences.
fn occurrences_of(hits: &[ContentHit]) -> usize {
    hits.iter().map(|hit| hit.matches.len()).sum()
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One replacement the worker thread performs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReplaceJob {
    /// The first occurrence on one line of one file.
    Selected { path: PathBuf, line: usize },
    /// Every occurrence in one file. `requested` is what the list showed.
    File { path: PathBuf, requested: usize },
    /// Every occurrence in the project, re-searched when the job runs.
    All,
}

impl ReplaceJob {
    /// Occurrences the user asked to replace, for the "skipped" count.
    /// Replace All has no known total, so it reports no skips of its own.
    fn requested(&self) -> usize {
        match self {
            Self::Selected { .. } => 1,
            Self::File { requested, .. } => *requested,
            Self::All => 0,
        }
    }
}

/// Runs on a worker thread. Only an invalid query is an error; per-file
/// problems come back inside the report.
fn run_replace_job(
    root: &Path,
    job: ReplaceJob,
    query: &str,
    options: SearchOptions,
    replacement: &str,
) -> Result<ReplaceReport, String> {
    let pattern = compile_pattern(query, options)?;
    let files: Vec<(PathBuf, Vec<Target>)> = match job {
        ReplaceJob::Selected { path, line } => {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            match first_match_target(&text, line, query, options, pattern.as_ref()) {
                Some(target) => vec![(path, vec![target])],
                None => Vec::new(),
            }
        }
        ReplaceJob::File { path, .. } => vec![(path, vec![Target::All])],
        ReplaceJob::All => {
            let (hits, _) = collect_content_hits(root, true, query, "", "", options, usize::MAX)?;
            let mut files: Vec<(PathBuf, Vec<Target>)> = Vec::new();
            for hit in hits {
                // The engine yields each file's hits contiguously.
                if files.last().is_none_or(|(path, _)| *path != hit.path) {
                    files.push((hit.path, vec![Target::All]));
                }
            }
            files
        }
    };
    apply_replacements(root, &files, query, options, replacement)
}

/// What the worker sends back for one replacement.
struct ReplaceMessage {
    requested: usize,
    result: Result<ReplaceReport, String>,
}

/// A replacement waiting for the user's `y`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    All {
        occurrences: usize,
        files: usize,
        capped: bool,
    },
    File {
        path: PathBuf,
        label: String,
        occurrences: usize,
    },
}

/// Which input receives typing in replace mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Query,
    Replacement,
}

/// The two buttons on the Replace row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplaceButton {
    /// Replace the selected item (same as Ctrl+R).
    Selected,
    /// Replace every match (same as Alt+Enter).
    All,
}

/// How the status row is styled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusKind {
    Plain,
    Error,
    Prompt,
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
    /// The replacement the preview was last built with (replace mode).
    preview_replacement: Option<String>,
    preview: Option<Preview>,
    preview_offset: usize,
    preview_recenter: bool,
    done: bool,
    /// The Replace row is shown. `run()` starts it open, so it is visible from
    /// the first frame; the chevron folds it, and Tab, Shift+Tab or Ctrl+R
    /// re-open it. `App::new` defaults to closed (tests rely on that). It gates
    /// every replace key.
    replace_open: bool,
    focus: Focus,
    replacement: String,
    /// Byte offset of the caret in `replacement`, on a char boundary.
    replacement_cursor: usize,
    /// Ctrl+A on the focused field: the next edit replaces the whole value.
    select_all: SelectAll,
    /// A replacement that needs `y` before it runs.
    pending: Option<Pending>,
    /// A worker is replacing files; new replacements wait for it.
    replacing: bool,
    /// The last successful replacement's summary, until the query changes.
    notice: Option<String>,
    replace_tx: mpsc::Sender<ReplaceMessage>,
    replace_rx: mpsc::Receiver<ReplaceMessage>,
    chevron_zone: Rect,
    query_field: Rect,
    replace_field: Rect,
    replace_zones: Vec<(Rect, ReplaceButton)>,
}

impl App {
    fn new(root: PathBuf, caller_pane_id: Option<String>, theme: IconTheme) -> Self {
        let (tx, rx) = mpsc::channel();
        let (replace_tx, replace_rx) = mpsc::channel();
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
            preview_replacement: None,
            preview: None,
            preview_offset: 0,
            preview_recenter: false,
            done: false,
            replace_open: false,
            focus: Focus::Query,
            replacement: String::new(),
            replacement_cursor: 0,
            select_all: SelectAll::default(),
            pending: None,
            replacing: false,
            notice: None,
            replace_tx,
            replace_rx,
            chevron_zone: Rect::default(),
            query_field: Rect::default(),
            replace_field: Rect::default(),
            replace_zones: Vec::new(),
        }
    }

    /// Something is pending that needs the loop to wake up soon.
    fn busy(&self) -> bool {
        self.in_flight || self.replacing || self.dirty_since.is_some()
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
        while let Ok(message) = self.replace_rx.try_recv() {
            self.finish_replace(message);
            changed = true;
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
        self.notice = None;
    }

    /// Typing goes to the Replacement input only while it has focus and its
    /// row is shown. Editing it never starts a search.
    fn editing_replacement(&self) -> bool {
        self.replace_open && self.focus == Focus::Replacement
    }

    /// The text and caret of the input that receives typing.
    fn field_mut(&mut self) -> (&mut String, &mut usize) {
        if self.editing_replacement() {
            (&mut self.replacement, &mut self.replacement_cursor)
        } else {
            (&mut self.query, &mut self.cursor)
        }
    }

    /// Called after an edit to the focused field: only the query searches.
    fn after_field_edit(&mut self) {
        if !self.editing_replacement() {
            self.mark_edited();
        }
    }

    fn toggle_replace_row(&mut self) {
        self.select_all.disarm();
        self.replace_open = !self.replace_open;
        if !self.replace_open {
            self.focus = Focus::Query;
            self.pending = None;
        }
    }

    /// Opens the collapsed Replace row and focuses its input (Tab, Shift+Tab
    /// and Ctrl+R on a collapsed row).
    fn reveal_replace(&mut self) {
        self.select_all.disarm();
        self.replace_open = true;
        self.focus = Focus::Replacement;
    }

    /// Tab / Shift+Tab: reveal the row when collapsed, otherwise move focus
    /// between the two inputs.
    fn toggle_focus(&mut self) {
        if !self.replace_open {
            self.reveal_replace();
            return;
        }
        // The armed selection belongs to the field it was made in.
        self.select_all.disarm();
        self.focus = match self.focus {
            Focus::Query => Focus::Replacement,
            Focus::Replacement => Focus::Query,
        };
    }

    /// Replace actions need the Replace row open, a query, and no replacement
    /// already running. Otherwise they do nothing.
    fn replace_ready(&self) -> bool {
        self.replace_open && !self.replacing && !self.query.is_empty()
    }

    /// Whether the Replace buttons do anything right now.
    fn can_replace(&self) -> bool {
        !self.replacing && self.results.as_ref().is_some_and(|r| !r.hits.is_empty())
    }

    /// Ctrl+R: a match row replaces its first occurrence now; a file header
    /// asks first and then replaces every occurrence in that file.
    fn request_replace_selected(&mut self) {
        if !self.replace_ready() {
            return;
        }
        let Some(results) = &self.results else {
            return;
        };
        let Some(&row) = results.rows.get(self.selected) else {
            return;
        };
        match row {
            Row::Match { hit } => {
                let Some(hit) = results.hits.get(hit) else {
                    return;
                };
                let job = ReplaceJob::Selected {
                    path: hit.path.clone(),
                    line: hit.line,
                };
                self.start_replace(job);
            }
            Row::Header { group } => {
                let Some(group) = results.groups.get(group) else {
                    return;
                };
                let occurrences =
                    occurrences_of(&results.hits[group.first_hit..group.first_hit + group.count]);
                self.pending = Some(Pending::File {
                    path: group.path.clone(),
                    label: group.label.clone(),
                    occurrences,
                });
            }
        }
    }

    /// Alt+Enter: every occurrence in the project, after a `y`.
    fn request_replace_all(&mut self) {
        if !self.replace_ready() {
            return;
        }
        let Some(results) = &self.results else {
            return;
        };
        if results.hits.is_empty() {
            return;
        }
        self.pending = Some(Pending::All {
            occurrences: occurrences_of(&results.hits),
            files: results.groups.len(),
            capped: results.truncated,
        });
    }

    /// Run a confirmed replacement on a worker thread.
    fn start_replace(&mut self, job: ReplaceJob) {
        let requested = job.requested();
        self.replacing = true;
        self.error = None;
        self.notice = None;
        let root = self.root.clone();
        let query = self.query.clone();
        let options = self.options;
        let replacement = self.replacement.clone();
        let tx = self.replace_tx.clone();
        std::thread::spawn(move || {
            let result = run_replace_job(&root, job, &query, options, &replacement);
            let _ = tx.send(ReplaceMessage { requested, result });
        });
    }

    /// Show the outcome, toast it, and search again so the list matches disk.
    fn finish_replace(&mut self, message: ReplaceMessage) {
        self.replacing = false;
        match message.result {
            Ok(report) => {
                let missing = message.requested.saturating_sub(report.replacements);
                let skipped = report.skipped.len() + missing;
                let mut text = format!(
                    "Replaced {} occurrence{} in {} file{}",
                    report.replacements,
                    plural(report.replacements),
                    report.files_changed,
                    plural(report.files_changed),
                );
                if skipped > 0 {
                    text.push_str(&format!(", {skipped} skipped"));
                }
                // Tests run inside live herdr sessions too; a toast from them
                // would land in the user's real session.
                if !cfg!(test) {
                    crate::git_action::notify("Replace in files", &text);
                }
                self.notice = Some(text);
                self.error = None;
                // Open buffers and previews hold the old text.
                self.file_cache = None;
                self.preview_key = None;
                self.start_search();
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// The replacement the preview should show, or `None` while the Replace
    /// row is collapsed.
    fn replacement_preview(&self) -> Option<String> {
        self.replace_open.then(|| self.replacement.clone())
    }

    fn pending_prompt(&self, pending: &Pending) -> String {
        match pending {
            Pending::All {
                occurrences,
                files,
                capped,
            } => {
                let plus = if *capped { "+" } else { "" };
                format!(
                    "Replace {occurrences}{plus} occurrence{} in {files}{plus} file{} with \"{}\"? (y/N)",
                    plural(*occurrences),
                    plural(*files),
                    self.replacement,
                )
            }
            Pending::File {
                label, occurrences, ..
            } => format!(
                "Replace {occurrences} occurrence{} in {label} with \"{}\"? (y/N)",
                plural(*occurrences),
                self.replacement,
            ),
        }
    }

    fn toggle(&mut self, toggle: Toggle) {
        match toggle {
            Toggle::MatchCase => self.options.match_case = !self.options.match_case,
            Toggle::WholeWord => self.options.whole_word = !self.options.whole_word,
            Toggle::Regex => self.options.regex = !self.options.regex,
        }
        self.mark_edited();
    }

    /// Ctrl+A on the focused field. There is no selection model here, so this
    /// arms [`SelectAll`]: the value is drawn as selected and the next edit
    /// replaces all of it (see `ui::SelectAll`).
    fn arm_select_all(&mut self) {
        let has_text = {
            let (text, _) = self.field_mut();
            !text.is_empty()
        };
        self.select_all.arm_if_nonempty(has_text);
    }

    /// Drop an armed select-all, clearing the focused field. True when it did,
    /// i.e. when the caller must skip the edit it was about to make.
    fn clear_selected(&mut self) -> bool {
        if !self.select_all.armed() {
            return false;
        }
        if self.editing_replacement() {
            self.select_all
                .take(&mut self.replacement, &mut self.replacement_cursor);
        } else {
            self.select_all.take(&mut self.query, &mut self.cursor);
        }
        true
    }

    fn insert_char(&mut self, character: char) {
        self.clear_selected();
        let (text, cursor) = self.field_mut();
        text.insert(*cursor, character);
        *cursor += character.len_utf8();
        self.after_field_edit();
    }

    fn delete_before_cursor(&mut self) {
        if self.clear_selected() {
            self.after_field_edit();
            return;
        }
        let (text, cursor) = self.field_mut();
        if let Some((index, _)) = text[..*cursor].char_indices().next_back() {
            text.remove(index);
            *cursor = index;
            self.after_field_edit();
        }
    }

    fn delete_at_cursor(&mut self) {
        if self.clear_selected() {
            self.after_field_edit();
            return;
        }
        let (text, cursor) = self.field_mut();
        if *cursor < text.len() {
            text.remove(*cursor);
            self.after_field_edit();
        }
    }

    fn cursor_left(&mut self) {
        self.select_all.disarm();
        let (text, cursor) = self.field_mut();
        if let Some((index, _)) = text[..*cursor].char_indices().next_back() {
            *cursor = index;
        }
    }

    fn cursor_right(&mut self) {
        self.select_all.disarm();
        let (text, cursor) = self.field_mut();
        if let Some(character) = text[*cursor..].chars().next() {
            *cursor += character.len_utf8();
        }
    }

    fn clear_field(&mut self) {
        self.select_all.disarm();
        let (text, cursor) = self.field_mut();
        text.clear();
        *cursor = 0;
        self.after_field_edit();
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // A pending replacement takes exactly one key: `y` confirms, anything
        // else (Esc included) cancels. The key is consumed either way.
        if let Some(pending) = self.pending.take() {
            let confirmed = matches!(key.code, KeyCode::Char('y' | 'Y')) && !ctrl && !alt;
            if confirmed {
                match pending {
                    Pending::All { .. } => self.start_replace(ReplaceJob::All),
                    Pending::File {
                        path, occurrences, ..
                    } => self.start_replace(ReplaceJob::File {
                        path,
                        requested: occurrences,
                    }),
                }
            }
            return;
        }
        let page = self.page() as isize;
        match key.code {
            // Esc first drops an armed Ctrl+A selection, then closes.
            KeyCode::Esc if self.select_all.armed() => self.select_all.disarm(),
            KeyCode::Esc => self.done = true,
            // Alt+Enter does nothing while the Replace row is collapsed.
            KeyCode::Enter if alt => self.request_replace_all(),
            KeyCode::Enter => self.activate(),
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Left => self.cursor_left(),
            KeyCode::Right => self.cursor_right(),
            KeyCode::Home => {
                self.select_all.disarm();
                let (_, cursor) = self.field_mut();
                *cursor = 0;
            }
            KeyCode::End => {
                self.select_all.disarm();
                let (text, cursor) = self.field_mut();
                *cursor = text.len();
            }
            KeyCode::Backspace => self.delete_before_cursor(),
            KeyCode::Delete => self.delete_at_cursor(),
            KeyCode::Char('a') if ctrl && !alt => self.arm_select_all(),
            KeyCode::Char('u') if ctrl && !alt => self.clear_field(),
            // Ctrl+R on a collapsed row reveals it instead of replacing.
            KeyCode::Char('r') if ctrl && !alt => {
                if self.replace_open {
                    self.request_replace_selected();
                } else {
                    self.reveal_replace();
                }
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
                // Any click answers a pending replacement with "no", and it
                // also leaves the field a Ctrl+A selection was made in.
                self.pending = None;
                self.select_all.disarm();
                if contains(self.chevron_zone, column, row) {
                    self.toggle_replace_row();
                    return;
                }
                if self.replace_open {
                    if let Some(&(_, button)) = self
                        .replace_zones
                        .iter()
                        .find(|(zone, _)| contains(*zone, column, row))
                    {
                        match button {
                            ReplaceButton::Selected => self.request_replace_selected(),
                            ReplaceButton::All => self.request_replace_all(),
                        }
                        return;
                    }
                    if contains(self.replace_field, column, row) {
                        self.focus = Focus::Replacement;
                        return;
                    }
                }
                if contains(self.query_field, column, row) {
                    self.focus = Focus::Query;
                }
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
        let replacement = self.replacement_preview();
        if self.preview_key.as_ref() == Some(&key) && self.preview_replacement == replacement {
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
            replacement.as_deref(),
        ));
        self.preview_replacement = replacement;
        self.preview_key = Some(key);
        self.preview_recenter = true;
    }

    fn status(&self) -> (String, StatusKind) {
        if let Some(error) = &self.error {
            return (error.clone(), StatusKind::Error);
        }
        if let Some(pending) = &self.pending {
            return (self.pending_prompt(pending), StatusKind::Prompt);
        }
        if self.replacing {
            return ("replacing…".into(), StatusKind::Plain);
        }
        if let Some(notice) = &self.notice {
            return (notice.clone(), StatusKind::Plain);
        }
        if self.query.is_empty() {
            return ("type to search file contents".into(), StatusKind::Plain);
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
        (text, StatusKind::Plain)
    }

    fn footer_hint(&self) -> &'static str {
        if self.replace_open {
            " ↑↓ move · tab field · ctrl+r replace · alt+enter all · ⏎ open · esc close"
        } else {
            " ↑↓ move · ⏎ open · tab replace · alt+c/w/r options · esc close"
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // The Replace mode needs one row more than Find alone (see `extra`).
        let min_height = 6 + u16::from(self.replace_open);
        if area.width < 20 || area.height < min_height {
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
        // The Replace row adds a separator row plus the row itself under the
        // query (so the two inputs do not read as one block), and everything
        // below moves down by that much.
        let replace_row = self.replace_open;
        let extra = 2 * u16::from(replace_row);
        let list_y = area.y + 4 + extra;
        let list_height = area.height.saturating_sub(5 + extra);
        self.list_area = Rect::new(area.x, list_y, left_width, list_height);

        // Row 0: title and the searched root.
        let root_name = self
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let title = if self.replace_open {
            " Replace in files"
        } else {
            " Search in files"
        };
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

        // Row 1: a disclosure chevron, then the query label and input with the
        // three toggle chips at its right end. Both rows share one label column,
        // so the query and Replace fields start at the same x.
        let fit = fit_replace_row(left_width, self.replace_open);
        let label_width = fit.label_width();
        let chevron = if self.replace_open { "▾ " } else { "▸ " };
        let header = Style::default()
            .fg(palette().header_accent)
            .add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(Span::styled(chevron, header)),
            Rect::new(area.x, area.y + 1, 2, 1),
        );
        self.chevron_zone = Rect::new(area.x, area.y + 1, 2, 1);
        let query_label = match fit.label {
            LabelMode::Full => "Find      ".to_string(),
            LabelMode::Icon => "  ".to_string(),
            LabelMode::Hidden => String::new(),
        };
        self.draw_label(
            frame,
            Rect::new(area.x + 2, area.y + 1, label_width, 1),
            query_label,
        );
        let query_x = area.x + 2 + label_width;
        let query_width = left_width.saturating_sub(2 + label_width);
        self.query_field = Rect::new(query_x, area.y + 1, query_width, 1);
        self.draw_input(frame, query_x, area.y + 1, query_width, dim);

        // Row 2 (replace mode, when open): a dim rule under Find, then the
        // Replace label, input and buttons. A full blank line read as tall as a
        // whole field and a half-block bar looked like a glitch (user-reported),
        // so the gap is a solid rule aligned with the fields.
        if replace_row {
            frame.render_widget(
                Paragraph::new(Span::styled("─".repeat(usize::from(query_width)), dim)),
                Rect::new(query_x, area.y + 2, query_width, 1),
            );
            self.draw_replace_row(frame, area.x, area.y + 3, left_width, fit);
        } else {
            self.replace_field = Rect::default();
            self.replace_zones.clear();
        }

        // Status (or error, confirmation, notice): one line under the inputs.
        let (status, kind) = self.status();
        let status_style = match kind {
            StatusKind::Error => Style::default().fg(palette().deleted),
            StatusKind::Prompt => accent.add_modifier(Modifier::BOLD),
            StatusKind::Plain => dim,
        };
        // A confirmation keeps its tail (`? (y/N)`) visible when it is too long.
        let status_text = match kind {
            StatusKind::Prompt => {
                ellipsize_middle(&format!(" {status}"), usize::from(left_width), 8)
            }
            _ => truncate_to(format!(" {status}"), usize::from(left_width)),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(status_text, status_style)),
            Rect::new(area.x, area.y + 2 + extra, left_width, 1),
        );

        // Divider.
        frame.render_widget(
            Paragraph::new(Span::styled("─".repeat(usize::from(left_width)), dim)),
            Rect::new(area.x, area.y + 3 + extra, left_width, 1),
        );

        self.draw_list(frame, left_width, list_height);

        if preview_visible {
            self.draw_preview(frame, area, left_width, dim);
        }

        // Last row: key hints.
        let footer = Rect::new(area.x, area.y + area.height - 1, width, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_to(self.footer_hint().to_string(), usize::from(width)),
                dim,
            )),
            footer,
        );
    }

    fn draw_input(&mut self, frame: &mut Frame, x: u16, y: u16, width: u16, dim: Style) {
        let field = field_style();
        fill_field(frame, Rect::new(x, y, width, 1));
        let input_width = width.saturating_sub(CHIPS_WIDTH + 1);
        let chips_x = x + width.saturating_sub(CHIPS_WIDTH + 1);
        let caret = !self.editing_replacement();
        let input = Rect::new(x, y, input_width, 1);
        if self.query.is_empty() && !caret {
            draw_placeholder(frame, input, "Search");
        } else {
            let selected = self.select_all.armed() && caret;
            let room = usize::from(input_width).saturating_sub(2);
            frame.render_widget(
                Paragraph::new(input_line(
                    &self.query,
                    self.cursor,
                    room,
                    field,
                    caret,
                    selected,
                )),
                input,
            );
        }

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
                dim.patch(field)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {label} "), style)),
                Rect::new(chip_x, y, CHIP_WIDTH, 1),
            );
            self.toggle_zones
                .push((Rect::new(chip_x, y, CHIP_WIDTH, 1), toggle));
        }
    }

    /// A label cell in the bold header accent (the Find / Replace labels).
    fn draw_label(&self, frame: &mut Frame, rect: Rect, text: String) {
        if rect.width == 0 || text.is_empty() {
            return;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(
                text,
                Style::default()
                    .fg(palette().header_accent)
                    .add_modifier(Modifier::BOLD),
            )),
            rect,
        );
    }

    /// The Replace row: the `⇄ Replace` label, a filled input box, then the
    /// Replace and All buttons at its right end. Buttons are never dropped;
    /// `fit` only trims the label and the button icons.
    fn draw_replace_row(
        &mut self,
        frame: &mut Frame,
        x: u16,
        y: u16,
        left_width: u16,
        fit: ReplaceFit,
    ) {
        let icon = replace_icon(self.theme);
        let label = match fit.label {
            LabelMode::Full => format!("{icon} Replace "),
            LabelMode::Icon => format!("{icon} "),
            LabelMode::Hidden => String::new(),
        };
        self.draw_label(frame, Rect::new(x + 2, y, fit.label_width(), 1), label);

        let (replace_label, all_label) = if fit.icons {
            (
                format!(" {} Replace ", replace_icon(self.theme)),
                format!(" {} All ", all_icon(self.theme)),
            )
        } else {
            (" Replace ".to_string(), " All ".to_string())
        };
        let replace_cells = replace_label.chars().count() as u16;
        let all_cells = all_label.chars().count() as u16;
        // One cell of margin on the right, like the query's chips.
        let buttons_x = x + left_width.saturating_sub(replace_cells + 1 + all_cells + 1);
        let all_x = buttons_x + replace_cells + 1;
        let field_x = x + 2 + fit.label_width();
        let field_width = buttons_x.saturating_sub(field_x).saturating_sub(1);
        let field = Rect::new(field_x, y, field_width, 1);
        self.replace_field = field;
        fill_field(frame, field);
        let caret = self.editing_replacement();
        if self.replacement.is_empty() && !caret {
            draw_placeholder(frame, field, "Replace with…");
        } else {
            let selected = self.select_all.armed() && caret;
            let room = usize::from(field_width).saturating_sub(2);
            frame.render_widget(
                Paragraph::new(input_line(
                    &self.replacement,
                    self.replacement_cursor,
                    room,
                    field_style(),
                    caret,
                    selected,
                )),
                field,
            );
        }

        let style = if self.can_replace() {
            Style::default()
                .fg(palette().button_fg)
                .bg(palette().button_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(palette().keycap_fg)
                .bg(palette().keycap_bg)
        };
        let replace_rect = Rect::new(buttons_x, y, replace_cells, 1);
        let all_rect = Rect::new(all_x, y, all_cells, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(replace_label, style)),
            replace_rect,
        );
        frame.render_widget(Paragraph::new(Span::styled(all_label, style)), all_rect);
        self.replace_zones = vec![
            (replace_rect, ReplaceButton::Selected),
            (all_rect, ReplaceButton::All),
        ];
    }

    fn draw_list(&mut self, frame: &mut Frame, left_width: u16, list_height: u16) {
        let visible = usize::from(list_height);
        let total = self.rows_len();
        let replacement = self.replacement_preview();
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
                    Row::Match { hit } => match &replacement {
                        Some(replacement) => {
                            replace_match_line(&results.hits[hit], replacement, width)
                        }
                        None => match_line(&results.hits[hit], width),
                    },
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
                        match (is_hit, &code.hit_replaced) {
                            (true, Some(replaced)) => {
                                band_line(gutter, replaced.clone(), usize::from(width), dim)
                            }
                            _ => code_line(
                                gutter,
                                line.spans.clone(),
                                is_hit.then_some(code.hit_ranges.as_slice()),
                                usize::from(width),
                                dim,
                            ),
                        }
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

/// The filled box behind every text input (query and Replace).
fn field_style() -> Style {
    Style::default()
        .fg(palette().keycap_fg)
        .bg(palette().keycap_bg)
}

/// Paint a field's whole rect with the input background, so an empty field is
/// still visibly a box.
fn fill_field(frame: &mut Frame, rect: Rect) {
    frame.render_widget(
        Paragraph::new(" ".repeat(usize::from(rect.width))).style(field_style()),
        rect,
    );
}

/// The dim hint shown in an unfocused, empty input.
fn draw_placeholder(frame: &mut Frame, rect: Rect, text: &str) {
    let style = field_style().add_modifier(Modifier::DIM | Modifier::ITALIC);
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_to(format!(" {text}"), usize::from(rect.width)),
            style,
        )),
        rect,
    );
}

/// How the Replace row trims itself to the popup width. Both rows share the
/// label column, so the query and Replace fields always start at one x.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelMode {
    /// `Find` / `⇄ Replace`.
    Full,
    /// Only the icon (the query label is blank).
    Icon,
    /// No label at all.
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplaceFit {
    /// Whether the buttons carry their icons.
    icons: bool,
    label: LabelMode,
}

impl ReplaceFit {
    fn label_width(self) -> u16 {
        match self.label {
            LabelMode::Full => 10,
            LabelMode::Icon => 2,
            LabelMode::Hidden => 0,
        }
    }
}

/// Button widths: ` ⇄ Replace ` + gap + ` ⇄ All ` and the plain ` Replace ` +
/// gap + ` All `.
const BUTTONS_WITH_ICONS: u16 = 20;
const BUTTONS_PLAIN: u16 = 15;
/// The narrowest field worth showing before the label gives way.
const FIELD_MIN: u16 = 8;

/// Drop the button icons first, then the label text (keeping its icon), then
/// the label itself, until the chevron, label, minimum field and buttons fit.
fn fit_replace_row(width: u16, replace_open: bool) -> ReplaceFit {
    let candidates = [
        ReplaceFit {
            icons: true,
            label: LabelMode::Full,
        },
        ReplaceFit {
            icons: false,
            label: LabelMode::Full,
        },
        ReplaceFit {
            icons: false,
            label: LabelMode::Icon,
        },
    ];
    for fit in candidates {
        let buttons = match (replace_open, fit.icons) {
            (false, _) => 0,
            (true, true) => BUTTONS_WITH_ICONS,
            (true, false) => BUTTONS_PLAIN,
        };
        // Chevron (2) + label + gap (1) + margin (1) + buttons + field minimum.
        if width >= 2 + fit.label_width() + buttons + 2 + FIELD_MIN {
            return fit;
        }
    }
    ReplaceFit {
        icons: false,
        label: LabelMode::Hidden,
    }
}

/// Nerd Font codicon cod-replace, or the plain arrows outside Material.
fn replace_icon(theme: IconTheme) -> &'static str {
    if matches!(theme, IconTheme::Material) {
        "\u{eb3d}"
    } else {
        "⇄"
    }
}

/// Nerd Font codicon cod-replace-all, or the plain double arrow.
fn all_icon(theme: IconTheme) -> &'static str {
    if matches!(theme, IconTheme::Material) {
        "\u{eb3c}"
    } else {
        "⇉"
    }
}

/// One input line: a one-cell pad, the text around the caret, and the caret
/// when this input has focus. Long text scrolls to keep the caret visible.
/// `base` is the field's background; `selected` draws the value as selected —
/// an armed Ctrl+A, waiting to be typed over.
fn input_line(
    text: &str,
    cursor: usize,
    room: usize,
    base: Style,
    caret: bool,
    selected: bool,
) -> Line<'static> {
    let text_style = if selected {
        base.patch(selected_text_style())
    } else {
        base
    };
    let before = input_tail(&text[..cursor], room);
    let used = before.chars().count();
    let after = truncate_to(text[cursor..].to_string(), room.saturating_sub(used));
    let mut spans = vec![Span::styled(" ", base), Span::styled(before, text_style)];
    if caret {
        spans.push(Span::styled(
            "█",
            base.patch(Style::default().fg(palette().accent)),
        ));
    }
    spans.push(Span::styled(after, text_style));
    Line::from(spans)
}

/// A replace-mode match row: the same `  <line>: ` prefix, then each match
/// struck through and followed by its replacement.
fn replace_match_line(hit: &ContentHit, replacement: &str, width: usize) -> Line<'static> {
    let prefix = format!("   {}: ", hit.line);
    let context_width = width.saturating_sub(prefix.len());
    let mut spans = vec![Span::styled(
        prefix,
        Style::default().fg(palette().accent_focus),
    )];
    spans.extend(replaced_context_spans(
        &hit.context,
        &hit.matches,
        replacement,
        context_width,
    ));
    Line::from(spans)
}

/// A preview line with the hit's band, for pre-styled spans (the replace-mode
/// hit line). Same gutter and padding as [`code_line`].
fn band_line(gutter: String, spans: Vec<Span<'static>>, width: usize, dim: Style) -> Line<'static> {
    let band = Style::default().bg(palette().hover_bg);
    let mut out = vec![Span::styled(gutter, dim.patch(band))];
    for span in spans {
        out.push(Span::styled(span.content, span.style.patch(band)));
    }
    let used: usize = out.iter().map(|span| span.width()).sum();
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), band));
    }
    Line::from(out)
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
    fn a_mouse_click_drops_an_armed_selection() {
        let mut app = App::new(PathBuf::from("/r"), None, IconTheme::Emoji);
        app.replace_open = true;
        for character in "needle".chars() {
            app.key(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.query_field = Rect::new(0, 0, 40, 1);
        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.select_all.armed());
        // Clicking anywhere (here: the field itself) is a focus move.
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.select_all.armed(), "a click leaves the field");
        assert_eq!(app.query, "needle", "and it must not eat the text");
        assert!(!app.done);
    }

    #[test]
    fn ctrl_a_replaces_the_whole_query_on_the_next_edit() {
        let mut app = App::new(PathBuf::from("/r"), None, IconTheme::Emoji);
        for character in "héllo".chars() {
            app.key(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(app.select_all.armed(), "ctrl+a selects the query");
        // The next typed character replaces everything, not just the caret.
        app.key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.query, "x");
        assert!(!app.select_all.armed());

        // Backspace over a selection clears the field instead of one char.
        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.query, "");

        // An empty field has nothing to select.
        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!app.select_all.armed());
    }

    #[test]
    fn ctrl_a_is_dropped_by_a_caret_move_and_by_esc() {
        let mut app = App::new(PathBuf::from("/r"), None, IconTheme::Emoji);
        for character in "abc".chars() {
            app.key(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.key(key(KeyCode::Left, KeyModifiers::NONE));
        assert!(!app.select_all.armed(), "moving the caret drops it");
        app.key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.query, "ac", "a plain backspace after the move");

        app.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.select_all.armed());
        assert!(!app.done, "the first Esc only drops the selection");
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

#[cfg(test)]
mod replace_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// A temp project with `src/needle.rs` holding one match on line 2.
    fn project(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-replace-popup-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/needle.rs"),
            "use crate::prelude;\nfn needle_body() {}\n// tail\n",
        )
        .unwrap();
        root
    }

    /// A replace-mode app over `root` with the query searched and the results
    /// in place. `replacement` is the text to type into the Replace input.
    fn replace_app(root: &Path, replacement: &str) -> App {
        let mut app = App::new(root.to_path_buf(), None, IconTheme::Emoji);
        app.replace_open = true;
        app.query = "needle".into();
        app.cursor = app.query.len();
        app.replacement = replacement.into();
        app.replacement_cursor = app.replacement.len();
        let (hits, _) =
            collect_content_hits(root, false, "needle", "", "", SearchOptions::default(), 20)
                .unwrap();
        app.set_results_for_test(hits, false);
        app
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn drawn(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let area = buffer.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    /// Wait for the replace worker and let `tick` apply its result.
    fn wait_for_replace(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.replacing && Instant::now() < deadline {
            app.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        app.tick();
        assert!(!app.replacing, "replacement did not finish");
    }

    #[test]
    fn replace_match_line_strikes_old_and_adds_new() {
        let hit = ContentHit {
            path: PathBuf::from("/r/a.rs"),
            label: "a.rs".into(),
            line: 2,
            context: "fn needle_body() {}".into(),
            matches: vec![(3, 9)],
        };
        let line = replace_match_line(&hit, "pin", 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "   2: fn needlepin_body() {}");
        let struck: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(struck, vec!["needle"]);
        assert!(
            line.spans
                .iter()
                .any(|s| s.content == "pin" && s.style.fg == Some(palette().diff_add_mark))
        );
    }

    #[test]
    fn clipped_replacement_row_ends_with_an_ellipsis() {
        let spans = clip_spans(vec![Span::raw("abcdef")], 4);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "abc…");
        assert_eq!(clip_spans(vec![Span::raw("abc")], 4).len(), 1);
    }

    #[test]
    fn draw_shows_replace_row_buttons_and_old_new_match() {
        let root = project("draw");
        let mut app = replace_app(&root, "pin");
        app.selected = 1;
        let buffer = drawn(&mut app, 120, 30);
        let text = buffer_text(&buffer);
        assert!(text.contains("Replace in files"), "title");
        assert!(text.contains("⇄ Replace"), "Replace label");
        assert!(text.contains(" ⇄ Replace "), "Replace button");
        assert!(text.contains(" ⇉ All "), "All button");
        assert!(text.contains("pin"), "replacement is drawn");
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == "n" && cell.modifier.contains(Modifier::CROSSED_OUT)),
            "old text is struck through"
        );
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.symbol() == "p" && cell.fg == palette().diff_add_mark),
            "replacement uses the diff add color"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_row_hidden_keeps_the_search_layout() {
        let root = project("hidden");
        let mut app = replace_app(&root, "pin");
        app.replace_open = false;
        app.selected = 1;
        let text = buffer_text(&drawn(&mut app, 120, 30));
        assert!(text.contains("▸"), "collapsed chevron");
        assert!(
            !text.contains("⇄ Replace"),
            "no Replace input when collapsed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn confirmation_prompt_renders_in_the_status_line() {
        let root = project("prompt");
        let mut app = replace_app(&root, "pin");
        app.selected = 0; // the file header
        app.key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let text = buffer_text(&drawn(&mut app, 200, 30));
        assert!(
            text.contains(r#"Replace 1 occurrence in src/needle.rs with "pin"? (y/N)"#),
            "file prompt: {text}"
        );
        let narrow = buffer_text(&drawn(&mut app, 120, 30));
        assert!(
            narrow.contains("Replace 1 occurrence in src/needle.rs") && narrow.contains("(y/N)"),
            "a narrow pane keeps the head and the (y/N) tail: {narrow}"
        );
        app.key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.done, "Esc cancels the prompt instead of closing");
        assert!(app.pending.is_none());

        app.key(key(KeyCode::Enter, KeyModifiers::ALT));
        let text = buffer_text(&drawn(&mut app, 200, 30));
        assert!(
            text.contains(r#"Replace 1 occurrence in 1 file with "pin"? (y/N)"#),
            "all prompt: {text}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tab_moves_focus_between_the_inputs_while_the_replace_row_is_open() {
        let root = project("focus");
        let mut app = replace_app(&root, "");
        assert_eq!(app.focus, Focus::Query);
        app.key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Replacement);
        app.key(key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.focus, Focus::Query);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A default (search) popup: the Replace row is collapsed, with results.
    fn search_app(root: &Path) -> App {
        let mut app = App::new(root.to_path_buf(), None, IconTheme::Emoji);
        app.query = "needle".into();
        app.cursor = app.query.len();
        let (hits, _) =
            collect_content_hits(root, false, "needle", "", "", SearchOptions::default(), 20)
                .unwrap();
        app.set_results_for_test(hits, false);
        app
    }

    #[test]
    fn search_popup_offers_a_collapsed_replace_row() {
        let root = project("collapsed");
        let mut app = search_app(&root);
        let text = buffer_text(&drawn(&mut app, 120, 30));
        assert!(text.contains("Search in files"), "search title");
        assert!(text.contains("▸"), "collapsed chevron is always drawn");
        assert!(
            !text.contains("⇄ Replace"),
            "no Replace input when collapsed"
        );

        // The chevron zone is populated by the draw above; click it.
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.replace_open, "chevron click opens the row");
        let text = buffer_text(&drawn(&mut app, 120, 30));
        assert!(text.contains("Replace in files"), "replace title");
        assert!(text.contains("▾"), "expanded chevron");
        assert!(text.contains("⇄ Replace"), "Replace input row");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tab_reveals_replace_from_search() {
        let root = project("tab-reveal");
        let mut app = search_app(&root);
        assert!(!app.replace_open);
        app.key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.replace_open, "Tab opens the collapsed row");
        assert_eq!(app.focus, Focus::Replacement);
        app.key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Query);
        assert!(app.replace_open, "moving focus keeps the row open");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_r_reveals_replace_before_replacing() {
        let root = project("reveal");
        let mut app = search_app(&root);
        app.selected = 1; // the match row, which Ctrl+R replaces when open
        app.key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.replace_open, "Ctrl+R opens the collapsed row");
        assert_eq!(app.focus, Focus::Replacement);
        assert!(app.pending.is_none(), "no prompt was set");
        assert!(!app.replacing, "no replacement started");
        assert_eq!(
            std::fs::read_to_string(root.join("src/needle.rs")).unwrap(),
            "use crate::prelude;\nfn needle_body() {}\n// tail\n",
            "the file is untouched"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn typing_in_the_replacement_does_not_search_again() {
        let root = project("typing");
        let mut app = replace_app(&root, "");
        app.key(key(KeyCode::Tab, KeyModifiers::NONE));
        let generation = app.generation;
        app.key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        app.key(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.replacement, "xy");
        assert_eq!(app.query, "needle", "query untouched");
        assert_eq!(app.generation, generation, "no new search generation");
        assert!(app.dirty_since.is_none(), "no debounced search scheduled");

        app.key(key(KeyCode::Tab, KeyModifiers::NONE));
        app.key(key(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(app.query, "needlez");
        assert!(app.dirty_since.is_some(), "query edits do search");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn y_confirms_and_any_other_key_cancels() {
        let root = project("confirm");
        let mut app = replace_app(&root, "pin");
        app.selected = 0;
        app.key(key(KeyCode::Enter, KeyModifiers::ALT));
        assert!(matches!(app.pending, Some(Pending::All { .. })));
        app.key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.pending.is_none(), "any other key cancels");
        assert!(!app.replacing);
        assert_eq!(
            std::fs::read_to_string(root.join("src/needle.rs")).unwrap(),
            "use crate::prelude;\nfn needle_body() {}\n// tail\n",
            "a cancelled prompt writes nothing"
        );

        let generation = app.generation;
        app.key(key(KeyCode::Enter, KeyModifiers::ALT));
        app.key(key(KeyCode::Char('Y'), KeyModifiers::SHIFT));
        assert!(app.pending.is_none());
        wait_for_replace(&mut app);
        assert_eq!(
            std::fs::read_to_string(root.join("src/needle.rs")).unwrap(),
            "use crate::prelude;\nfn pin_body() {}\n// tail\n"
        );
        assert_eq!(
            app.notice.as_deref(),
            Some("Replaced 1 occurrence in 1 file")
        );
        assert!(
            app.generation > generation,
            "a fresh search starts after the replace"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ctrl_r_on_a_match_replaces_without_confirmation() {
        let root = project("selected");
        let mut app = replace_app(&root, "pin");
        app.selected = 1; // the match row
        app.key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.pending.is_none(), "single occurrence needs no prompt");
        wait_for_replace(&mut app);
        assert_eq!(
            std::fs::read_to_string(root.join("src/needle.rs")).unwrap(),
            "use crate::prelude;\nfn pin_body() {}\n// tail\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_actions_need_the_row_open_and_a_query() {
        let root = project("guards");
        let mut app = replace_app(&root, "pin");
        app.replace_open = false;
        app.selected = 1;
        app.key(key(KeyCode::Enter, KeyModifiers::ALT));
        assert!(app.pending.is_none() && !app.replacing && !app.replace_open);

        app.replace_open = true;
        app.query.clear();
        app.key(key(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(app.pending.is_none() && !app.replacing);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collapsed_row_alt_enter_does_nothing() {
        let root = project("search-mode");
        let mut app = search_app(&root);
        app.selected = 1;
        app.key(key(KeyCode::Enter, KeyModifiers::ALT));
        assert!(app.pending.is_none() && !app.replacing);
        assert!(!app.replace_open, "Alt+Enter does not expand the row");
        assert_eq!(
            std::fs::read_to_string(root.join("src/needle.rs")).unwrap(),
            "use crate::prelude;\nfn needle_body() {}\n// tail\n"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The `(x, y)` columns where `needle` starts on row `y`, one char per cell.
    fn columns_of(buffer: &Buffer, y: u16, needle: &str) -> Vec<u16> {
        let width = buffer.area.width;
        let cells: Vec<String> = (0..width)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect();
        let want: Vec<&str> = needle.chars().map(|_| "").collect();
        let chars: Vec<String> = needle.chars().map(|c| c.to_string()).collect();
        let _ = want;
        (0..width.saturating_sub(chars.len() as u16))
            .filter(|&x| {
                chars
                    .iter()
                    .enumerate()
                    .all(|(i, c)| cells[usize::from(x) + i] == *c)
            })
            .collect()
    }

    #[test]
    fn replace_row_is_drawn_as_a_filled_box_with_visible_buttons() {
        let root = project("box");
        // Nothing to replace yet: an empty query leaves the buttons disabled.
        let mut empty = App::new(root.clone(), None, IconTheme::Emoji);
        empty.replace_open = true;
        let buffer = drawn(&mut empty, 120, 30);
        let row = buffer_text(&buffer);
        // Row 2 is the dim rule that separates Replace from Find; the Replace
        // row itself is row 3.
        let spacer = row.lines().nth(2).unwrap();
        assert!(spacer.contains('─'), "separator rule: {spacer}");
        // Row 3 is the Replace row itself.
        let row2 = row.lines().nth(3).unwrap();
        assert!(row2.contains("⇄ Replace"), "label: {row2}");
        assert!(row2.contains("⇉ All"), "All button: {row2}");
        assert!(row2.contains("Replace with…"), "placeholder: {row2}");
        let all_x = columns_of(&buffer, 3, "All")[0];
        assert_eq!(buffer[(all_x, 3)].bg, palette().keycap_bg, "disabled bg");
        assert_eq!(buffer[(all_x, 3)].fg, palette().keycap_fg, "disabled fg");

        // With results the buttons take the enabled button colors.
        let root = project("box-enabled");
        let mut app = replace_app(&root, "pin");
        let buffer = drawn(&mut app, 120, 30);
        let all_x = columns_of(&buffer, 3, "All")[0];
        assert_eq!(buffer[(all_x, 3)].bg, palette().button_bg, "enabled bg");
        assert_eq!(buffer[(all_x, 3)].fg, palette().button_fg, "enabled fg");
        let replace_x = *columns_of(&buffer, 3, "Replace").last().unwrap();
        assert_eq!(buffer[(replace_x, 3)].bg, palette().button_bg);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replace_row_survives_a_narrow_popup() {
        let root = project("narrow");
        for width in [40, 24] {
            let mut app = replace_app(&root, "pin");
            let buffer = drawn(&mut app, width, 20);
            let text = buffer_text(&buffer);
            assert!(text.contains("All"), "All button at width {width}: {text}");
            assert!(text.contains("Replace"), "Replace button at width {width}");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
