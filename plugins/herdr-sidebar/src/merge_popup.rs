//! The 3-way merge editor popup: git's conflict markers shown the way VS Code
//! shows them. Incoming and Current sit side by side on top (one shared scroll),
//! the Result below. Each conflict has a header row whose checkboxes and
//! buttons are the click targets; the keys do the same things.
//!
//! The model lives in [`crate::merge_conflict`] (parse, resolve, render, save).
//! This module is the popup process around it: which files are conflicted,
//! what each one's body shows, the keys, the mouse zones and the dialogs.
//!
//! Opened from the Source Control view (`open_file`, one file) or from a key
//! (`open`, the focused pane's repository), through the same
//! `plugin.pane.open` popup path as the symbol and search popups.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::file_popup::{open_file_in_workspace, request_popup_pane_with_env};
use crate::git::{ConflictKind, Git};
use crate::merge_conflict::{self, LineKind, MergeDoc, Resolution, SaveError, Side, ViewLine};
use crate::syntax;
use crate::ui::{self, draw_confirm_dialog, hits, palette, truncate_to, wrap_hints};

/// Absolute path of the file to open first (set by the Source Control view).
pub const MERGE_FILE_ENV: &str = "HERDR_SIDEBAR_MERGE_FILE";
/// Repository root to list conflicts in.
pub const MERGE_ROOT_ENV: &str = "HERDR_SIDEBAR_MERGE_ROOT";
/// The `[[panes]]` id stage 3 adds for this popup.
pub const ENTRYPOINT: &str = "merge-editor";

const BUSY_STEP: Duration = Duration::from_millis(100);
const BUSY_LIMIT: Duration = Duration::from_secs(3);
const IDLE_POLL: Duration = Duration::from_millis(250);
const DISK_POLL: Duration = Duration::from_secs(1);
/// Below this width the Incoming/Current panes are hidden and only Result shows.
const NARROW_WIDTH: u16 = 72;
/// Line-number gutter: five digits and a separating space.
const GUTTER: usize = 6;
const HSTEP: usize = 8;
const WHEEL_STEP: isize = 3;
const UNDO_LIMIT: usize = 200;
/// The popup's size as a share of the terminal: nearly the whole screen, so
/// the Incoming / Current / Result panes have room.
const POPUP_WIDTH: &str = "98%";
const POPUP_HEIGHT: &str = "96%";

// ── Action side: request Herdr to spawn the popup pane ──────────────────

/// Open the merge editor for the focused pane's repository. Forwards
/// `MERGE_ROOT_ENV` when this process has it set.
pub fn open() -> io::Result<()> {
    let root = env_path(MERGE_ROOT_ENV).map(|root| root.to_string_lossy().into_owned());
    let extra: Vec<(&str, &str)> = root
        .as_deref()
        .map(|root| vec![(MERGE_ROOT_ENV, root)])
        .unwrap_or_default();
    retry_while_busy(
        || request_popup_pane_with_env(ENTRYPOINT, POPUP_WIDTH, POPUP_HEIGHT, &extra),
        BUSY_STEP,
        BUSY_LIMIT,
    )
}

/// Open the merge editor on one file, from the Source Control view. The
/// repository is the file's own.
pub fn open_file(file: &Path) -> io::Result<()> {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let git = Git::discover(dir).map_err(io::Error::other)?;
    let root = git.root().to_string_lossy().into_owned();
    let file = file.to_string_lossy().into_owned();
    let extra = [
        (MERGE_FILE_ENV, file.as_str()),
        (MERGE_ROOT_ENV, root.as_str()),
    ];
    retry_while_busy(
        || request_popup_pane_with_env(ENTRYPOINT, POPUP_WIDTH, POPUP_HEIGHT, &extra),
        BUSY_STEP,
        BUSY_LIMIT,
    )
}

/// Herdr allows one popup per session. A caller that just closed its own
/// popup can race the new one, so `ui_busy` is retried every `step` until
/// `limit` has been waited out; then the last error is returned.
fn retry_while_busy(
    mut attempt: impl FnMut() -> io::Result<()>,
    step: Duration,
    limit: Duration,
) -> io::Result<()> {
    let mut waited = Duration::ZERO;
    loop {
        match attempt() {
            Err(err) if err.to_string().contains("ui_busy") && waited < limit => {
                std::thread::sleep(step);
                waited += step;
            }
            other => return other,
        }
    }
}

// ── Popup TUI process side ──────────────────────────────────────────────

