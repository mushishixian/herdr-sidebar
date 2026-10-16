//! Git popups in herdr: `--git-popup switch|rename|merge|history`.
//!
//! Two halves. `--git-popup-open <op>` runs as a plugin ACTION (bound to host
//! keys): it finds the focused pane's working directory and asks herdr to open
//! the matching `[[panes]]` entrypoint as a session-modal popup there. The
//! popup process (`--git-popup <op>`) is a small one-shot TUI against the
//! repository containing that directory; it exits — closing the popup — once
//! the operation is done or cancelled.
//!
//! - switch: type to filter local and remote branches; Enter checks one out.
//!   A name that matches no local branch can be created on the spot.
//! - rename: edit the current branch's name.
//! - merge: pick a branch and confirm; merges it into the current branch and
//!   shows git's summary, or the conflicted files with an abort option.
//! - history: `git log -L` for a line range of one file (the Fresh editor
//!   opens it directly with env vars; there is no action half). See
//!   `HistoryApp`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use crate::actions::{ClipboardWrite, copy_to_clipboard};
use crate::diffview;
use crate::git::{Branch, Git, LineCommit, MergeOutcome};
use crate::ui::{input_tail, keep_visible_scroll, palette, selection_style, truncate_to};

/// Set by the Fresh editor plugin: absolute path of the file to inspect.
pub const HISTORY_FILE_ENV: &str = "HERDR_SIDEBAR_HISTORY_FILE";
/// Set by the Fresh editor plugin: `START,END`, 1-based and inclusive.
pub const HISTORY_LINES_ENV: &str = "HERDR_SIDEBAR_HISTORY_LINES";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Switch,
    Rename,
    Merge,
    History,
}

impl Op {
    pub fn from_flag(flag: &str) -> Option<Op> {
        match flag {
            "switch" => Some(Op::Switch),
            "rename" => Some(Op::Rename),
            "merge" => Some(Op::Merge),
            "history" => Some(Op::History),
            _ => None,
        }
    }

    /// The manifest `[[panes]]` id that runs this operation.
    fn entrypoint(self) -> &'static str {
        match self {
            Op::Switch => "git-switch",
            Op::Rename => "git-rename",
            Op::Merge => "git-merge",
            Op::History => "git-line-history",
        }
    }

    /// Outer popup size: list popups get room, rename is one input line.
    fn popup_size(self) -> (serde_json::Value, serde_json::Value) {
        match self {
            Op::Rename => (serde_json::json!("60%"), serde_json::json!(8)),
            Op::History => (serde_json::json!("90%"), serde_json::json!("90%")),
            _ => (serde_json::json!("70%"), serde_json::json!("70%")),
        }
    }
}

const USAGE: &str = "expected switch, rename, merge or history";

// ── action side ─────────────────────────────────────────────────────────

/// Open the popup for `op` in the focused pane's directory.
pub fn open(flag: Option<&str>) -> io::Result<()> {
    let op = flag
        .and_then(Op::from_flag)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?;
    if op == Op::History {
        // Needs a file and a line range, which only the editor knows.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history is opened by the editor with a file and line range, not as an action",
        ));
    }
    let mut env = crate::state::spawn_env();
    if let Some(cwd) = focused_cwd()
        && let Some(env) = env.as_object_mut()
    {
        env.insert(
            crate::state::SPAWN_CWD_ENV.to_string(),
            serde_json::Value::String(cwd),
        );
    }
    let (width, height) = op.popup_size();
    crate::ipc::call_text(
        "plugin.pane.open",
        serde_json::json!({
            "plugin_id": "herdr-sidebar",
            "entrypoint": op.entrypoint(),
            "placement": "popup",
            "width": width,
            "height": height,
            "env": env,
        }),
    )
    .map(drop)
}

/// Live cwd of the pane the action was invoked from (herdr passes it as
/// HERDR_PANE_ID, or inside the context JSON).
fn focused_cwd() -> Option<String> {
    let pane_id = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| {
            let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
            let context: serde_json::Value = serde_json::from_str(&context).ok()?;
            ["focused_pane_id", "pane_id"]
                .iter()
                .find_map(|key| context.get(*key)?.as_str().map(str::to_string))
        })?;
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

fn notify(title: &str, body: &str) {
    let _ = crate::ipc::call_text(
        "notification.show",
        serde_json::json!({ "title": title, "body": body }),
    );
}

// ── popup side ──────────────────────────────────────────────────────────

