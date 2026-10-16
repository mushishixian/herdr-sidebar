//! Branch operations in a herdr popup: `--git-popup switch|rename|merge`.
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

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use crate::git::{Branch, Git, MergeOutcome};
use crate::ui::{input_tail, keep_visible_scroll, palette, selection_style, truncate_to};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Switch,
    Rename,
    Merge,
}

impl Op {
    pub fn from_flag(flag: &str) -> Option<Op> {
        match flag {
            "switch" => Some(Op::Switch),
            "rename" => Some(Op::Rename),
            "merge" => Some(Op::Merge),
            _ => None,
        }
    }

    /// The manifest `[[panes]]` id that runs this operation.
    fn entrypoint(self) -> &'static str {
        match self {
            Op::Switch => "git-switch",
            Op::Rename => "git-rename",
            Op::Merge => "git-merge",
        }
    }

    /// Outer popup size: list popups get room, rename is one input line.
    fn popup_size(self) -> (serde_json::Value, serde_json::Value) {
        match self {
            Op::Rename => (serde_json::json!("60%"), serde_json::json!(8)),
            _ => (serde_json::json!("70%"), serde_json::json!("70%")),
        }
    }
}

const USAGE: &str = "expected switch, rename or merge";

// ── action side ─────────────────────────────────────────────────────────

/// Open the popup for `op` in the focused pane's directory.
pub fn open(flag: Option<&str>) -> io::Result<()> {
    let op = flag
        .and_then(Op::from_flag)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        ] {
            assert_eq!(Op::from_flag(flag).unwrap().entrypoint(), entry);
        }
        assert!(Op::from_flag("push").is_none());
    }
}