pub fn run() -> io::Result<()> {
    if let Some(cwd) = std::env::var_os(crate::state::SPAWN_CWD_ENV).filter(|cwd| !cwd.is_empty()) {
        let _ = std::env::set_current_dir(cwd);
    }
    ui::set_color_theme(crate::state::load_state().color_theme);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file = env_path(MERGE_FILE_ENV);
    let root_hint = env_path(MERGE_ROOT_ENV);
    let caller = std::env::var("HERDR_CALLER_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty());

    let mut app = match resolve_repo(file.as_deref(), root_hint.as_deref(), &cwd) {
        Some(git) => App::open(git, caller, file.as_deref()),
        None => {
            let name = name_of(&cwd);
            let mut app = App::blank(None, cwd.clone(), caller, name.clone());
            app.set_notice(vec![format!("No merge conflicts in {name}")]);
            app
        }
    };

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let mut needs_draw = true;
    let result = (|| -> io::Result<()> {
        while !app.done {
            if app.poll_disk() {
                needs_draw = true;
            }
            if needs_draw {
                terminal.draw(|frame| app.draw(frame))?;
                needs_draw = false;
            }
            if event::poll(IDLE_POLL)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        app.key(key);
                        needs_draw = true;
                    }
                    Event::Mouse(mouse) => {
                        app.mouse(mouse);
                        needs_draw = true;
                    }
                    Event::Resize(..) => {
                        ui::repaint_after_resize(&mut terminal)?;
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

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Repository for the popup: the file's own, then the root hint, then cwd.
fn resolve_repo(file: Option<&Path>, root: Option<&Path>, cwd: &Path) -> Option<Git> {
    file.and_then(Path::parent)
        .and_then(|dir| Git::discover(dir).ok())
        .or_else(|| root.and_then(|root| Git::discover(root).ok()))
        .or_else(|| Git::discover(cwd).ok())
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository")
        .to_string()
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// `abs` relative to `root` with `/` separators, the form git lists paths in.
fn rel_of(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs
        .strip_prefix(root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let abs = abs.canonicalize().ok()?;
            let root = root.canonicalize().ok()?;
            abs.strip_prefix(root).ok().map(Path::to_path_buf)
        })?;
    let parts: Vec<String> = rel
        .components()
        .filter_map(|part| part.as_os_str().to_str().map(str::to_string))
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn count_word(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn resolution_name(resolution: Resolution) -> &'static str {
    match resolution {
        Resolution::Unresolved => "unresolved",
        Resolution::Current => "current",
        Resolution::Incoming => "incoming",
        Resolution::CurrentThenIncoming => "current then incoming",
        Resolution::IncomingThenCurrent => "incoming then current",
        Resolution::Base => "base",
    }
}

/// The Result header's label once a conflict has a choice.
fn resolved_label(resolution: Resolution) -> &'static str {
    match resolution {
        Resolution::Unresolved => "⚠ Unresolved",
        Resolution::Current => "✓ Current",
        Resolution::Incoming => "✓ Incoming",
        Resolution::CurrentThenIncoming => "✓ Current + Incoming",
        Resolution::IncomingThenCurrent => "✓ Incoming + Current",
        Resolution::Base => "✓ Base",
    }
}

/// Toast for the finished merge. Silent under test.
fn toast(title: &str, body: &str) {
    #[cfg(not(test))]
    crate::git_action::notify(title, body);
    #[cfg(test)]
    let _ = (title, body);
}

// ── State ───────────────────────────────────────────────────────────────

/// One side's rows plus their highlighted spans, built once per change.
#[derive(Default)]
struct Rows {
    view: Vec<ViewLine>,
    styled: Vec<Line<'static>>,
}

impl Rows {
    fn new(view: Vec<ViewLine>, name: &str, first: Option<&str>) -> Self {
        let texts: Vec<String> = view.iter().map(|row| row.text.clone()).collect();
        let styled = syntax::highlight_lines(name, first, &texts);
        Self { view, styled }
    }
}

/// Which rows of the editor a scroll applies to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// Incoming and Current, which always move together.
    Top,
    Result,
    Both,
}

/// A conflicted file with conflict blocks, ready to resolve.
struct Editor {
    doc: MergeDoc,
    /// The bytes read from disk; `merge_conflict::save` writes only over these.
    baseline: Vec<u8>,
    name: String,
    /// The file's first line, for shebang-based highlighting.
    first: Option<String>,
    /// Conflict ordinal of the selected block.
    selected: usize,
    top_scroll: usize,
    result_scroll: usize,
    /// Scroll the selected conflict into view on the next draw (heights are
    /// only known once something is drawn).
    pending_reveal: bool,
    undo: Vec<Vec<Resolution>>,
    incoming: Rows,
    current: Rows,
    result: Rows,
}

impl Editor {
    fn new(doc: MergeDoc, baseline: Vec<u8>, name: String) -> Self {
        let first = String::from_utf8_lossy(&baseline)
            .lines()
            .next()
            .map(str::to_string);
        let incoming = Rows::new(doc.view(Side::Incoming), &name, first.as_deref());
        let current = Rows::new(doc.view(Side::Current), &name, first.as_deref());
        let result = Rows::new(doc.view(Side::Result), &name, first.as_deref());
        let selected = doc.next_unresolved(None).unwrap_or(0);
        Self {
            doc,
            baseline,
            name,
            first,
            selected,
            top_scroll: 0,
            result_scroll: 0,
            pending_reveal: true,
            undo: Vec::new(),
            incoming,
            current,
            result,
        }
    }

    fn rebuild_result(&mut self) {
        self.result = Rows::new(
            self.doc.view(Side::Result),
            &self.name,
            self.first.as_deref(),
        );
    }

    /// Record `before` for undo when the resolutions really changed, and
    /// refresh the Result view.
    fn commit(&mut self, before: Vec<Resolution>) -> bool {
        if self.doc.resolutions() == before {
            return false;
        }
        if self.undo.len() == UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(before);
        self.rebuild_result();
        true
    }

    fn set(&mut self, ordinal: usize, resolution: Resolution) -> bool {
        let before = self.doc.resolutions();
        self.doc.set_resolution(ordinal, resolution);
        self.commit(before)
    }

    fn toggle(&mut self, ordinal: usize, current: bool) -> bool {
        let Some(conflict) = self.doc.conflict(ordinal) else {
            return false;
        };
        let mut next = conflict.clone();
        if current {
            next.toggle_current();
        } else {
            next.toggle_incoming();
        }
        self.set(ordinal, next.resolution)
    }

    /// Resolve every unresolved conflict to `resolution`. One undo step.
    fn accept_unresolved(&mut self, resolution: Resolution) -> usize {
        let before = self.doc.resolutions();
        let mut changed = 0;
        for ordinal in 0..self.doc.conflict_count() {
            if self
                .doc
                .conflict(ordinal)
                .is_some_and(|c| c.resolution == Resolution::Unresolved)
            {
                self.doc.set_resolution(ordinal, resolution);
                changed += 1;
            }
        }
        self.commit(before);
        changed
    }

    fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.doc.restore_resolutions(&previous);
        self.rebuild_result();
        true
    }

    fn header_position(rows: &Rows, ordinal: usize) -> Option<usize> {
        rows.view
            .iter()
            .position(|row| row.kind == LineKind::Header && row.conflict == Some(ordinal))
    }

    /// Put the selected conflict's header about a third of the way down.
    fn reveal(&mut self, top_h: usize, result_h: usize) {
        if let Some(pos) = Self::header_position(&self.incoming, self.selected) {
            self.top_scroll = pos.saturating_sub(top_h / 3);
        }
        if let Some(pos) = Self::header_position(&self.result, self.selected) {
            self.result_scroll = pos.saturating_sub(result_h / 3);
        }
    }

    fn clamp(&mut self, top_h: usize, result_h: usize) {
        self.top_scroll = self
            .top_scroll
            .min(max_scroll(self.incoming.view.len(), top_h));
        self.result_scroll = self
            .result_scroll
            .min(max_scroll(self.result.view.len(), result_h));
    }

    fn scroll(&mut self, target: Target, delta: isize, top_h: usize, result_h: usize) {
        if matches!(target, Target::Top | Target::Both) {
            self.top_scroll = shift(
                self.top_scroll,
                delta,
                max_scroll(self.incoming.view.len(), top_h),
            );
        }
        if matches!(target, Target::Result | Target::Both) {
            self.result_scroll = shift(
                self.result_scroll,
                delta,
                max_scroll(self.result.view.len(), result_h),
            );
        }
    }

    fn jump(&mut self, bottom: bool, top_h: usize, result_h: usize) {
        self.top_scroll = if bottom {
            max_scroll(self.incoming.view.len(), top_h)
        } else {
            0
        };
        self.result_scroll = if bottom {
            max_scroll(self.result.view.len(), result_h)
        } else {
            0
        };
    }
}

/// The last scroll position that still fills a viewport of `height` rows.
fn max_scroll(len: usize, height: usize) -> usize {
    len.saturating_sub(height.max(1))
}

fn shift(scroll: usize, delta: isize, max: usize) -> usize {
    let next = scroll as isize + delta;
    (next.max(0) as usize).min(max)
}

/// What the file pane shows.
enum Body {
    Editor(Box<Editor>),
    /// Read failed (not UTF-8, too large, I/O). Only `e` and Tab apply.
    Unreadable(String),
    /// Deleted on one side, or missing from disk: `k` keeps, `d` deletes.
    Deleted {
        missing: bool,
        kind: ConflictKind,
    },
    /// A message with no file behind it. `close_on_key` closes on any key.
    Notice {
        lines: Vec<String>,
        close_on_key: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Then {
    Close,
    Switch(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Confirm {
    /// Unsaved choices: Yes saves then does it, No discards then does it.
    Leave(Then),
    /// `r` with unsaved choices.
    Reload,
    KeepFile,
    DeleteFile,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    Yes,
    No,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    ToggleCurrent,
    ToggleIncoming,
    Choose(Resolution),
}

/// A clickable rectangle drawn last frame and the conflict and action it maps to.
#[derive(Clone, Copy, Debug)]
struct Zone {
    rect: Rect,
    ordinal: usize,
    action: Action,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneKind {
    Incoming,
    Current,
    Result,
}

/// Where the panes were drawn last frame, for mouse hit-testing and scrolling.
#[derive(Clone, Copy, Default, Debug)]
struct Panes {
    incoming: Rect,
    current: Rect,
    result: Rect,
    top_h: usize,
    result_h: usize,
}

struct Flash {
    text: String,
    warn: bool,
}

struct App {
    git: Option<Git>,
    root: PathBuf,
    repo_name: String,
    caller: Option<String>,
    files: Vec<String>,
    current: usize,
    /// Repo-relative path of the open file, and its absolute path.
    rel: String,
    abs: PathBuf,
    body: Body,
    flash: Option<Flash>,
    confirm: Option<Confirm>,
    help: bool,
    done: bool,
    hoffset: usize,
    hover: Option<(u16, u16)>,
    zones: Vec<Zone>,
    confirm_zones: Option<(Rect, Rect)>,
    panes: Panes,
    last_disk_check: Instant,
}

impl App {
    fn blank(git: Option<Git>, root: PathBuf, caller: Option<String>, repo_name: String) -> Self {
        Self {
            git,
            root,
            repo_name,
            caller,
            files: Vec::new(),
            current: 0,
            rel: String::new(),
            abs: PathBuf::new(),
            body: Body::Notice {
                lines: Vec::new(),
                close_on_key: true,
            },
            flash: None,
            confirm: None,
            help: false,
            done: false,
            hoffset: 0,
            hover: None,
            zones: Vec::new(),
            confirm_zones: None,
            panes: Panes::default(),
            last_disk_check: Instant::now(),
        }
    }

    /// The popup for a repository: its conflicted files, starting on `prefer`
    /// when that path is one of them (or holds conflict blocks of its own).
    fn open(git: Git, caller: Option<String>, prefer: Option<&Path>) -> Self {
        let root = git.root().to_path_buf();
        let repo_name = name_of(&root);
        let mut app = Self::blank(Some(git), root, caller, repo_name);
        app.load_files(prefer);
        app
    }

    fn load_files(&mut self, prefer: Option<&Path>) {
        let Some(git) = self.git.as_ref() else {
            return;
        };
        let mut files = match git.conflicted_paths() {
            Ok(paths) => paths,
            Err(err) => {
                self.set_notice(vec![format!("Can't list the conflicts: {err}")]);
                return;
            }
        };
        let mut start = 0;
        if let Some(abs) = prefer
            && let Some(rel) = rel_of(&self.root, abs)
        {
            if let Some(pos) = files.iter().position(|listed| *listed == rel) {
                start = pos;
            } else if self.root.join(&rel).is_file()
                && merge_conflict::load(&self.root.join(&rel))
                    .is_ok_and(|(doc, _)| doc.conflict_count() > 0)
            {
                files.insert(0, rel);
            }
        }
        if files.is_empty() {
            self.set_notice(vec![format!("No merge conflicts in {}", self.repo_name)]);
            return;
        }
        self.files = files;
        self.open_index(start);
    }

    fn set_notice(&mut self, lines: Vec<String>) {
        self.body = Body::Notice {
            lines,
            close_on_key: true,
        };
    }

    fn open_index(&mut self, index: usize) {
        let Some(rel) = self.files.get(index).cloned() else {
            return;
        };
        self.current = index;
        self.abs = self.root.join(&rel);
        self.body = self.load_body(&rel);
        self.rel = rel;
        self.flash = None;
    }

    fn load_body(&self, rel: &str) -> Body {
        let abs = self.root.join(rel);
        let kind = self
            .git
            .as_ref()
            .map_or(ConflictKind::Other, |git| git.conflict_kind(rel));
        let missing = !abs.exists();
        if missing
            || matches!(
                kind,
                ConflictKind::DeletedByUs | ConflictKind::DeletedByThem
            )
        {
            return Body::Deleted { missing, kind };
        }
        match merge_conflict::load(&abs) {
            Ok((doc, baseline)) => Body::Editor(Box::new(Editor::new(
                doc,
                baseline,
                file_name(rel).to_string(),
            ))),
            Err(err) => Body::Unreadable(err.to_string()),
        }
    }

    /// Re-read the open file from disk, discarding unsaved choices.
    fn reopen(&mut self) {
        let index = self.current;
        self.open_index(index);
    }

    fn editor(&self) -> Option<&Editor> {
        match &self.body {
            Body::Editor(ed) => Some(ed.as_ref()),
            _ => None,
        }
    }

    fn editor_mut(&mut self) -> Option<&mut Editor> {
        match &mut self.body {
            Body::Editor(ed) => Some(ed.as_mut()),
            _ => None,
        }
    }

    fn dirty(&self) -> bool {
        self.editor().is_some_and(|ed| ed.doc.is_dirty())
    }

    fn flash(&mut self, text: impl Into<String>, warn: bool) {
        self.flash = Some(Flash {
            text: text.into(),
            warn,
        });
    }

    fn select(&mut self, ordinal: usize, reveal: bool) {
        let (top_h, result_h) = (self.panes.top_h, self.panes.result_h);
        let Some(ed) = self.editor_mut() else {
            return;
        };
        if ed.doc.conflict(ordinal).is_none() {
            return;
        }
        ed.selected = ordinal;
        if reveal {
            ed.reveal(top_h, result_h);
        }
    }

    fn next_conflict(&mut self, forward: bool) {
        let Some(ed) = self.editor() else {
            return;
        };
        let count = ed.doc.conflict_count();
        if count == 0 {
            return;
        }
        let next = if forward {
            (ed.selected + 1) % count
        } else {
            (ed.selected + count - 1) % count
        };
        self.select(next, true);
    }

    fn scroll(&mut self, target: Target, delta: isize) {
        let (top_h, result_h) = (self.panes.top_h, self.panes.result_h);
        if let Some(ed) = self.editor_mut() {
            ed.scroll(target, delta, top_h, result_h);
        }
    }

    fn page(&self) -> isize {
        self.panes.top_h.max(self.panes.result_h).max(1) as isize
    }

    /// Set the selected conflict's resolution. A real choice moves on to the
    /// next unresolved conflict; `Unresolved` (the reset) stays put.
    fn choose(&mut self, resolution: Resolution) {
        let Some(ed) = self.editor() else {
            return;
        };
        let ordinal = ed.selected;
        let Some(has_base) = ed.doc.conflict(ordinal).map(|c| c.base.is_some()) else {
            return;
        };
        if resolution == Resolution::Base && !has_base {
            self.flash("no base (enable merge.conflictStyle=diff3)", true);
            return;
        }
        let Some(ed) = self.editor_mut() else {
            return;
        };
        ed.set(ordinal, resolution);
        if resolution != Resolution::Unresolved
            && let Some(next) = ed.doc.next_unresolved(Some(ordinal))
        {
            self.select(next, true);
        }
    }

    fn accept_all(&mut self, resolution: Resolution) {
        let Some(ed) = self.editor_mut() else {
            return;
        };
        let count = ed.accept_unresolved(resolution);
        let message = if count == 0 {
            "no unresolved conflicts".to_string()
        } else {
            format!(
                "accepted {} for {}",
                resolution_name(resolution),
                count_word(count, "conflict")
            )
        };
        self.flash(message, false);
    }

    fn toggle_at(&mut self, ordinal: usize, current: bool) {
        self.select(ordinal, false);
        if let Some(ed) = self.editor_mut() {
            ed.toggle(ordinal, current);
        }
    }

    fn undo(&mut self) {
        let Some(ed) = self.editor_mut() else {
            return;
        };
        if !ed.undo() {
            self.flash("nothing to undo", false);
        }
    }

    // ── keys ───────────────────────────────────────────────────────────

    fn key(&mut self, key: KeyEvent) {
        if let Body::Notice {
            close_on_key: true, ..
        } = self.body
        {
            self.done = true;
            return;
        }
        if self.confirm.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.answer(Answer::Yes),
                KeyCode::Char('n' | 'N') => self.answer(Answer::No),
                KeyCode::Esc => self.answer(Answer::Cancel),
                _ => {}
            }
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.request_close(),
            KeyCode::Char('q') if !alt => self.request_close(),
            KeyCode::Char('c') if ctrl => self.request_close(),
            KeyCode::Char('s') if ctrl => self.save_key(),
            KeyCode::Char('z') if ctrl => self.undo(),
            KeyCode::Tab => self.switch(true),
            KeyCode::BackTab => self.switch(false),
            KeyCode::Down if alt => self.next_conflict(true),
            KeyCode::Up if alt => self.next_conflict(false),
            KeyCode::Char(c) if !ctrl && !alt => self.plain_key(c),
            KeyCode::Enter => self.complete_file(false),
            KeyCode::Up => self.scroll(Target::Both, -1),
            KeyCode::Down => self.scroll(Target::Both, 1),
            KeyCode::PageUp => self.scroll(Target::Both, -self.page()),
            KeyCode::PageDown => self.scroll(Target::Both, self.page()),
            KeyCode::Left => self.hoffset = self.hoffset.saturating_sub(HSTEP),
            KeyCode::Right => self.hoffset += HSTEP,
            _ => {}
        }
    }

    fn plain_key(&mut self, c: char) {
        match c {
            'n' | ']' => self.next_conflict(true),
            'p' | '[' => self.next_conflict(false),
            'c' => self.choose(Resolution::Current),
            'i' => self.choose(Resolution::Incoming),
            'b' => self.choose(Resolution::CurrentThenIncoming),
            'B' => self.choose(Resolution::IncomingThenCurrent),
            'a' => self.choose(Resolution::Base),
            'x' => self.choose(Resolution::Unresolved),
            'C' => self.accept_all(Resolution::Current),
            'I' => self.accept_all(Resolution::Incoming),
            'u' => self.undo(),
            'g' | 'G' => {
                let (top_h, result_h) = (self.panes.top_h, self.panes.result_h);
                if let Some(ed) = self.editor_mut() {
                    ed.jump(c == 'G', top_h, result_h);
                }
            }
            '0' => self.hoffset = 0,
            's' => self.save_key(),
            'r' => self.reload_key(),
            'e' => self.edit(),
            'k' if matches!(self.body, Body::Deleted { .. }) => {
                self.confirm = Some(Confirm::KeepFile);
            }
            'd' if matches!(self.body, Body::Deleted { .. }) => {
                self.confirm = Some(Confirm::DeleteFile);
            }
            '?' => self.help = !self.help,
            _ => {}
        }
    }

    /// Ctrl+S and `s`. With every conflict in the file resolved this finishes
    /// the file (save, `git add`, close when it was the last one); otherwise it
    /// saves the partial choices.
    fn save_key(&mut self) {
        if self.ready_to_finish() {
            self.complete_file(true);
        } else if self.dirty() {
            self.save();
        } else {
            self.flash("no changes to save", false);
        }
    }

    /// An open editor with no conflicts left, and either conflict blocks to
    /// finish or unsaved choices to write.
    fn ready_to_finish(&self) -> bool {
        self.editor().is_some_and(|ed| {
            ed.doc.unresolved_count() == 0 && (ed.doc.conflict_count() > 0 || ed.doc.is_dirty())
        })
    }

    fn reload_key(&mut self) {
        if self.dirty() {
            self.confirm = Some(Confirm::Reload);
        } else {
            self.reopen();
        }
    }

    fn request_close(&mut self) {
        if self.dirty() {
            self.confirm = Some(Confirm::Leave(Then::Close));
        } else {
            self.done = true;
        }
    }

    fn switch(&mut self, forward: bool) {
        let count = self.files.len();
        if count < 2 {
            return;
        }
        let next = if forward {
            (self.current + 1) % count
        } else {
            (self.current + count - 1) % count
        };
        if self.dirty() {
            self.confirm = Some(Confirm::Leave(Then::Switch(next)));
        } else {
            self.open_index(next);
        }
    }

    fn perform(&mut self, then: Then) {
        match then {
            Then::Close => self.done = true,
            Then::Switch(index) => self.open_index(index),
        }
    }

    fn answer(&mut self, answer: Answer) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        match (confirm, answer) {
            (Confirm::Leave(Then::Close), Answer::Yes) if self.ready_to_finish() => {
                self.complete_file(true);
            }
            (Confirm::Leave(then), Answer::Yes) => {
                if self.save() {
                    self.perform(then);
                }
            }
            (Confirm::Leave(then), Answer::No) => self.perform(then),
            (Confirm::Reload, Answer::Yes) => self.reopen(),
            (Confirm::KeepFile, Answer::Yes) => self.keep(),
            (Confirm::DeleteFile, Answer::Yes) => self.delete(),
            _ => {}
        }
    }

    /// Write the resolved file. On success the file is re-read, so resolved
    /// blocks become ordinary text and the editor is clean again.
    fn save(&mut self) -> bool {
        let Some(ed) = self.editor() else {
            return true;
        };
        if !ed.doc.is_dirty() {
            return true;
        }
        let result = merge_conflict::save(&self.abs, &ed.doc, &ed.baseline);
        let name = file_name(&self.rel).to_string();
        match result {
            Ok(_) => {
                self.reopen();
                let left = self.editor().map_or(0, |ed| ed.doc.conflict_count());
                self.flash(
                    format!("Saved {name} · {} left", count_word(left, "conflict")),
                    false,
                );
                true
            }
            Err(SaveError::ChangedOnDisk) => {
                self.flash(
                    format!("{name} changed on disk — r reloads (your choices are lost)"),
                    true,
                );
                false
            }
            Err(err) => {
                self.flash(format!("Save failed: {err}"), true);
                false
            }
        }
    }

    /// Enter, and Ctrl+S on a fully resolved file: mark the file resolved, after
    /// saving any choices. `close_when_done` closes the popup when this was the
    /// last conflicted file; Enter shows the Done notice instead.
    fn complete_file(&mut self, close_when_done: bool) {
        let unresolved = self.editor().map(|ed| ed.doc.unresolved_count());
        match (&self.body, unresolved) {
            (_, Some(left)) if left > 0 => {
                self.flash(
                    format!(
                        "{} left — resolve them first (or e to edit by hand)",
                        count_word(left, "conflict")
                    ),
                    true,
                );
                return;
            }
            (Body::Deleted { .. }, _) => {
                self.flash("k keeps the file, d deletes it", true);
                return;
            }
            (Body::Unreadable(_), _) => {
                self.flash(
                    "can't resolve this file here — e opens it in the editor",
                    true,
                );
                return;
            }
            (Body::Notice { .. }, _) => return,
            _ => {}
        }
        if self.dirty() && !self.save() {
            return;
        }
        let rel = self.rel.clone();
        let Some(git) = self.git.as_ref() else {
            return;
        };
        match git.mark_resolved(&rel) {
            Ok(()) => {
                let message = format!("✓ {} marked resolved", file_name(&rel));
                self.advance(&rel, message, close_when_done);
            }
            Err(err) => self.flash(format!("Couldn't mark {rel} resolved: {err}"), true),
        }
    }

    /// After a file is resolved or removed: move to the next conflicted file,
    /// or to the done screen when none is left (closing the popup instead when
    /// `close_when_done`).
    fn advance(&mut self, resolved: &str, message: String, close_when_done: bool) {
        let Some(git) = self.git.as_ref() else {
            return;
        };
        let paths = match git.conflicted_paths() {
            Ok(paths) => paths,
            Err(err) => {
                self.flash(format!("Couldn't list the conflicts: {err}"), true);
                return;
            }
        };
        if paths.is_empty() && close_when_done {
            toast(
                "Merge conflicts resolved",
                "Commit the merge in Source Control.",
            );
            self.done = true;
            return;
        }
        if paths.is_empty() {
            self.files.clear();
            self.body = Body::Notice {
                lines: vec![
                    "All conflicts resolved.".to_string(),
                    "Commit the merge in Source Control.".to_string(),
                ],
                close_on_key: true,
            };
            toast(
                "Merge conflicts resolved",
                "Commit the merge in Source Control.",
            );
            return;
        }
        let next = paths
            .iter()
            .position(|path| path.as_str() > resolved)
            .unwrap_or(0);
        self.files = paths;
        self.open_index(next);
        self.flash(message, false);
    }

    fn keep(&mut self) {
        if !self.abs.exists() {
            self.flash(
                "nothing on disk to keep — d deletes the conflict instead",
                true,
            );
            return;
        }
        let rel = self.rel.clone();
        let Some(git) = self.git.as_ref() else {
            return;
        };
        match git.mark_resolved(&rel) {
            Ok(()) => {
                let message = format!("✓ kept {}", file_name(&rel));
                self.advance(&rel, message, false);
            }
            Err(err) => self.flash(format!("Couldn't keep {rel}: {err}"), true),
        }
    }

    fn delete(&mut self) {
        let rel = self.rel.clone();
        let Some(git) = self.git.as_ref() else {
            return;
        };
        match git.remove_conflicted(&rel) {
            Ok(()) => {
                let message = format!("✓ deleted {}", file_name(&rel));
                self.advance(&rel, message, false);
            }
            Err(err) => self.flash(format!("Couldn't delete {rel}: {err}"), true),
        }
    }

    /// `e`: open the file in the user's editor at the selected conflict.
    fn edit(&mut self) {
        if matches!(self.body, Body::Notice { .. }) {
            return;
        }
        if self.dirty() && !self.save() {
            return;
        }
        let line = self.editor().map(|ed| ed.doc.first_line_of(ed.selected));
        match open_file_in_workspace(self.caller.as_deref(), &self.root, &self.abs, line) {
            Ok(()) => self.done = true,
            Err(err) => self.flash(err, true),
        }
    }

    // ── mouse ──────────────────────────────────────────────────────────

    fn mouse(&mut self, mouse: MouseEvent) {
        self.hover = Some((mouse.column, mouse.row));
        if self.confirm.is_some() {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
                && let Some((yes, no)) = self.confirm_zones
            {
                if hits(yes, mouse.column, mouse.row) {
                    self.answer(Answer::Yes);
                } else if hits(no, mouse.column, mouse.row) {
                    self.answer(Answer::No);
                }
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.wheel(mouse.column, mouse.row, -WHEEL_STEP),
            MouseEventKind::ScrollDown => self.wheel(mouse.column, mouse.row, WHEEL_STEP),
            MouseEventKind::Down(MouseButton::Left) => self.click(mouse.column, mouse.row),
            _ => {}
        }
    }

    fn wheel(&mut self, x: u16, y: u16, delta: isize) {
        let panes = self.panes;
        if hits(panes.incoming, x, y) || hits(panes.current, x, y) {
            self.scroll(Target::Top, delta);
        } else if hits(panes.result, x, y) {
            self.scroll(Target::Result, delta);
        }
    }

    fn click(&mut self, x: u16, y: u16) {
        if let Some(zone) = self
            .zones
            .iter()
            .find(|zone| hits(zone.rect, x, y))
            .copied()
        {
            self.select(zone.ordinal, false);
            match zone.action {
                Action::ToggleIncoming => self.toggle_at(zone.ordinal, false),
                Action::ToggleCurrent => self.toggle_at(zone.ordinal, true),
                Action::Choose(resolution) => self.choose(resolution),
            }
            return;
        }
        if let Some(ordinal) = self.row_conflict_at(x, y) {
            self.select(ordinal, false);
        }
    }

    /// The conflict a drawn row belongs to, if the cell is on a row of a pane.
    fn row_conflict_at(&self, x: u16, y: u16) -> Option<usize> {
        let ed = self.editor()?;
        let panes = self.panes;
        let candidates = [
            (panes.incoming, &ed.incoming.view, ed.top_scroll),
            (panes.current, &ed.current.view, ed.top_scroll),
            (panes.result, &ed.result.view, ed.result_scroll),
        ];
        candidates
            .into_iter()
            .find(|(rect, _, _)| hits(*rect, x, y))
            .and_then(|(rect, rows, scroll)| {
                rows.get(scroll + usize::from(y - rect.y))
                    .and_then(|row| row.conflict)
            })
    }

    // ── disk ───────────────────────────────────────────────────────────

    /// Every second, while nothing is unsaved: reload the file if it changed on
    /// disk. Returns whether the view changed.
    fn poll_disk(&mut self) -> bool {
        if self.last_disk_check.elapsed() < DISK_POLL {
            return false;
        }
        self.last_disk_check = Instant::now();
        if self.confirm.is_some() || self.dirty() {
            return false;
        }
        let Some(ed) = self.editor() else {
            return false;
        };
        let Ok(bytes) = std::fs::read(&self.abs) else {
            return false;
        };
        if bytes == ed.baseline {
            return false;
        }
        let (selected, top, result) = (ed.selected, ed.top_scroll, ed.result_scroll);
        self.reopen();
        let name = file_name(&self.rel).to_string();
        if let Some(ed) = self.editor_mut() {
            ed.selected = selected.min(ed.doc.conflict_count().saturating_sub(1));
            ed.top_scroll = top;
            ed.result_scroll = result;
            ed.pending_reveal = false;
        }
        self.flash(format!("{name} changed on disk — reloaded"), false);
        true
    }

    // ── drawing ────────────────────────────────────────────────────────

    /// Text for the bodies that have no panes.
    fn message_lines(&self) -> Option<Vec<String>> {
        match &self.body {
            Body::Editor(ed) if ed.doc.conflict_count() == 0 => Some(vec![
                format!("No conflict markers left in {}.", self.rel),
                String::new(),
                "⏎ marks it resolved · Tab moves on".to_string(),
            ]),
            Body::Editor(_) => None,
            Body::Unreadable(err) => Some(vec![
                format!("Can't open {} here: {err}", self.rel),
                String::new(),
                "e opens it in the editor · Tab moves on".to_string(),
            ]),
            Body::Deleted { missing, kind } => {
                let why = if *missing {
                    format!("{} is not on disk.", self.rel)
                } else {
                    match kind {
                        ConflictKind::DeletedByUs => {
                            "Deleted here, changed on the other side.".to_string()
                        }
                        ConflictKind::DeletedByThem => {
                            "Deleted on the other side, changed here.".to_string()
                        }
                        _ => format!("{} has no text conflict to merge.", self.rel),
                    }
                };
                Some(vec![
                    why,
                    String::new(),
                    "k keeps the file as it is on disk · d deletes it · Tab moves on".to_string(),
                ])
            }
            Body::Notice { lines, .. } => Some(lines.clone()),
        }
    }

    fn confirm_text(&self, confirm: Confirm) -> (String, String) {
        let name = file_name(&self.rel);
        match confirm {
            Confirm::Leave(Then::Close) => (
                "Unsaved choices".to_string(),
                format!("Save your choices in {name}?"),
            ),
            Confirm::Leave(Then::Switch(_)) => (
                "Unsaved choices".to_string(),
                format!("Save your choices in {name} first?"),
            ),
            Confirm::Reload => (
                "Reload".to_string(),
                format!("Discard your choices and reload {name}?"),
            ),
            Confirm::KeepFile => (
                "Keep file".to_string(),
                format!("Keep {name} as it is on disk?"),
            ),
            Confirm::DeleteFile => (
                "Delete file".to_string(),
                format!("Delete {name} and resolve the conflict?"),
            ),
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.zones.clear();
        self.confirm_zones = None;
        self.panes = Panes::default();
        let area = frame.area();
        if area.width < 10 || area.height < 4 {
            return;
        }

        let hints: &[(&'static str, &'static str)] = if self.confirm.is_some() {
            &[("y", "yes"), ("n", "no"), ("esc", "cancel")]
        } else if self.help {
            HELP_HINTS
        } else {
            NORMAL_HINTS
        };
        let footer_lines = wrap_hints(hints, area.width, 0);
        let footer_h = (footer_lines.len() as u16).clamp(1, (area.height / 3).max(1));
        let footer = Rect::new(
            area.x,
            area.y + area.height - footer_h,
            area.width,
            footer_h,
        );
        let flash_area = Rect::new(area.x, footer.y - 1, area.width, 1);
        let title_area = Rect::new(area.x, area.y, area.width, 1);
        let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2 - footer_h);

        frame.render_widget(Paragraph::new(self.title_line(area.width)), title_area);
        if let Some(flash) = &self.flash {
            let color = if flash.warn {
                palette().warning
            } else {
                palette().header_accent
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    truncate_to(format!(" {}", flash.text), usize::from(area.width)),
                    Style::default().fg(color),
                )),
                flash_area,
            );
        }
        frame.render_widget(Paragraph::new(footer_lines), footer);

        if let Some(lines) = self.message_lines() {
            let lines: Vec<Line<'static>> = lines
                .into_iter()
                .map(|text| Line::from(format!(" {text}")))
                .collect();
            frame.render_widget(Paragraph::new(lines), body);
        } else if let Body::Editor(ed) = &mut self.body {
            self.panes = draw_editor(frame, body, ed, self.hoffset, &mut self.zones);
        }

        if let Some(confirm) = self.confirm {
            let (title, message) = self.confirm_text(confirm);
            self.confirm_zones = Some(draw_confirm_dialog(frame, &title, &message, self.hover));
        }
    }

    fn title_line(&self, width: u16) -> Line<'static> {
        let accent = palette();
        // Resolved files leave the list, so the count is what is still open.
        let files_left = format!("{} left", count_word(self.files.len(), "file"));
        let text = match &self.body {
            Body::Editor(ed) => {
                let total = ed.doc.conflict_count();
                let noun = if total == 1 { "conflict" } else { "conflicts" };
                format!(
                    " Merge · {}   {files_left} · {} of {total} {noun} left",
                    self.rel,
                    ed.doc.unresolved_count(),
                )
            }
            Body::Notice { .. } => " Merge".to_string(),
            _ => format!(" Merge · {}   {files_left}", self.rel),
        };
        let width = usize::from(width);
        let dirty = self.dirty();
        let room = if dirty {
            width.saturating_sub(3)
        } else {
            width
        };
        let mut spans = vec![Span::styled(
            truncate_to(text, room),
            Style::default()
                .fg(accent.header_accent)
                .add_modifier(Modifier::BOLD),
        )];
        if dirty {
            let used: usize = spans.iter().map(Span::width).sum();
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used + 2))));
            spans.push(Span::styled("● ", Style::default().fg(accent.modified)));
        }
        Line::from(spans)
    }
}