pub fn run(flag: Option<&str>) -> io::Result<()> {
    let op = flag
        .and_then(Op::from_flag)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?;
    if op == Op::History {
        return run_history();
    }
    if let Some(cwd) = std::env::var_os(crate::state::SPAWN_CWD_ENV).filter(|cwd| !cwd.is_empty()) {
        let _ = std::env::set_current_dir(cwd);
    }
    crate::ui::set_color_theme(crate::state::load_state().color_theme);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut app = App::new(op, Git::discover(&cwd));

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let result = (|| -> io::Result<()> {
        while !app.done {
            terminal.draw(|frame| app.draw(frame))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key),
                Event::Mouse(mouse) => app.mouse(mouse),
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some((title, body)) = app.toast {
        notify(&title, &body);
    }
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Create(String),
    Branch(Branch),
}

enum Screen {
    /// Filter + list (switch, merge) or the name field (rename).
    Edit,
    /// Merge only: the chosen branch, waiting for a second Enter.
    ConfirmMerge(Branch),
    /// A final message; any key closes the popup. `conflicts` offers `a`.
    Message {
        lines: Vec<Line<'static>>,
        conflicts: bool,
    },
}

struct App {
    op: Op,
    git: Option<Git>,
    current: Option<String>,
    branches: Vec<Branch>,
    input: String,
    selected: usize,
    scroll: usize,
    list_area: Rect,
    error: Option<String>,
    screen: Screen,
    done: bool,
    /// Shown as a herdr notification after the popup closes.
    toast: Option<(String, String)>,
}

impl App {
    fn new(op: Op, git: Result<Git, String>) -> App {
        let mut app = App {
            op,
            git: None,
            current: None,
            branches: Vec::new(),
            input: String::new(),
            selected: 0,
            scroll: 0,
            list_area: Rect::default(),
            error: None,
            screen: Screen::Edit,
            done: false,
            toast: None,
        };
        let git = match git {
            Ok(git) => git,
            Err(_) => {
                app.message(vec![Line::from("Not inside a git repository.")], false);
                return app;
            }
        };
        app.current = git.current_branch();
        match op {
            Op::Rename => match &app.current {
                Some(name) => app.input = name.clone(),
                None => app.message(
                    vec![Line::from(
                        "HEAD is detached: check out a branch to rename it.",
                    )],
                    false,
                ),
            },
            Op::Switch | Op::Merge => match git.branch_choices() {
                Ok(branches) => app.branches = branches,
                Err(error) => app.error = Some(error),
            },
            Op::History => {}
        }
        if op == Op::Merge && app.current.is_none() {
            app.message(
                vec![Line::from(
                    "HEAD is detached: check out a branch to merge into.",
                )],
                false,
            );
        }
        app.git = Some(git);
        app
    }

    fn message(&mut self, lines: Vec<Line<'static>>, conflicts: bool) {
        self.screen = Screen::Message { lines, conflicts };
    }

    fn rows(&self) -> Vec<Row> {
        let query = self.input.trim().to_lowercase();
        let mut rows = Vec::new();
        if self.op == Op::Switch
            && !query.is_empty()
            && !self
                .branches
                .iter()
                .any(|b| !b.remote && b.name == self.input.trim())
        {
            rows.push(Row::Create(self.input.trim().to_string()));
        }
        rows.extend(
            self.branches
                .iter()
                .filter(|b| !(self.op == Op::Merge && b.current))
                .filter(|b| query.is_empty() || b.name.to_lowercase().contains(&query))
                .cloned()
                .map(Row::Branch),
        );
        rows
    }

    // ── input ───────────────────────────────────────────────────────────
    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.done = true;
            return;
        }
        match &self.screen {
            Screen::Message { conflicts, .. } => {
                if *conflicts && key.code == KeyCode::Char('a') {
                    self.abort_merge();
                } else {
                    self.done = true;
                }
            }
            Screen::ConfirmMerge(branch) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    let branch = branch.clone();
                    self.merge(&branch);
                }
                _ => self.screen = Screen::Edit,
            },
            Screen::Edit => self.edit_key(key, ctrl),
        }
    }

    fn edit_key(&mut self, key: KeyEvent, ctrl: bool) {
        let rows = if self.op == Op::Rename {
            0
        } else {
            self.rows().len()
        };
        match key.code {
            KeyCode::Esc => self.done = true,
            KeyCode::Enter => self.activate(self.selected),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(rows.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(rows.saturating_sub(1)),
            KeyCode::Backspace => {
                self.input.pop();
                self.edited();
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.edited();
            }
            KeyCode::Char(c) if !ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.push(c);
                self.edited();
            }
            _ => {}
        }
    }

    fn edited(&mut self) {
        self.selected = 0;
        self.error = None;
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.screen, Screen::Edit) || self.op == Op::Rename {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && !matches!(self.screen, Screen::Edit)
                && !matches!(
                    self.screen,
                    Screen::Message {
                        conflicts: true,
                        ..
                    }
                )
            {
                self.done = true;
            }
            return;
        }
        let rows = self.rows().len();
        match mouse.kind {
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                self.selected = (self.selected + 3).min(rows.saturating_sub(1))
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.list_area;
                if mouse.row >= area.y
                    && mouse.row < area.y + area.height
                    && mouse.column >= area.x
                    && mouse.column < area.x + area.width
                {
                    let index = self.scroll + usize::from(mouse.row - area.y);
                    if index < rows {
                        self.selected = index;
                        self.activate(index);
                    }
                }
            }
            _ => {}
        }
    }

    // ── operations ──────────────────────────────────────────────────────
    fn activate(&mut self, index: usize) {
        let Some(git) = self.git.clone() else { return };
        if self.op == Op::Rename {
            let Some(old) = self.current.clone() else {
                return;
            };
            let new = self.input.trim().to_string();
            match git.rename_branch(&old, &new) {
                Ok(()) => {
                    self.toast = Some(("Branch renamed".into(), format!("{old} → {new}")));
                    self.done = true;
                }
                Err(error) => self.error = Some(error),
            }
            return;
        }
        let Some(row) = self.rows().into_iter().nth(index) else {
            return;
        };
        match (self.op, row) {
            (Op::Switch, Row::Create(name)) => match git.create_branch(&name) {
                Ok(()) => {
                    self.toast = Some(("Branch created".into(), format!("switched to {name}")));
                    self.done = true;
                }
                Err(error) => self.error = Some(error),
            },
            (Op::Switch, Row::Branch(branch)) => {
                if branch.current {
                    self.done = true;
                    return;
                }
                match git.checkout_branch(&branch) {
                    Ok(()) => {
                        let now = git.current_branch().unwrap_or_else(|| branch.name.clone());
                        self.toast = Some(("Switched branch".into(), now));
                        self.done = true;
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            (Op::Merge, Row::Branch(branch)) => self.screen = Screen::ConfirmMerge(branch),
            _ => {}
        }
    }

    fn merge(&mut self, branch: &Branch) {
        let Some(git) = self.git.clone() else { return };
        let into = self.current.clone().unwrap_or_else(|| "HEAD".into());
        match git.merge_branch(&branch.name) {
            Ok(MergeOutcome::Done(summary)) => {
                let mut lines = vec![Line::from(Span::styled(
                    format!("✓ Merged {} into {into}", branch.name),
                    Style::default()
                        .fg(palette().added)
                        .add_modifier(Modifier::BOLD),
                ))];
                lines.push(Line::default());
                lines.extend(summary.lines().map(|line| Line::from(line.to_string())));
                self.toast = Some(("Merged".into(), format!("{} → {into}", branch.name)));
                self.message(lines, false);
            }
            Ok(MergeOutcome::Conflicts(files)) => {
                let mut lines = vec![Line::from(Span::styled(
                    format!(
                        "Merging {} into {into} stopped with {} conflicted file{}:",
                        branch.name,
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ),
                    Style::default()
                        .fg(palette().conflict)
                        .add_modifier(Modifier::BOLD),
                ))];
                lines.push(Line::default());
                lines.extend(
                    files
                        .into_iter()
                        .map(|file| Line::from(format!("  {file}"))),
                );
                lines.push(Line::default());
                lines.push(Line::from(
                    "Resolve them in the editor, then stage and commit in Source Control.",
                ));
                self.message(lines, true);
            }
            Err(error) => {
                self.screen = Screen::Edit;
                self.error = Some(error);
            }
        }
    }

    fn abort_merge(&mut self) {
        let Some(git) = self.git.clone() else { return };
        let line = match git.merge_abort() {
            Ok(()) => Line::from("Merge aborted; the branch is back where it was."),
            Err(error) => Line::from(Span::styled(
                format!("merge --abort failed: {error}"),
                Style::default().fg(palette().deleted),
            )),
        };
        self.message(vec![line], false);
    }

    // ── view ────────────────────────────────────────────────────────────
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 10 || area.height < 4 {
            return;
        }
        let width = usize::from(area.width);
        let accent = Style::default().fg(palette().accent);
        let dim = Style::default().add_modifier(Modifier::DIM);

        let title = match self.op {
            Op::Switch => "Switch branch",
            Op::Rename => "Rename branch",
            Op::Merge => "Merge into current branch",
            Op::History => "Line history",
        };
        let on = self
            .current
            .as_deref()
            .map_or_else(|| "detached HEAD".to_string(), |name| format!("on {name}"));
        let repo = self.git.as_ref().map(|git| git.name()).unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {title}"), accent.add_modifier(Modifier::BOLD)),
                Span::styled(
                    truncate_to(
                        format!("  {repo} · {on}"),
                        width.saturating_sub(title.len() + 2),
                    ),
                    dim,
                ),
            ])),
            Rect::new(area.x, area.y, area.width, 1),
        );

        let body = Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(3),
        );
        // Messages and the merge question line up with the title's indent.
        let text = Rect::new(
            body.x + 1,
            body.y,
            body.width.saturating_sub(1),
            body.height,
        );
        let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1);

        match &self.screen {
            Screen::Message { lines, conflicts } => {
                frame.render_widget(Paragraph::new(lines.clone()), text);
                let hint = if *conflicts {
                    " a abort merge · any other key: close and resolve"
                } else {
                    " any key to close"
                };
                frame.render_widget(Paragraph::new(Span::styled(hint, dim)), footer);
                return;
            }
            Screen::ConfirmMerge(branch) => {
                let into = self.current.clone().unwrap_or_default();
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::raw("Merge "),
                            Span::styled(branch.name.clone(), accent.add_modifier(Modifier::BOLD)),
                            Span::raw(" into "),
                            Span::styled(into, Style::default().add_modifier(Modifier::BOLD)),
                            Span::raw("?"),
                        ]),
                        Line::default(),
                        Line::from(Span::styled(
                            "git merge --no-edit: fast-forwards when it can, else makes a merge commit.",
                            dim,
                        )),
                    ]),
                    text,
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(" ⏎ merge · esc back", dim)),
                    footer,
                );
                return;
            }
            Screen::Edit => {}
        }

        // Input line.
        let prompt = if self.op == Op::Rename {
            " new name › "
        } else {
            " › "
        };
        let room = width.saturating_sub(prompt.len() + 2);
        let mut input = vec![
            Span::styled(prompt, accent),
            Span::raw(input_tail(&self.input, room)),
            Span::styled("█", dim),
        ];
        if self.input.is_empty() && self.op != Op::Rename {
            let placeholder = if self.op == Op::Switch {
                "type to filter, or a new branch name"
            } else {
                "type to filter branches"
            };
            input.push(Span::styled(format!(" {placeholder}"), dim));
        }
        frame.render_widget(
            Paragraph::new(Line::from(input)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );

        let hint = match self.op {
            Op::Switch => " ↑↓ select · ⏎ switch · esc cancel",
            Op::Merge => " ↑↓ select · ⏎ merge… · esc cancel",
            Op::Rename => " ⏎ rename · esc cancel",
            Op::History => "",
        };
        let footer_line = match &self.error {
            Some(error) => Span::styled(
                truncate_to(format!(" {error}"), width),
                Style::default().fg(palette().deleted),
            ),
            None => Span::styled(hint, dim),
        };
        frame.render_widget(Paragraph::new(footer_line), footer);

        if self.op == Op::Rename {
            return;
        }

        let list = Rect::new(
            area.x,
            body.y + 1,
            area.width,
            body.height.saturating_sub(1),
        );
        self.list_area = list;
        frame.render_widget(
            Paragraph::new(Span::styled("─".repeat(width), dim)),
            Rect::new(area.x, body.y, area.width, 1),
        );
        let rows = self.rows();
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        let visible = usize::from(list.height);
        self.scroll = keep_visible_scroll(self.selected, visible, rows.len());
        let items: Vec<ListItem> = rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .map(|(index, row)| {
                let line = match row {
                    Row::Create(name) => Line::from(vec![
                        Span::styled(" + ", accent),
                        Span::styled(
                            truncate_to(format!("Create branch '{name}'"), width.saturating_sub(4)),
                            accent,
                        ),
                    ]),
                    Row::Branch(branch) => {
                        let mark = if branch.current { " ✓ " } else { "   " };
                        let tag = if branch.remote { "remote " } else { "" };
                        let name =
                            truncate_to(branch.name.clone(), width.saturating_sub(4 + tag.len()));
                        let pad =
                            width.saturating_sub(3 + Span::raw(name.as_str()).width() + tag.len());
                        Line::from(vec![
                            Span::raw(mark),
                            Span::raw(name),
                            Span::raw(" ".repeat(pad)),
                            Span::styled(tag, dim),
                        ])
                    }
                };
                if index == self.selected {
                    ListItem::new(line).style(selection_style(true))
                } else {
                    ListItem::new(line)
                }
            })
            .collect();
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("   no matching branches", dim)),
                list,
            );
        } else {
            frame.render_widget(List::new(items), list);
        }
    }
}

// ── line history ────────────────────────────────────────────────────────

const HISTORY_HINT: &str = " ↑↓ commit · PgUp/PgDn scroll diff · y copy hash · esc close";
/// Below this width the commit list stacks above the diff instead of beside it.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 100;
/// Columns before the subject in a list row: " hash  date       author      ".
const LIST_PREFIX_WIDTH: usize = 33;

#[derive(Debug, PartialEq, Eq)]
struct HistoryTarget {
    file: PathBuf,
    start: usize,
    end: usize,
}

/// Validate the editor's env vars: an absolute file and a 1-based inclusive
/// `START,END` range. The error text is shown to the user as is.
fn parse_history_target(file: Option<&str>, lines: Option<&str>) -> Result<HistoryTarget, String> {
    let file = file
        .map(str::trim)
        .filter(|file| !file.is_empty())
        .ok_or_else(|| format!("{HISTORY_FILE_ENV} is not set"))?;
    let lines = lines.ok_or_else(|| format!("{HISTORY_LINES_ENV} is not set"))?;
    let (start, end) = lines
        .split_once(',')
        .ok_or_else(|| format!("{HISTORY_LINES_ENV} must be START,END (got {lines:?})"))?;
    let number = |text: &str| text.trim().parse::<usize>().ok().filter(|n| *n >= 1);
    match (number(start), number(end)) {
        (Some(start), Some(end)) if start <= end => Ok(HistoryTarget {
            file: PathBuf::from(file),
            start,
            end,
        }),
        _ => Err(format!(
            "{HISTORY_LINES_ENV} must be 1-based START,END with START <= END (got {lines:?})"
        )),
    }
}