const NORMAL_HINTS: &[(&str, &str)] = &[
    ("c", "current"),
    ("i", "incoming"),
    ("b", "both"),
    ("n/p", "next/prev"),
    ("⏎", "complete"),
    ("^S", "save · finish"),
    ("e", "editor"),
    ("?", "more"),
    ("esc", "close"),
];

const HELP_HINTS: &[(&str, &str)] = &[
    ("c", "accept current"),
    ("i", "accept incoming"),
    ("b", "both: current then incoming"),
    ("B", "both: incoming then current"),
    ("a", "accept base"),
    ("x", "reset"),
    ("C / I", "all current / incoming"),
    ("n / p", "next / prev conflict"),
    ("alt+↑↓", "prev / next conflict"),
    ("↑↓ pgup pgdn g G", "scroll"),
    ("← → 0", "sideways"),
    ("u", "undo"),
    ("s / ^S", "save (finishes a resolved file)"),
    ("r", "reload"),
    ("⏎", "complete merge"),
    ("tab", "next file"),
    ("k / d", "keep / delete"),
    ("e", "editor"),
    ("?", "fewer keys"),
    ("esc", "close"),
];

/// Draw the two-pane top area (when wide enough) and the Result below it.
fn draw_editor(
    frame: &mut Frame,
    area: Rect,
    ed: &mut Editor,
    hoffset: usize,
    zones: &mut Vec<Zone>,
) -> Panes {
    let wide = area.width >= NARROW_WIDTH && area.height >= 7;
    let top_h = if wide {
        (area.height * 45 / 100).max(3)
    } else {
        0
    };
    let top = Rect::new(area.x, area.y, area.width, top_h);
    let result = Rect::new(area.x, area.y + top_h, area.width, area.height - top_h);
    let result_rows = Rect::new(
        result.x,
        result.y + 1,
        result.width,
        result.height.saturating_sub(1),
    );
    let mut panes = Panes {
        result: result_rows,
        result_h: usize::from(result_rows.height),
        ..Panes::default()
    };
    let left_w = area.width.saturating_sub(1) / 2;
    if top_h > 0 {
        let rows_y = top.y + 1;
        let rows_h = top_h - 1;
        panes.incoming = Rect::new(area.x, rows_y, left_w, rows_h);
        panes.current = Rect::new(area.x + left_w + 1, rows_y, area.width - left_w - 1, rows_h);
        panes.top_h = usize::from(rows_h);
    }

    if ed.pending_reveal {
        ed.reveal(panes.top_h, panes.result_h);
        ed.pending_reveal = false;
    }
    ed.clamp(panes.top_h, panes.result_h);

    if top_h > 0 {
        let first = ed
            .doc
            .conflict(0)
            .map(|c| (c.incoming_label.clone(), c.current_label.clone()))
            .unwrap_or_default();
        let accent = palette();
        let heading = Style::default()
            .fg(accent.header_accent)
            .add_modifier(Modifier::BOLD);
        let left = format!(" Incoming · {}", first.0);
        let right = format!(" Current · {}", first.1);
        let title = Line::from(vec![
            Span::styled(
                format!(
                    "{:<width$}",
                    truncate_to(left, usize::from(left_w)),
                    width = usize::from(left_w)
                ),
                heading,
            ),
            Span::styled("│", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                truncate_to(right, usize::from(panes.current.width)),
                heading,
            ),
        ]);
        frame.render_widget(
            Paragraph::new(title),
            Rect::new(area.x, top.y, area.width, 1),
        );

        let lines_in = pane_lines(ed, PaneKind::Incoming, panes.incoming, hoffset, zones);
        frame.render_widget(Paragraph::new(lines_in), panes.incoming);
        let lines_cur = pane_lines(ed, PaneKind::Current, panes.current, hoffset, zones);
        frame.render_widget(Paragraph::new(lines_cur), panes.current);
        let divider: Vec<Line<'static>> = (0..panes.incoming.height)
            .map(|_| Line::styled("│", Style::default().add_modifier(Modifier::DIM)))
            .collect();
        frame.render_widget(
            Paragraph::new(divider),
            Rect::new(area.x + left_w, panes.incoming.y, 1, panes.incoming.height),
        );
    }

    let heading = Line::styled(
        truncate_to(format!(" Result · {}", ed.name), usize::from(result.width)),
        Style::default()
            .fg(palette().header_accent)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(
        Paragraph::new(heading),
        Rect::new(result.x, result.y, result.width, 1),
    );
    let result_lines = pane_lines(ed, PaneKind::Result, result_rows, hoffset, zones);
    frame.render_widget(Paragraph::new(result_lines), result_rows);
    panes
}

/// The visible rows of one pane, from its scroll offset, as styled lines.
/// Header rows become bands (with clickable zones) and every other row is a
/// gutter plus its text.
fn pane_lines(
    ed: &Editor,
    pane: PaneKind,
    area: Rect,
    hoffset: usize,
    zones: &mut Vec<Zone>,
) -> Vec<Line<'static>> {
    let (rows, scroll) = match pane {
        PaneKind::Incoming => (&ed.incoming, ed.top_scroll),
        PaneKind::Current => (&ed.current, ed.top_scroll),
        PaneKind::Result => (&ed.result, ed.result_scroll),
    };
    let height = usize::from(area.height);
    let mut out = Vec::with_capacity(height);
    // Every row but a header band or filler takes the next number, counted
    // from the top of THIS pane's rows. The numbers are the lines this pane's
    // text occupies in its own file (Result counts its raw markers).
    let mut number = 0;
    for (offset, row) in rows.view.iter().enumerate() {
        let counted = !matches!(row.kind, LineKind::Header | LineKind::Filler);
        let this_number = counted.then(|| {
            number += 1;
            number
        });
        if offset < scroll {
            continue;
        }
        if offset >= scroll + height {
            break;
        }
        let y = area.y + (offset - scroll) as u16;
        let line = if row.kind == LineKind::Header {
            header_line(ed, pane, row, Rect::new(area.x, y, area.width, 1), zones)
        } else {
            content_line(
                row,
                this_number,
                rows.styled.get(offset),
                area.width,
                hoffset,
            )
        };
        out.push(line);
    }
    out
}