/// `file` relative to the repository `root`, `/`-separated. Canonical paths
/// are tried first (symlinked checkouts), then the paths as given.
fn repo_relative(root: &Path, file: &Path) -> Option<String> {
    let canon = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (file_canon, root_canon) = (canon(file), canon(root));
    let rel = match file_canon.strip_prefix(&root_canon) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => file.strip_prefix(root).ok()?.to_path_buf(),
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    (!rel.is_empty()).then_some(rel)
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.x + area.width && row >= area.y && row < area.y + area.height
}

/// The `--git-popup history` TUI: a commit list for one line range beside (or
/// above, when narrow) the selected commit's coloured patch.
struct HistoryApp {
    /// `<relpath>:<start>-<end>  · N commits`, or a short label for errors.
    header: String,
    /// Repo-relative path, for the diff's syntax grammar.
    rel: String,
    commits: Vec<LineCommit>,
    selected: usize,
    list_scroll: usize,
    list_area: Rect,
    diff_area: Rect,
    diff_scroll: usize,
    /// The rendered patch for the commit at the stored index.
    rendered: Option<(usize, Vec<Line<'static>>)>,
    /// A final message (bad env, no history, git errors); any key closes.
    message: Option<Vec<Line<'static>>>,
    /// Footer feedback after `y`: text, and whether it is an error.
    notice: Option<(String, bool)>,
    done: bool,
}

impl HistoryApp {
    fn load() -> HistoryApp {
        let file = std::env::var(HISTORY_FILE_ENV).ok();
        let lines = std::env::var(HISTORY_LINES_ENV).ok();
        let target = match parse_history_target(file.as_deref(), lines.as_deref()) {
            Ok(target) => target,
            Err(error) => return HistoryApp::with_message("", vec![Line::from(error)]),
        };
        let dir = match target.file.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let git = match Git::discover(dir) {
            Ok(git) => git,
            Err(_) => {
                return HistoryApp::with_message(
                    "",
                    vec![Line::from("Not inside a git repository.")],
                );
            }
        };
        let Some(rel) = repo_relative(git.root(), &target.file) else {
            return HistoryApp::with_message(
                "",
                vec![Line::from(format!(
                    "{} is not inside its repository.",
                    target.file.display()
                ))],
            );
        };
        let range = format!("{}:{}-{}", rel, target.start, target.end);
        match git.line_history(&rel, target.start, target.end) {
            Ok(commits) if !commits.is_empty() => {
                let count = commits.len();
                let header = format!(
                    "{range}  · {count} commit{}",
                    if count == 1 { "" } else { "s" }
                );
                HistoryApp::new(header, rel, commits)
            }
            Ok(_) => HistoryApp::with_message(&range, no_history_lines(None)),
            Err(error) => HistoryApp::with_message(&range, no_history_lines(Some(&error))),
        }
    }

    fn new(header: String, rel: String, commits: Vec<LineCommit>) -> HistoryApp {
        HistoryApp {
            header,
            rel,
            commits,
            selected: 0,
            list_scroll: 0,
            list_area: Rect::default(),
            diff_area: Rect::default(),
            diff_scroll: 0,
            rendered: None,
            message: None,
            notice: None,
            done: false,
        }
    }

    fn with_message(label: &str, lines: Vec<Line<'static>>) -> HistoryApp {
        let mut app = HistoryApp::new(label.to_string(), String::new(), Vec::new());
        app.message = Some(lines);
        app
    }

    fn last(&self) -> usize {
        self.commits.len().saturating_sub(1)
    }

    /// Move the selection (clamped to the list); a new commit's diff starts at the top.
    fn select(&mut self, index: usize) {
        let index = index.min(self.last());
        if index != self.selected {
            self.selected = index;
            self.diff_scroll = 0;
        }
    }

    // ── input ───────────────────────────────────────────────────────────
    fn key(&mut self, key: KeyEvent) {
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.message.is_some() || (ctrl && key.code == KeyCode::Char('c')) {
            self.done = true;
            return;
        }
        let page = usize::from(self.diff_area.height.max(1));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.done = true,
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select((self.selected + 1).min(self.last())),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(self.last()),
            KeyCode::PageUp => self.diff_scroll = self.diff_scroll.saturating_sub(page),
            KeyCode::PageDown => self.diff_scroll += page,
            KeyCode::Char('y') => self.copy_hash(),
            _ => {}
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if self.message.is_some() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.done = true;
            }
            return;
        }
        let (column, row) = (mouse.column, mouse.row);
        let in_list = contains(self.list_area, column, row);
        let in_diff = contains(self.diff_area, column, row);
        match mouse.kind {
            MouseEventKind::ScrollUp if in_list => {
                self.select(self.selected.saturating_sub(1));
            }
            MouseEventKind::ScrollDown if in_list => {
                self.select((self.selected + 1).min(self.last()));
            }
            MouseEventKind::ScrollUp if in_diff => {
                self.diff_scroll = self.diff_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if in_diff => self.diff_scroll += 3,
            MouseEventKind::Down(MouseButton::Left) if in_list => {
                let index = self.list_scroll + usize::from(row - self.list_area.y);
                if index < self.commits.len() {
                    self.select(index);
                }
            }
            _ => {}
        }
    }