/// A header band: the checkbox row of a top pane, or the action row of Result.
fn header_line(
    ed: &Editor,
    pane: PaneKind,
    row: &ViewLine,
    rect: Rect,
    zones: &mut Vec<Zone>,
) -> Line<'static> {
    let colors = palette();
    let Some(ordinal) = row.conflict else {
        return Line::default();
    };
    let selected = ordinal == ed.selected;
    let prefix = if selected { "▶ " } else { "  " };
    let emphasis = if selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let conflict = ed.doc.conflict(ordinal);
    let (band, fg) = match pane {
        PaneKind::Incoming => (colors.merge_incoming_header_bg, colors.merge_header_fg),
        PaneKind::Current => (colors.merge_current_header_bg, colors.merge_header_fg),
        PaneKind::Result => (colors.keycap_bg, colors.keycap_fg),
    };
    let base = Style::default().bg(band).fg(fg).add_modifier(emphasis);
    let mut segments: Vec<(String, Style, Option<Action>)> = Vec::new();
    match pane {
        PaneKind::Incoming | PaneKind::Current => {
            let (on, label, action) = if pane == PaneKind::Incoming {
                (
                    conflict.is_some_and(|c| c.has_incoming()),
                    "Accept Incoming",
                    Action::ToggleIncoming,
                )
            } else {
                (
                    conflict.is_some_and(|c| c.has_current()),
                    "Accept Current",
                    Action::ToggleCurrent,
                )
            };
            let checkbox = if on { "[x]" } else { "[ ]" };
            segments.push((format!("{prefix}{checkbox} {label}"), base, None));
            if !row.text.is_empty() {
                segments.push((
                    format!("  · {}", row.text),
                    base.add_modifier(Modifier::DIM),
                    None,
                ));
            }
            // The whole band toggles, like VS Code's checkbox row.
            zones.push(Zone {
                rect,
                ordinal,
                action,
            });
        }
        PaneKind::Result => {
            let resolution = conflict.map_or(Resolution::Unresolved, |c| c.resolution);
            segments.push((prefix.to_string(), base, None));
            if resolution == Resolution::Unresolved {
                segments.push((format!("⚠ Conflict {}  ", ordinal + 1), base, None));
                let buttons = [
                    ("Accept Current", Resolution::Current),
                    ("Accept Incoming", Resolution::Incoming),
                    ("Accept Both", Resolution::CurrentThenIncoming),
                ];
                for (index, (label, choice)) in buttons.into_iter().enumerate() {
                    if index > 0 {
                        segments.push((" │ ".to_string(), base, None));
                    }
                    segments.push((
                        label.to_string(),
                        base.add_modifier(Modifier::UNDERLINED),
                        Some(Action::Choose(choice)),
                    ));
                }
            } else {
                segments.push((resolved_label(resolution).to_string(), base, None));
                segments.push((" │ ".to_string(), base, None));
                segments.push((
                    "Reset".to_string(),
                    base.add_modifier(Modifier::UNDERLINED),
                    Some(Action::Choose(Resolution::Unresolved)),
                ));
            }
        }
    }

    let mut spans = Vec::with_capacity(segments.len() + 1);
    let mut col: u16 = 0;
    for (text, style, action) in segments {
        let width = Span::raw(text.as_str()).width() as u16;
        if let Some(action) = action
            && col < rect.width
        {
            zones.push(Zone {
                rect: Rect::new(rect.x + col, rect.y, width.min(rect.width - col), 1),
                ordinal,
                action,
            });
        }
        col = col.saturating_add(width);
        spans.push(Span::styled(text, style));
    }
    if col < rect.width {
        spans.push(Span::styled(
            " ".repeat(usize::from(rect.width - col)),
            base,
        ));
    }
    Line::from(spans)
}