    fn copy_hash(&mut self) {
        let Some(commit) = self.commits.get(self.selected) else {
            return;
        };
        self.notice = Some(match copy_to_clipboard(&commit.hash) {
            Ok(ClipboardWrite::Native) => (format!("copied {}", commit.short), false),
            Ok(ClipboardWrite::Osc52Unacknowledged) => (
                format!("sent {} to the terminal clipboard", commit.short),
                false,
            ),
            Err(err) => (format!("copy failed: {err}"), true),
        });
    }

    // ── view ────────────────────────────────────────────────────────────
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 10 || area.height < 4 {
            return;
        }
        let width = usize::from(area.width);
        let accent = Style::default().fg(palette().accent);
        let dim = Style::default().add_modifier(Modifier::DIM);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" Line history", accent.add_modifier(Modifier::BOLD)),
                Span::styled(
                    truncate_to(format!("  {}", self.header), width.saturating_sub(13)),
                    dim,
                ),
            ])),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);

        if let Some(lines) = &self.message {
            frame.render_widget(
                Paragraph::new(lines.clone()).wrap(Wrap { trim: false }),
                Rect::new(
                    body.x + 1,
                    body.y,
                    body.width.saturating_sub(1),
                    body.height,
                ),
            );
            frame.render_widget(
                Paragraph::new(Span::styled(" any key to close", dim)),
                footer,
            );
            return;
        }

        let (list, diff) = if area.width >= SIDE_BY_SIDE_MIN_WIDTH {
            // List on the left (40%), a dim rule, the patch on the right.
            let list_width = (width * 2 / 5) as u16;
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled("│", dim));
                    usize::from(body.height)
                ]),
                Rect::new(body.x + list_width, body.y, 1, body.height),
            );
            (
                Rect::new(body.x, body.y, list_width, body.height),
                Rect::new(
                    body.x + list_width + 1,
                    body.y,
                    body.width - list_width - 1,
                    body.height,
                ),
            )
        } else {
            // Narrow: list on top (about 35%), a dim rule, the patch below.
            let list_height = (usize::from(body.height) * 35 / 100)
                .max(3)
                .min(usize::from(body.height) / 2) as u16;
            frame.render_widget(
                Paragraph::new(Span::styled("─".repeat(width), dim)),
                Rect::new(body.x, body.y + list_height, body.width, 1),
            );
            let used = list_height + 1;
            (
                Rect::new(body.x, body.y, body.width, list_height),
                Rect::new(
                    body.x,
                    body.y + used,
                    body.width,
                    body.height.saturating_sub(used),
                ),
            )
        };
        self.list_area = list;
        self.diff_area = diff;
        self.draw_list(frame, list, accent, dim);
        self.draw_diff(frame, diff, dim);

        let footer_line = match &self.notice {
            Some((text, failed)) => {
                let color = if *failed {
                    palette().deleted
                } else {
                    palette().added
                };
                Span::styled(
                    truncate_to(format!(" {text}"), width),
                    Style::default().fg(color),
                )
            }
            None => Span::styled(HISTORY_HINT, dim),
        };
        frame.render_widget(Paragraph::new(footer_line), footer);
    }

    fn draw_list(&mut self, frame: &mut Frame, list: Rect, accent: Style, dim: Style) {
        let visible = usize::from(list.height);
        self.list_scroll = keep_visible_scroll(self.selected, visible, self.commits.len());
        let subject_width = usize::from(list.width).saturating_sub(LIST_PREFIX_WIDTH);
        let items: Vec<ListItem> = self
            .commits
            .iter()
            .enumerate()
            .skip(self.list_scroll)
            .take(visible)
            .map(|(index, commit)| {
                let line = Line::from(vec![
                    Span::styled(format!(" {:<7} ", commit.short), accent),
                    Span::styled(format!("{:<10} ", commit.date), dim),
                    Span::styled(format!("{:<12.12} ", commit.author), dim),
                    Span::raw(truncate_to(commit.subject.clone(), subject_width)),
                ]);
                let item = ListItem::new(line);
                if index == self.selected {
                    item.style(selection_style(true))
                } else {
                    item
                }
            })
            .collect();
        frame.render_widget(List::new(items), list);
    }

    fn draw_diff(&mut self, frame: &mut Frame, diff: Rect, dim: Style) {
        if diff.height == 0 {
            return;
        }
        if self
            .rendered
            .as_ref()
            .is_none_or(|(index, _)| *index != self.selected)
        {
            let commit = &self.commits[self.selected];
            let lines = if commit.patch.is_empty() {
                vec![Line::from(Span::styled("(no patch for these lines)", dim))]
            } else {
                diffview::render(&self.rel, &commit.patch)
            };
            self.rendered = Some((self.selected, lines));
        }
        let Some((_, lines)) = &self.rendered else {
            return;
        };
        let height = usize::from(diff.height);
        self.diff_scroll = self.diff_scroll.min(lines.len().saturating_sub(height));
        let visible: Vec<Line<'static>> = lines
            .iter()
            .skip(self.diff_scroll)
            .take(height)
            .cloned()
            .collect();
        frame.render_widget(Paragraph::new(visible), diff);
    }
}

fn no_history_lines(error: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "No committed history for these lines — is the file committed and saved?",
        Style::default()
            .fg(palette().accent)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(error) = error.map(str::trim).filter(|error| !error.is_empty()) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines
}

fn run_history() -> io::Result<()> {
    crate::ui::set_color_theme(crate::state::load_state().color_theme);
    let mut app = HistoryApp::load();

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let result = (|| -> io::Result<()> {
        while !app.done {
            terminal.draw(|frame| app.draw(frame))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key),
                Event::Mouse(mouse) => app.mouse(mouse),
                _ => {}
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

    #[test]
    fn history_env_accepts_a_range_and_rejects_garbage() {
        assert_eq!(
            parse_history_target(Some("/a/f.rs"), Some("3,9")).unwrap(),
            HistoryTarget {
                file: "/a/f.rs".into(),
                start: 3,
                end: 9,
            }
        );
        assert_eq!(
            parse_history_target(Some("/a/f.rs"), Some("4,4"))
                .unwrap()
                .start,
            4,
            "a single line is a valid range"
        );
        for lines in ["9,3", "x", "3", "0,4", "1,x", "", "-1,2"] {
            assert!(
                parse_history_target(Some("/a/f.rs"), Some(lines)).is_err(),
                "{lines:?} must be rejected"
            );
        }
        assert!(parse_history_target(None, Some("3,9")).is_err());
        assert!(parse_history_target(Some(" "), Some("3,9")).is_err());
        assert!(parse_history_target(Some("/a/f.rs"), None).is_err());
    }

    #[test]
    fn history_path_is_relative_to_the_repository() {
        let root = Path::new("/nonexistent-root/repo");
        assert_eq!(
            repo_relative(root, Path::new("/nonexistent-root/repo/src/a.rs")).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(repo_relative(root, Path::new("/elsewhere/a.rs")), None);
    }

    #[test]
    fn history_selection_resets_the_diff_scroll() {
        let mut app = HistoryApp::new(String::new(), String::new(), Vec::new());
        app.commits = vec![
            LineCommit {
                hash: "a".into(),
                short: "a".into(),
                author: String::new(),
                date: String::new(),
                subject: String::new(),
                patch: String::new(),
            },
            LineCommit {
                hash: "b".into(),
                short: "b".into(),
                author: String::new(),
                date: String::new(),
                subject: String::new(),
                patch: String::new(),
            },
        ];
        app.diff_scroll = 7;
        app.select(1);
        assert_eq!((app.selected, app.diff_scroll), (1, 0));
        app.select(9);
        assert_eq!(app.selected, 1, "selection cannot leave the list");
    }

    #[test]
    fn history_draws_side_by_side_and_stacked_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let patch =
            "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -2,2 +2,2 @@\n B\n-c\n+C\n";
        let commit = |n: usize| LineCommit {
            hash: format!("{n:040}"),
            short: format!("{n:07}"),
            author: "Ann".into(),
            date: "2026-10-16".into(),
            subject: format!("change {n}"),
            patch: patch.into(),
        };
        let mut app = HistoryApp::new(
            "f.txt:2-3  · 3 commits".into(),
            "f.txt".into(),
            (1..=3).map(commit).collect(),
        );
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        for (width, height) in [(140, 40), (80, 24), (12, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Line history"));
        assert!(text.contains("change 2"));
        assert!(text.contains("+ C"), "patch body rendered");

        let mut message = HistoryApp::with_message("f.txt:9-9", no_history_lines(Some("fatal: x")));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| message.draw(frame)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("No committed history"));
        assert!(text.contains("fatal: x"));
    }

    fn branch(name: &str, current: bool, remote: bool) -> Branch {
        Branch {
            name: name.into(),
            current,
            remote,
        }
    }

    fn app(op: Op) -> App {
        let mut app = App::new(op, Err("no repo".into()));
        app.screen = Screen::Edit;
        app.current = Some("main".into());
        app.branches = vec![
            branch("main", true, false),
            branch("feature/login", false, false),
            branch("origin/feature/login", false, true),
            branch("origin/release", false, true),
        ];
        app
    }

    #[test]
    fn switch_offers_create_only_for_a_new_local_name() {
        let mut switch = app(Op::Switch);
        assert_eq!(
            switch.rows().len(),
            4,
            "no filter: every branch, no create row"
        );
        switch.input = "login".into();
        assert_eq!(switch.rows()[0], Row::Create("login".into()));
        assert_eq!(switch.rows().len(), 3);
        switch.input = "feature/login".into();
        assert!(
            !matches!(switch.rows()[0], Row::Create(_)),
            "an existing local branch is never offered for creation"
        );
    }

    #[test]
    fn merge_lists_everything_but_the_current_branch() {
        let mut merge = app(Op::Merge);
        let names: Vec<String> = merge
            .rows()
            .into_iter()
            .map(|row| match row {
                Row::Branch(b) => b.name,
                Row::Create(name) => format!("create {name}"),
            })
            .collect();
        assert_eq!(
            names,
            ["feature/login", "origin/feature/login", "origin/release"]
        );
        merge.input = "REL".into();
        assert_eq!(
            merge.rows().len(),
            1,
            "filter is case-insensitive, never creates"
        );
    }

    #[test]
    fn op_flags_round_trip_to_entrypoints() {
        for (flag, entry) in [
            ("switch", "git-switch"),
            ("rename", "git-rename"),
            ("merge", "git-merge"),
            ("history", "git-line-history"),
        ] {
            assert_eq!(Op::from_flag(flag).unwrap().entrypoint(), entry);
        }
        assert!(Op::from_flag("push").is_none());
        assert_eq!(Op::History.popup_size().1, serde_json::json!("90%"));
    }
}