/// One content row: the gutter, the text with tabs expanded and scrolled by
/// `hoffset`, on the row's tint. `number` is the row's line number in its
/// pane (`None` for header and filler rows).
fn content_line(
    row: &ViewLine,
    number: Option<usize>,
    styled: Option<&Line<'static>>,
    width: u16,
    hoffset: usize,
) -> Line<'static> {
    let colors = palette();
    let width = usize::from(width);
    let gutter = if width > GUTTER { GUTTER } else { 0 };
    if row.kind == LineKind::Filler {
        // A faint hatch: every other cell, after the gutter, no background.
        let hatch: String = (0..width - gutter)
            .map(|cell| if cell % 2 == 0 { '╱' } else { ' ' })
            .collect();
        return Line::from(vec![
            Span::raw(" ".repeat(gutter)),
            Span::styled(
                hatch,
                Style::default()
                    .fg(colors.ignored)
                    .add_modifier(Modifier::DIM),
            ),
        ]);
    }
    let bg = match row.kind {
        LineKind::Current => Some(colors.merge_current_bg),
        LineKind::Incoming => Some(colors.merge_incoming_bg),
        LineKind::Base => Some(colors.merge_base_bg),
        _ => None,
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    if gutter > 0 {
        let text = match number {
            Some(line) => format!("{line:>5} "),
            None => " ".repeat(gutter),
        };
        spans.push(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let content_width = width - gutter;
    let marker = (row.kind == LineKind::Marker).then(|| {
        Style::default()
            .fg(colors.conflict)
            .add_modifier(Modifier::BOLD)
    });
    let cells = cell_spans(row, styled, hoffset, content_width, marker);
    let used: usize = cells.iter().map(Span::width).sum();
    spans.extend(cells);
    if used < content_width {
        spans.push(Span::raw(" ".repeat(content_width - used)));
    }
    if let Some(bg) = bg {
        for span in &mut spans {
            span.style = span.style.bg(bg);
        }
    }
    Line::from(spans)
}

/// The visible cells of a row's text. Tabs expand to four-column stops; a
/// wide character that would straddle either edge is left out.
fn cell_spans(
    row: &ViewLine,
    styled: Option<&Line<'static>>,
    hoffset: usize,
    width: usize,
    base: Option<Style>,
) -> Vec<Span<'static>> {
    let flat: Vec<(char, Style)> = match styled {
        Some(line)
            if line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                == row.text =>
        {
            line.spans
                .iter()
                .flat_map(|span| {
                    let style = span.style;
                    span.content.chars().map(move |c| (c, style))
                })
                .collect()
        }
        _ => row.text.chars().map(|c| (c, Style::default())).collect(),
    };

    // (column, width, char, style) for every cell after tab expansion.
    let mut cells: Vec<(usize, usize, char, Style)> = Vec::new();
    let mut column = 0;
    for (c, style) in flat {
        if c == '\t' {
            let stop = 4 - column % 4;
            for offset in 0..stop {
                cells.push((column + offset, 1, ' ', style));
            }
            column += stop;
        } else {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            cells.push((column, w, c, style));
            column += w;
        }
    }

    let end = hoffset + width;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Style> = None;
    for (column, w, c, style) in cells {
        if column < hoffset || column >= end || column + w > end {
            continue;
        }
        let style = match base {
            Some(base) => style.patch(base),
            None => style,
        };
        if current != Some(style) {
            if let Some(prev) = current {
                spans.push(Span::styled(std::mem::take(&mut text), prev));
            }
            current = Some(style);
        }
        text.push(c);
    }
    if let Some(style) = current {
        spans.push(Span::styled(text, style));
    }
    spans
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::cell::Cell;
    use std::process::Command;

    // Two changes far enough apart that git keeps them as two blocks.
    const BASE: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    const OURS: &str = "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nNINE\n";
    const THEIRS: &str = "uno\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnueve\n";

    /// A throwaway repository with a merge that conflicts in `a.txt` and
    /// `b.txt`, each with two conflict blocks. Removed on drop.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git_run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn conflict_repo(name: &str) -> (Git, Scratch) {
        let dir =
            std::env::temp_dir().join(format!("hs-merge-popup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = Scratch(dir.clone());
        git_run(&dir, &["init", "-q"]);
        let write_all = |text: &str| {
            for file in ["a.txt", "b.txt"] {
                std::fs::write(dir.join(file), text).unwrap();
            }
        };
        write_all(BASE);
        git_run(&dir, &["add", "."]);
        git_run(&dir, &["commit", "-q", "-m", "base"]);
        git_run(&dir, &["checkout", "-q", "-b", "side"]);
        write_all(THEIRS);
        git_run(&dir, &["commit", "-q", "-am", "side"]);
        git_run(&dir, &["checkout", "-q", "-"]);
        write_all(OURS);
        git_run(&dir, &["commit", "-q", "-am", "ours"]);
        // The merge stops with conflicts, so its non-zero exit is expected.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args([
                "-c",
                "merge.conflictStyle=merge",
                "merge",
                "-q",
                "--no-edit",
                "side",
            ])
            .output();
        let git = Git::discover(&dir).expect("repo");
        (git, scratch)
    }

    fn app_for(name: &str) -> (App, Scratch) {
        let (git, scratch) = conflict_repo(name);
        (App::open(git, None, None), scratch)
    }

    fn key(app: &mut App, c: char) {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    fn editor(app: &App) -> &Editor {
        app.editor().expect("an editor body")
    }

    fn resolutions(app: &App) -> Vec<Resolution> {
        editor(app).doc.resolutions()
    }

    fn draw(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    fn click(app: &mut App, column: u16, row: u16) {
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    fn zone_rect(app: &App, action: Action) -> Rect {
        app.zones
            .iter()
            .find(|zone| zone.action == action)
            .unwrap_or_else(|| panic!("no zone for {action:?}"))
            .rect
    }

    #[test]
    fn busy_popup_is_retried_until_it_opens() {
        let attempts = Cell::new(0);
        let result = retry_while_busy(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 3 {
                    Err(io::Error::other("ui_busy: a popup pane is already open"))
                } else {
                    Ok(())
                }
            },
            Duration::from_millis(1),
            Duration::from_millis(50),
        );
        assert!(result.is_ok());
        assert_eq!(attempts.get(), 3);
    }

    #[test]
    fn busy_popup_gives_up_at_the_deadline_with_the_last_error() {
        let attempts = Cell::new(0);
        let result = retry_while_busy(
            || {
                attempts.set(attempts.get() + 1);
                Err(io::Error::other("ui_busy"))
            },
            Duration::from_millis(1),
            Duration::from_millis(3),
        );
        assert!(result.unwrap_err().to_string().contains("ui_busy"));
        // One attempt per step inside the limit, plus the final one.
        assert_eq!(attempts.get(), 4);
    }

    #[test]
    fn other_errors_are_not_retried() {
        let attempts = Cell::new(0);
        let result = retry_while_busy(
            || {
                attempts.set(attempts.get() + 1);
                Err(io::Error::other("no herdr socket"))
            },
            Duration::from_millis(1),
            Duration::from_secs(3),
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn choose_undo_and_auto_advance() {
        use Resolution::*;
        let (mut app, _scratch) = app_for("choose");
        assert_eq!(app.rel, "a.txt");
        assert_eq!(editor(&app).selected, 0);

        key(&mut app, 'i');
        assert_eq!(resolutions(&app), [Incoming, Unresolved]);
        assert_eq!(editor(&app).selected, 1, "auto-advances to the next block");

        key(&mut app, 'x');
        assert_eq!(resolutions(&app), [Incoming, Unresolved]);
        assert_eq!(editor(&app).selected, 1, "reset does not advance");

        key(&mut app, 'b');
        assert_eq!(resolutions(&app), [Incoming, CurrentThenIncoming]);
        assert_eq!(editor(&app).selected, 1, "nothing left to advance to");

        key(&mut app, 'u');
        assert_eq!(resolutions(&app), [Incoming, Unresolved], "undo the choice");
        key(&mut app, 'u');
        assert_eq!(
            resolutions(&app),
            [Unresolved, Unresolved],
            "undo the first"
        );

        key(&mut app, 'c');
        assert_eq!(resolutions(&app), [Unresolved, Current]);
        assert_eq!(editor(&app).selected, 0, "wraps back to the open block");
        key(&mut app, 'B');
        assert_eq!(resolutions(&app), [IncomingThenCurrent, Current]);
        // No base section in a plain merge, so `a` refuses.
        key(&mut app, 'a');
        assert_eq!(resolutions(&app), [IncomingThenCurrent, Current]);
        assert!(app.flash.as_ref().unwrap().text.contains("diff3"));
    }

    #[test]
    fn capital_c_resolves_every_open_conflict_in_one_undo_step() {
        let (mut app, _scratch) = app_for("accept-all");
        key(&mut app, 'C');
        assert_eq!(editor(&app).doc.unresolved_count(), 0);
        assert_eq!(
            resolutions(&app),
            [Resolution::Current, Resolution::Current]
        );
        key(&mut app, 'u');
        assert_eq!(editor(&app).doc.unresolved_count(), 2);
    }

    #[test]
    fn header_click_toggles_and_result_button_chooses() {
        let (mut app, _scratch) = app_for("clicks");
        draw(&mut app, 100, 30);
        let incoming_header = zone_rect(&app, Action::ToggleIncoming);
        click(&mut app, incoming_header.x + 1, incoming_header.y);
        assert_eq!(resolutions(&app)[0], Resolution::Incoming);

        draw(&mut app, 100, 30);
        let incoming_again = app
            .zones
            .iter()
            .find(|zone| zone.action == Action::ToggleIncoming && zone.ordinal == 0)
            .unwrap()
            .rect;
        click(&mut app, incoming_again.x + 1, incoming_again.y);
        assert_eq!(resolutions(&app)[0], Resolution::Unresolved, "toggled off");

        draw(&mut app, 100, 30);
        // The Result header's "Accept Current" button for block 0.
        let button = zone_rect(&app, Action::Choose(Resolution::Current));
        click(&mut app, button.x, button.y);
        assert_eq!(resolutions(&app)[0], Resolution::Current);
        assert_eq!(editor(&app).selected, 1, "the button advances");

        // The block-0 row is off screen after the advance: go back to it, then
        // a click on a visible row of block 1 selects block 1 again.
        key(&mut app, 'p');
        draw(&mut app, 100, 60);
        assert_eq!(editor(&app).selected, 0);
        let ed = editor(&app);
        let top = ed.top_scroll;
        let rect = app.panes.incoming;
        let visible = ed
            .incoming
            .view
            .iter()
            .enumerate()
            .skip(top)
            .take(usize::from(rect.height))
            .find(|(_, row)| row.conflict == Some(1) && row.kind == LineKind::Incoming)
            .map(|(index, _)| index)
            .expect("block 1 is in view after reveal");
        let y = rect.y + (visible - top) as u16;
        click(&mut app, rect.x + 1, y);
        assert_eq!(editor(&app).selected, 1);
    }

    #[test]
    fn enter_refuses_while_conflicts_are_unresolved() {
        let (mut app, _scratch) = app_for("enter-refuse");
        key(&mut app, 'c');
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.flash.as_ref().unwrap().text.contains("1 conflict left"));
        assert!(
            app.git
                .as_ref()
                .unwrap()
                .conflicted_paths()
                .unwrap()
                .contains(&"a.txt".to_string())
        );
    }

    #[test]
    fn accepting_all_and_completing_writes_the_file_and_clears_the_merge() {
        let (mut app, _scratch) = app_for("complete");
        let git_root = app.root.clone();
        key(&mut app, 'C');
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            std::fs::read_to_string(git_root.join("a.txt")).unwrap(),
            OURS,
            "current on every block gives the ours side"
        );
        assert_eq!(app.rel, "b.txt", "moves to the next conflicted file");

        key(&mut app, 'C');
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            std::fs::read_to_string(git_root.join("b.txt")).unwrap(),
            OURS
        );
        assert!(matches!(
            app.body,
            Body::Notice {
                close_on_key: true,
                ..
            }
        ));
        let git = app.git.as_ref().unwrap();
        assert!(git.conflicted_paths().unwrap().is_empty());
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.done);
    }

    #[test]
    fn escape_on_unsaved_choices_asks_and_n_closes_without_writing() {
        let (mut app, _scratch) = app_for("dirty-esc");
        let before = std::fs::read(&app.abs).unwrap();
        key(&mut app, 'i');
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.confirm.is_some(), "a confirm is open");
        assert!(!app.done);
        key(&mut app, 'n');
        assert!(app.done, "No closes");
        assert_eq!(std::fs::read(&app.abs).unwrap(), before, "nothing written");
    }

    #[test]
    fn escape_inside_the_dialog_keeps_the_editor_open() {
        let (mut app, _scratch) = app_for("dirty-cancel");
        key(&mut app, 'i');
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.confirm.is_none());
        assert!(!app.done);
        assert_eq!(resolutions(&app)[0], Resolution::Incoming);
    }

    #[test]
    fn save_refuses_when_the_file_changed_on_disk() {
        let (mut app, _scratch) = app_for("changed");
        key(&mut app, 'i');
        std::fs::write(&app.abs, "someone else\n").unwrap();
        key(&mut app, 's');
        let flash = app.flash.as_ref().unwrap();
        assert!(flash.warn && flash.text.contains("changed on disk"));
        assert_eq!(std::fs::read_to_string(&app.abs).unwrap(), "someone else\n");
    }

    /// The gutter number of each drawn row of one pane, `None` for rows with
    /// no number (header bands, fillers).
    fn pane_numbers(app: &App, pane: PaneKind) -> Vec<Option<usize>> {
        let ed = editor(app);
        let area = Rect::new(0, 0, 40, 200);
        let mut zones = Vec::new();
        pane_lines(ed, pane, area, 0, &mut zones)
            .iter()
            .map(|line| {
                let gutter: String = line
                    .spans
                    .first()
                    .map(|span| span.content.to_string())
                    .unwrap_or_default();
                gutter.trim().parse::<usize>().ok()
            })
            .collect()
    }

    #[test]
    fn each_pane_numbers_its_own_rows_from_one() {
        let (app, _scratch) = app_for("gutter");
        let ed = editor(&app);
        for (pane, rows) in [
            (PaneKind::Incoming, &ed.incoming),
            (PaneKind::Current, &ed.current),
            (PaneKind::Result, &ed.result),
        ] {
            let numbers = pane_numbers(&app, pane);
            let counted: Vec<usize> = numbers.iter().flatten().copied().collect();
            let expected: Vec<usize> = (1..=rows
                .view
                .iter()
                .filter(|row| !matches!(row.kind, LineKind::Header | LineKind::Filler))
                .count())
                .collect();
            assert_eq!(counted, expected, "pane {pane:?} numbers in order");
            for (row, number) in rows.view.iter().zip(&numbers) {
                let header_or_filler = matches!(row.kind, LineKind::Header | LineKind::Filler);
                assert_eq!(number.is_none(), header_or_filler, "{:?}", row.kind);
            }
        }
        // The Result pane keeps the raw conflict markers, so its numbering
        // runs past the source line of the last content row above them.
        assert!(
            ed.result
                .view
                .iter()
                .any(|row| row.kind == LineKind::Marker)
        );
        let result = pane_numbers(&app, PaneKind::Result);
        let last = result.iter().flatten().copied().last().unwrap();
        assert_eq!(
            last,
            ed.result
                .view
                .iter()
                .filter(|row| !matches!(row.kind, LineKind::Header | LineKind::Filler))
                .count()
        );
    }

    #[test]
    fn filler_rows_are_a_faint_hatch_after_the_gutter() {
        let filler = ViewLine {
            text: String::new(),
            kind: LineKind::Filler,
            conflict: None,
            source_line: None,
        };
        let line = content_line(&filler, None, None, 16, 0);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "      ╱ ╱ ╱ ╱ ╱ ");
        assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn title_counts_files_and_conflicts_in_words() {
        let (mut app, _scratch) = app_for("title");
        let text = |app: &App| -> String {
            app.title_line(200)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let title = text(&app);
        assert!(title.contains("2 files left"), "{title}");
        assert!(title.contains("of 2 conflicts left"), "{title}");
        assert!(!title.contains("file 1/"), "{title}");
        key(&mut app, 'c');
        let title = text(&app);
        assert!(title.contains("1 of 2 conflicts left"), "{title}");
    }

    #[test]
    fn narrow_width_draws_only_the_result_pane() {
        let (mut app, _scratch) = app_for("narrow");
        draw(&mut app, 60, 20);
        assert_eq!(app.panes.incoming, Rect::default());
        assert_eq!(app.panes.current, Rect::default());
        assert!(
            app.zones
                .iter()
                .all(|zone| matches!(zone.action, Action::Choose(_)))
        );
        assert!(
            app.zones
                .iter()
                .any(|zone| zone.action == Action::Choose(Resolution::Current))
        );
    }

    #[test]
    fn tiny_sizes_draw_without_panicking() {
        let (mut app, _scratch) = app_for("tiny");
        key(&mut app, 'c');
        for (width, height) in [(10, 5), (1, 1), (3, 3), (20, 4), (80, 2)] {
            draw(&mut app, width, height);
        }
        app.help = true;
        draw(&mut app, 10, 5);
        key(&mut app, 'r');
        draw(&mut app, 10, 5);
    }

    #[test]
    fn a_file_without_conflict_blocks_completes_from_its_notice() {
        let (git, _scratch) = conflict_repo("no-blocks");
        std::fs::write(git.root().join("a.txt"), "resolved by hand\n").unwrap();
        let mut app = App::open(git, None, None);
        draw(&mut app, 80, 20);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.rel, "b.txt");
        assert!(
            !app.git
                .as_ref()
                .unwrap()
                .conflicted_paths()
                .unwrap()
                .contains(&"a.txt".to_string())
        );
    }

    #[test]
    fn tab_switches_files_and_asks_before_leaving_unsaved_choices() {
        let (mut app, _scratch) = app_for("tab");
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.rel, "b.txt");
        key(&mut app, 'i');
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert!(matches!(app.confirm, Some(Confirm::Leave(Then::Switch(0)))));
        key(&mut app, 'n');
        assert_eq!(app.rel, "a.txt", "No discards and switches");
    }

    #[test]
    fn a_missing_repository_shows_the_notice() {
        let dir = std::env::temp_dir().join(format!("hs-merge-popup-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::blank(None, dir.clone(), None, "none".to_string());
        app.set_notice(vec!["No merge conflicts in none".to_string()]);
        draw(&mut app, 60, 10);
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The repository with `b.txt` already marked resolved, so `a.txt` is the
    /// only conflicted file left.
    fn single_file_app(name: &str) -> (App, Scratch) {
        let (git, scratch) = conflict_repo(name);
        assert!(git.mark_resolved("b.txt").is_ok(), "b.txt marked resolved");
        (App::open(git, None, None), scratch)
    }

    fn ctrl(app: &mut App, c: char) {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn enter(app: &mut App) {
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn conflicted(app: &App) -> Vec<String> {
        app.git.as_ref().unwrap().conflicted_paths().unwrap()
    }

    #[test]
    fn ctrl_s_finishes_the_last_file_and_closes() {
        let (mut app, _scratch) = single_file_app("ctrl-s-last");
        let git_root = app.root.clone();
        key(&mut app, 'C');
        ctrl(&mut app, 's');
        assert_eq!(
            std::fs::read_to_string(git_root.join("a.txt")).unwrap(),
            OURS
        );
        assert!(conflicted(&app).is_empty());
        assert!(app.done);
    }

    #[test]
    fn ctrl_s_on_a_file_with_more_to_do_opens_the_next_one() {
        let (mut app, _scratch) = app_for("ctrl-s-next");
        key(&mut app, 'C');
        ctrl(&mut app, 's');
        assert!(!app.done);
        assert_eq!(app.rel, "b.txt");
        assert!(!conflicted(&app).contains(&"a.txt".to_string()));
    }

    #[test]
    fn ctrl_s_with_conflicts_left_saves_the_partial_choices_only() {
        let (mut app, _scratch) = app_for("ctrl-s-partial");
        key(&mut app, 'c');
        ctrl(&mut app, 's');
        assert!(!app.done);
        assert_eq!(app.rel, "a.txt");
        assert_eq!(
            editor(&app).doc.conflict_count(),
            1,
            "one block left on disk"
        );
        assert!(!editor(&app).doc.is_dirty());
        assert!(conflicted(&app).contains(&"a.txt".to_string()));
    }

    #[test]
    fn escape_then_yes_finishes_a_resolved_file_on_the_close_prompt() {
        let (mut app, _scratch) = single_file_app("close-yes");
        key(&mut app, 'C');
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.confirm, Some(Confirm::Leave(Then::Close))));
        key(&mut app, 'y');
        assert!(conflicted(&app).is_empty());
        assert!(app.done);
    }

    #[test]
    fn close_prompt_yes_with_conflicts_left_saves_and_closes() {
        let (mut app, _scratch) = app_for("close-partial");
        key(&mut app, 'c');
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        key(&mut app, 'y');
        assert!(app.done);
        assert!(conflicted(&app).contains(&"a.txt".to_string()));
    }

    #[test]
    fn enter_on_the_last_file_still_shows_the_done_notice() {
        let (mut app, _scratch) = single_file_app("enter-done");
        key(&mut app, 'C');
        enter(&mut app);
        assert!(!app.done, "the done notice waits for a key");
        assert!(matches!(
            app.body,
            Body::Notice {
                close_on_key: true,
                ..
            }
        ));
        key(&mut app, ' ');
        assert!(app.done);
    }
}
