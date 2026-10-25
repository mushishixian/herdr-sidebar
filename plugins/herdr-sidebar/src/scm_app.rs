//! TUI state and rendering: the VS Code Source Control panel — commit message
//! box (with the ✨ suggest button), Commit button, collapsible Staged/Changes
//! sections, Git-Graph-style drawers (GRAPH, COMMITS, FILE HISTORY, BRANCHES,
//! REMOTES, STASHES, TAGS), theme-matched file icons, mouse support, and a
//! Ctrl+right-click context menu — kept interaction-consistent with
//! herdr-aa-filetree. No own border/title: herdr already frames the pane and
//! titles it with the pane label.
//!
//! When herdr-aa-filetree is also installed, the panel can merge with it into
//! a single "Sidebar" pane with an activity-bar view switcher (see sidebar.rs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};

use herdr_sidebar::actions::{self, ClipboardWrite, copy_to_clipboard, open_external, reveal};
use herdr_sidebar::branch_ui::{
    BranchPicker, FooterZones, PickerAction, draw_git_footer, sync_glyph,
};
use herdr_sidebar::fswatch::{FsWatch, WatchSet};
use herdr_sidebar::git::{DeleteRefused, FileEntry, Git, Status};
use herdr_sidebar::icons::{IconTheme, icon};
use herdr_sidebar::pane_menu::{self, PaneAction};
use herdr_sidebar::state::Exit;
use herdr_sidebar::state::{self as sidebar, View};
use herdr_sidebar::suggest;
use herdr_sidebar::ui::{
    SelectAll, TitleAction, activity_button_style, activity_icons, branch_icon,
    chrome_button_style, draw_activity_caps, draw_scrollbar, gear_icon, hits, hits_activity_button,
    hits_collapse_button, hover_style, icon_style as ui_icon_style, keep_visible_scroll, palette,
    selected_text_style, selection_style, set_color_theme, sibling_panes_of, sparkle_icon,
    status_color, title_action_spans, title_actions_visible, title_actions_width, truncate_to,
    within, wrap_footer_message, wrap_hints,
};

/// How many log lines the history-ish drawers fetch.
const DRAWER_LIMIT: usize = 30;
const REPO_HEADER_ACTIONS: &str = " ⇅  ✓ ";

/// How long two clicks on the same row still count as a double click (to pin
/// a diff/show tab), matching the file explorer.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);
/// How often this sidebar checks its tab's AI commit request file.
const AI_REQUEST_POLL: std::time::Duration = std::time::Duration::from_millis(300);
/// How often the focused pane re-reads `git status`. The idle cadence
/// (the old 1.5 s one) made a file edited in another
/// pane show up late (user-reported); when the user is looking at this pane, a
/// status read is cheap (one process per repo) and `refresh` skips the rebuild
/// when nothing changed.
const FOCUSED_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);
/// How often the event loop wakes while this pane is UNFOCUSED. herdr delivers
/// no per-pane focus events (verified: nothing arrives on the pane's stdin when
/// focus moves), so gaining focus is only noticed by polling `pane.list` — at the
/// old 1.5 s cadence that meant "click the sidebar, then wait up to 1.5 s plus a
/// `git status` before the list moved" (user-reported). 400 ms costs one socket
/// call (and no git process: the refresh guard below still refuses while
/// unfocused) and makes the catch-up feel immediate.
const IDLE_TICK: std::time::Duration = std::time::Duration::from_millis(400);
/// How long a divider drag must stay put before it is adopted as the sidebar
/// width setting — a drag is a burst of resize events, not one.
const WIDTH_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

fn sync_is_primary(status: &Status) -> bool {
    status.staged.is_empty()
        && status.unstaged.is_empty()
        && status.has_upstream
        && status.ahead + status.behind > 0
}

/// The default action on a changed file (click, `o`, the hover `↗`) hands the
/// FILE to the editor — that is what "go to the change" means when you are about
/// to work on it, and for a conflicted (`!`) entry Fresh's inline merge resolver
/// (Alt+C / Alt+I / Alt+M, Alt+D next) is the only thing that can resolve it.
/// A DELETED entry has no file left on disk, so it keeps its read-only diff, and
/// the menu's "Open Diff" / "Open Diff Preview" still mean the diff.
fn opens_in_editor(entry: &FileEntry) -> bool {
    entry.letter != 'D'
}

fn sync_label_for_status(status: &Status, syncing: bool) -> Option<String> {
    if syncing {
        // A branch with no upstream is being PUBLISHED, not synced.
        let verb = if status.has_upstream {
            "Syncing"
        } else {
            "Publishing"
        };
        return Some(format!("{} {verb}…", sync_glyph(true)));
    }
    if !status.has_upstream {
        // A branch created here has no upstream, so git reports no ahead/behind
        // and this row used to disappear — the one case that needs a push had
        // no button at all. VS Code calls it Publish Branch; `Git::push` sets
        // the upstream on the way out.
        return Some(format!("{} Publish Branch", sync_glyph(false)));
    }
    if status.ahead + status.behind == 0 {
        return None;
    }
    let mut counts = Vec::new();
    if status.ahead > 0 {
        counts.push(format!("{}↑", status.ahead));
    }
    if status.behind > 0 {
        counts.push(format!("{}↓", status.behind));
    }
    Some(format!("⟳ Sync Changes {}", counts.join(" ")))
}

/// Braille spinner frames for the AI commit button.
const AI_SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// One animation step of the AI commit button (matches the event loop's frame).
const AI_FRAME_MS: u128 = 120;
/// Width of the shimmer band, in cells.
const SHIMMER_BAND: usize = 6;
/// Cells the shimmer band advances per animation step.
const SHIMMER_STEP: usize = 2;

/// Which half of the AI commit a repo's button is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AiPhase {
    /// OpenCommit is writing the message.
    Generating,
}

/// Animation frame index since the AI commit started.
fn ai_frame(elapsed: std::time::Duration) -> usize {
    (elapsed.as_millis() / AI_FRAME_MS) as usize
}

/// The AI button's label, with the spinner frame for `elapsed`.
fn ai_button_label(phase: AiPhase, elapsed: std::time::Duration) -> String {
    let frame = AI_SPINNER[ai_frame(elapsed) % AI_SPINNER.len()];
    match phase {
        AiPhase::Generating => format!("{frame} AI generating…"),
    }
}

/// The text a Commit button shows: the AI label while an AI commit runs for
/// its repo (it overrides Sync Changes), otherwise the sync or commit label.
fn button_label(
    status: &Status,
    syncing: bool,
    ai: Option<(AiPhase, std::time::Duration)>,
) -> String {
    if let Some((phase, elapsed)) = ai {
        return ai_button_label(phase, elapsed);
    }
    if sync_is_primary(status) {
        return sync_label_for_status(status, syncing)
            .unwrap_or_else(|| "⟳ Sync Changes".to_string());
    }
    // Behind the upstream with something staged: clicking asks whether to pull
    // first, so the button says so.
    if status.behind > 0 {
        return format!("✓ Commit · {}↓", status.behind);
    }
    "✓ Commit".to_string()
}

/// `width` cells of `label` (centered, truncated like the static button) with
/// a [`SHIMMER_BAND`]-cell highlight sweeping left to right. The band cells
/// take `band_bg`; every other cell keeps `base`'s background, and the label
/// keeps `base`'s foreground everywhere.
fn shimmer_spans(
    label: &str,
    width: usize,
    elapsed: std::time::Duration,
    base: Style,
    band_bg: Color,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;

    if width == 0 {
        return Vec::new();
    }
    let label = truncate_to(label.to_string(), width);
    let label_width = Span::raw(label.as_str()).width();
    let left = width.saturating_sub(label_width) / 2;
    let period = width + SHIMMER_BAND;
    let band_start = (SHIMMER_STEP * ai_frame(elapsed)) % period;

    // One entry per glyph: its text and first cell. Zero-width characters
    // join the glyph before them.
    let mut glyphs: Vec<(String, usize)> = Vec::new();
    let mut cell = 0usize;
    for _ in 0..left {
        glyphs.push((" ".to_string(), cell));
        cell += 1;
    }
    for c in label.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if cw == 0 {
            if let Some(last) = glyphs.last_mut() {
                last.0.push(c);
            }
            continue;
        }
        glyphs.push((c.to_string(), cell));
        cell += cw;
    }
    while cell < width {
        glyphs.push((" ".to_string(), cell));
        cell += 1;
    }

    let band_style = base.bg(band_bg);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run: Option<(String, bool)> = None;
    for (text, start) in glyphs {
        let in_band = start >= band_start && start < band_start + SHIMMER_BAND;
        match run.as_mut() {
            Some((buf, band)) if *band == in_band => buf.push_str(&text),
            _ => {
                if let Some((buf, band)) = run.take() {
                    spans.push(Span::styled(buf, if band { band_style } else { base }));
                }
                run = Some((text, in_band));
            }
        }
    }
    if let Some((buf, band)) = run {
        spans.push(Span::styled(buf, if band { band_style } else { base }));
    }
    spans
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Message,
    Commit,
    List,
}

/// What the `git-commit-ai` worker produced for one repository.
enum AiOutcome {
    /// OpenCommit's message for the staged changes, and the large files it
    /// saw only in shortened form.
    Message(herdr_sidebar::ai_commit::Generated),
    /// Nothing was staged after `git add -A` semantics: no changes at all.
    NothingToCommit,
    Failed(String),
}

/// The Git-Graph-style drawers below the Changes section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drawer {
    Graph,
    Commits,
    FileHistory,
    Branches,
    Worktrees,
    Remotes,
    Stashes,
    Tags,
}

impl Drawer {
    const ALL: [Drawer; 8] = [
        Drawer::Graph,
        Drawer::Commits,
        Drawer::FileHistory,
        Drawer::Branches,
        Drawer::Worktrees,
        Drawer::Remotes,
        Drawer::Stashes,
        Drawer::Tags,
    ];

    fn title(self) -> &'static str {
        match self {
            Drawer::Graph => "Graph",
            Drawer::Commits => "Commits",
            Drawer::FileHistory => "File History",
            Drawer::Branches => "Branches",
            Drawer::Worktrees => "Worktrees",
            Drawer::Remotes => "Remotes",
            Drawer::Stashes => "Stashes",
            Drawer::Tags => "Tags",
        }
    }

    fn index(self) -> usize {
        Drawer::ALL.iter().position(|d| *d == self).unwrap_or(0)
    }
}

#[derive(Default)]
struct DrawerPanel {
    expanded: bool,
    lines: Vec<String>,
    /// What each line points at, parallel to `lines`.
    refs: Vec<DrawerRef>,
}

/// What a drawer line points at, for clicks and context menus.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
enum DrawerRef {
    #[default]
    None,
    /// A short commit hash (GRAPH / COMMITS / FILE HISTORY lines).
    Commit(String),
    /// `stash@{n}`.
    Stash(usize),
    Branch {
        name: String,
        current: bool,
    },
    Remote {
        name: String,
        url: String,
    },
    Tag(String),
    /// A worktree's checkout path.
    Worktree(String),
}

/// Parse the actionable reference out of one drawer line.
/// Display form of a `git worktree list` line: the folder NAME plus its
/// branch — the raw absolute path clipped uselessly in a narrow pane.
fn pretty_worktree_line(raw: &str) -> String {
    let path = raw.split_whitespace().next().unwrap_or("");
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if let (Some(start), Some(end)) = (raw.find('['), raw.rfind(']'))
        && start < end
    {
        return format!("{name}  ⎇ {}", &raw[start + 1..end]);
    }
    if raw.contains("(bare)") {
        return format!("{name}  (bare)");
    }
    if raw.contains("detached") {
        return format!("{name}  (detached)");
    }
    name.to_string()
}

/// Display form of a remote line: `name  owner/repo` for hosted URLs (the
/// interesting part), the folder name for local-path remotes.
fn pretty_remote_line(raw: &str) -> String {
    let mut it = raw.split_whitespace();
    match (it.next(), it.next()) {
        (Some(name), Some(url)) => format!("{name}  {}", pretty_remote_url(url)),
        _ => raw.to_string(),
    }
}

fn pretty_remote_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let hosted = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .or_else(|| trimmed.strip_prefix("git@"));
    if let Some(rest) = hosted {
        // git@host:owner/repo and host/owner/repo both → owner/repo.
        let rest = rest.replace(':', "/");
        return match rest.split_once('/') {
            Some((_host, path)) if !path.is_empty() => path.to_string(),
            _ => rest,
        };
    }
    // A local-path remote: its folder name is the recognizable bit.
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

fn parse_drawer_ref(kind: Drawer, line: &str) -> DrawerRef {
    match kind {
        Drawer::Graph | Drawer::Commits | Drawer::FileHistory => line
            .split_whitespace()
            .find(|tok| {
                tok.len() >= 7
                    && tok
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
            .map(|h| DrawerRef::Commit(h.to_string()))
            .unwrap_or(DrawerRef::None),
        Drawer::Branches => {
            let current = line.starts_with('*');
            let name = line.trim_start_matches('*').trim().to_string();
            if name.is_empty() || name.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Branch { name, current }
            }
        }
        Drawer::Remotes => {
            let mut it = line.split_whitespace();
            match it.next() {
                Some(name) => DrawerRef::Remote {
                    name: name.to_string(),
                    url: it.next().unwrap_or("").to_string(),
                },
                None => DrawerRef::None,
            }
        }
        Drawer::Worktrees => {
            let path = line.split_whitespace().next().unwrap_or("").to_string();
            if path.is_empty() || path.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Worktree(path)
            }
        }
        Drawer::Stashes => line
            .strip_prefix("stash@{")
            .and_then(|rest| rest.split('}').next())
            .and_then(|n| n.parse::<usize>().ok())
            .map(DrawerRef::Stash)
            .unwrap_or(DrawerRef::None),
        Drawer::Tags => {
            let name = line.trim().to_string();
            if name.is_empty() || name.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Tag(name)
            }
        }
    }
}

/// The stable, repo-independent handle a drawer line points at — for mirroring
/// a selection across tabs. `None` for blank graph-edge rows.
fn drawer_spec(dref: &DrawerRef) -> Option<String> {
    match dref {
        DrawerRef::Commit(h) => Some(h.clone()),
        DrawerRef::Stash(n) => Some(format!("stash@{{{n}}}")),
        DrawerRef::Branch { name, .. } => Some(name.clone()),
        DrawerRef::Tag(t) => Some(t.clone()),
        DrawerRef::Remote { name, .. } => Some(name.clone()),
        DrawerRef::Worktree(p) => Some(p.clone()),
        DrawerRef::None => None,
    }
}

/// An object name safe to pass to git commands. Tags are fully qualified so
/// their namespace is unambiguous; branch drawer rows can be local or remote,
/// so they stay in git's displayed short form. Every command call site also
/// terminates option parsing before this value.
fn drawer_git_spec(dref: &DrawerRef) -> Option<String> {
    match dref {
        DrawerRef::Tag(name) => Some(format!("refs/tags/{name}")),
        _ => drawer_spec(dref),
    }
}

fn drawer_checkout_args(dref: &DrawerRef, spec: &str, git_spec: &str) -> Option<Vec<String>> {
    match dref {
        DrawerRef::Branch { .. } => Some(vec![
            "checkout".into(),
            "--end-of-options".into(),
            spec.into(),
        ]),
        DrawerRef::Commit(_) | DrawerRef::Tag(_) => Some(vec![
            "switch".into(),
            "--detach".into(),
            "--".into(),
            git_spec.into(),
        ]),
        _ => None,
    }
}

fn drawer_reset_args(git_spec: &str) -> Vec<String> {
    vec![
        "reset".into(),
        "--mixed".into(),
        "--end-of-options".into(),
        git_spec.into(),
        "--".into(),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirEntry {
    path: String,
    name: String,
    depth: usize,
    expanded: bool,
    count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TreeItem {
    Dir(usize),
    File(usize),
}

#[derive(Clone, Debug)]
struct ScmTreeNode {
    name: String,
    path: String,
    is_dir: bool,
    file_index: Option<usize>,
    children: Vec<ScmTreeNode>,
}

impl ScmTreeNode {
    fn new_dir(name: String, path: String) -> Self {
        Self {
            name,
            path,
            is_dir: true,
            file_index: None,
            children: Vec::new(),
        }
    }

    fn new_file(name: String, path: String, file_index: usize) -> Self {
        Self {
            name,
            path,
            is_dir: false,
            file_index: Some(file_index),
            children: Vec::new(),
        }
    }

    fn add_path(&mut self, components: &[&str], full_path: &str, file_index: usize) {
        if components.is_empty() {
            return;
        }
        if components.len() == 1 {
            self.children.push(ScmTreeNode::new_file(
                components[0].to_string(),
                full_path.to_string(),
                file_index,
            ));
            return;
        }
        let dir_name = components[0];
        let sub_path = if self.path.is_empty() {
            dir_name.to_string()
        } else {
            format!("{}/{}", self.path, dir_name)
        };
        if let Some(pos) = self
            .children
            .iter()
            .position(|c| c.is_dir && c.name == dir_name)
        {
            self.children[pos].add_path(&components[1..], full_path, file_index);
        } else {
            let mut new_dir = ScmTreeNode::new_dir(dir_name.to_string(), sub_path);
            new_dir.add_path(&components[1..], full_path, file_index);
            self.children.push(new_dir);
        }
    }

    fn compact(&mut self) {
        for child in &mut self.children {
            child.compact();
        }
        if self.is_dir && self.children.len() == 1 && self.children[0].is_dir {
            let only_child = self.children.remove(0);
            self.name = format!("{}/{}", self.name, only_child.name);
            self.path = only_child.path;
            self.children = only_child.children;
        }
    }

    fn sort(&mut self) {
        for child in &mut self.children {
            child.sort();
        }
        self.children.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
    }

    fn file_count(&self) -> usize {
        if !self.is_dir {
            1
        } else {
            self.children.iter().map(|c| c.file_count()).sum()
        }
    }
}

/// Drop collapsed folders that no longer exist in `files`. Existence is
/// judged against EVERY current (compacted) folder path, not the visible
/// rows: a folder hidden inside a collapsed parent still exists, and must
/// come back collapsed when the parent is expanded again.
fn prune_collapsed(collapsed: &mut std::collections::BTreeSet<String>, files: &[FileEntry]) {
    if files.is_empty() {
        return;
    }
    let (all, _, _) = build_tree_model(files, &std::collections::BTreeSet::new());
    let existing: std::collections::BTreeSet<&str> =
        all.iter().map(|dir| dir.path.as_str()).collect();
    collapsed.retain(|path| existing.contains(path.as_str()));
}

fn build_tree_model(
    files: &[FileEntry],
    collapsed_dirs: &std::collections::BTreeSet<String>,
) -> (Vec<DirEntry>, Vec<usize>, Vec<TreeItem>) {
    let mut root = ScmTreeNode::new_dir(String::new(), String::new());
    for (i, entry) in files.iter().enumerate() {
        let parts: Vec<&str> = entry.path.split('/').filter(|s| !s.is_empty()).collect();
        root.add_path(&parts, &entry.path, i);
    }
    root.sort();
    for child in &mut root.children {
        child.compact();
    }
    let mut dirs = Vec::new();
    let mut depths = vec![0; files.len()];
    let mut rows = Vec::new();
    flatten_scm_tree(
        &root.children,
        0,
        collapsed_dirs,
        &mut dirs,
        &mut depths,
        &mut rows,
    );
    (dirs, depths, rows)
}

fn flatten_scm_tree(
    nodes: &[ScmTreeNode],
    depth: usize,
    collapsed_dirs: &std::collections::BTreeSet<String>,
    dirs_out: &mut Vec<DirEntry>,
    file_depths_out: &mut [usize],
    rows_out: &mut Vec<TreeItem>,
) {
    for node in nodes {
        if node.is_dir {
            let expanded = !collapsed_dirs.contains(&node.path);
            let dir_idx = dirs_out.len();
            dirs_out.push(DirEntry {
                path: node.path.clone(),
                name: node.name.clone(),
                depth,
                expanded,
                count: node.file_count(),
            });
            rows_out.push(TreeItem::Dir(dir_idx));
            if expanded {
                flatten_scm_tree(
                    &node.children,
                    depth + 1,
                    collapsed_dirs,
                    dirs_out,
                    file_depths_out,
                    rows_out,
                );
            }
        } else if let Some(idx) = node.file_index {
            if idx < file_depths_out.len() {
                file_depths_out[idx] = depth;
            }
            rows_out.push(TreeItem::File(idx));
        }
    }
}

/// One discovered repository and its per-repo view state — including its own
/// commit message, so the multi-repo view mirrors VS Code's per-repo inputs.
struct Repo {
    git: Git,
    name: String,
    status: Status,
    /// The git dir's `HEAD` as last seen; a change means the branch moved (see
    /// [`Git::head_signature`]).
    head_sig: Option<String>,
    collapsed: bool,
    staged_collapsed: bool,
    changes_collapsed: bool,
    message: Vec<char>,
    cursor: usize,
    /// Ctrl+A in the commit box: the next edit replaces the whole message
    /// (`ui::SelectAll`).
    select_all: SelectAll,
    staged_dirs: Vec<DirEntry>,
    changes_dirs: Vec<DirEntry>,
    staged_file_depths: Vec<usize>,
    changes_file_depths: Vec<usize>,
    staged_tree_rows: Vec<TreeItem>,
    changes_tree_rows: Vec<TreeItem>,
    staged_collapsed_dirs: std::collections::BTreeSet<String>,
    changes_collapsed_dirs: std::collections::BTreeSet<String>,
}

impl Repo {
    fn new(git: Git) -> Self {
        Self {
            name: git.name(),
            head_sig: git.head_signature(),
            git,
            status: Status::default(),
            collapsed: false,
            staged_collapsed: false,
            changes_collapsed: false,
            message: Vec::new(),
            cursor: 0,
            select_all: SelectAll::default(),
            staged_dirs: Vec::new(),
            changes_dirs: Vec::new(),
            staged_file_depths: Vec::new(),
            changes_file_depths: Vec::new(),
            staged_tree_rows: Vec::new(),
            changes_tree_rows: Vec::new(),
            staged_collapsed_dirs: std::collections::BTreeSet::new(),
            changes_collapsed_dirs: std::collections::BTreeSet::new(),
        }
    }

    /// An armed Ctrl+A (`ui::SelectAll`) means "replace it all": clear the
    /// message and the caret instead, and report that the edit the caller was
    /// about to make must be skipped. (`SelectAll::take` wants a `String`; this
    /// buffer is a `Vec<char>`.)
    fn take_selected_message(&mut self) -> bool {
        if !self.select_all.armed() {
            return false;
        }
        self.select_all.disarm();
        self.message.clear();
        self.cursor = 0;
        true
    }

    /// Esc in the commit box: drop an armed Ctrl+A and spend the press. False
    /// when nothing was armed, so the caller knows the Esc means "leave the
    /// box".
    fn esc_message(&mut self) -> bool {
        if !self.select_all.armed() {
            return false;
        }
        self.select_all.disarm();
        true
    }

    /// The commit box's editing keys: Backspace/Delete, the caret moves,
    /// Ctrl+A/Ctrl+U and typed text. The focus keys (Enter, Esc, Tab…) stay in
    /// `App::on_message_key`, which owns the focus.
    fn message_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // An armed Ctrl+A means "replace it all": the delete takes the
            // whole message instead of one character.
            KeyCode::Backspace if !self.take_selected_message() && self.cursor > 0 => {
                self.cursor -= 1;
                self.message.remove(self.cursor);
            }
            KeyCode::Delete
                if !self.take_selected_message() && self.cursor < self.message.len() =>
            {
                self.message.remove(self.cursor);
            }
            // A caret move disarms: the selection belongs where it was made.
            KeyCode::Left => {
                self.select_all.disarm();
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.select_all.disarm();
                self.cursor = (self.cursor + 1).min(self.message.len());
            }
            KeyCode::Home => {
                self.select_all.disarm();
                self.cursor = 0;
            }
            KeyCode::End => {
                self.select_all.disarm();
                self.cursor = self.message.len();
            }
            KeyCode::Char('a') if ctrl && !alt => {
                self.select_all.arm_if_nonempty(!self.message.is_empty());
            }
            KeyCode::Char('u') if ctrl && !alt => {
                self.select_all.disarm();
                self.message.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl || alt => {
                // Typing over a selection replaces the whole message.
                self.take_selected_message();
                self.message.insert(self.cursor, c);
                self.cursor += 1;
            }
            _ => {}
        }
    }

    fn build_trees(&mut self) {
        let (dirs, depths, rows) =
            build_tree_model(&self.status.staged, &self.staged_collapsed_dirs);
        self.staged_dirs = dirs;
        self.staged_file_depths = depths;
        self.staged_tree_rows = rows;

        let (dirs, depths, rows) =
            build_tree_model(&self.status.unstaged, &self.changes_collapsed_dirs);
        self.changes_dirs = dirs;
        self.changes_file_depths = depths;
        self.changes_tree_rows = rows;

        // Forget folders that no longer exist, so the persisted set cannot
        // grow forever (Collapse All records every folder shown). An empty
        // section proves nothing — a repo whose status has not loaded yet
        // must not wipe the restored state — so prune only against content.
        prune_collapsed(&mut self.staged_collapsed_dirs, &self.status.staged);
        prune_collapsed(&mut self.changes_collapsed_dirs, &self.status.unstaged);
    }

    /// The repo row's branch decoration: `name*` when the tree is dirty.
    fn branch_decor(&self) -> String {
        let dirty = if self.status.staged.is_empty() && self.status.unstaged.is_empty() {
            ""
        } else {
            "*"
        };
        format!("{}{dirty}", self.status.branch)
    }
}

/// What the shared name-field handler decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldKey {
    Nothing,
    Close,
    Confirm,
    /// Ctrl+C: hand the field's text to the clipboard, keep the dialog open.
    Copy,
}

/// One text field of a dialog: caret-less editing, Ctrl+A = select all,
/// Esc drops the selection before it closes. Shared by the pane and branch
/// rename dialogs so the two cannot drift.
fn field_key(key: KeyEvent, input: &mut String, select_all: &mut SelectAll) -> FieldKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Esc if select_all.armed() => {
            select_all.disarm();
            FieldKey::Nothing
        }
        KeyCode::Esc => FieldKey::Close,
        KeyCode::Backspace => {
            if !take_selected(input, select_all) {
                input.pop();
            }
            FieldKey::Nothing
        }
        KeyCode::Char('a') if ctrl && !alt => {
            select_all.arm_if_nonempty(!input.is_empty());
            FieldKey::Nothing
        }
        KeyCode::Char('u') if ctrl && !alt => {
            select_all.disarm();
            input.clear();
            FieldKey::Nothing
        }
        // Ctrl+C used to fall into the `Char(c)` arm below and TYPE a "c" over
        // the field — with Ctrl+A armed, that replaced the whole name
        // (user-reported). It copies the field instead.
        KeyCode::Char('c') if ctrl && !alt => FieldKey::Copy,
        KeyCode::Char(c) => {
            take_selected(input, select_all);
            input.push(c);
            FieldKey::Nothing
        }
        KeyCode::Enter => FieldKey::Confirm,
        _ => FieldKey::Nothing,
    }
}

/// Drop an armed Ctrl+A (`ui::SelectAll`) by clearing `input`, and report that
/// the edit about to happen must be skipped. The rename dialog has no caret, so
/// `SelectAll::take`'s cursor is the end of the text.
fn take_selected(input: &mut String, select_all: &mut SelectAll) -> bool {
    let mut caret = input.len();
    select_all.take(input, &mut caret)
}

fn commit_draft_present<'a>(messages: impl IntoIterator<Item = &'a [char]>) -> bool {
    messages.into_iter().any(|message| !message.is_empty())
}

/// List rows; the first index on the repo-scoped variants is the repo.
#[derive(Clone, Copy)]
enum Row {
    /// Only rendered when more than one repository is visible.
    RepoHeader(usize),
    /// The repo's inline message box (3 screen lines) — multi-repo only.
    Message(usize),
    /// The repo's inline ✓ Commit button — multi-repo only.
    Commit(usize),
    StagedHeader(usize),
    ChangesHeader(usize),
    StagedDir(usize, usize),
    ChangesDir(usize, usize),
    Staged(usize, usize),
    Unstaged(usize, usize),
    DrawerHeader(Drawer),
    DrawerLine(Drawer, usize),
}

impl Row {
    /// The repository a row belongs to (drawers follow the active repo).
    fn repo(self) -> Option<usize> {
        match self {
            Row::RepoHeader(r)
            | Row::Message(r)
            | Row::Commit(r)
            | Row::StagedHeader(r)
            | Row::ChangesHeader(r)
            | Row::StagedDir(r, _)
            | Row::ChangesDir(r, _)
            | Row::Staged(r, _)
            | Row::Unstaged(r, _) => Some(r),
            Row::DrawerHeader(_) | Row::DrawerLine(..) => None,
        }
    }

    /// Keyboard navigation (j/k, wheel) skips widget rows — they are clicked,
    /// like VS Code's inputs, not list entries.
    fn selectable(self) -> bool {
        !matches!(self, Row::Message(_) | Row::Commit(_))
    }
}

/// What a context menu is about.
#[derive(Clone)]
enum MenuTarget {
    Dir {
        repo: usize,
        path: String,
        staged: bool,
    },
    File {
        repo: usize,
        entry: FileEntry,
        staged: bool,
    },
    Drawer {
        kind: Drawer,
        index: usize,
    },
}

/// The Branches drawer's menu, without the pane section.
///
/// Pure so the rules are testable: the current branch cannot be deleted; a
/// remote-tracking ref (`origin/main`) can be neither deleted nor renamed — the
/// delete used to reach git and come back as "branch not found"; and the
/// "undo the last commit" shortcut belongs to the CHECKED OUT branch only, since
/// `git reset --soft HEAD~1` acts on HEAD whatever the selection is.
fn branch_menu_entries(current: bool, local: bool) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    if current {
        entries.push(MenuEntry::Action(MenuAction::ShowRef, "Show Tip Commit"));
        entries.push(MenuEntry::Action(
            MenuAction::UndoLastCommit,
            "Undo Last Commit (soft)…",
        ));
    } else {
        entries.push(MenuEntry::Action(MenuAction::Checkout, "Checkout Branch"));
        entries.push(MenuEntry::Action(
            MenuAction::MergeInto,
            "Merge into Current Branch",
        ));
        if local {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Action(
                MenuAction::DeleteBranch,
                "Delete Branch…",
            ));
        }
    }
    if local {
        entries.push(MenuEntry::Action(
            MenuAction::RenameBranch,
            "Rename Branch…",
        ));
    }
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Action(MenuAction::CopyRef, "Copy Branch Name"));
    entries
}

/// A row that owns no action of its own, with the hint `o` / `m` show for it.
/// Section headers are keyboard stops (Enter folds them), so a user picks one up
/// by accident and then presses a file key that used to be silent.
fn section_row_hint(row: Row) -> Option<&'static str> {
    match row {
        Row::StagedHeader(_) => {
            Some("Staged Changes is a section - pick a file (j/k) for its menu")
        }
        Row::ChangesHeader(_) => Some("Changes is a section - pick a file (j/k) for its menu"),
        Row::RepoHeader(_) => Some("a repository header has no menu - pick a file"),
        Row::Message(_) | Row::Commit(_) => Some("that is the commit box - Enter/Tab to use it"),
        _ => None,
    }
}

/// Context-menu actions for file rows and drawer lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    OpenDiff,
    OpenDiffPreview,
    /// Hand the file itself to the configured editor (Fresh), not its diff.
    OpenInEditor,
    StageOrUnstage,
    Discard,
    CopyPath,
    CopyRelativePath,
    OpenExternal,
    Reveal,
    // Drawer-line actions (commits, branches, stashes, remotes, tags).
    ShowRef,
    Checkout,
    MergeInto,
    DeleteBranch,
    /// Rename the selected LOCAL branch (current or not) via an input dialog.
    RenameBranch,
    /// `git reset --soft HEAD~1` on the current branch: the commit goes, the
    /// changes stay staged.
    UndoLastCommit,
    /// Edit a commit's subject (commit rows on HEAD's history only); the body is kept.
    RewordCommit,
    CherryPick,
    Revert,
    ResetHere,
    StashApply,
    StashPop,
    StashDrop,
    FetchRemote,
    CopyRef,
    DeleteTag,
    RemoveWorktree,
    /// herdr's own pane-menu item, offered at the end of every menu.
    Herdr(PaneAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangesHeaderAction {
    Discard,
    Stash,
    Stage,
}

impl ChangesHeaderAction {
    fn footer_hint(self) -> &'static str {
        match self {
            Self::Discard => "↶ Discard All Changes",
            Self::Stash => "⇩ Stash Changes",
            Self::Stage => "+ Stage All Changes",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileHoverAction {
    Open,
    Discard,
    Stage,
    Unstage,
}

impl FileHoverAction {
    fn glyph(self) -> &'static str {
        match self {
            Self::Open => "↗",
            Self::Discard => "↶",
            Self::Stage => "+",
            Self::Unstage => "−",
        }
    }

    fn footer_hint(self) -> &'static str {
        match self {
            Self::Open => "↗ Open in Editor",
            Self::Discard => "↶ Discard Changes",
            Self::Stage => "+ Stage Changes",
            Self::Unstage => "− Unstage Changes",
        }
    }
}

#[derive(Clone, Copy)]
enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

/// A modal layered over the list; while open it owns keyboard and mouse input.
enum Overlay {
    BranchPicker(BranchPicker),
    Menu {
        x: u16,
        y: u16,
        target: MenuTarget,
        entries: Vec<MenuEntry>,
        selected: usize,
        rect: Rect,
    },
    ConfirmDiscard {
        repo: usize,
        entry: FileEntry,
    },
    ConfirmDiscardAll {
        repo: usize,
    },
    /// A y/N dialog guarding a destructive git command (reset, delete, drop).
    ConfirmGit {
        repo: usize,
        title: String,
        prompt: String,
        args: Vec<String>,
    },
    /// The commit button's "pull first?" question: the branch is `behind` its
    /// upstream and there is something staged. `y` pulls (plain `git pull`) and
    /// then commits; any other key commits right away, as before.
    ConfirmPullThenCommit {
        repo: usize,
        behind: usize,
    },
    /// The ⚙ settings modal: mouse-toggleable panel settings.
    Settings {
        selected: usize,
        rect: Rect,
        scroll: usize,
    },
    /// The name typed for our own pane ("Rename pane…"), edited in a dialog.
    PaneRename {
        input: String,
        /// Ctrl+A: the next edit replaces the whole name (`ui::SelectAll`).
        select_all: SelectAll,
    },
    /// A new name for a local branch ("Rename Branch…"), prefilled with the old
    /// one so a typo fix does not need retyping the whole name.
    BranchRename {
        repo: usize,
        from: String,
        input: String,
        select_all: SelectAll,
    },
    /// The subject of a commit's message ("Edit Commit Message…"). `body` is the
    /// rest of the message, kept verbatim; only the subject is edited.
    CommitReword {
        repo: usize,
        commit: String,
        body: String,
        input: String,
        select_all: SelectAll,
    },
}

/// One row of the Settings modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
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
    AutoOpen,
    StrictToggle,
    FocusOnOpen,
    FollowCwd,
    GitDecorations,
    GitFooter,
    Hotkeys,
    Folder,
    ScmViewMode,
}

/// (setting, label, current value, enabled) — disabled rows render dimmed and
/// don't toggle.
type SettingRow = (Setting, &'static str, String, bool);

/// Smallest row index `top` such that the rows from `top` to the end fit in
/// `height` lines. Returns the last index when even that row alone is taller
/// than `height`, and 0 when every row fits.
fn max_scroll_for(heights: &[usize], height: usize) -> usize {
    let mut used = 0usize;
    let mut top = heights.len();
    for (i, &h) in heights.iter().enumerate().rev() {
        if used + h > height {
            break;
        }
        used += h;
        top = i;
    }
    if top == heights.len() {
        return heights.len().saturating_sub(1);
    }
    top
}

/// Where the list body was drawn last frame, for mouse hit-testing.
#[derive(Clone, Copy, Default)]
struct BodyGeom {
    top: u16,
    height: u16,
    offset: usize,
}

fn row_hit_with_heights(
    body: BodyGeom,
    mouse_row: u16,
    rows: impl IntoIterator<Item = (usize, u16)>,
) -> Option<(usize, u16)> {
    if mouse_row < body.top || mouse_row >= body.top + body.height {
        return None;
    }
    let mut y = body.top;
    for (index, height) in rows {
        if mouse_row < y + height {
            return Some((index, mouse_row - y));
        }
        y += height;
    }
    None
}

/// Clickable regions of the activity bar / header / message box, from the
/// last draw.
#[derive(Clone, Copy, Default)]
struct ClickZones {
    activity_row: u16,
    explorer: (u16, u16),
    search: (u16, u16),
    source_control: (u16, u16),
    /// The ⚙ button (activity bar in unified mode, header otherwise).
    gear: Rect,
    message: Rect,
    sparkle: Rect,
    button: Rect,
    /// The Sync Changes row (zero-sized when hidden).
    sync: Rect,
    /// Branch text in the Source Control header.
    header_branch: Rect,
    /// Yes / No buttons of the y/N confirm (zero-sized when none is open).
    confirm_yes: Rect,
    confirm_no: Rect,
    git_footer: FooterZones,
}

/// Handle for identity/label control of our own pane over the socket API.
struct PaneCtl {
    pane_id: String,
    /// The pane's tab, so a split ratio can be set on the whole tab rather than
    /// on whichever pane happens to be focused.
    tab_id: Option<String>,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty())?;
        Some(Self {
            pane_id,
            tab_id: std::env::var("HERDR_TAB_ID")
                .ok()
                .filter(|id| !id.is_empty()),
        })
    }

    /// Set or clear the pane label — cleared while collapsed so the sliver
    /// has no border title.
    fn set_label(&self, label: Option<&str>) {
        let mut params = serde_json::json!({ "pane_id": self.pane_id });
        if let Some(label) = label {
            params["label"] = serde_json::Value::String(label.to_string());
        }
        let _ = herdr_sidebar::ipc::call_text("pane.rename", params);
    }

    /// Resize our pane to `target` terminal columns over the socket API
    /// (`pane.resize` takes a split-RATIO delta; the plan converts columns).
    fn resize_to(&self, current: u16, target: u16, dock_right: bool) {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) =
            herdr_sidebar::launch::resize_plan(&layout, &self.pane_id, current, target, dock_right)
        else {
            return;
        };
        let _ = herdr_sidebar::ipc::call_text(
            "pane.resize",
            serde_json::json!({
                "pane_id": self.pane_id,
                "direction": step.direction,
                "amount": step.amount,
            }),
        );
    }

    fn resize_preferred(&self, current: u16, target: u16, dock_right: bool) {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) = herdr_sidebar::launch::preferred_resize_plan(
            &layout,
            &self.pane_id,
            current,
            target,
            dock_right,
        ) else {
            return;
        };
        let _ = herdr_sidebar::ipc::call_text(
            "pane.resize",
            serde_json::json!({
                "pane_id": self.pane_id,
                "direction": step.direction,
                "amount": step.amount,
            }),
        );
    }

    /// Raw `pane.layout` JSON for this pane (one IPC round trip per resize).
    fn layout(&self) -> Option<String> {
        herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        )
        .ok()
    }

    /// Rows the bottom pane of this tab's editor/terminal column occupies right
    /// now — the seed for the first `←`/`→` on an "auto" Terminal height.
    fn terminal_rows_now(&self) -> Option<u16> {
        let layout = self.layout()?;
        let (_, ratio, height) = herdr_sidebar::launch::foreign_down_split(&layout, &self.pane_id)?;
        let rows = (1.0 - ratio) * height as f64;
        Some(rows.round().max(0.0) as u16)
    }

    /// Size this tab's editor/terminal split so its BOTTOM pane is `rows` rows
    /// tall. False when the tab has no such split (nothing to size), so the
    /// caller can say so instead of looking like it worked.
    fn set_terminal_rows(&self, rows: u16) -> bool {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return false;
        };
        let Some((path, _ratio, height)) =
            herdr_sidebar::launch::foreign_down_split(&layout, &self.pane_id)
        else {
            return false;
        };
        self.apply_split_ratio(path, herdr_sidebar::launch::ratio_for_rows(rows, height))
    }

    /// Columns the Agent column of this tab occupies right now — the seed for
    /// the first `←`/`→` on an "auto" Agent width.
    fn agent_cols_now(&self) -> Option<u16> {
        let layout = self.layout()?;
        let (_, ratio, width) = herdr_sidebar::launch::foreign_right_split(&layout, &self.pane_id)?;
        let cols = (1.0 - ratio) * width as f64;
        Some(cols.round().max(0.0) as u16)
    }

    /// Size this tab's Agent column so it is `cols` columns wide. False when the
    /// tab has no such split (nothing to size), so the caller can say so
    /// instead of looking like it worked.
    fn set_agent_cols(&self, cols: u16) -> bool {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return false;
        };
        let Some((path, _ratio, width)) =
            herdr_sidebar::launch::foreign_right_split(&layout, &self.pane_id)
        else {
            return false;
        };
        self.apply_split_ratio(path, herdr_sidebar::launch::ratio_for_cols(cols, width))
    }

    /// Set one of this tab's splits to `ratio`. False when the host refused it
    /// (call_text returns the reply line even for an error response).
    fn apply_split_ratio(&self, path: Vec<bool>, ratio: f64) -> bool {
        let mut params = serde_json::json!({ "path": path, "ratio": ratio });
        // Prefer the tab form; a launch that did not tell us its tab still
        // resolves the same tab from the pane.
        match &self.tab_id {
            Some(tab) => params["tab_id"] = serde_json::Value::String(tab.clone()),
            None => params["pane_id"] = serde_json::Value::String(self.pane_id.clone()),
        }
        match herdr_sidebar::ipc::call_text("layout.set_split_ratio", params) {
            Ok(reply) => {
                serde_json::from_str::<serde_json::Value>(reply.trim_start_matches('\u{feff}'))
                    .map(|value| value.get("error").is_none())
                    .unwrap_or(false)
            }
            Err(_) => false,
        }
    }

    /// Report identity tokens: always our own; in merged mode also the other
    /// view's (one Sidebar pane satisfies both plugins' launchers), otherwise
    /// clear the other view's token.
    fn report_tokens(&self, my: View, merged: bool) {
        herdr_sidebar::ipc::report_identity(&self.pane_id, my, merged);
    }
}

pub struct App {
    /// Every repository visible from the cwd (VS Code style: the containing
    /// repo plus child repos). Empty = "not a git repository".
    repos: Vec<Repo>,
    /// Why discovery came up empty, for the placeholder screen.
    discover_err: String,
    /// The repo the commit box / drawers / sync act on: the one the selection
    /// is in.
    active: usize,
    cwd: PathBuf,
    rows: Vec<Row>,
    /// Explicit selection — `None` until the user picks a row (nothing is
    /// highlighted by default; hover stays subtle).
    selected: Option<usize>,
    /// View scroll offset in ROWS, independent of the selection.
    scroll: usize,
    /// Bring the selection into view on the next draw (keyboard nav only).
    snap: bool,
    focus: Focus,
    theme: IconTheme,
    drawers: [DrawerPanel; 8],
    /// The file the FILE HISTORY drawer follows: the last selected file row.
    history_target: Option<String>,
    /// One-shot footer notice: (text, is_error). Cleared on the next key press.
    flash: Option<(String, bool)>,
    /// Pending ✧ commit-message generation, polled from tick().
    suggesting: Option<Receiver<String>>,
    /// Pending Sync Changes run, polled from tick().
    syncing: Option<(usize, Receiver<Result<String, String>>)>,
    /// Pending "pull, then commit" run, polled from tick(); holds the repo to
    /// commit in once the pull lands.
    pulling: Option<(usize, Receiver<Result<String, String>>)>,
    /// AI commit claimed from this tab's request file: the repo index and the
    /// worker's outcome (see [`herdr_sidebar::ai_commit`]).
    ai_commit: Option<(usize, std::sync::mpsc::Receiver<AiOutcome>)>,
    /// Last time this sidebar looked for an AI commit request (throttle).
    ai_commit_checked: Option<std::time::Instant>,
    /// When the claimed AI commit started; drives the button animation.
    ai_commit_started: Option<std::time::Instant>,
    /// The tab this sidebar lives in; AI commit requests are keyed by it.
    my_tab: Option<String>,
    overlay: Option<Overlay>,
    hovered: Option<usize>,
    body: BodyGeom,
    zones: ClickZones,
    /// The hover title-bar buttons' click zones from the last draw (empty
    /// while they are hidden).
    title_zones: Vec<(Rect, TitleAction)>,
    /// When the mouse last moved/clicked/scrolled over this pane — the hover
    /// approximation that shows the title-bar buttons.
    last_mouse: Option<std::time::Instant>,
    /// Last known mouse position, for the button hover highlight.
    mouse_pos: Option<(u16, u16)>,
    /// Last left-click (row index, when) for double-click detection.
    last_click: Option<(usize, std::time::Instant)>,
    /// Where the most recent preview landed, with its document key — so a
    /// double click pins that exact tab instead of re-opening it (mirrors the
    /// file explorer's pin).
    last_preview: Option<(String, herdr_sidebar::viewer::PreviewTarget)>,
    /// Remaining conflict blocks per conflicted (`!`) working-tree file, keyed
    /// by absolute path. NOT git state: resolving every block leaves the file
    /// unmerged until it is staged, so this is re-read on each refresh and
    /// decides between `!N` and `✓` in the row's marker.
    conflict_counts: HashMap<PathBuf, usize>,
    page: usize,
    last_width: u16,
    /// Previous `pane.layout` snapshot. Divider-only resizes leave the tab area
    /// width unchanged and only move this pane's own edge, which is what
    /// [`herdr_sidebar::launch::dragged_width`] compares against.
    last_layout: Option<String>,
    /// A divider drag observed in [`Self::on_resize`], waiting for
    /// [`Self::flush_pending_width`] to adopt it as the width setting.
    pending_width: Option<(u16, std::time::Instant)>,
    last_height: u16,
    // Merged-sidebar state.
    sidebar_state: sidebar::State,
    other_exe: Option<PathBuf>,
    pane_ctl: Option<PaneCtl>,
    /// Our pane carries a label. `apply_identity` sets one; "Clear pane name"
    /// unsets it (the pane menu offers that item only while this holds).
    pane_named: bool,
    /// Last heartbeat stamp, throttling the token refresh.
    last_beat: std::time::Instant,
    /// Reports which agent runs in each herdr space for the spaces rail's
    /// `$agents` line (own 5 s throttle; see [`herdr_sidebar::agent_tokens`]).
    agent_tokens: herdr_sidebar::agent_tokens::AgentTokens,
    /// Keeps a space on `working` while a pi agent's BACKGROUND subagent runs
    /// (pi's own hook reports `idle` when its turn settles). Own 3 s throttle;
    /// see [`herdr_sidebar::pi_state`].
    pi_state: herdr_sidebar::pi_state::PiState,
    /// A native folder picker running on a background thread; its result
    /// arrives here (None = cancelled).
    picking: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
    /// Shared across rebuilds and unified-view switches so a manual folder
    /// choice keeps its precedence until a known neighbour actually moves.
    cwd_follower: std::rc::Rc<std::cell::RefCell<herdr_sidebar::launch::CwdFollower>>,
    /// Draft roots this pane loaded or successfully persisted. An empty box
    /// only clears one of these roots, so a stale sibling pane cannot erase a
    /// newer draft it never observed.
    persisted_draft_roots: std::collections::BTreeSet<String>,
    pending_unified_width: Option<(u16, std::time::Instant)>,
    pub tree_view: bool,
    /// Keep polling Git status while unfocused ([`sidebar::BACKGROUND_REFRESH_ENV`]).
    background_refresh: bool,
    /// When `git status` was last read (focus-gated, see `tick`).
    last_git_read: Option<std::time::Instant>,
    /// Seen focused on the previous tick: a false -> true transition refreshes
    /// at once.
    was_focused: bool,
    /// OS file-change watcher over the repos' work trees and git dirs
    /// (event-driven refresh, runs regardless of focus). `None` until started.
    fs_watch: Option<FsWatch>,
    /// The set `fs_watch` was last given, so unchanged repos cost nothing.
    fs_watch_set: Option<WatchSet>,
    /// The watcher could not be created; focused polling is the only refresh.
    fs_watch_failed: bool,
}

const MY_VIEW: View = View::SourceControl;

impl App {
    pub fn new(
        cwd: PathBuf,
        cwd_follower: std::rc::Rc<std::cell::RefCell<herdr_sidebar::launch::CwdFollower>>,
    ) -> Self {
        let mut repos: Vec<Repo> = Git::discover_all(&cwd).into_iter().map(Repo::new).collect();
        let discover_err = if repos.is_empty() {
            Git::discover(&cwd)
                .err()
                .unwrap_or_else(|| "no repositories found".to_string())
        } else {
            String::new()
        };
        let theme = IconTheme::resolve(
            std::env::var("HERDR_SIDEBAR_ICONS")
                .or_else(|_| std::env::var("HERDR_AA_GIT_ICONS"))
                .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                .ok()
                .as_deref(),
            sidebar::load_state().icons,
        );
        // The other view ships in this same binary — always available.
        let other_exe = std::env::current_exe().ok();
        let sidebar_state = sidebar::load_state();
        set_color_theme(sidebar_state.color_theme);
        let pane_ctl = PaneCtl::from_env();
        let last_layout = pane_ctl.as_ref().and_then(PaneCtl::layout);

        // Mirror the SCM view the user was already looking at: a sidebar docked
        // into a brand-new preview tab starts with the same drawers expanded,
        // the same repo active, and the same row selected. Parallel to the
        // explorer's tree-state restore.
        let saved = sidebar::load_scm_state(&cwd);
        let persisted_draft_roots = saved.drafts.keys().cloned().collect();
        // One source of truth: the global setting, so every pane and every
        // folder agrees (a per-folder copy used to override it forever).
        let tree_view = sidebar_state.scm_tree_view;
        let mut drawers: [DrawerPanel; 8] = Default::default();
        for kind in Drawer::ALL {
            drawers[kind.index()].expanded = saved.drawers.iter().any(|d| d == kind.title());
        }
        let active = saved
            .active_root
            .as_deref()
            .and_then(|root| {
                let saved_root = sidebar::scm_path_key(std::path::Path::new(root));
                repos
                    .iter()
                    .position(|r| sidebar::scm_path_key(r.git.root()) == saved_root)
            })
            .unwrap_or(0);
        let history_target = saved.history_target;
        let selected_id = saved.selected;
        let saved_scroll = saved.scroll;
        for repo in &mut repos {
            let root = sidebar::scm_path_key(repo.git.root());
            if let Some(message) = saved.drafts.get(&root) {
                repo.message = message.chars().collect();
                repo.cursor = repo.message.len();
            }
            for dir in &saved.collapsed_dirs {
                if let Ok((root, staged, path)) =
                    serde_json::from_str::<(String, bool, String)>(dir)
                    && root == repo.git.root().to_string_lossy()
                {
                    if staged {
                        repo.staged_collapsed_dirs.insert(path);
                    } else {
                        repo.changes_collapsed_dirs.insert(path);
                    }
                }
            }
        }

        // Sidebar panes inherit HERDR_TAB_ID; otherwise look our tab up once.
        let my_tab = std::env::var("HERDR_TAB_ID")
            .ok()
            .filter(|t| !t.is_empty())
            .or_else(|| {
                let pane_id = std::env::var("HERDR_PANE_ID")
                    .ok()
                    .filter(|p| !p.is_empty())?;
                let list =
                    herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
                let tab = herdr_sidebar::launch::tab_of(&list, &pane_id);
                (!tab.is_empty()).then_some(tab)
            });

        let mut app = Self {
            repos,
            discover_err,
            active,
            cwd,
            rows: Vec::new(),
            selected: None,
            scroll: 0,
            snap: false,
            focus: Focus::List,
            theme,
            drawers,
            history_target,
            flash: None,
            suggesting: None,
            syncing: None,
            pulling: None,
            ai_commit: None,
            ai_commit_checked: None,
            ai_commit_started: None,
            my_tab,
            overlay: None,
            hovered: None,
            body: BodyGeom::default(),
            zones: ClickZones::default(),
            title_zones: Vec::new(),
            last_mouse: None,
            mouse_pos: None,
            last_click: None,
            last_preview: None,
            conflict_counts: HashMap::new(),
            page: 20,
            last_width: sidebar_state.sidebar_width,
            last_layout,
            pending_width: None,
            last_height: 24,
            sidebar_state,
            other_exe,
            pane_ctl,
            pane_named: true,
            last_beat: std::time::Instant::now(),
            agent_tokens: Default::default(),
            pi_state: Default::default(),
            picking: None,
            cwd_follower,
            persisted_draft_roots,
            pending_unified_width: None,
            tree_view,
            background_refresh: sidebar::background_refresh_enabled(
                std::env::var(sidebar::BACKGROUND_REFRESH_ENV)
                    .ok()
                    .as_deref(),
            ),
            last_git_read: None,
            was_focused: false,
            fs_watch: None,
            fs_watch_set: None,
            fs_watch_failed: false,
        };
        app.apply_identity();
        app.refresh();
        // Rows are built now: re-find the selected row by its stable id and
        // restore the scroll the user left it on.
        if let Some(id) = selected_id
            && let Some(i) = app.find_row_by_stable_id(&id)
        {
            app.selected = Some(i);
        }
        app.scroll = saved_scroll.min(app.rows.len().saturating_sub(1));
        app
    }

    pub fn root_path(&self) -> &Path {
        &self.cwd
    }

    fn active_repo(&self) -> Option<&Repo> {
        self.repos.get(self.active)
    }

    fn active_repo_mut(&mut self) -> Option<&mut Repo> {
        let i = self.active;
        self.repos.get_mut(i)
    }

    /// More than one repo: VS Code-style per-repo inline inputs in the list.
    fn multi(&self) -> bool {
        self.repos.len() > 1
    }

    /// The merged sidebar is on and actually usable (other plugin present).
    fn merged(&self) -> bool {
        self.sidebar_state.merged && self.other_exe.is_some()
    }

    /// Push our label + metadata tokens to herdr for the current mode.
    fn apply_identity(&mut self) {
        self.pane_named = true;
        let Some(ctl) = &self.pane_ctl else { return };
        let label = if self.merged() {
            sidebar::SIDEBAR_LABEL
        } else {
            MY_VIEW.label()
        };
        ctl.set_label(Some(label));
        ctl.report_tokens(MY_VIEW, self.merged());
    }

    pub fn clear_identity(&self) {
        if let Some(ctl) = &self.pane_ctl {
            herdr_sidebar::ipc::clear_identity(&ctl.pane_id);
        }
    }

    /// Hide the sidebar: snooze this tab (so the quiet ensure hook doesn't
    /// immediately re-dock a fresh one) and close our own pane. The herdr
    /// prefix+b keybinding (→ the toggle action) brings it back.
    fn hide(&mut self) {
        self.close(true);
    }

    fn close(&mut self, snooze: bool) {
        // Closing our own pane kills this process: save a pending remembered
        // root first (the outer loop's flush never runs on this path).
        crate::flush_pending_root();
        // A direct pane close kills the process without a Drop/signal hook.
        // Persist drafts first; failure keeps the live pane open with the
        // existing error notice from persist_scm().
        if !self.persist_scm() {
            return;
        }
        let Some(ctl) = &self.pane_ctl else { return };
        if snooze {
            let tab = match herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) {
                Ok(json) => herdr_sidebar::launch::tab_of(&json, &ctl.pane_id),
                Err(e) => {
                    self.flash = Some((format!("hide failed: {e}"), true));
                    return;
                }
            };
            if tab.is_empty() {
                self.flash = Some(("hide failed: could not resolve this tab".into(), true));
                return;
            }
            // Set the marker BEFORE closing: if it fails, closing anyway
            // would let the very next focus event re-dock a sidebar the
            // user just asked to hide.
            if let Err(e) = herdr_sidebar::snooze::set(&herdr_sidebar::snooze::dir(), &tab) {
                self.flash = Some((format!("hide failed: {e}"), true));
                return;
            }
        }
        let _ = herdr_sidebar::ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": ctl.pane_id }),
        );
    }

    /// How often the event loop calls [`Self::tick`]: a uniform, git-free probe
    /// cadence. The `git status` cadence is decided INSIDE `tick`
    /// ([`FOCUSED_REFRESH`]) so that gaining focus can refresh at once instead of
    /// waiting out a slow interval that has just become the active one.
    pub fn tick_interval(&self) -> std::time::Duration {
        IDLE_TICK
    }

    /// Re-read every repo's git status (this is the change auto-detection —
    /// tick() calls it every [`Self::refresh_interval`]); keeps the flash so
    /// periodic ticks don't eat notices.
    ///
    /// The list is only rebuilt when a status actually CHANGED, which is what
    /// makes the fast focused cadence affordable: an idle poll costs one `git
    /// status` per repo and nothing else.
    pub fn refresh(&mut self) {
        let mut error = None;
        let mut changed = false;
        for repo in &mut self.repos {
            match repo.git.status() {
                Ok(status) => {
                    if status != repo.status {
                        repo.status = status;
                        changed = true;
                    }
                }
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            self.flash = Some((e, true));
        }
        // Git's status cannot see a resolution: a fully resolved file stays
        // `UU` until it is staged, so the marker is re-read every refresh.
        let counts_changed = self.refresh_conflict_counts();
        if !changed && !counts_changed {
            return;
        }
        self.reload_expanded_drawers();
        self.rebuild();
    }

    /// Re-read the drawers and rebuild the list even when `git status` did not
    /// change. Ref-changing operations (branch delete / rename, checkout, reset,
    /// merge) leave the working tree exactly as it was, so `refresh()` alone
    /// returned early and the Branches drawer kept listing a branch that no
    /// longer existed (found live: `✓ deleted feature`, `feature` still on
    /// screen).
    fn refresh_refs(&mut self) {
        self.reload_expanded_drawers();
        self.rebuild();
    }

    /// Recount the conflict blocks left in every conflicted (`!`) file and
    /// return whether any of them changed. Cheap: only conflicted entries are
    /// read, on the refresh path (2 s while focused), never per frame.
    fn refresh_conflict_counts(&mut self) -> bool {
        let mut counts = HashMap::new();
        for repo in &self.repos {
            for entry in &repo.status.unstaged {
                if entry.letter != '!' {
                    continue;
                }
                let path = repo.git.root().join(&entry.path);
                if let Some(blocks) = herdr_sidebar::git::count_conflict_blocks(&path) {
                    counts.insert(path, blocks);
                }
            }
        }
        if counts == self.conflict_counts {
            return false;
        }
        self.conflict_counts = counts;
        true
    }

    /// Conflict blocks still in `entry`'s file, or `None` when it is not a
    /// conflicted row (or its file could not be scanned).
    fn conflicts_for(&self, repo: usize, entry: &FileEntry) -> Option<usize> {
        if entry.letter != '!' {
            return None;
        }
        let path = self.repos.get(repo)?.git.root().join(&entry.path);
        self.conflict_counts.get(&path).copied()
    }

    /// Columns the row's right-aligned marker occupies (`!3` is 2, `✓` is 1).
    /// The hover buttons are laid out to its left, so hit-testing needs it.
    fn marker_width_of(&self, repo: usize, staged: bool, index: usize) -> u16 {
        let entry = self.repos.get(repo).and_then(|r| {
            if staged {
                r.status.staged.get(index)
            } else {
                r.status.unstaged.get(index)
            }
        });
        let Some(entry) = entry else { return 1 };
        let width = status_marker(entry.letter, self.conflicts_for(repo, entry))
            .0
            .chars()
            .count();
        u16::try_from(width).unwrap_or(1)
    }

    /// The branch-moved probe: cheap (one small file per repo), so the event
    /// loop can run it several times a second. A popup (own process) or a
    /// `git switch` in a terminal rewrites the git dir's HEAD, and the view then
    /// refreshes at once instead of waiting for the next [`FOCUSED_REFRESH`]
    /// status poll. Returns true when it triggered a refresh.
    pub fn poll_branch(&mut self) -> bool {
        let mut moved = false;
        for repo in &mut self.repos {
            let now = repo.git.head_signature();
            if now != repo.head_sig {
                repo.head_sig = now;
                moved = true;
            }
        }
        if moved {
            self.refresh();
        }
        moved
    }
    /// Re-stamp the identity tokens so launchers know this pane is alive.
    pub fn heartbeat(&mut self) {
        self.poll_unified_close();
        if self.last_beat.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_beat = std::time::Instant::now();
        if let Some(ctl) = &self.pane_ctl {
            ctl.report_tokens(MY_VIEW, self.merged());
        }
        // The spaces rail's `$agents` line. Self-throttled and failure-tolerant.
        self.agent_tokens.report_if_due();
        // The first tab's label follows the checked-out branch (own 5 s
        // throttle; elected session-wide like the rail reporters).
        herdr_sidebar::tab_title::sync_if_due();
        // pi's hook goes idle while a background subagent still runs: report
        // `working` for it from the persisted transcript.
        self.pi_state.report_if_due();
        self.follow_sibling_cwd();
    }

    fn poll_unified_close(&mut self) -> bool {
        let Some((target, started)) = self.pending_unified_width else {
            return false;
        };
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            self.pending_unified_width = None;
            return false;
        };
        let Ok(json) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) else {
            return false;
        };
        if sibling_panes_of(&json, &pane_id, MY_VIEW.other()).is_empty() {
            self.pending_unified_width = None;
            if let Some(ctl) = &self.pane_ctl {
                ctl.resize_to(self.last_width, target, self.sidebar_state.dock_right);
            }
            return true;
        }
        if started.elapsed() >= std::time::Duration::from_secs(2) {
            self.pending_unified_width = None;
            self.sidebar_state = sidebar::update_state(|state| {
                state.merged = false;
                state.active = MY_VIEW;
            });
            self.apply_identity();
            self.flash = Some(("Explorer stayed open; unified mode cancelled".into(), true));
        }
        false
    }

    fn pane_is_focused(&self) -> bool {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.as_str()) else {
            return true;
        };
        herdr_sidebar::ipc::pane_focused(pane_id).unwrap_or(false)
    }

    fn follow_sibling_cwd(&mut self) {
        if !self.sidebar_state.follow_cwd
            || self.overlay.is_some()
            || self.picking.is_some()
            || self.suggesting.is_some()
            || self.syncing.is_some()
            || self.is_ai_commit_busy()
            || commit_draft_present(self.repos.iter().map(|repo| repo.message.as_slice()))
        {
            return;
        }
        let Some(ctl) = &self.pane_ctl else { return };
        let Ok(panes) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) else {
            return;
        };
        let target = self
            .cwd_follower
            .borrow_mut()
            .next_cwd(&panes, &ctl.pane_id, &self.cwd);
        let Some(target) = target else { return };
        let target = PathBuf::from(target);
        if target == self.cwd || !target.is_dir() || std::env::set_current_dir(&target).is_err() {
            return;
        }
        let root = std::env::current_dir().unwrap_or(target);
        let cwd_follower = std::rc::Rc::clone(&self.cwd_follower);
        *self = App::new(root, cwd_follower);
    }

    /// Periodic timer tick: retry repo discovery if we started outside one,
    /// pick up external changes, and collect finished ✧ suggestion / sync runs.
    pub fn tick(&mut self) {
        self.collect_editor_failures();
        self.collect_fs_watch();
        let shared = sidebar::load_state();
        self.sidebar_state.custom_editor_on_click = shared.custom_editor_on_click;
        self.sidebar_state.custom_editor_inline = shared.custom_editor_inline;
        self.sidebar_state.git_deco = shared.git_deco;
        self.sidebar_state.show_git_footer = shared.show_git_footer;
        self.sidebar_state.dock_right = shared.dock_right;
        self.sidebar_state.strict_toggle = shared.strict_toggle;
        self.sidebar_state.focus_on_open = shared.focus_on_open;
        if shared.scm_tree_view != self.tree_view {
            // Toggled from another pane (separated view, a preview tab).
            self.tree_view = shared.scm_tree_view;
            self.sidebar_state.scm_tree_view = shared.scm_tree_view;
            self.rebuild();
        }
        if shared.color_theme != self.sidebar_state.color_theme {
            self.sidebar_state.color_theme = shared.color_theme;
            set_color_theme(shared.color_theme);
        }
        if shared.sidebar_width != self.sidebar_state.sidebar_width {
            self.sidebar_state.sidebar_width = shared.sidebar_width;
            if let Some(ctl) = &self.pane_ctl {
                ctl.resize_preferred(self.last_width, shared.sidebar_width, shared.dock_right);
            }
        }
        self.poll_ai_commit();
        if let Some(rx) = &self.suggesting {
            match rx.try_recv() {
                Ok(message) => {
                    if let Some(repo) = self.active_repo_mut() {
                        // A fresh suggestion replaces the draft, so an armed
                        // Ctrl+A no longer describes what is on screen.
                        repo.select_all.disarm();
                        repo.message = message.chars().collect();
                        repo.cursor = repo.message.len();
                    }
                    self.focus = Focus::Message;
                    self.flash = Some(("✧ suggestion ready — edit or ⏎ to commit".into(), false));
                    self.suggesting = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.flash = Some(("✧ generation failed".into(), true));
                    self.suggesting = None;
                }
            }
        }
        if let Some((_, rx)) = &self.syncing {
            match rx.try_recv() {
                Ok(Ok(_)) => {
                    self.syncing = None;
                }
                Ok(Err(e)) => {
                    self.flash = Some((e, true));
                    self.syncing = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.flash = Some(("sync failed".into(), true));
                    self.syncing = None;
                }
            }
        }
        if let Some((_, rx)) = &self.pulling {
            let landed = match rx.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("pull failed".to_string()))
                }
            };
            if let Some(outcome) = landed
                && let Some((index, _)) = self.pulling.take()
            {
                match outcome {
                    Ok(summary) => {
                        self.flash = Some((summary, false));
                        // The commit the button was clicked for, now on top of
                        // the pulled branch.
                        self.refresh();
                        self.commit_repo(index);
                    }
                    Err(error) => self.flash = Some((error, true)),
                }
            }
        }
        // No `git status` at all while this pane is unfocused: one session holds a
        // sidebar per IDE tab, and polling them all multiplied git processes. The
        // collections above still run at the loop cadence (finished AI-commit /
        // sync / suggestion work is never stranded).
        let focused = self.background_refresh || self.pane_is_focused();
        if !focused {
            self.was_focused = false;
            return;
        }
        // Gaining focus reads immediately (the loop only probes every
        // [`IDLE_TICK`], so this is what makes "click the sidebar" feel instant);
        // while focused it is [`FOCUSED_REFRESH`].
        let gained_focus = !self.was_focused;
        self.was_focused = true;
        if !gained_focus
            && self
                .last_git_read
                .is_some_and(|at| at.elapsed() < FOCUSED_REFRESH)
        {
            return;
        }
        self.last_git_read = Some(std::time::Instant::now());
        if self.repos.is_empty() {
            self.repos = Git::discover_all(&self.cwd)
                .into_iter()
                .map(Repo::new)
                .collect();
            if !self.repos.is_empty() {
                self.discover_err.clear();
            }
        }
        self.refresh();
    }

    /// An inline custom editor that died on the spot. Its stdio is null, so
    /// without this the click reads as a silent success (the launch itself was
    /// only a spawn: the editor's exit status arrives later).
    fn collect_editor_failures(&mut self) {
        if let Some(failure) = actions::take_editor_failures().pop() {
            self.flash = Some((failure, true));
        }
    }

    /// The repos' work trees and git dirs, as the file-change watcher sees them.
    fn fs_watch_set(&self) -> WatchSet {
        WatchSet {
            tree_dirs: Vec::new(),
            repo_roots: self
                .repos
                .iter()
                .map(|r| r.git.root().to_path_buf())
                .collect(),
            git_dirs: self.repos.iter().filter_map(|r| r.git.git_dir()).collect(),
        }
    }

    /// Keep the watcher on the current repos. Starts it on first use and only
    /// calls `update` when the set actually changed.
    fn sync_fs_watch(&mut self) {
        if self.fs_watch_failed {
            return;
        }
        let set = self.fs_watch_set();
        if self.fs_watch_set.as_ref() == Some(&set) {
            return;
        }
        match &self.fs_watch {
            Some(watch) => watch.update(set.clone()),
            None => match FsWatch::start(set.clone()) {
                Some(watch) => self.fs_watch = Some(watch),
                None => {
                    self.fs_watch_failed = true;
                    return;
                }
            },
        }
        self.fs_watch_set = Some(set);
    }

    /// Event-driven refresh, ahead of the focus gate in `tick` so unfocused
    /// views stay current too. Idle (no git process) while nothing changes. A
    /// ref-changing git operation (commit, checkout, merge) rewrites git-dir
    /// files, so those events also reload the drawers.
    fn collect_fs_watch(&mut self) {
        self.sync_fs_watch();
        let Some(watch) = &self.fs_watch else { return };
        let changes = watch.take();
        if changes.tree || changes.git {
            self.refresh();
        }
        if changes.refs {
            self.refresh_refs();
        }
    }

    pub fn is_syncing(&self) -> bool {
        self.syncing.is_some() || self.pulling.is_some()
    }

    /// AI commit is generating; the event loop polls faster while this holds.
    pub fn is_ai_commit_busy(&self) -> bool {
        self.ai_commit.is_some()
    }

    pub fn on_resize(&mut self, width: u16) {
        self.last_width = width;
        let unified_close_completed = self.poll_unified_close();
        if let Some(ctl) = &self.pane_ctl {
            let layout = ctl.layout();
            let area_now = layout
                .as_deref()
                .and_then(herdr_sidebar::launch::layout_width);
            let surrounding_changed = self
                .last_layout
                .as_deref()
                .and_then(herdr_sidebar::launch::layout_width)
                .zip(area_now)
                .is_some_and(|(before, now)| before != now);
            // Set on EVERY resize event: a candidate left over from an
            // earlier drag must not survive a real layout change.
            self.pending_width = if unified_close_completed || surrounding_changed {
                None
            } else {
                self.last_layout
                    .as_deref()
                    .zip(layout.as_deref())
                    .and_then(|(before, after)| {
                        herdr_sidebar::launch::dragged_width(before, after, &ctl.pane_id, width)
                    })
                    .map(|cols| (cols, std::time::Instant::now()))
            };
            if let Some(snapshot) = layout {
                self.last_layout = Some(snapshot);
            }
            if !unified_close_completed && surrounding_changed {
                ctl.resize_preferred(
                    width,
                    self.sidebar_state.sidebar_width,
                    self.sidebar_state.dock_right,
                );
            }
        }
    }

    /// Adopt a settled divider drag as the persisted sidebar width. The pane is
    /// already that wide, so no resize is issued; the value is one global
    /// setting, which is why a separated Explorer/Source Control pair follows
    /// whichever pane was dragged last.
    pub fn flush_pending_width(&mut self) {
        let Some((cols, at)) = self.pending_width else {
            return;
        };
        if at.elapsed() < WIDTH_SETTLE {
            return;
        }
        self.pending_width = None;
        let cols = sidebar::clamp_sidebar_width(cols);
        if cols != self.sidebar_state.sidebar_width {
            self.sidebar_state = sidebar::update_state(|state| state.sidebar_width = cols);
        }
    }

    /// A drag candidate is waiting for its settle window; the event loop polls
    /// faster so it can be adopted promptly.
    pub fn has_pending_width(&self) -> bool {
        self.pending_width.is_some()
    }

    /// The title bar's Collapse All: fold every repo section and drawer. The
    /// headers all stay visible to re-expand — except a single repo's own
    /// header, which is only rendered in multi-repo mode, so a lone repo
    /// keeps its Staged/Changes headers instead of vanishing entirely.
    fn collapse_all(&mut self) {
        let multi = self.multi();
        for repo in &mut self.repos {
            if multi {
                repo.collapsed = true;
            }
            repo.staged_collapsed = true;
            repo.changes_collapsed = true;
            if self.tree_view {
                repo.staged_collapsed_dirs
                    .extend(repo.staged_dirs.iter().map(|d| d.path.clone()));
                repo.changes_collapsed_dirs
                    .extend(repo.changes_dirs.iter().map(|d| d.path.clone()));
            }
        }
        for drawer in &mut self.drawers {
            drawer.expanded = false;
        }
        self.scroll = 0;
        self.rebuild();
        self.persist_scm();
    }

    fn reload_expanded_drawers(&mut self) {
        let Some(git) = self.active_repo().map(|r| r.git.clone()) else {
            return;
        };
        let git = &git;
        for kind in Drawer::ALL {
            if !self.drawers[kind.index()].expanded {
                continue;
            }
            let lines = match kind {
                Drawer::Graph => git.graph(DRAWER_LIMIT),
                Drawer::Commits => git.commits(DRAWER_LIMIT),
                Drawer::FileHistory => match &self.history_target {
                    Some(path) => git.file_history(path, DRAWER_LIMIT),
                    None => Ok(vec!["(select a file above)".to_string()]),
                },
                Drawer::Branches => git.branches(),
                Drawer::Worktrees => git.worktrees(),
                Drawer::Remotes => git.remotes(),
                Drawer::Stashes => git.stashes(),
                Drawer::Tags => git.tags(),
            };
            self.drawers[kind.index()].lines = match lines {
                Ok(lines) if lines.is_empty() => vec!["(none)".to_string()],
                Ok(lines) => lines,
                Err(e) => vec![format!("({e})")],
            };
            let panel = &mut self.drawers[kind.index()];
            panel.refs = panel
                .lines
                .iter()
                .map(|l| parse_drawer_ref(kind, l))
                .collect();
            match kind {
                Drawer::Worktrees => {
                    panel.lines = panel
                        .lines
                        .iter()
                        .map(|l| pretty_worktree_line(l))
                        .collect();
                }
                Drawer::Remotes => {
                    panel.lines = panel.lines.iter().map(|l| pretty_remote_line(l)).collect();
                }
                _ => {}
            }
        }
    }

    fn rebuild(&mut self) {
        self.rows.clear();
        let multi = self.repos.len() > 1;
        if self.tree_view {
            for repo in &mut self.repos {
                repo.build_trees();
            }
        }
        for (r, repo) in self.repos.iter().enumerate() {
            if multi {
                self.rows.push(Row::RepoHeader(r));
                if repo.collapsed {
                    continue;
                }
                // VS Code gives every repo its own message box and Commit
                // button, inline in the list.
                self.rows.push(Row::Message(r));
                self.rows.push(Row::Commit(r));
            }
            // Like VS Code, the Staged section only exists while something is staged.
            if !repo.status.staged.is_empty() {
                self.rows.push(Row::StagedHeader(r));
                if !repo.staged_collapsed {
                    if self.tree_view {
                        for item in &repo.staged_tree_rows {
                            match item {
                                TreeItem::Dir(d) => self.rows.push(Row::StagedDir(r, *d)),
                                TreeItem::File(i) => self.rows.push(Row::Staged(r, *i)),
                            }
                        }
                    } else {
                        for i in 0..repo.status.staged.len() {
                            self.rows.push(Row::Staged(r, i));
                        }
                    }
                }
            }
            self.rows.push(Row::ChangesHeader(r));
            if !repo.changes_collapsed {
                if self.tree_view {
                    for item in &repo.changes_tree_rows {
                        match item {
                            TreeItem::Dir(d) => self.rows.push(Row::ChangesDir(r, *d)),
                            TreeItem::File(i) => self.rows.push(Row::Unstaged(r, *i)),
                        }
                    }
                } else {
                    for i in 0..repo.status.unstaged.len() {
                        self.rows.push(Row::Unstaged(r, i));
                    }
                }
            }
        }
        for kind in Drawer::ALL {
            self.rows.push(Row::DrawerHeader(kind));
            if self.drawers[kind.index()].expanded {
                for i in 0..self.drawers[kind.index()].lines.len() {
                    self.rows.push(Row::DrawerLine(kind, i));
                }
            }
        }
        if self.rows.is_empty() {
            self.selected = None;
            self.scroll = 0;
            self.hovered = None;
            return;
        }
        if let Some(sel) = self.selected {
            let index = sel.min(self.rows.len() - 1);
            self.selected = Some(self.nearest_selectable(index));
        }
        self.scroll = self.scroll.min(self.rows.len() - 1);
        self.follow_selection();
    }

    /// The closest keyboard-selectable row to `from` (widget rows — inline
    /// message boxes and commit buttons — are skipped).
    fn nearest_selectable(&self, from: usize) -> usize {
        if self.rows.get(from).is_some_and(|r| r.selectable()) {
            return from;
        }
        let after = (from..self.rows.len()).find(|&i| self.rows[i].selectable());
        let before = (0..from).rev().find(|&i| self.rows[i].selectable());
        after.or(before).unwrap_or(0)
    }

    /// Keep the active repo and the FILE HISTORY drawer following the
    /// selection: drawers, commit box, and sync all act on the selected
    /// row's repository.
    fn follow_selection(&mut self) {
        let selected = self.selected.and_then(|i| self.rows.get(i)).copied();
        if let Some(r) = selected.and_then(Row::repo)
            && r != self.active
            && r < self.repos.len()
        {
            self.active = r;
            self.history_target = None;
            self.reload_expanded_drawers();
            let keep = self.selected;
            self.rebuild();
            if let Some(i) = keep {
                self.selected = Some(i.min(self.rows.len().saturating_sub(1)));
            }
            return;
        }
        let path = match selected {
            Some(Row::Staged(r, i)) if r == self.active => {
                self.repos[r].status.staged.get(i).map(|e| e.path.clone())
            }
            Some(Row::Unstaged(r, i)) if r == self.active => {
                self.repos[r].status.unstaged.get(i).map(|e| e.path.clone())
            }
            _ => return, // keep the last file while browsing elsewhere
        };
        if path.is_some() && path != self.history_target {
            self.history_target = path;
            if self.drawers[Drawer::FileHistory.index()].expanded {
                self.reload_expanded_drawers();
                // Line count may have changed; rebuild WITHOUT re-entering
                // follow_selection (path is unchanged now).
                let selected = self.selected;
                self.rebuild();
                if let Some(i) = selected {
                    self.selected = Some(i.min(self.rows.len() - 1));
                }
            }
        }
    }

    /// Handle one key press; `Some(exit)` ends the event loop.
    /// The pane lost focus: dismiss transient popups (menus, the branch
    /// picker, settings, and confirmations, where closing cancels). The pane
    /// rename field keeps what was typed; the commit message lives outside.
    pub fn on_focus_lost(&mut self) {
        if self.has_transient_popup() {
            self.overlay = None;
        }
    }

    pub fn has_transient_popup(&self) -> bool {
        self.overlay.is_some()
            && !matches!(
                self.overlay,
                Some(Overlay::PaneRename { .. })
                    | Some(Overlay::BranchRename { .. })
                    | Some(Overlay::CommitReword { .. })
            )
    }

    /// Fallback for hosts that don't forward focus reports: while a popup is
    /// open, ask herdr whether this pane still has focus.
    pub fn dismiss_popups_if_unfocused(&mut self) {
        if !self.has_transient_popup() {
            return;
        }
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            return;
        };
        if herdr_sidebar::ipc::pane_focused(&pane_id) == Some(false) {
            self.on_focus_lost();
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        if key.code == KeyCode::Char('q')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.close(false);
            return None;
        }
        self.flash = None;
        if ((key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT))
            || key.code == KeyCode::F(12))
            && self.merged()
        {
            self.sidebar_state = sidebar::update_state(|state| {
                state.active = View::Explorer;
                state.search_active = false;
            });
            return Some(Exit::QuickOpen);
        }
        // View switching has to reach past the commit message box, where bare
        // 1/2/3 type into the draft — Ctrl+1/2/3 mirror VS Code's activity bar
        // from any focus (1 Explorer, 2 Search, 3 Source Control). Bare 1/2/3
        // still switch from the file list.
        let injected_view = match key.code {
            KeyCode::F(9) => Some('1'),
            KeyCode::F(10) => Some('2'),
            KeyCode::F(11) => Some('3'),
            _ => None,
        };
        let keyboard_view = match key.code {
            KeyCode::Char(c @ ('1' | '2' | '3'))
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                Some(c)
            }
            _ => None,
        };
        if let Some(c) = injected_view.or(keyboard_view) {
            self.overlay = None;
            return match c {
                '1' => self.switch_to(View::Explorer),
                '2' => self.open_search(false),
                _ => self.switch_to(View::SourceControl),
            };
        }
        if self.overlay.is_some() {
            self.overlay_key(key);
            return None;
        }
        if matches!(key.code, KeyCode::Char('f' | 'F'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return self.open_search(true);
        }
        match self.focus {
            Focus::Message => self.on_message_key(key),
            Focus::Commit => self.on_button_key(key),
            Focus::List => return self.on_list_key(key),
        }
        None
    }

    fn on_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.commit();
                return;
            }
            // Esc first drops an armed Ctrl+A selection, then leaves the box.
            KeyCode::Esc if self.active_repo_mut().is_some_and(Repo::esc_message) => return,
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab => self.leave_message(Focus::Commit),
            KeyCode::BackTab => self.leave_message(Focus::List),
            KeyCode::Down => self.leave_message(Focus::Commit),
            _ => {}
        }
        let Some(repo) = self.active_repo_mut() else {
            return;
        };
        repo.message_key(key);
    }

    /// An armed Ctrl+A (`ui::SelectAll`) belongs to the field it was made in,
    /// so switching fields from the commit box drops it.
    fn leave_message(&mut self, focus: Focus) {
        if let Some(repo) = self.active_repo_mut() {
            repo.select_all.disarm();
        }
        self.focus = focus;
    }

    fn on_button_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => self.run_primary_action(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab | KeyCode::Down => self.focus = Focus::List,
            KeyCode::BackTab | KeyCode::Up => self.focus = Focus::Message,
            _ => {}
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) -> Option<Exit> {
        match key.code {
            KeyCode::Char('q') => return Some(Exit::Quit),
            // Esc never quits the sidebar — it closes the preview instead.
            KeyCode::Esc => self.close_preview(),
            KeyCode::Tab => self.focus = Focus::Message,
            KeyCode::BackTab => self.focus = Focus::Commit,
            KeyCode::Char('c') => self.focus = Focus::Message,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Left => self.fold_selected(false),
            KeyCode::Right => self.fold_selected(true),
            KeyCode::Char('t') => self.toggle_tree_view(),
            KeyCode::Char('a') => self.stage_all(),
            KeyCode::Char('u') => self.unstage_all(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('i') => self.set_theme(self.theme.toggled()),
            KeyCode::Char('A') => self.suggest_message(),
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('S') => self.sync_changes(),
            KeyCode::Char('o') => self.open_selected_entry(),
            KeyCode::Char('m') => self.open_menu_for_selection(),
            KeyCode::Char('b') => self.hide(),
            KeyCode::Char('1') => return self.switch_to(View::Explorer),
            KeyCode::Char('2') => return self.open_search(false),
            KeyCode::Char('3') => return self.switch_to(View::SourceControl),
            _ => {}
        }
        None
    }

    /// `Some(exit)` ends the event loop, mirroring on_key.
    /// Drop an armed Ctrl+A on the commit box or the rename dialog: a click
    /// means the user left that field.
    fn disarm_input_selection(&mut self) {
        if let Some(Overlay::PaneRename { select_all, .. }) = self.overlay.as_mut() {
            select_all.disarm();
        }
        if let Some(Overlay::BranchRename { select_all, .. }) = self.overlay.as_mut() {
            select_all.disarm();
        }
        if let Some(Overlay::CommitReword { select_all, .. }) = self.overlay.as_mut() {
            select_all.disarm();
        }
        if let Some(repo) = self.active_repo_mut() {
            repo.select_all.disarm();
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        // Any mouse activity = "the mouse is over this pane": it shows the
        // hover title-bar buttons until the linger expires.
        self.last_mouse = Some(std::time::Instant::now());
        self.mouse_pos = Some((mouse.column, mouse.row));
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            // A click is a focus move: drop an armed Ctrl+A where it was made.
            self.disarm_input_selection();
        }
        if self.overlay.is_some() {
            self.overlay_mouse(mouse);
            return None;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                self.hovered = self.row_at(mouse.row);
            }
            MouseEventKind::ScrollUp => self.scroll_view(-3),
            MouseEventKind::ScrollDown => self.scroll_view(3),
            MouseEventKind::Down(MouseButton::Left) => return self.left_click(mouse),
            MouseEventKind::Down(MouseButton::Right) => {
                // The sidebar routes its pane's right-clicks to itself at
                // startup (pane.input.set); Ctrl+right-click works either way.
                self.flash = None;
                self.open_context_menu(mouse.column, mouse.row);
            }
            _ => {}
        }
        None
    }

    fn left_click(&mut self, mouse: MouseEvent) -> Option<Exit> {
        self.flash = None;
        if hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height) {
            self.hide();
            return None;
        }
        let (x, y) = (mouse.column, mouse.row);
        let z = self.zones;
        if self.merged() {
            if hits_activity_button(z.explorer, z.activity_row, x, y) {
                return self.switch_to(View::Explorer);
            }
            if hits_activity_button(z.search, z.activity_row, x, y) {
                return self.open_search(false);
            }
            if hits_activity_button(z.source_control, z.activity_row, x, y) {
                return self.switch_to(View::SourceControl);
            }
        }
        if hits(z.gear, x, y) {
            self.open_settings();
            return None;
        }
        if let Some(&(_, action)) = self.title_zones.iter().find(|(rect, _)| hits(*rect, x, y)) {
            match action {
                TitleAction::ViewAsTree | TitleAction::ViewAsList => self.toggle_tree_view(),
                TitleAction::Refresh => self.refresh(),
                TitleAction::CollapseAll => self.collapse_all(),
                _ => {}
            }
            return None;
        }
        if hits(z.sparkle, x, y) {
            self.suggest_message();
            return None;
        }
        if hits(z.message, x, y) {
            self.focus = Focus::Message;
            return None;
        }
        if hits(z.button, x, y) {
            self.focus = Focus::Commit;
            self.run_primary_action();
            return None;
        }
        if hits(z.sync, x, y) {
            self.sync_changes();
            return None;
        }
        if hits(z.header_branch, x, y) || hits(z.git_footer.branch, x, y) {
            self.open_branch_picker();
            return None;
        }
        if hits(z.git_footer.sync, x, y) {
            self.sync_changes();
            return None;
        }
        if let Some((index, line)) = self.row_hit(y) {
            let _ = line;
            // Double click = second click on the same row inside the window;
            // for preview rows it pins the tab the first click opened.
            let now = std::time::Instant::now();
            let double = self
                .last_click
                .take()
                .is_some_and(|(i, at)| i == index && now.duration_since(at) < DOUBLE_CLICK);
            self.last_click = Some((index, now));
            match self.rows[index] {
                Row::StagedDir(r, d) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if let Some(dir) = self
                        .repos
                        .get(r)
                        .and_then(|repo| repo.staged_dirs.get(d))
                        .cloned()
                    {
                        // The button is only drawn on the hovered row; a tap with no
                        // hover (mobile clients, send-keys) must fold, not stage.
                        let action = (self.hovered == Some(index))
                            .then(|| dir_hover_action_at(x, self.last_width, true, dir.count))
                            .flatten();
                        if action == Some(FileHoverAction::Unstage) {
                            self.unstage_dir(r, &dir.path);
                        } else {
                            self.toggle_staged_dir(r, d);
                        }
                    }
                }
                Row::ChangesDir(r, d) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if let Some(dir) = self
                        .repos
                        .get(r)
                        .and_then(|repo| repo.changes_dirs.get(d))
                        .cloned()
                    {
                        // The button is only drawn on the hovered row; a tap with no
                        // hover (mobile clients, send-keys) must fold, not stage.
                        let action = (self.hovered == Some(index))
                            .then(|| dir_hover_action_at(x, self.last_width, false, dir.count))
                            .flatten();
                        if action == Some(FileHoverAction::Stage) {
                            self.stage_dir(r, &dir.path);
                        } else {
                            self.toggle_changes_dir(r, d);
                        }
                    }
                }
                // Clicking a changed file shows its diff, like VS Code — except
                // a conflicted one, which opens in the editor to be resolved.
                // Hover actions open, discard, stage, or unstage that exact entry.
                Row::Staged(r, i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if let Some(entry) = self.repos[r].status.staged.get(i).cloned() {
                        let conflicts = self.conflicts_for(r, &entry);
                        let marker_width = status_marker(entry.letter, conflicts).0.chars().count();
                        match (self.hovered == Some(index))
                            .then(|| {
                                file_hover_action_at(x, self.last_width, true, marker_width as u16)
                            })
                            .flatten()
                        {
                            Some(FileHoverAction::Open) => self.open_entry(r, &entry, true),
                            Some(FileHoverAction::Unstage) => {
                                if let Err(error) = self.repos[r].git.unstage(&entry) {
                                    self.flash = Some((error, true));
                                }
                                self.refresh();
                            }
                            _ if double && self.pin_if_open(index) => {
                                // pinned the first click's tab
                            }
                            _ => self.open_entry(r, &entry, true),
                        }
                    }
                }
                Row::Unstaged(r, i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if let Some(entry) = self.repos[r].status.unstaged.get(i).cloned() {
                        let conflicts = self.conflicts_for(r, &entry);
                        let marker_width = status_marker(entry.letter, conflicts).0.chars().count();
                        match (self.hovered == Some(index))
                            .then(|| {
                                file_hover_action_at(x, self.last_width, false, marker_width as u16)
                            })
                            .flatten()
                        {
                            Some(FileHoverAction::Open) => self.open_entry(r, &entry, false),
                            Some(FileHoverAction::Discard) => {
                                self.overlay = Some(Overlay::ConfirmDiscard { repo: r, entry });
                            }
                            Some(FileHoverAction::Stage) => {
                                if let Err(error) = self.repos[r].git.stage(&entry) {
                                    self.flash = Some((error, true));
                                }
                                self.refresh();
                            }
                            _ if double && self.pin_if_open(index) => {
                                // pinned the first click's tab
                            }
                            _ => self.open_entry(r, &entry, false),
                        }
                    }
                }
                // The inline widgets: click focuses/acts without selecting.
                Row::Message(r) => {
                    self.active = r;
                    // The box's middle line holds the input and the ✧ button.
                    if line == 1 && x >= self.last_width.saturating_sub(4) {
                        self.suggest_message();
                    } else {
                        self.focus = Focus::Message;
                    }
                    self.follow_selection();
                }
                Row::Commit(r) => {
                    if line <= 2 && x > 0 && x < self.last_width.saturating_sub(1) {
                        if self.ai_commit_busy_for(r) {
                            // Already committing; the flash says so.
                        } else if self
                            .repos
                            .get(r)
                            .is_some_and(|repo| sync_is_primary(&repo.status))
                        {
                            self.sync_repo(r);
                        } else {
                            self.commit_repo(r);
                        }
                    }
                }
                Row::RepoHeader(r) => {
                    self.focus = Focus::List;
                    self.select(index);
                    // Right-side action icons: ⟳ sync · ✓ commit (fixed
                    // offsets from the right edge, see repo_header_item).
                    let w = self.last_width;
                    let (sync_zone, commit_zone) = repo_header_action_zones(w);
                    if within(x, commit_zone) {
                        self.commit_repo(r);
                    } else if within(x, sync_zone) {
                        self.sync_repo(r);
                    } else if within(x, repo_header_branch_zone(&self.repos[r], self.theme, w)) {
                        self.open_branch_picker_for(r);
                    } else {
                        self.activate();
                    }
                }
                // Header hover actions unstage/stage or manage the whole section.
                Row::StagedHeader(r) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if x >= self.last_width.saturating_sub(6) {
                        if let Some(repo) = self.repos.get(r)
                            && let Err(e) = repo.git.unstage_all()
                        {
                            self.flash = Some((e, true));
                        }
                        self.refresh();
                    } else {
                        self.activate();
                    }
                }
                Row::ChangesHeader(r) => {
                    self.focus = Focus::List;
                    self.select(index);
                    let count = self.repos[r].status.unstaged.len();
                    if self.hovered == Some(index)
                        && let Some(action) = changes_header_action_at(x, self.last_width, count)
                    {
                        self.run_changes_header_action(r, action);
                    } else {
                        self.activate();
                    }
                }
                Row::DrawerHeader(_) => {
                    self.focus = Focus::List;
                    self.select(index);
                    self.activate();
                }
                Row::DrawerLine(kind, i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if double && self.pin_if_open(index) {
                        // pinned the first click's show/diff tab
                    } else {
                        self.open_drawer_ref(kind, i);
                    }
                }
            }
        }
        None
    }

    /// `m`: the row context menu from the KEYBOARD (issue #18 — mobile herdr
    /// clients have no right-click at all). Anchors on the selected row, and
    /// routes through the same builder ctrl+right-click uses, so the file and
    /// drawer menus stay identical either way.
    fn open_menu_for_selection(&mut self) {
        let Some(index) = self.selected else { return };
        // `m` and `o` used to do nothing at all with a section header selected,
        // which reads as "the key is broken" (user-reported). Say what to do.
        if let Some(row) = self.rows.get(index)
            && let Some(hint) = section_row_hint(*row)
        {
            self.flash = Some((hint.into(), false));
            return;
        }
        let Some(y) = self.row_y(index) else { return };
        self.open_context_menu(2, y);
    }

    /// herdr's pane-menu items, appended to a menu once the sidebar knows its
    /// own pane. Empty otherwise.
    fn pane_menu_section(&self) -> Vec<MenuEntry> {
        if self.pane_ctl.is_none() {
            return Vec::new();
        }
        let mut entries = vec![MenuEntry::Separator];
        entries.extend(
            pane_menu::items(self.pane_named)
                .into_iter()
                .map(|item| MenuEntry::Action(MenuAction::Herdr(item), item.label())),
        );
        entries
    }

    /// herdr's pane-menu items (see `pane_menu`). Failures go to the flash
    /// line, like the other menu actions.
    fn run_pane_item(&mut self, item: PaneAction) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            return;
        };
        match item {
            PaneAction::Rename => {
                self.overlay = Some(Overlay::PaneRename {
                    input: String::new(),
                    select_all: SelectAll::default(),
                });
            }
            // Close through the sidebar's own hide path (the `b` key): it
            // snoozes this tab first, so the ensure hook does not re-dock us.
            PaneAction::Close => self.hide(),
            item => {
                let Some((method, params)) = pane_menu::request(item, &pane_id) else {
                    return;
                };
                match pane_menu::run(method, params) {
                    Ok(()) if item == PaneAction::ClearName => self.pane_named = false,
                    Ok(()) => {}
                    Err(error) => self.flash = Some((format!("{}: {error}", item.label()), true)),
                }
            }
        }
    }

    /// The text of the open name dialog, for Ctrl+C. `None` when no such
    /// overlay is open (or its field is empty, where a copy would send nothing).
    fn overlay_input(&self) -> Option<String> {
        let text = match &self.overlay {
            Some(Overlay::PaneRename { input, .. })
            | Some(Overlay::BranchRename { input, .. })
            | Some(Overlay::CommitReword { input, .. }) => input.clone(),
            _ => return None,
        };
        (!text.is_empty()).then_some(text)
    }

    /// Name our pane with the text typed into the rename overlay.
    fn confirm_pane_rename(&mut self, input: &str) {
        let name = input.trim();
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            return;
        };
        if name.is_empty() {
            self.flash = Some(("pane name is empty (use Clear pane name)".into(), true));
            return;
        }
        let (method, params) = pane_menu::rename(&pane_id, Some(name));
        match pane_menu::run(method, params) {
            Ok(()) => self.pane_named = true,
            Err(error) => self.flash = Some((format!("rename pane failed: {error}"), true)),
        }
    }

    /// Ctrl+right-click: the VS Code-style context menu.
    fn open_context_menu(&mut self, x: u16, y: u16) {
        let Some(index) = self.row_at(y) else { return };
        self.select(index);
        let (repo, entry, staged) = match self.rows[index] {
            Row::Staged(r, i) => (r, self.repos[r].status.staged.get(i), true),
            Row::Unstaged(r, i) => (r, self.repos[r].status.unstaged.get(i), false),
            Row::DrawerLine(kind, i) => {
                self.open_drawer_menu(x, y, kind, i);
                return;
            }
            Row::StagedDir(r, d) | Row::ChangesDir(r, d) => {
                let staged = matches!(self.rows[index], Row::StagedDir(..));
                let Some(path) = self.repos.get(r).and_then(|repo| {
                    if staged {
                        repo.staged_dirs.get(d)
                    } else {
                        repo.changes_dirs.get(d)
                    }
                    .map(|dir| dir.path.clone())
                }) else {
                    return;
                };
                let mut entries = vec![
                    MenuEntry::Action(
                        MenuAction::StageOrUnstage,
                        if staged {
                            "Unstage Folder"
                        } else {
                            "Stage Folder"
                        },
                    ),
                    MenuEntry::Separator,
                    MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
                    MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
                    MenuEntry::Separator,
                    MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
                ];
                entries.extend(self.pane_menu_section());
                self.overlay = Some(Overlay::Menu {
                    x,
                    y,
                    target: MenuTarget::Dir {
                        repo: r,
                        path,
                        staged,
                    },
                    entries,
                    selected: 0,
                    rect: Rect::default(),
                });
                return;
            }
            _ => return, // section headers have no menu
        };
        let Some(entry) = entry.cloned() else { return };
        let mut entries = vec![
            MenuEntry::Action(MenuAction::OpenDiff, "Open Diff"),
            MenuEntry::Action(MenuAction::OpenDiffPreview, "Open Diff Preview"),
        ];
        // A deleted file has nothing left on disk to hand to the shell, and the
        // diff already covers it — so both of these are for existing files only.
        if entry.letter != 'D' {
            entries.push(MenuEntry::Action(
                MenuAction::OpenInEditor,
                "Open File in Editor",
            ));
            entries.push(MenuEntry::Action(
                MenuAction::OpenExternal,
                "Open with Default App",
            ));
        }
        entries.push(MenuEntry::Action(
            MenuAction::StageOrUnstage,
            if staged {
                "Unstage Changes"
            } else {
                "Stage Changes"
            },
        ));
        if !staged {
            entries.push(MenuEntry::Action(MenuAction::Discard, "Discard Changes…"));
        }
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
            MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
        ]);
        entries.extend(self.pane_menu_section());
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target: MenuTarget::File {
                repo,
                entry,
                staged,
            },
            entries,
            selected: 0,
            rect: Rect::default(),
        });
    }

    /// The context menu for a commit / branch / stash / remote / tag.
    fn open_drawer_menu(&mut self, x: u16, y: u16, kind: Drawer, index: usize) {
        let Some(dref) = self.drawers[kind.index()].refs.get(index) else {
            return;
        };
        let mut entries: Vec<MenuEntry> = match dref {
            DrawerRef::Commit(h) => {
                // Rewording rewrites HEAD's history, so only commits on it qualify.
                let on_head = self
                    .repos
                    .get(self.active)
                    .is_some_and(|repo| repo.git.is_ancestor_of_head(h));
                let mut entries = vec![
                    MenuEntry::Action(MenuAction::ShowRef, "Show Changes"),
                    MenuEntry::Separator,
                ];
                if on_head {
                    entries.push(MenuEntry::Action(
                        MenuAction::RewordCommit,
                        "Edit Commit Message…",
                    ));
                }
                entries.extend([
                    MenuEntry::Action(MenuAction::Checkout, "Checkout (Detached)"),
                    MenuEntry::Action(MenuAction::CherryPick, "Cherry-Pick"),
                    MenuEntry::Action(MenuAction::Revert, "Revert"),
                    MenuEntry::Action(MenuAction::ResetHere, "Reset Current Branch Here…"),
                    MenuEntry::Separator,
                    MenuEntry::Action(MenuAction::CopyRef, "Copy Hash"),
                ]);
                entries
            }
            DrawerRef::Branch { name, current } => {
                // `git branch -a` also lists remote-tracking refs (`origin/main`):
                // deleting one ran `git branch -d -- origin/main` and failed with
                // "branch not found" (user-reported), and renaming one is
                // meaningless. The menu says which kind it is.
                let local = self
                    .repos
                    .get(self.active)
                    .is_some_and(|repo| repo.git.is_local_branch(name));
                branch_menu_entries(*current, local)
            }
            DrawerRef::Stash(_) => vec![
                MenuEntry::Action(MenuAction::ShowRef, "Show Changes"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::StashApply, "Apply Stash"),
                MenuEntry::Action(MenuAction::StashPop, "Pop Stash"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::StashDrop, "Drop Stash…"),
            ],
            DrawerRef::Remote { .. } => vec![
                MenuEntry::Action(MenuAction::FetchRemote, "Fetch"),
                MenuEntry::Action(MenuAction::CopyRef, "Copy URL"),
            ],
            DrawerRef::Tag(_) => vec![
                MenuEntry::Action(MenuAction::ShowRef, "Show Changes"),
                MenuEntry::Action(MenuAction::Checkout, "Checkout Tag"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::DeleteTag, "Delete Tag…"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRef, "Copy Tag Name"),
            ],
            DrawerRef::Worktree(_) => vec![
                MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
                MenuEntry::Action(MenuAction::CopyRef, "Copy Path"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::RemoveWorktree, "Remove Worktree…"),
            ],
            DrawerRef::None => return,
        };
        entries.extend(self.pane_menu_section());
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target: MenuTarget::Drawer { kind, index },
            entries,
            selected: 0,
            rect: Rect::default(),
        });
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            AdjustWidth(bool),
            AdjustHeight(bool),
            AdjustAgentWidth(bool),
            DiscardConfirmed(usize, FileEntry),
            DiscardAllConfirmed(usize),
            GitConfirmed(usize, Vec<String>),
            PullThenCommit(usize),
            CommitNow(usize),
            Picker(PickerAction),
            PaneRenameConfirmed,
            BranchRenameConfirmed,
            CommitRewordConfirmed,
            /// Ctrl+C in a name field: copy it and keep the dialog open.
            CopyInput,
        }

        let settings = self.settings_rows();
        let row_count = settings.len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Menu {
                entries, selected, ..
            }) => match key.code {
                KeyCode::Esc => Cmd::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = step_menu(entries, *selected, -1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = step_menu(entries, *selected, 1);
                    Cmd::Nothing
                }
                KeyCode::Enter => Cmd::Activate,
                _ => Cmd::Nothing,
            },
            Some(Overlay::Settings { selected, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => Cmd::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(row_count.saturating_sub(1));
                    Cmd::Nothing
                }
                KeyCode::Left | KeyCode::Char('h')
                    if settings.get(*selected).map(|row| row.0) == Some(Setting::SidebarWidth) =>
                {
                    Cmd::AdjustWidth(false)
                }
                KeyCode::Right | KeyCode::Char('l')
                    if settings.get(*selected).map(|row| row.0) == Some(Setting::SidebarWidth) =>
                {
                    Cmd::AdjustWidth(true)
                }
                KeyCode::Left | KeyCode::Char('h')
                    if settings.get(*selected).map(|row| row.0)
                        == Some(Setting::TerminalHeight) =>
                {
                    Cmd::AdjustHeight(false)
                }
                KeyCode::Right | KeyCode::Char('l')
                    if settings.get(*selected).map(|row| row.0)
                        == Some(Setting::TerminalHeight) =>
                {
                    Cmd::AdjustHeight(true)
                }
                KeyCode::Left | KeyCode::Char('h')
                    if settings.get(*selected).map(|row| row.0) == Some(Setting::AgentWidth) =>
                {
                    Cmd::AdjustAgentWidth(false)
                }
                KeyCode::Right | KeyCode::Char('l')
                    if settings.get(*selected).map(|row| row.0) == Some(Setting::AgentWidth) =>
                {
                    Cmd::AdjustAgentWidth(true)
                }
                KeyCode::Enter | KeyCode::Char(' ') => Cmd::ToggleSetting(*selected),
                _ => Cmd::Nothing,
            },
            Some(Overlay::BranchPicker(picker)) => Cmd::Picker(picker.key(key)),
            Some(Overlay::PaneRename { input, select_all }) => {
                match field_key(key, input, select_all) {
                    FieldKey::Close => Cmd::Close,
                    FieldKey::Confirm => Cmd::PaneRenameConfirmed,
                    FieldKey::Copy => Cmd::CopyInput,
                    FieldKey::Nothing => Cmd::Nothing,
                }
            }
            Some(Overlay::BranchRename {
                input, select_all, ..
            }) => match field_key(key, input, select_all) {
                FieldKey::Close => Cmd::Close,
                FieldKey::Confirm => Cmd::BranchRenameConfirmed,
                FieldKey::Copy => Cmd::CopyInput,
                FieldKey::Nothing => Cmd::Nothing,
            },
            Some(Overlay::CommitReword {
                input, select_all, ..
            }) => match field_key(key, input, select_all) {
                FieldKey::Close => Cmd::Close,
                FieldKey::Confirm => Cmd::CommitRewordConfirmed,
                FieldKey::Copy => Cmd::CopyInput,
                FieldKey::Nothing => Cmd::Nothing,
            },
            Some(Overlay::ConfirmDiscard { repo, entry }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Cmd::DiscardConfirmed(*repo, entry.clone())
                }
                _ => Cmd::Close,
            },
            Some(Overlay::ConfirmDiscardAll { repo }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Cmd::DiscardAllConfirmed(*repo),
                _ => Cmd::Close,
            },
            Some(Overlay::ConfirmGit { repo, args, .. }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Cmd::GitConfirmed(*repo, args.clone()),
                _ => Cmd::Close,
            },
            Some(Overlay::ConfirmPullThenCommit { repo, .. }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Cmd::PullThenCommit(*repo),
                // Not a refusal to commit: the answer is "not now", and the
                // click/key press was a commit request.
                _ => Cmd::CommitNow(*repo),
            },
            None => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::AdjustWidth(wider) => self.adjust_sidebar_width(wider),
            Cmd::AdjustHeight(taller) => self.adjust_terminal_height(taller),
            Cmd::AdjustAgentWidth(wider) => self.adjust_agent_width(wider),
            Cmd::GitConfirmed(repo, args) => {
                self.overlay = None;
                match safe_branch_delete(&args) {
                    Some(branch) => self.delete_branch(repo, branch.to_string()),
                    None => {
                        let strs: Vec<&str> = args.iter().map(String::as_str).collect();
                        self.run_git(repo, &strs);
                    }
                }
            }
            Cmd::PullThenCommit(repo) => {
                self.overlay = None;
                self.pull_then_commit(repo);
            }
            Cmd::CommitNow(repo) => {
                self.overlay = None;
                self.commit_repo(repo);
            }
            Cmd::DiscardConfirmed(repo, entry) => {
                self.overlay = None;
                let result = match self.repos.get(repo) {
                    Some(r) => r.git.discard(&entry),
                    None => Err("repository is gone".to_string()),
                };
                match result {
                    Ok(()) => self.flash = Some((format!("discarded {}", entry.path), false)),
                    Err(e) => self.flash = Some((e, true)),
                }
                self.refresh();
            }
            Cmd::DiscardAllConfirmed(repo) => {
                self.overlay = None;
                self.discard_all(repo);
            }
            Cmd::Picker(action) => self.handle_picker_action(action),
            Cmd::PaneRenameConfirmed => {
                if let Some(Overlay::PaneRename { input, .. }) = self.overlay.take() {
                    self.confirm_pane_rename(&input);
                }
            }
            Cmd::BranchRenameConfirmed => {
                if let Some(Overlay::BranchRename {
                    repo, from, input, ..
                }) = self.overlay.take()
                {
                    self.confirm_branch_rename(repo, from, input);
                }
            }
            Cmd::CommitRewordConfirmed => {
                if let Some(Overlay::CommitReword {
                    repo,
                    commit,
                    body,
                    input,
                    ..
                }) = self.overlay.take()
                {
                    self.confirm_commit_reword(repo, commit, body, input);
                }
            }
            Cmd::CopyInput => {
                if let Some(text) = self.overlay_input() {
                    self.flash = Some(match copy_to_clipboard(&text) {
                        Ok(ClipboardWrite::Native) => (format!("copied: {text}"), false),
                        Ok(ClipboardWrite::Osc52Unacknowledged) => {
                            (format!("sent to terminal clipboard: {text}"), false)
                        }
                        Err(err) => (format!("copy failed: {err}"), true),
                    });
                }
            }
        }
    }

    fn overlay_mouse(&mut self, mouse: MouseEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            Reopen(u16, u16),
            Picker(PickerAction),
        }
        // A left-click on a y/N confirm's buttons is the same key as the
        // keyboard: Yes is `y`, No is Esc (any other key cancels too). Clicks
        // anywhere else do nothing.
        if matches!(
            self.overlay,
            Some(Overlay::ConfirmDiscard { .. })
                | Some(Overlay::ConfirmDiscardAll { .. })
                | Some(Overlay::ConfirmGit { .. })
                | Some(Overlay::ConfirmPullThenCommit { .. })
        ) {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if hits(self.zones.confirm_yes, mouse.column, mouse.row) {
                    self.overlay_key(KeyEvent::from(KeyCode::Char('y')));
                } else if hits(self.zones.confirm_no, mouse.column, mouse.row) {
                    self.overlay_key(KeyEvent::from(KeyCode::Esc));
                }
            }
            return;
        }
        let row_count = self.settings_rows().len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Settings {
                selected,
                rect,
                scroll,
            }) => {
                // Rows start just inside the top border (the title renders ON
                // the border, not on its own line).
                let row_at = |row: u16, col: u16| -> Option<usize> {
                    let index = usize::from(row.saturating_sub(rect.y + 1)) + *scroll;
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
                            *selected = i;
                        }
                        Cmd::Nothing
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        match row_at(mouse.row, mouse.column) {
                            Some(i) => {
                                *selected = i;
                                Cmd::ToggleSetting(i)
                            }
                            None if hits(*rect, mouse.column, mouse.row) => Cmd::Nothing,
                            None => Cmd::Close,
                        }
                    }
                    _ => Cmd::Nothing,
                }
            }
            Some(Overlay::Menu {
                entries,
                selected,
                rect,
                ..
            }) => {
                let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
                let item_at = |row: u16, col: u16| -> Option<usize> {
                    (col >= inner.x
                        && col < inner.x + inner.width
                        && row >= inner.y
                        && row < inner.y + inner.height)
                        .then(|| usize::from(row - inner.y))
                        .filter(|i| {
                            *i < entries.len() && matches!(entries[*i], MenuEntry::Action(..))
                        })
                };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(i) = item_at(mouse.row, mouse.column) {
                            *selected = i;
                        }
                        Cmd::Nothing
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(i) = item_at(mouse.row, mouse.column) {
                            *selected = i;
                            Cmd::Activate
                        } else {
                            Cmd::Close
                        }
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        Cmd::Reopen(mouse.column, mouse.row)
                    }
                    _ => Cmd::Nothing,
                }
            }
            Some(Overlay::BranchPicker(picker)) => Cmd::Picker(picker.mouse(mouse)),
            _ => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::Reopen(x, y) => {
                self.overlay = None;
                self.open_context_menu(x, y);
            }
            Cmd::Picker(action) => self.handle_picker_action(action),
        }
    }

    fn open_branch_picker(&mut self) {
        self.open_branch_picker_for(self.active);
    }

    fn open_branch_picker_for(&mut self, repo: usize) {
        let Some(git) = self.repos.get(repo).map(|repo| repo.git.clone()) else {
            return;
        };
        self.active = repo;
        match BranchPicker::open(git) {
            Ok(picker) => self.overlay = Some(Overlay::BranchPicker(picker)),
            Err(error) => self.flash = Some((error, true)),
        }
    }

    fn handle_picker_action(&mut self, action: PickerAction) {
        match action {
            PickerAction::None => {}
            PickerAction::Close => self.overlay = None,
            PickerAction::Checkout(branch) => {
                let Some(Overlay::BranchPicker(picker)) = self.overlay.take() else {
                    return;
                };
                match picker.git.checkout_branch(&branch) {
                    Ok(()) => {
                        self.flash = Some((format!("switched to {}", branch.name), false));
                        self.refresh();
                    }
                    Err(error) => self.flash = Some((error, true)),
                }
            }
            PickerAction::Create(name) => {
                let Some(Overlay::BranchPicker(picker)) = self.overlay.take() else {
                    return;
                };
                match picker.git.create_branch(&name) {
                    Ok(()) => {
                        self.flash = Some((format!("created {name}"), false));
                        self.refresh();
                    }
                    Err(error) => self.flash = Some((error, true)),
                }
            }
        }
    }

    // ---- Settings modal ----

    fn open_settings(&mut self) {
        herdr_sidebar::updates::check();
        self.overlay = Some(Overlay::Settings {
            selected: 0,
            rect: Rect::default(),
            scroll: 0,
        });
    }

    /// The modal's rows for the current state.
    fn settings_rows(&self) -> Vec<SettingRow> {
        let update = herdr_sidebar::updates::row();
        vec![
            (
                Setting::Version,
                "Sidebar version",
                herdr_sidebar::updates::VERSION.into(),
                false,
            ),
            (Setting::Update, update.0, update.1, update.2),
            (
                Setting::UnifiedSidebar,
                "Unified sidebar",
                if self.merged() { "on" } else { "off" }.to_string(),
                self.other_exe.is_some(),
            ),
            (
                Setting::DockRight,
                "Dock on the right",
                if self.sidebar_state.dock_right {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::SidebarWidth,
                "Sidebar width",
                format!("{} cols", self.sidebar_state.sidebar_width),
                true,
            ),
            (
                Setting::TerminalHeight,
                "Terminal height",
                match self.sidebar_state.terminal_height {
                    Some(rows) => format!("{rows} rows"),
                    None => "auto".to_string(),
                },
                true,
            ),
            (
                Setting::AgentWidth,
                "Agent width",
                match self.sidebar_state.agent_width {
                    Some(cols) => format!("{cols} cols"),
                    None => "auto".to_string(),
                },
                true,
            ),
            (
                Setting::IconTheme,
                "Icon theme",
                match self.theme {
                    IconTheme::Material => "material",
                    IconTheme::Emoji => "emoji",
                }
                .to_string(),
                true,
            ),
            (
                Setting::ColorTheme,
                "Color theme",
                self.sidebar_state.color_theme.label().to_string(),
                true,
            ),
            (
                Setting::PreviewPlacement,
                "Preview opens in",
                self.sidebar_state.preview_placement.label().to_string(),
                true,
            ),
            (
                Setting::Hotkeys,
                "Footer hotkeys",
                if self.show_hotkeys() {
                    "shown"
                } else {
                    "hidden"
                }
                .to_string(),
                true,
            ),
            (
                Setting::AutoOpen,
                "Auto-open sidebar",
                if self.sidebar_state.auto_open {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::StrictToggle,
                "Strict toggle",
                if self.sidebar_state.strict_toggle {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::FocusOnOpen,
                "Focus on open",
                if self.sidebar_state.focus_on_open {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::FollowCwd,
                "Follow pane folder",
                sidebar::follow_cwd_setting_value(self.sidebar_state.follow_cwd),
                true,
            ),
            (
                Setting::GitDecorations,
                "Git decorations",
                if self.sidebar_state.git_deco {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::GitFooter,
                "Git footer",
                if self.sidebar_state.show_git_footer {
                    "shown"
                } else {
                    "hidden"
                }
                .to_string(),
                true,
            ),
            (
                Setting::ScmViewMode,
                "View as",
                if self.tree_view { "tree" } else { "list" }.to_string(),
                true,
            ),
            (
                Setting::Folder,
                "Change folder…",
                self.cwd
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.cwd.display().to_string()),
                true,
            ),
        ]
    }

    fn toggle_setting(&mut self, index: usize) {
        let rows = self.settings_rows();
        let Some(row) = rows.get(index) else { return };
        let (setting, enabled) = (row.0, row.3);
        if !enabled {
            return;
        }
        match setting {
            Setting::Version => {}
            Setting::Update => {
                self.flash = Some(match herdr_sidebar::updates::activate() {
                    Ok(()) => ("Update requested".into(), false),
                    Err(error) => (error, true),
                });
            }
            Setting::ScmViewMode => self.toggle_tree_view(),
            Setting::UnifiedSidebar => {
                // The pane layout changes underneath the modal; close it.
                self.overlay = None;
                let on = !self.merged();
                self.set_unified(on);
            }
            Setting::DockRight => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.dock_right = !state.dock_right);
            }
            Setting::SidebarWidth => self.adjust_sidebar_width(true),
            Setting::TerminalHeight => self.clear_terminal_height(),
            Setting::AgentWidth => self.clear_agent_width(),
            Setting::IconTheme => self.set_theme(self.theme.toggled()),
            Setting::ColorTheme => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.color_theme = state.color_theme.next();
                });
                set_color_theme(self.sidebar_state.color_theme);
            }
            Setting::PreviewPlacement => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.preview_placement = state.preview_placement.other();
                });
            }
            Setting::Hotkeys => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.show_hotkeys = !state.show_hotkeys);
            }
            Setting::AutoOpen => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.auto_open = !state.auto_open);
            }
            Setting::StrictToggle => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.strict_toggle = !state.strict_toggle);
            }
            Setting::FocusOnOpen => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.focus_on_open = !state.focus_on_open);
            }
            Setting::FollowCwd => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.follow_cwd = !state.follow_cwd);
                self.cwd_follower.borrow_mut().reset();
            }
            Setting::GitDecorations => {
                self.sidebar_state =
                    sidebar::update_state(|state| state.git_deco = !state.git_deco);
            }
            Setting::GitFooter => {
                self.sidebar_state = sidebar::update_state(|state| {
                    state.show_git_footer = !state.show_git_footer;
                });
            }
            Setting::Folder => {
                self.overlay = None;
                self.change_folder_dialog();
            }
        }
    }

    fn adjust_sidebar_width(&mut self, wider: bool) {
        self.sidebar_state = sidebar::update_state(|state| {
            state.sidebar_width = sidebar::step_sidebar_width(state.sidebar_width, wider);
        });
        if let Some(ctl) = &self.pane_ctl {
            ctl.resize_preferred(
                self.last_width,
                self.sidebar_state.sidebar_width,
                self.sidebar_state.dock_right,
            );
        }
    }

    /// `←`/`→` on the Terminal height row: step the persisted default and size
    /// THIS tab's editor/terminal column right away. The setting is a default
    /// (the IDE layer's remembered per-project ratio still wins when a layout is
    /// built); applying it here just makes the row feel immediate.
    fn adjust_terminal_height(&mut self, taller: bool) {
        // Step from the STORED value: a failed apply leaves the tab's layout
        // alone, so re-measuring it every press would land on the same two
        // values forever.
        let seed = match self.sidebar_state.terminal_height {
            Some(rows) => rows,
            None => self
                .pane_ctl
                .as_ref()
                .and_then(|ctl| ctl.terminal_rows_now())
                .unwrap_or(sidebar::SEED_TERMINAL_HEIGHT),
        };
        let rows = sidebar::step_terminal_height(seed, taller);
        self.sidebar_state = sidebar::update_state(|state| state.terminal_height = Some(rows));
        let applied = self
            .pane_ctl
            .as_ref()
            .is_some_and(|ctl| ctl.set_terminal_rows(rows));
        if !applied {
            // Keep the stored value: it still applies to tabs built later.
            self.flash = Some((
                "Terminal height: this tab has no editor/terminal split".to_string(),
                false,
            ));
        }
    }

    /// `⏎` on the Terminal height row: back to "auto", so a built layout uses
    /// the IDE layer's own default again. The current layout is left alone.
    fn clear_terminal_height(&mut self) {
        self.sidebar_state = sidebar::update_state(|state| state.terminal_height = None);
    }

    /// `←`/`→` on the Agent width row: step the persisted default and size THIS
    /// tab's Agent column right away. The setting is a default (the IDE layer's
    /// remembered per-project ratio still wins when a layout is built); applying
    /// it here just makes the row feel immediate.
    fn adjust_agent_width(&mut self, wider: bool) {
        // Step from the STORED value: a failed apply leaves the tab's layout
        // alone, so re-measuring it every press would land on the same two
        // values forever.
        let seed = match self.sidebar_state.agent_width {
            Some(cols) => cols,
            None => self
                .pane_ctl
                .as_ref()
                .and_then(|ctl| ctl.agent_cols_now())
                .unwrap_or(sidebar::SEED_AGENT_WIDTH),
        };
        let cols = sidebar::step_agent_width(seed, wider);
        self.sidebar_state = sidebar::update_state(|state| state.agent_width = Some(cols));
        let applied = self
            .pane_ctl
            .as_ref()
            .is_some_and(|ctl| ctl.set_agent_cols(cols));
        if !applied {
            // Keep the stored value: it still applies to tabs built later.
            self.flash = Some((
                "Agent width: this tab has no Agent column".to_string(),
                false,
            ));
        }
    }

    /// `⏎` on the Agent width row: back to "auto", so a built layout uses the
    /// IDE layer's own default again. The current layout is left alone.
    fn clear_agent_width(&mut self) {
        self.sidebar_state = sidebar::update_state(|state| state.agent_width = None);
    }

    /// The NATIVE folder picker on a background thread (the pane's liveness
    /// heartbeat must keep beating while the dialog is open).
    #[cfg(any(windows, target_os = "macos"))]
    fn change_folder_dialog(&mut self) {
        if self.picking.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let start = self.cwd.clone();
        std::thread::spawn(move || {
            let _ = tx.send(herdr_sidebar::actions::pick_folder(&start));
        });
        self.picking = Some(rx);
        self.flash = Some((
            "folder picker open… (check your other windows)".into(),
            false,
        ));
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn change_folder_dialog(&mut self) {
        self.flash = Some((
            "no native picker here — use c in the Files view".into(),
            true,
        ));
    }

    /// Collect a finished folder pick, if any (called from the tick loop).
    pub fn poll_picker(&mut self) {
        let Some(rx) = &self.picking else { return };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.picking = None;
                if std::env::set_current_dir(&path).is_ok() {
                    let root = std::env::current_dir().unwrap_or(path);
                    if self.sidebar_state.follow_cwd {
                        self.cwd_follower.borrow_mut().mark_manual_folder();
                    }
                    let cwd_follower = std::rc::Rc::clone(&self.cwd_follower);
                    *self = App::new(root, cwd_follower);
                } else {
                    self.flash = Some((format!("cannot open {}", path.display()), true));
                }
            }
            Ok(None) => {
                self.picking = None;
                self.flash = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(_) => self.picking = None,
        }
    }

    /// Render the centered Settings popup and remember its rect for clicks.
    fn draw_settings(&mut self, frame: &mut Frame) {
        let rows = self.settings_rows();
        let area = frame.area();
        let desired_width = rows
            .iter()
            .map(|(_, label, value, _)| label.chars().count() + value.chars().count() + 5)
            .max()
            .unwrap_or(30)
            .max(30) as u16;
        let width = desired_width.min(area.width);
        // The hotkey reference lives here now; the footer chips are opt-in.
        let hint_lines = wrap_hints(&self.hints(), width.saturating_sub(2), 0);
        let Some(Overlay::Settings {
            selected,
            rect,
            scroll,
        }) = self.overlay.as_mut()
        else {
            return;
        };
        let height = (rows.len() as u16 + 5 + hint_lines.len() as u16).min(area.height);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        *rect = popup;
        let inner_height = usize::from(height.saturating_sub(2));
        let content_height = rows.len() + 3 + hint_lines.len();
        *scroll = keep_visible_scroll(*selected, inner_height, content_height);

        let inner_w = usize::from(width.saturating_sub(2));
        let mut lines: Vec<Line> = Vec::new();
        for (i, (_, label, value, enabled)) in rows.iter().enumerate() {
            let pad = inner_w.saturating_sub(label.chars().count() + value.chars().count() + 2);
            let text = format!(" {label}{}{value} ", " ".repeat(pad.max(1)));
            let style = if !enabled {
                Style::default().dim()
            } else if i == *selected {
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
            Paragraph::new(lines).scroll((*scroll as u16, 0)).block(
                Block::bordered()
                    .title(" Settings ")
                    .border_style(Style::default().dim()),
            ),
            popup,
        );
    }

    fn activate_menu_entry(&mut self) {
        let Some(Overlay::Menu {
            target,
            entries,
            selected,
            ..
        }) = self.overlay.take()
        else {
            return;
        };
        let MenuEntry::Action(action, _) = entries[selected] else {
            return;
        };
        if let MenuAction::Herdr(item) = action {
            self.run_pane_item(item);
            return;
        }
        match target {
            MenuTarget::File {
                repo,
                entry,
                staged,
            } => self.file_menu_action(action, repo, entry, staged),
            MenuTarget::Drawer { kind, index } => self.drawer_menu_action(action, kind, index),
            MenuTarget::Dir { repo, path, staged } => {
                self.dir_menu_action(action, repo, path, staged)
            }
        }
    }

    /// The tree view's folder menu: the keyboard way (`m`) to reach the
    /// stage/unstage button that is otherwise hover-only.
    fn dir_menu_action(&mut self, action: MenuAction, repo: usize, path: String, staged: bool) {
        let root = self.repos.get(repo).map(|r| r.git.root().to_path_buf());
        let rel = path.replace('/', std::path::MAIN_SEPARATOR_STR);
        match action {
            MenuAction::StageOrUnstage if staged => self.unstage_dir(repo, &path),
            MenuAction::StageOrUnstage => self.stage_dir(repo, &path),
            MenuAction::CopyPath | MenuAction::CopyRelativePath => {
                let text = if action == MenuAction::CopyPath {
                    root.unwrap_or_else(|| self.cwd.clone())
                        .join(&rel)
                        .display()
                        .to_string()
                } else {
                    rel
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(ClipboardWrite::Native) => (format!("copied: {text}"), false),
                    Ok(ClipboardWrite::Osc52Unacknowledged) => {
                        (format!("sent to terminal clipboard: {text}"), false)
                    }
                    Err(err) => (format!("copy failed: {err}"), true),
                });
            }
            MenuAction::Reveal => {
                reveal(&root.unwrap_or_else(|| self.cwd.clone()).join(rel), true);
            }
            _ => {}
        }
    }

    /// ←/→ in the tree view: collapse/expand the selected folder; ← on a
    /// file moves to its folder, like VS Code's tree.
    fn fold_selected(&mut self, expand: bool) {
        let Some(index) = self.selected else { return };
        let dir = |row: Row| match row {
            Row::StagedDir(r, d) => Some((r, d, true)),
            Row::ChangesDir(r, d) => Some((r, d, false)),
            _ => None,
        };
        if let Some((r, d, staged)) = self.rows.get(index).copied().and_then(dir) {
            let Some(repo) = self.repos.get(r) else {
                return;
            };
            let dirs = if staged {
                &repo.staged_dirs
            } else {
                &repo.changes_dirs
            };
            if dirs.get(d).is_some_and(|entry| entry.expanded != expand) {
                if staged {
                    self.toggle_staged_dir(r, d);
                } else {
                    self.toggle_changes_dir(r, d);
                }
            }
            return;
        }
        if expand || !self.tree_view {
            return;
        }
        let depth_of = |row: &Row| match *row {
            Row::StagedDir(r, d) => self
                .repos
                .get(r)
                .and_then(|x| x.staged_dirs.get(d))
                .map(|e| e.depth),
            Row::ChangesDir(r, d) => self
                .repos
                .get(r)
                .and_then(|x| x.changes_dirs.get(d))
                .map(|e| e.depth),
            Row::Staged(r, i) => self
                .repos
                .get(r)
                .and_then(|x| x.staged_file_depths.get(i))
                .copied(),
            Row::Unstaged(r, i) => self
                .repos
                .get(r)
                .and_then(|x| x.changes_file_depths.get(i))
                .copied(),
            _ => None,
        };
        let Some(depth) = self.rows.get(index).and_then(depth_of) else {
            return;
        };
        if let Some(parent) = (0..index).rev().find(|&i| {
            dir(self.rows[i]).is_some() && depth_of(&self.rows[i]).is_some_and(|d| d < depth)
        }) {
            self.select(parent);
        }
    }

    fn file_menu_action(
        &mut self,
        action: MenuAction,
        repo: usize,
        entry: FileEntry,
        staged: bool,
    ) {
        let repo_root = self.repos.get(repo).map(|r| r.git.root().to_path_buf());
        match action {
            MenuAction::StageOrUnstage => {
                let result = match self.repos.get(repo) {
                    Some(r) if staged => r.git.unstage(&entry),
                    Some(r) => r.git.stage(&entry),
                    None => Err("repository is gone".to_string()),
                };
                if let Err(e) = result {
                    self.flash = Some((e, true));
                }
                self.refresh();
            }
            MenuAction::OpenDiff => self.open_diff(repo, &entry, staged),
            MenuAction::OpenDiffPreview => self.open_diff_viewer(repo, &entry, staged),
            // `o` and a click on a changed row show the diff (read-only); this is
            // the explicit "take me to the file" that hands it to the editor.
            MenuAction::OpenInEditor => {
                let root = repo_root.unwrap_or_else(|| self.cwd.clone());
                let file = root.join(&entry.path);
                let pane_id = self.pane_ctl.as_ref().map(|c| c.pane_id.clone());
                if let Err(error) = herdr_sidebar::file_popup::open_file_in_workspace(
                    pane_id.as_deref(),
                    &root,
                    &file,
                    None,
                ) {
                    self.flash = Some((error, true));
                }
            }
            MenuAction::Discard => self.overlay = Some(Overlay::ConfirmDiscard { repo, entry }),
            MenuAction::CopyPath | MenuAction::CopyRelativePath => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let text = if action == MenuAction::CopyPath {
                    repo_root
                        .unwrap_or_else(|| self.cwd.clone())
                        .join(&rel)
                        .display()
                        .to_string()
                } else {
                    rel
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(ClipboardWrite::Native) => (format!("copied: {text}"), false),
                    Ok(ClipboardWrite::Osc52Unacknowledged) => {
                        (format!("sent to terminal clipboard: {text}"), false)
                    }
                    Err(err) => (format!("copy failed: {err}"), true),
                });
            }
            MenuAction::Reveal => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let path = repo_root.unwrap_or_else(|| self.cwd.clone()).join(rel);
                reveal(&path, false);
            }
            MenuAction::OpenExternal => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let path = repo_root.unwrap_or_else(|| self.cwd.clone()).join(&rel);
                self.flash = Some(match open_external(&path) {
                    Ok(()) => (format!("opened: {rel}"), false),
                    Err(err) => (format!("open failed: {err}"), true),
                });
            }
            _ => {}
        }
    }

    fn drawer_menu_action(&mut self, action: MenuAction, kind: Drawer, index: usize) {
        let Some(dref) = self.drawers[kind.index()].refs.get(index).cloned() else {
            return;
        };
        let repo = self.active;
        let spec = match &dref {
            DrawerRef::Commit(h) => h.clone(),
            DrawerRef::Stash(n) => format!("stash@{{{n}}}"),
            DrawerRef::Branch { name, .. } => name.clone(),
            DrawerRef::Remote { name, .. } => name.clone(),
            DrawerRef::Tag(t) => t.clone(),
            DrawerRef::Worktree(p) => p.clone(),
            DrawerRef::None => return,
        };
        let git_spec = drawer_git_spec(&dref).unwrap_or_else(|| spec.clone());
        match action {
            MenuAction::ShowRef => self.open_drawer_ref(kind, index),
            MenuAction::Reveal => reveal(std::path::Path::new(&spec), true),
            MenuAction::RemoveWorktree => self.confirm_git(
                repo,
                "Remove worktree",
                format!("Remove worktree '{spec}'?"),
                vec!["worktree".into(), "remove".into(), "--".into(), spec],
            ),
            MenuAction::CopyRef => {
                let text = match &dref {
                    DrawerRef::Remote { url, .. } if !url.is_empty() => url.clone(),
                    _ => spec,
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(ClipboardWrite::Native) => (format!("copied: {text}"), false),
                    Ok(ClipboardWrite::Osc52Unacknowledged) => {
                        (format!("sent to terminal clipboard: {text}"), false)
                    }
                    Err(err) => (format!("copy failed: {err}"), true),
                });
            }
            MenuAction::Checkout => {
                if let Some(args) = drawer_checkout_args(&dref, &spec, &git_spec) {
                    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
                    self.run_git(repo, &args);
                }
            }
            MenuAction::MergeInto => {
                self.run_git(repo, &["merge", "--no-edit", "--", &git_spec]);
            }
            MenuAction::CherryPick => self.run_git(repo, &["cherry-pick", "--", &git_spec]),
            MenuAction::Revert => {
                self.run_git(repo, &["revert", "--no-edit", "--", &git_spec]);
            }
            MenuAction::StashApply => self.run_git(repo, &["stash", "apply", "--", &spec]),
            MenuAction::StashPop => self.run_git(repo, &["stash", "pop", "--", &spec]),
            MenuAction::FetchRemote => self.run_git(repo, &["fetch", "--", &spec]),
            MenuAction::ResetHere => self.confirm_git(
                repo,
                "Reset branch",
                format!("Reset current branch to {spec} (mixed)?"),
                drawer_reset_args(&git_spec),
            ),
            MenuAction::DeleteBranch => self.confirm_git(
                repo,
                "Delete branch",
                format!("Delete branch '{spec}'?"),
                vec!["branch".into(), "-d".into(), "--".into(), spec],
            ),
            MenuAction::StashDrop => self.confirm_git(
                repo,
                "Drop stash",
                format!("Drop {spec}?"),
                vec!["stash".into(), "drop".into(), "--".into(), spec],
            ),
            MenuAction::RenameBranch => {
                if matches!(dref, DrawerRef::Branch { .. }) {
                    self.overlay = Some(Overlay::BranchRename {
                        repo,
                        from: spec.clone(),
                        input: spec.clone(),
                        select_all: SelectAll::default(),
                    });
                }
            }
            MenuAction::RewordCommit => {
                if let DrawerRef::Commit(h) = &dref {
                    match self.repos.get(repo).map(|r| r.git.commit_message(h)) {
                        Some(Ok(raw)) => {
                            let (subject, rest) =
                                raw.split_once('\n').unwrap_or((raw.as_str(), ""));
                            let body = rest.trim_start_matches('\n').trim_end().to_string();
                            self.overlay = Some(Overlay::CommitReword {
                                repo,
                                commit: h.clone(),
                                body,
                                input: subject.to_string(),
                                select_all: SelectAll::default(),
                            });
                        }
                        Some(Err(error)) => self.flash = Some((error, true)),
                        None => self.flash = Some(("repository is gone".to_string(), true)),
                    }
                }
            }
            MenuAction::UndoLastCommit => self.confirm_git(
                repo,
                "Undo last commit",
                format!("Undo the last commit on '{spec}'? Its changes stay staged."),
                vec!["reset".into(), "--soft".into(), "HEAD~1".into()],
            ),
            MenuAction::DeleteTag => self.confirm_git(
                repo,
                "Delete tag",
                format!("Delete tag '{spec}'?"),
                vec!["tag".into(), "-d".into(), "--".into(), spec],
            ),
            _ => {}
        }
    }

    /// Run a git op for `repo`, flash the outcome, refresh everything (merge
    /// conflicts and the like surface as the flashed git error).
    fn run_git(&mut self, repo: usize, args: &[&str]) {
        let result = match self.repos.get(repo) {
            Some(r) => r.git.raw(args),
            None => Err("repository is gone".to_string()),
        };
        match result {
            Ok(_) => self.flash = Some((format!("git {} ✓", args.join(" ")), false)),
            Err(e) => self.flash = Some((e, true)),
        }
        self.refresh();
        // The command may have moved refs without touching the status.
        self.refresh_refs();
    }

    /// Open the y/N dialog for a git command: `title` names the action on the
    /// border, `prompt` is the question in the body.
    fn confirm_git(&mut self, repo: usize, title: &str, prompt: String, args: Vec<String>) {
        self.overlay = Some(Overlay::ConfirmGit {
            repo,
            title: title.to_string(),
            prompt,
            args,
        });
    }

    /// Rename a local branch from the Branches drawer — any of them, not only the
    /// checked-out one — reporting git's own refusal verbatim.
    fn confirm_branch_rename(&mut self, repo: usize, from: String, new: String) {
        let new = new.trim().to_string();
        if new.is_empty() || new == from {
            return;
        }
        let Some(git) = self.repos.get(repo).map(|r| r.git.clone()) else {
            self.flash = Some(("repository is gone".to_string(), true));
            return;
        };
        match git.rename_branch(&from, &new) {
            Ok(()) => {
                self.flash = Some((format!("renamed {from} -> {new}"), false));
                self.refresh();
                self.refresh_refs();
            }
            Err(error) => self.flash = Some((error, true)),
        }
    }

    /// Reword a commit's subject; the body is kept verbatim. The rewrite never
    /// touches the index or work tree, so the only outcomes are done or refused.
    fn confirm_commit_reword(&mut self, repo: usize, commit: String, body: String, input: String) {
        let new = input.trim().to_string();
        if new.is_empty() {
            self.flash = Some(("Commit message is empty.".to_string(), true));
            return;
        }
        let message = if body.is_empty() {
            new
        } else {
            format!("{new}\n\n{body}")
        };
        let Some(git) = self.repos.get(repo).map(|r| r.git.clone()) else {
            self.flash = Some(("repository is gone".to_string(), true));
            return;
        };
        match git.reword_commit(&commit, &message) {
            Ok(done) if done.old == done.new => {
                self.flash = Some(("message unchanged".to_string(), false));
            }
            Ok(done) => {
                let mut text = format!("reworded {} -> {}", done.old, done.new);
                if done.pushed {
                    text.push_str(" (already pushed: force-push to update the remote)");
                }
                self.flash = Some((text, false));
                self.refresh();
                self.refresh_refs();
            }
            Err(error) => self.flash = Some((error, true)),
        }
    }

    /// The confirmed non-forcing delete. Unmerged commits are the one refusal
    /// worth re-asking about, and only here can the question name what would
    /// be lost; the forced retry is an ordinary confirmed `branch -D`.
    fn delete_branch(&mut self, repo: usize, branch: String) {
        let git = match self.repos.get(repo) {
            Some(r) => r.git.clone(),
            None => {
                self.flash = Some(("repository is gone".to_string(), true));
                return;
            }
        };
        match git.delete_branch(&branch) {
            Ok(()) => {
                self.flash = Some((format!("deleted {branch}"), false));
                self.refresh();
                self.refresh_refs();
            }
            Err(DeleteRefused::Failed(error)) => {
                self.flash = Some((error, true));
                self.refresh();
                self.refresh_refs();
            }
            Err(DeleteRefused::NotMerged) => self.confirm_git(
                repo,
                "Force delete branch",
                format!("'{branch}' is not fully merged — delete anyway and lose its commits?"),
                vec!["branch".into(), "-D".into(), "--".into(), branch],
            ),
        }
    }

    fn run_changes_header_action(&mut self, repo: usize, action: ChangesHeaderAction) {
        match action {
            ChangesHeaderAction::Discard => {
                self.overlay = Some(Overlay::ConfirmDiscardAll { repo });
            }
            ChangesHeaderAction::Stash => {
                self.run_git(repo, &["stash", "push", "--include-untracked"]);
            }
            ChangesHeaderAction::Stage => {
                if let Some(repo) = self.repos.get(repo)
                    && let Err(error) = repo.git.stage_all()
                {
                    self.flash = Some((error, true));
                }
                self.refresh();
            }
        }
    }

    fn discard_all(&mut self, repo: usize) {
        let Some(repo) = self.repos.get(repo) else {
            self.flash = Some(("repository is gone".to_string(), true));
            return;
        };
        let entries = repo.status.unstaged.clone();
        let mut errors = Vec::new();
        for entry in &entries {
            if let Err(error) = repo.git.discard(entry) {
                errors.push(format!("{}: {error}", entry.path));
            }
        }
        self.flash = Some(if errors.is_empty() {
            (format!("discarded {} changes", entries.len()), false)
        } else {
            (errors.join("; "), true)
        });
        self.refresh();
    }

    /// Click/⏎ on a drawer line: show the commit / stash / tag / branch tip
    /// in the preview pane (scrollable colored `git show`).
    fn open_drawer_ref(&mut self, kind: Drawer, index: usize) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.flash = Some(("preview needs a herdr pane".into(), true));
            return;
        };
        let Some(repo) = self.repos.get(self.active) else {
            return;
        };
        let Some(spec) = self.drawers[kind.index()]
            .refs
            .get(index)
            .and_then(drawer_git_spec)
        else {
            return;
        };
        let path = (kind == Drawer::FileHistory)
            .then(|| self.history_target.clone())
            .flatten();
        let payload = herdr_sidebar::viewer::show_request(repo.git.root(), &spec, path.as_deref());
        let doc_key =
            herdr_sidebar::viewer::doc_key_for_show(repo.git.root(), &spec, path.as_deref());
        match herdr_sidebar::viewer::open_in_pane(&pane_id, repo.git.root(), &doc_key, &payload) {
            Ok(target) => {
                self.last_preview = Some((doc_key, target));
            }
            Err(e) => {
                self.flash = Some((e, true));
            }
        }
    }

    /// Open a changed file's diff. With the custom editor opted in (inline
    /// mode, on click) the file opens in that editor with
    /// `HERDR_SIDEBAR_OPEN_DIFF=1`; otherwise, or when the editor cannot be
    /// launched, the diff goes to the preview pane.
    /// The default action on a changed file (click, hover "open", `o`). A
    /// conflicted file opens in the editor so Fresh's inline merge resolver can
    /// take over; anything else shows the read-only diff preview.
    fn open_entry(&mut self, repo: usize, entry: &FileEntry, staged: bool) {
        if !opens_in_editor(entry) {
            self.open_diff(repo, entry, staged);
            return;
        }
        let Some(root) = self.repos.get(repo).map(|r| r.git.root().to_path_buf()) else {
            return;
        };
        let pane_id = self.pane_ctl.as_ref().map(|c| c.pane_id.clone());
        let file = root.join(&entry.path);
        // A conflict can also be a deletion ("deleted by them"): there is no
        // file to resolve in the editor, so that case keeps the diff.
        if !file.exists() {
            self.open_diff(repo, entry, staged);
            return;
        }
        if let Err(error) = herdr_sidebar::file_popup::open_file_in_workspace(
            pane_id.as_deref(),
            &root,
            &file,
            None,
        ) {
            self.flash = Some((error, true));
        }
    }

    /// Show a changed file's diff. This is READ-ONLY viewing, so it always uses
    /// the built-in preview: routing it to the custom editor made "look at this
    /// diff" hand the file to the editor instead, where nothing (or a live-diff
    /// overlay that needs its own plugin) appeared, and the preview it displaced
    /// had to be closed again (user-reported). Opening a FILE - tree click, Quick
    /// Open, a search hit - still goes to the editor when one is configured.
    /// A conflicted entry never reaches here from a click or `o` (see
    /// `open_entry`): it opens in the editor so Fresh can resolve the merge.
    fn open_diff(&mut self, repo: usize, entry: &FileEntry, staged: bool) {
        self.open_diff_viewer(repo, entry, staged);
    }

    /// Show a file's diff in the preview pane beside the sidebar. Staged
    /// rows show the staged diff; untracked files render as one addition.
    fn open_diff_viewer(&mut self, repo: usize, entry: &FileEntry, staged: bool) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.flash = Some(("diff preview needs a herdr pane".into(), true));
            return;
        };
        let Some(repo) = self.repos.get(repo) else {
            return;
        };
        let kind = if staged {
            "staged"
        } else if entry.letter == 'U' {
            "untracked"
        } else {
            "worktree"
        };
        let payload = herdr_sidebar::viewer::diff_request(repo.git.root(), &entry.path, kind);
        let doc_key = herdr_sidebar::viewer::doc_key_for_diff(repo.git.root(), &entry.path, kind);
        match herdr_sidebar::viewer::open_in_pane(&pane_id, repo.git.root(), &doc_key, &payload) {
            Ok(target) => {
                self.last_preview = Some((doc_key, target));
            }
            Err(e) => {
                self.flash = Some((e, true));
            }
        }
    }

    /// The document key a click on `index` would open, for the rows that
    /// preview (changed files and drawer refs). `None` for non-preview rows.
    fn doc_key_for_row(&self, index: usize) -> Option<String> {
        match self.rows.get(index)? {
            // A file row opens in the editor, so there is no preview document to
            // pin a double click to; the deleted entries below still diff.
            Row::Staged(r, i) => {
                let repo = self.repos.get(*r)?;
                let entry = repo.status.staged.get(*i)?;
                if opens_in_editor(entry) {
                    return None;
                }
                Some(herdr_sidebar::viewer::doc_key_for_diff(
                    repo.git.root(),
                    &entry.path,
                    "staged",
                ))
            }
            Row::Unstaged(r, i) => {
                let repo = self.repos.get(*r)?;
                let entry = repo.status.unstaged.get(*i)?;
                if opens_in_editor(entry) {
                    return None;
                }
                let kind = if entry.letter == 'U' {
                    "untracked"
                } else {
                    "worktree"
                };
                Some(herdr_sidebar::viewer::doc_key_for_diff(
                    repo.git.root(),
                    &entry.path,
                    kind,
                ))
            }
            Row::DrawerLine(kind, i) => {
                let repo = self.repos.get(self.active)?;
                let spec = drawer_git_spec(self.drawers[kind.index()].refs.get(*i)?)?;
                let path = (*kind == Drawer::FileHistory)
                    .then(|| self.history_target.clone())
                    .flatten();
                Some(herdr_sidebar::viewer::doc_key_for_show(
                    repo.git.root(),
                    &spec,
                    path.as_deref(),
                ))
            }
            _ => None,
        }
    }

    /// On a double click, pin the tab the first click opened — if this row's
    /// document is the one currently previewing. Returns true when it pinned.
    fn pin_if_open(&mut self, index: usize) -> bool {
        let Some(doc_key) = self.doc_key_for_row(index) else {
            return false;
        };
        let Some((key, target)) = self.last_preview.as_ref() else {
            return false;
        };
        if *key != doc_key {
            return false;
        }
        herdr_sidebar::viewer::pin_target(target, &doc_key)
    }

    /// A stable, repo-independent id for `index`'s row — survives a rebuild
    /// so a fresh sidebar can re-find the row the user had selected. `None`
    /// for non-selectable widget rows and blank graph-edge lines.
    fn row_stable_id(&self, index: usize) -> Option<String> {
        let row = self.rows.get(index)?;
        match row {
            Row::RepoHeader(r) => self
                .repos
                .get(*r)
                .map(|r| format!("repo:{}", r.git.root().display())),
            Row::StagedHeader(r) => self
                .repos
                .get(*r)
                .map(|r| format!("staged-h:{}", r.git.root().display())),
            Row::ChangesHeader(r) => self
                .repos
                .get(*r)
                .map(|r| format!("changes-h:{}", r.git.root().display())),
            Row::StagedDir(r, d) => self.repos.get(*r).and_then(|repo| {
                repo.staged_dirs
                    .get(*d)
                    .map(|dir| format!("staged-dir:{}:{}", repo.git.root().display(), dir.path))
            }),
            Row::ChangesDir(r, d) => self.repos.get(*r).and_then(|repo| {
                repo.changes_dirs
                    .get(*d)
                    .map(|dir| format!("changes-dir:{}:{}", repo.git.root().display(), dir.path))
            }),
            Row::Staged(r, i) => self.repos.get(*r).and_then(|repo| {
                repo.status
                    .staged
                    .get(*i)
                    .map(|entry| format!("staged:{}:{}", repo.git.root().display(), entry.path))
            }),
            Row::Unstaged(r, i) => self.repos.get(*r).and_then(|repo| {
                repo.status
                    .unstaged
                    .get(*i)
                    .map(|entry| format!("unstaged:{}:{}", repo.git.root().display(), entry.path))
            }),
            Row::DrawerHeader(kind) => Some(format!("drawer-h:{}", kind.title())),
            Row::DrawerLine(kind, i) => self.drawers[kind.index()]
                .refs
                .get(*i)
                .and_then(drawer_spec)
                .map(|s| format!("drawer:{}:{}", kind.title(), s)),
            Row::Message(_) | Row::Commit(_) => None,
        }
    }

    /// Inverse of [`row_stable_id`]: the row whose id matches, if any.
    fn find_row_by_stable_id(&self, id: &str) -> Option<usize> {
        (0..self.rows.len()).find(|&i| self.row_stable_id(i).as_deref() == Some(id))
    }

    /// A snapshot of the view state worth mirroring into a new tab.
    fn snapshot_scm(&self) -> sidebar::ScmState {
        let drawers = Drawer::ALL
            .iter()
            .filter(|k| self.drawers[k.index()].expanded)
            .map(|k| k.title().to_string())
            .collect();
        let active_root = self
            .repos
            .get(self.active)
            .map(|r| sidebar::scm_path_key(r.git.root()));
        let selected = self.selected.and_then(|i| self.row_stable_id(i));
        let drafts = self
            .repos
            .iter()
            .filter(|repo| !repo.message.is_empty())
            .map(|repo| {
                (
                    sidebar::scm_path_key(repo.git.root()),
                    repo.message.iter().collect(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let visible_roots = self
            .repos
            .iter()
            .map(|repo| sidebar::scm_path_key(repo.git.root()))
            .collect::<std::collections::BTreeSet<_>>();
        let cleared_drafts = self
            .persisted_draft_roots
            .iter()
            .filter(|root| visible_roots.contains(*root) && !drafts.contains_key(*root))
            .cloned()
            .collect();
        let mut collapsed_dirs = Vec::new();
        for repo in &self.repos {
            collapsed_dirs.extend(repo.staged_collapsed_dirs.iter().map(|path| {
                serde_json::to_string(&(repo.git.root().to_string_lossy(), true, path)).unwrap()
            }));
            collapsed_dirs.extend(repo.changes_collapsed_dirs.iter().map(|path| {
                serde_json::to_string(&(repo.git.root().to_string_lossy(), false, path)).unwrap()
            }));
        }
        sidebar::ScmState {
            drawers,
            active_root,
            selected,
            history_target: self.history_target.clone(),
            scroll: self.scroll,
            drafts,
            cleared_drafts,
            collapsed_dirs,
        }
    }

    /// Persist the view state for the next sidebar started in this cwd.
    pub fn persist_scm(&mut self) -> bool {
        let snapshot = self.snapshot_scm();
        let mut saved = sidebar::save_scm_state(&self.cwd, &snapshot);
        for repo in &self.repos {
            if repo.git.root() != self.cwd {
                saved &= sidebar::save_scm_state(repo.git.root(), &snapshot);
            }
        }
        if saved {
            self.persisted_draft_roots
                .retain(|root| !snapshot.cleared_drafts.contains(root));
            self.persisted_draft_roots
                .extend(snapshot.drafts.keys().cloned());
        } else {
            self.flash = Some((
                "Could not save Source Control state; action cancelled.".into(),
                true,
            ));
        }
        saved
    }

    /// `o` on the selected file row: the same routing a click uses, so a
    /// conflicted file opens in the editor and everything else as a diff.
    fn open_selected_entry(&mut self) {
        let Some(&row) = self.selected.and_then(|i| self.rows.get(i)) else {
            return;
        };
        match row {
            Row::Staged(r, i) => {
                if let Some(entry) = self.repos[r].status.staged.get(i).cloned() {
                    self.open_entry(r, &entry, true);
                }
            }
            Row::Unstaged(r, i) => {
                if let Some(entry) = self.repos[r].status.unstaged.get(i).cloned() {
                    self.open_entry(r, &entry, false);
                }
            }
            other => {
                if let Some(hint) = section_row_hint(other) {
                    self.flash = Some((hint.into(), false));
                }
            }
        }
    }

    // ---- Unified-sidebar operations ----

    /// Toggle the unified sidebar. On: adopt this pane as the Sidebar and
    /// close the other panel's standalone pane in this tab. Off: split the
    /// other view back out into its own pane. Deliberately silent — the
    /// layout change is its own feedback.
    fn set_unified(&mut self, on: bool) {
        if on == self.merged() || self.other_exe.is_none() {
            return;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.merged = on;
            state.active = MY_VIEW;
        });
        self.apply_identity();
        if on {
            let width = self.last_width;
            match self.close_other_standalone_pane() {
                Ok(true) => {
                    self.pending_unified_width = Some((width, std::time::Instant::now()));
                }
                Ok(false) => {}
                Err(error) => {
                    self.sidebar_state = sidebar::update_state(|state| {
                        state.merged = false;
                        state.active = MY_VIEW;
                    });
                    self.apply_identity();
                    self.flash = Some((format!("unified mode cancelled: {error}"), true));
                }
            }
        } else {
            self.spawn_other_pane();
        }
    }

    /// Hand the pane to the other view (the supervisor swaps processes).
    fn switch_to(&mut self, view: View) -> Option<Exit> {
        if !self.merged() || view == MY_VIEW {
            return None;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.active = view;
            state.search_active = false;
        });
        Some(Exit::Switch)
    }

    fn open_search(&mut self, focus_query: bool) -> Option<Exit> {
        if !self.merged() {
            return None;
        }
        self.sidebar_state = sidebar::update_state(|state| {
            state.active = View::Explorer;
            state.search_active = true;
        });
        Some(Exit::Search { focus_query })
    }

    /// Close the other panel's standalone pane in our tab, if one is open.
    fn close_other_standalone_pane(&self) -> std::io::Result<bool> {
        let Some(ctl) = &self.pane_ctl else {
            return Ok(false);
        };
        let json = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({}))?;
        let ids = sibling_panes_of(&json, &ctl.pane_id, MY_VIEW.other());
        if ids.is_empty() {
            return Ok(false);
        }
        let mut failure = None;
        for id in ids {
            if let Err(error) = herdr_sidebar::ensure::request_close(&json, &id) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(true), Err)
    }

    /// Open the other view in a fresh pane beside this one (detach).
    fn spawn_other_pane(&self) {
        let (Some(ctl), Some(_)) = (&self.pane_ctl, &self.other_exe) else {
            return;
        };
        let Some(_lock) = herdr_sidebar::ensure::LaunchLock::acquire(true) else {
            return;
        };
        // Grow to double width FIRST, then split 50/50 — each separated panel
        // keeps the width the unified sidebar had, instead of halving.
        ctl.resize_to(
            self.last_width,
            self.last_width.saturating_mul(2).saturating_add(1),
            self.sidebar_state.dock_right,
        );
        let other = MY_VIEW.other();
        #[cfg(unix)]
        let _ = herdr_sidebar::ipc::open_plugin_pane(&ctl.pane_id, other, &self.cwd, false, None);
        #[cfg(windows)]
        {
            let response = herdr_sidebar::ipc::call_text(
                "pane.split",
                serde_json::json!({
                    "target_pane_id": ctl.pane_id,
                    "direction": "right",
                    "ratio": 0.5,
                    "focus": false,
                    "cwd": self.cwd.display().to_string(),
                    "env": sidebar::spawn_env(),
                }),
            );
            let Some(new_pane) = response
                .ok()
                .and_then(|r| herdr_sidebar::launch::split_pane_id(&r))
            else {
                return;
            };
            if herdr_sidebar::ipc::report_starting_identity(&new_pane, other, false).is_err() {
                let _ = herdr_sidebar::ipc::call_text(
                    "pane.close",
                    serde_json::json!({ "pane_id": new_pane }),
                );
                return;
            }
            let flag = other.view_flag();
            let command = format!("{} --view {flag}", sidebar::EXECUTABLE_NAME);
            let _ = herdr_sidebar::ipc::call_text(
                "pane.send_input",
                serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
            );
            let _ = herdr_sidebar::ipc::call_text(
                "pane.rename",
                serde_json::json!({ "pane_id": new_pane, "label": other.label() }),
            );
        }
    }

    // ---- Git operations ----

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.selected = Some(index.min(self.rows.len() - 1));
            self.snap = true;
            self.follow_selection();
        }
        self.persist_scm();
    }

    /// Wheel: move the VIEW only — the selection stays where it is.
    fn scroll_view(&mut self, delta: isize) {
        let max = if self.body.height == 0 {
            self.rows.len().saturating_sub(1)
        } else {
            self.max_scroll(self.body.height as usize)
        } as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
        self.persist_scm();
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        // First keyboard step on a selection-less list picks the first stop.
        let Some(sel) = self.selected else {
            let first = self.nearest_selectable(0);
            self.select(first);
            return;
        };
        let len = self.rows.len() as isize;
        let current = sel as isize;
        let step = if delta >= 0 { 1 } else { -1 };
        let mut next = (current + delta).clamp(0, len - 1);
        // Widget rows aren't keyboard stops: keep going in the same direction,
        // falling back to the nearest stop at the ends.
        while (0..len).contains(&next) && !self.rows[next as usize].selectable() {
            next += step;
        }
        let next = if (0..len).contains(&next) {
            next as usize
        } else {
            self.nearest_selectable((current + delta).clamp(0, len - 1) as usize)
        };
        self.select(next);
    }

    /// Enter/Space on the selected row: toggle a section/drawer, or move a
    /// file between the staged and unstaged lists.
    fn activate(&mut self) {
        let Some(&row) = self.selected.and_then(|i| self.rows.get(i)) else {
            return;
        };
        match row {
            // Widget rows aren't keyboard-selectable; nothing to activate.
            Row::Message(_) | Row::Commit(_) => {}
            Row::DrawerLine(kind, i) => self.open_drawer_ref(kind, i),
            Row::RepoHeader(r) => {
                self.repos[r].collapsed = !self.repos[r].collapsed;
                self.rebuild();
            }
            Row::StagedHeader(r) => {
                self.repos[r].staged_collapsed = !self.repos[r].staged_collapsed;
                self.rebuild();
            }
            Row::ChangesHeader(r) => {
                self.repos[r].changes_collapsed = !self.repos[r].changes_collapsed;
                self.rebuild();
            }
            Row::DrawerHeader(kind) => {
                self.drawers[kind.index()].expanded = !self.drawers[kind.index()].expanded;
                self.reload_expanded_drawers();
                self.rebuild();
            }
            Row::StagedDir(r, d) => self.toggle_staged_dir(r, d),
            Row::ChangesDir(r, d) => self.toggle_changes_dir(r, d),
            Row::Staged(r, i) => self.run_op(|git, e| git.unstage(e), r, i, true),
            Row::Unstaged(r, i) => self.run_op(|git, e| git.stage(e), r, i, false),
        }
        self.persist_scm();
    }

    fn run_op(
        &mut self,
        op: impl Fn(&Git, &FileEntry) -> Result<(), String>,
        repo: usize,
        index: usize,
        staged: bool,
    ) {
        let Some(repo) = self.repos.get(repo) else {
            return;
        };
        let list = if staged {
            &repo.status.staged
        } else {
            &repo.status.unstaged
        };
        let Some(entry) = list.get(index) else { return };
        if let Err(e) = op(&repo.git, entry) {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn stage_all(&mut self) {
        let Some(repo) = self.active_repo() else {
            return;
        };
        if let Err(e) = repo.git.stage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn unstage_all(&mut self) {
        let Some(repo) = self.active_repo() else {
            return;
        };
        if let Err(e) = repo.git.unstage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    pub fn toggle_tree_view(&mut self) {
        self.tree_view = !self.tree_view;
        self.sidebar_state = sidebar::update_state(|state| {
            state.scm_tree_view = self.tree_view;
        });
        self.rebuild();
        self.persist_scm();
    }

    fn toggle_staged_dir(&mut self, repo_idx: usize, dir_idx: usize) {
        let Some(repo) = self.repos.get_mut(repo_idx) else {
            return;
        };
        if let Some(dir) = repo.staged_dirs.get(dir_idx) {
            let path = dir.path.clone();
            if repo.staged_collapsed_dirs.contains(&path) {
                repo.staged_collapsed_dirs.remove(&path);
            } else {
                repo.staged_collapsed_dirs.insert(path);
            }
        }
        self.rebuild();
        self.persist_scm();
    }

    fn toggle_changes_dir(&mut self, repo_idx: usize, dir_idx: usize) {
        let Some(repo) = self.repos.get_mut(repo_idx) else {
            return;
        };
        if let Some(dir) = repo.changes_dirs.get(dir_idx) {
            let path = dir.path.clone();
            if repo.changes_collapsed_dirs.contains(&path) {
                repo.changes_collapsed_dirs.remove(&path);
            } else {
                repo.changes_collapsed_dirs.insert(path);
            }
        }
        self.rebuild();
        self.persist_scm();
    }

    /// Stage a tree-view folder. Delegates to [`Git::stage_under`], the same
    /// batched, nested-repository-safe path the Explorer uses: one process per
    /// 64 paths instead of per file (a large folder must not stall the event
    /// loop past the heartbeat limit), and an embedded repository is never
    /// recorded as a gitlink.
    fn stage_dir(&mut self, repo_idx: usize, dir_path: &str) {
        let Some(repo) = self.repos.get(repo_idx) else {
            return;
        };
        let target = repo
            .git
            .root()
            .join(dir_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        self.flash = Some(match repo.git.stage_under(&target) {
            Ok(done) if done.count == 0 && done.skipped_nested > 0 => (
                format!(
                    "{dir_path}: nothing staged — {} path(s) belong to a nested repo",
                    done.skipped_nested
                ),
                true,
            ),
            Ok(done) if done.skipped_nested > 0 => (
                format!(
                    "staged {} path(s) under {dir_path}; skipped {} in a nested repo",
                    done.count, done.skipped_nested
                ),
                false,
            ),
            Ok(done) => (
                format!("staged {} path(s) under {dir_path}", done.count),
                false,
            ),
            Err(error) => (error, true),
        });
        self.refresh();
    }

    fn unstage_dir(&mut self, repo_idx: usize, dir_path: &str) {
        let Some(repo) = self.repos.get(repo_idx) else {
            return;
        };
        self.flash = Some(match repo.git.unstage_under(dir_path) {
            Ok(count) => (format!("unstaged {count} path(s) under {dir_path}"), false),
            Err(error) => (error, true),
        });
        self.refresh();
    }

    /// Kick off ✧ commit-message generation in the background.
    fn suggest_message(&mut self) {
        if self.suggesting.is_some() {
            return;
        }
        let Some(repo) = self.active_repo() else {
            return;
        };
        match repo.git.diff_for_message() {
            Ok((diff, files)) if diff.trim().is_empty() && files.is_empty() => {
                self.flash = Some(("no changes to describe".into(), true));
            }
            Ok((diff, files)) => {
                self.suggesting = Some(suggest::spawn(diff, files));
                self.flash = Some(("✧ generating commit message…".into(), false));
            }
            Err(e) => self.flash = Some((e, true)),
        }
    }

    /// `git pull` (plain merge), then the commit that was waiting on it: see
    /// [`Self::commit_with_pull_offer`]. The pull runs on a worker, since it may
    /// talk to the network. A failure flashes and does NOT commit — the branch
    /// is not where the user thinks it is.
    fn pull_then_commit(&mut self, index: usize) {
        if self.pulling.is_some() {
            return;
        }
        let Some(repo) = self.repos.get(index) else {
            return;
        };
        let git = repo.git.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git.pull());
        });
        self.pulling = Some((index, rx));
        self.flash = Some(("Pulling…".to_string(), false));
    }
    /// VS Code's Sync Changes (pull, then push — no strategy flag, so the
    /// user's git config decides) on a background
    /// thread; tick() collects the outcome.
    fn sync_changes(&mut self) {
        self.sync_repo(self.active);
    }

    fn sync_repo(&mut self, index: usize) {
        if self.syncing.is_some() {
            return;
        }
        let Some(repo) = self.repos.get(index) else {
            return;
        };
        // No `has_upstream` gate here: with no upstream there is nothing to
        // pull and `Git::sync` publishes the branch instead (VS Code's Publish).
        let git = repo.git.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git.sync());
        });
        self.syncing = Some((index, rx));
    }

    /// Claim a fresh AI commit request for this tab and collect the worker. The
    /// generated message only fills the Message box; it is never committed here.
    fn poll_ai_commit(&mut self) {
        if self.ai_commit.is_none() {
            self.take_ai_request();
        }
        if let Some((index, rx)) = &self.ai_commit {
            let index = *index;
            let received = match rx.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(AiOutcome::Failed(
                    "generation stopped unexpectedly".to_string(),
                )),
            };
            if let Some(outcome) = received {
                self.ai_commit = None;
                self.finish_ai_commit(index, outcome);
            }
        }
        if !self.is_ai_commit_busy() {
            self.ai_commit_started = None;
        }
    }

    /// The AI commit phase running for repo `index`, with its elapsed time.
    fn ai_state(&self, index: usize) -> Option<(AiPhase, std::time::Duration)> {
        if self.ai_commit.as_ref().map(|(r, _)| *r) != Some(index) {
            return None;
        }
        let elapsed = self
            .ai_commit_started
            .map(|at| at.elapsed())
            .unwrap_or_default();
        Some((AiPhase::Generating, elapsed))
    }

    /// A user-initiated commit while the branch is BEHIND its upstream offers a
    /// plain `git pull` first ([`Overlay::ConfirmPullThenCommit`]). The AI-commit
    /// path calls `commit_repo` directly — it must not stop to ask — and a pull
    /// that fails never commits.
    fn commit_with_pull_offer(&mut self, index: usize) {
        let Some(repo) = self.repos.get(index) else {
            return;
        };
        if repo.status.behind > 0 && !repo.status.staged.is_empty() {
            let behind = repo.status.behind;
            self.active = index;
            self.overlay = Some(Overlay::ConfirmPullThenCommit {
                repo: index,
                behind,
            });
            return;
        }
        self.commit_repo(index);
    }

    /// True, with a flash, while repo `index`'s AI commit is still running:
    /// its button must not start a second commit.
    fn ai_commit_busy_for(&mut self, index: usize) -> bool {
        if self.ai_state(index).is_none() {
            return false;
        }
        self.flash = Some(("AI commit in progress…".into(), false));
        true
    }

    fn take_ai_request(&mut self) {
        let Some(tab) = self.my_tab.clone() else {
            return;
        };
        // A sidebar opened for this request discovers its repositories on a
        // later tick; leave the request for then instead of rejecting it.
        if self.repos.is_empty() {
            return;
        }
        if self
            .ai_commit_checked
            .is_some_and(|at| at.elapsed() < AI_REQUEST_POLL)
        {
            return;
        }
        self.ai_commit_checked = Some(std::time::Instant::now());
        let Some(root) = herdr_sidebar::ai_commit::take_request(&tab) else {
            return;
        };
        let Some(index) = self
            .repos
            .iter()
            .position(|repo| same_path(repo.git.root(), &root))
        else {
            let text = format!("AI commit: {} is not shown in this sidebar", root.display());
            herdr_sidebar::git_action::notify("AI Commit Failed", &text);
            self.flash = Some((text, true));
            return;
        };
        self.active = index;
        let git = self.repos[index].git.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ai_commit_outcome(&git));
        });
        self.ai_commit = Some((index, rx));
        self.ai_commit_started = Some(std::time::Instant::now());
        self.flash = Some((
            "✧ AI commit: generating message with OpenCommit…".into(),
            false,
        ));
    }

    /// Put a generated message in the Message box, or report why there is none.
    fn finish_ai_commit(&mut self, index: usize, outcome: AiOutcome) {
        let name = self
            .repos
            .get(index)
            .map(|repo| repo.name.clone())
            .unwrap_or_default();
        match outcome {
            AiOutcome::Message(generated) => {
                // Keep the staged list current for the commit that follows the
                // user's edit.
                self.refresh();
                if let Some(repo) = self.repos.get_mut(index) {
                    // The generated message replaces the draft: drop an armed
                    // Ctrl+A, which no longer describes what is on screen.
                    repo.select_all.disarm();
                    repo.message = generated.message.chars().collect();
                    repo.cursor = repo.message.len();
                }
                self.active = index;
                self.focus = Focus::Message;
                let summarised = generated.summarised.len();
                let flash = if summarised == 0 {
                    "✧ AI message ready — edit it, then ✓ Commit".to_string()
                } else {
                    format!(
                        "✧ AI message ready ({summarised} large file(s) summarised for the AI) — edit it, then ✓ Commit"
                    )
                };
                self.flash = Some((flash, false));
                herdr_sidebar::git_action::notify(
                    &format!("AI Commit [{name}]"),
                    "Message ready in Source Control — edit it, then press ✓ Commit.",
                );
            }
            AiOutcome::NothingToCommit => {
                self.flash = Some(("AI commit: nothing to commit".into(), false));
                herdr_sidebar::git_action::notify(
                    &format!("AI Commit [{name}]"),
                    "Nothing to commit.",
                );
            }
            AiOutcome::Failed(error) => {
                herdr_sidebar::git_action::notify(&format!("AI Commit Failed [{name}]"), &error);
                self.flash = Some((format!("AI commit: {error}"), true));
            }
        }
    }

    fn commit(&mut self) {
        self.commit_with_pull_offer(self.active);
    }

    fn run_primary_action(&mut self) {
        if self.ai_commit_busy_for(self.active) {
            return;
        }
        if self
            .active_repo()
            .is_some_and(|repo| sync_is_primary(&repo.status))
        {
            self.sync_changes();
        } else {
            self.commit();
        }
    }

    fn commit_repo(&mut self, index: usize) {
        if self.ai_commit_busy_for(index) {
            return;
        }
        let Some(repo) = self.repos.get_mut(index) else {
            return;
        };
        let message: String = repo.message.iter().collect();
        if message.trim().is_empty() {
            self.active = index;
            self.flash = Some(("Commit message is empty.".to_string(), true));
            self.focus = Focus::Message;
            return;
        }
        if repo.status.staged.is_empty() {
            self.flash = Some(("No staged changes to commit.".to_string(), true));
            return;
        }
        match repo.git.commit(message.trim()) {
            Ok(summary) => {
                self.flash = Some((summary, false));
                repo.select_all.disarm();
                repo.message.clear();
                repo.cursor = 0;
                self.focus = Focus::List;
            }
            Err(e) => self.flash = Some((e, true)),
        }
        self.refresh();
        self.persist_scm();
    }

    /// Screen lines a row occupies; the inline message boxes grow with
    /// their (wrapped) message, up to [`MESSAGE_MAX_ROWS`] content rows.
    fn row_height(&self, row: Row) -> u16 {
        match row {
            Row::Message(r) => 2 + self.message_rows_inline(r) as u16,
            // A breathing row above and below the button.
            Row::Commit(_) => 3,
            _ => 1,
        }
    }

    /// Largest scroll offset that still shows the bottom of the list in a
    /// `height`-line viewport (0 when everything fits).
    fn max_scroll(&self, height: usize) -> usize {
        let heights: Vec<usize> = self
            .rows
            .iter()
            .map(|row| self.row_height(*row) as usize)
            .collect();
        max_scroll_for(&heights, height)
    }

    /// Content rows repo `r`'s inline message box shows right now.
    fn message_rows_inline(&self, r: usize) -> usize {
        let Some(repo) = self.repos.get(r) else {
            return 1;
        };
        let field = usize::from(inline_field_width(self.last_width));
        wrap_message(&repo.message, repo.cursor, field)
            .0
            .len()
            .min(MESSAGE_MAX_ROWS)
    }

    /// Content rows the single-repo message box shows at `width`.
    fn single_message_rows(&self, width: u16) -> usize {
        let sparkle_w = Span::raw(sparkle_icon(self.theme)).width() + 1;
        let field = usize::from(width).saturating_sub(2 + sparkle_w).max(1);
        match self.active_repo() {
            Some(r) => wrap_message(&r.message, r.cursor, field)
                .0
                .len()
                .min(MESSAGE_MAX_ROWS),
            None => 1,
        }
    }

    /// The visible row at a pane-local mouse row plus the line within it
    /// (rows vary in height: message boxes and buttons span several lines).
    fn row_hit(&self, mouse_row: u16) -> Option<(usize, u16)> {
        row_hit_with_heights(
            self.body,
            mouse_row,
            (self.body.offset..self.rows.len())
                .map(|index| (index, self.row_height(self.rows[index]))),
        )
    }

    /// The visible row index at a pane-local mouse row, if it lands on one.
    fn row_at(&self, mouse_row: u16) -> Option<usize> {
        self.row_hit(mouse_row).map(|(index, _)| index)
    }

    fn hovered_action_hint(&self) -> Option<&'static str> {
        let index = self.hovered?;
        let x = self.mouse_pos?.0;
        match *self.rows.get(index)? {
            Row::ChangesHeader(repo) => {
                changes_header_action_at(x, self.last_width, self.repos[repo].status.unstaged.len())
                    .map(ChangesHeaderAction::footer_hint)
            }
            Row::StagedDir(repo, dir_idx) => {
                let count = self
                    .repos
                    .get(repo)
                    .and_then(|r| r.staged_dirs.get(dir_idx))
                    .map_or(0, |d| d.count);
                dir_hover_action_at(x, self.last_width, true, count)
                    .map(FileHoverAction::footer_hint)
            }
            Row::ChangesDir(repo, dir_idx) => {
                let count = self
                    .repos
                    .get(repo)
                    .and_then(|r| r.changes_dirs.get(dir_idx))
                    .map_or(0, |d| d.count);
                dir_hover_action_at(x, self.last_width, false, count)
                    .map(FileHoverAction::footer_hint)
            }
            Row::Staged(repo, i) => file_hover_action_at(
                x,
                self.last_width,
                true,
                self.marker_width_of(repo, true, i),
            )
            .map(FileHoverAction::footer_hint),
            Row::Unstaged(repo, i) => file_hover_action_at(
                x,
                self.last_width,
                false,
                self.marker_width_of(repo, false, i),
            )
            .map(FileHoverAction::footer_hint),
            _ => None,
        }
    }

    /// The screen row where `index`'s first line is drawn, if visible.
    fn row_y(&self, index: usize) -> Option<u16> {
        let mut y = self.body.top;
        for i in self.body.offset..self.rows.len() {
            if i == index {
                return (y < self.body.top + self.body.height).then_some(y);
            }
            y += self.row_height(self.rows[i]);
        }
        None
    }

    // ---- Rendering ----

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.last_width = area.width;
        self.last_height = area.height;

        if self.repos.is_empty() {
            self.zones = ClickZones::default();
            let [activity, body] = no_repo_layout(area, self.merged());
            if self.merged() {
                self.draw_activity_bar(frame, activity);
            }
            let hint = if self.merged() {
                "Press 1 for Explorer, 2 for Search, or q to quit."
            } else {
                "Open this pane inside a repo,\nor press q to quit."
            };
            let text = format!("Not a git repository.\n\n{}\n\n{hint}", self.discover_err);
            frame.render_widget(Paragraph::new(text).dim().wrap(Wrap { trim: false }), body);
            self.draw_overlay(frame);
            return;
        }

        // With several repos, VS Code puts a message box + Commit button
        // INSIDE each repo's section (rendered as list rows); the single-repo
        // view keeps them fixed at the top. The Sync Changes row only appears
        // when there is something to sync (or a sync is running).
        let multi = self.multi();
        let message_height = if multi {
            0
        } else {
            2 + self.single_message_rows(area.width) as u16
        };
        let button_height = if multi { 0 } else { 3 };
        let sync_height = u16::from(
            !multi
                && self.sync_label().is_some()
                && !self
                    .active_repo()
                    .is_some_and(|repo| sync_is_primary(&repo.status)),
        );
        let git_footer = self.sidebar_state.show_git_footer && self.active_repo().is_some();
        // A breathing row above and below the icons keeps the activity bar
        // from crowding the pane border.
        let activity_height = if self.merged() { 3 } else { 0 };
        let layout = |footer_height| {
            Layout::vertical([
                Constraint::Length(activity_height),
                Constraint::Length(1),
                Constraint::Length(message_height),
                Constraint::Length(button_height),
                Constraint::Length(sync_height),
                Constraint::Min(0),
                Constraint::Length(footer_height),
            ])
            .areas(area)
        };

        let mut footer_lines = self.footer_lines(area.width);
        let mut menu_hint = git_footer && footer_lines.is_empty();
        let mut sections = layout(
            (footer_lines.len() as u16 + 2 * u16::from(menu_hint) + u16::from(git_footer)).max(1),
        );
        self.prepare_list(sections[5]);
        let mut action_hint = self.hovered_action_hint();
        if action_hint.is_some() && self.overlay.is_none() && self.flash.is_none() {
            footer_lines.clear();
        }
        menu_hint = git_footer && footer_lines.is_empty();
        sections = layout(
            (footer_lines.len() as u16 + 2 * u16::from(menu_hint) + u16::from(git_footer)).max(1),
        );
        self.prepare_list(sections[5]);
        action_hint = self.hovered_action_hint();

        let [activity, header, message, button, sync, list, footer] = sections;
        self.page = list.height.saturating_sub(1).max(1) as usize;

        if self.merged() {
            self.draw_activity_bar(frame, activity);
        }
        self.draw_header(frame, header);
        if !multi {
            self.draw_message(frame, message);
            self.draw_button(frame, button);
            self.draw_sync(frame, sync);
        } else {
            self.zones.message = Rect::default();
            self.zones.sparkle = Rect::default();
            self.zones.button = Rect::default();
            self.zones.sync = Rect::default();
        }
        self.draw_list(frame, list);
        let footer_empty = footer_lines.is_empty();
        let content_height = footer.height.saturating_sub(u16::from(git_footer));
        let footer_content = Rect::new(footer.x, footer.y, footer.width, content_height);
        frame.render_widget(Paragraph::new(footer_lines), footer_content);
        if menu_hint {
            frame.render_widget(
                Paragraph::new(action_hint.unwrap_or("right-click / m for menus"))
                    .style(Style::default().fg(Color::DarkGray))
                    .alignment(Alignment::Right),
                footer_content,
            );
        }
        // Collapse button at the bottom-right of the last footer line,
        // mirroring the explorer (and herdr's own sidebar).
        let last_line = Rect::new(
            footer.x,
            footer.y + footer.height.saturating_sub(1),
            footer.width,
            1,
        );
        let [footer_status, footer_button] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(3)]).areas(last_line);
        self.zones.git_footer = FooterZones::default();
        if git_footer {
            if let Some(status) = self.active_repo().map(|repo| repo.status.clone()) {
                self.zones.git_footer = draw_git_footer(
                    frame,
                    footer_status,
                    self.theme,
                    &status,
                    self.syncing.is_some(),
                    self.mouse_pos,
                );
            }
        } else if footer_empty {
            frame.render_widget(
                Paragraph::new(action_hint.unwrap_or("right-click / m for menus"))
                    .style(Style::default().fg(Color::DarkGray))
                    .alignment(Alignment::Right),
                footer_status,
            );
        }
        frame.render_widget(
            Paragraph::new(Span::styled(
                "«",
                Style::default().bold().fg(palette().header_accent),
            ))
            .centered(),
            footer_button,
        );

        self.draw_overlay(frame);
    }

    fn draw_overlay(&mut self, frame: &mut Frame) {
        match self.overlay {
            Some(Overlay::BranchPicker(_)) => {
                if let Some(Overlay::BranchPicker(picker)) = self.overlay.as_mut() {
                    picker.draw(frame);
                }
            }
            Some(Overlay::Menu { .. }) => self.draw_menu(frame),
            Some(Overlay::Settings { .. }) => self.draw_settings(frame),
            Some(Overlay::PaneRename { .. }) => self.draw_pane_rename(frame),
            Some(Overlay::BranchRename { .. }) => self.draw_branch_rename(frame),
            Some(Overlay::CommitReword { .. }) => self.draw_commit_reword(frame),
            Some(Overlay::ConfirmDiscard { .. })
            | Some(Overlay::ConfirmDiscardAll { .. })
            | Some(Overlay::ConfirmGit { .. }) => self.draw_confirm(frame),
            Some(Overlay::ConfirmPullThenCommit { .. }) => self.draw_confirm(frame),
            _ => {}
        }
    }

    /// Render the y/N confirm (discard, or a git command) as a bordered dialog
    /// over the body. `y` or a click on Yes confirms, and any other key or a
    /// click on No cancels.
    fn draw_confirm(&mut self, frame: &mut Frame) {
        self.zones.confirm_yes = Rect::default();
        self.zones.confirm_no = Rect::default();
        let (title, message) = match self.overlay.as_ref() {
            Some(Overlay::ConfirmDiscard { entry, .. }) => (
                "Discard changes".to_string(),
                format!("Discard changes to '{}'?", entry.path),
            ),
            Some(Overlay::ConfirmDiscardAll { .. }) => (
                "Discard all changes".to_string(),
                "Discard all unstaged changes?".to_string(),
            ),
            Some(Overlay::ConfirmGit { title, prompt, .. }) => (title.clone(), prompt.clone()),
            Some(Overlay::ConfirmPullThenCommit { repo, behind }) => {
                let branch = self
                    .repos
                    .get(*repo)
                    .map(|r| r.status.branch.clone())
                    .unwrap_or_default();
                (
                    "Pull before committing".to_string(),
                    format!("{branch} is {behind} commit(s) behind its upstream. Pull first?"),
                )
            }
            _ => return,
        };
        let (yes, no) =
            herdr_sidebar::ui::draw_confirm_dialog(frame, &title, &message, self.mouse_pos);
        self.zones.confirm_yes = yes;
        self.zones.confirm_no = no;
    }

    /// The pane-rename field as a bordered dialog over the body — a footer
    /// line was too easy to miss while typing a new pane name.
    fn draw_pane_rename(&mut self, frame: &mut Frame) {
        let Some(Overlay::PaneRename { input, select_all }) = self.overlay.as_ref() else {
            return;
        };
        herdr_sidebar::ui::draw_input_dialog(
            frame,
            "Rename pane",
            input.as_str(),
            select_all.armed(),
        );
    }

    /// The branch-rename field, drawn like the pane-rename one.
    fn draw_branch_rename(&mut self, frame: &mut Frame) {
        let Some(Overlay::BranchRename {
            input, select_all, ..
        }) = self.overlay.as_ref()
        else {
            return;
        };
        herdr_sidebar::ui::draw_input_dialog(
            frame,
            "Rename branch",
            input.as_str(),
            select_all.armed(),
        );
    }

    fn draw_commit_reword(&mut self, frame: &mut Frame) {
        let Some(Overlay::CommitReword {
            input, select_all, ..
        }) = self.overlay.as_ref()
        else {
            return;
        };
        herdr_sidebar::ui::draw_input_dialog(
            frame,
            "Edit commit message",
            input.as_str(),
            select_all.armed(),
        );
    }

    /// The VS Code activity bar: view-switcher icons plus a detach button.
    /// The area is three rows tall — icons on the middle one, one blank
    /// spacer row each side.
    fn draw_activity_bar(&mut self, frame: &mut Frame, area: Rect) {
        // Three rows in the plain pane background; only the ACTIVE icon's
        // highlight chip extends into the outer rows by a half block — a tall
        // button with built-in breathing room, no strip container.
        let outer_top = area.y;
        let outer_bottom = area.y + 2;
        let area = Rect::new(area.x, area.y + 1, area.width, 1);
        let (exp_icon, search_icon, git_icon) = activity_icons(self.theme);
        // Both FA glyphs (folder, code-fork) render two cells wide in the
        // non-Mono Nerd Font; reserve the second cell in each chip so the
        // highlights are equal-sized with centered icons.
        let slack = if self.theme == IconTheme::Material {
            " "
        } else {
            ""
        };
        let mut spans = [
            Span::raw(" "),
            Span::raw(format!(" {exp_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {search_icon}{slack} ")),
            Span::raw(" "),
            Span::raw(format!(" {git_icon}{slack} ")),
        ];
        // Hit zones from the actual span widths (emoji vs nerd-glyph widths differ).
        let mut x = area.x;
        let mut bounds = Vec::new();
        for span in &spans {
            let w = span.width() as u16;
            bounds.push((x, x + w));
            x += w;
        }
        self.zones.activity_row = area.y;
        self.zones.explorer = bounds[1];
        self.zones.search = bounds[3];
        self.zones.source_control = bounds[5];
        let hovered = |bounds| {
            self.mouse_pos
                .is_some_and(|(x, y)| hits_activity_button(bounds, area.y, x, y))
        };
        let explorer_hovered = hovered(bounds[1]);
        let search_hovered = hovered(bounds[3]);
        let git_hovered = hovered(bounds[5]);
        spans[1].style = activity_button_style(false, explorer_hovered);
        spans[3].style = activity_button_style(false, search_hovered);
        spans[5].style = activity_button_style(true, git_hovered);
        draw_activity_caps(
            frame,
            bounds[5],
            outer_top,
            outer_bottom,
            palette().selection_bg,
        );
        for (is_hovered, button_bounds) in
            [(explorer_hovered, bounds[1]), (search_hovered, bounds[3])]
        {
            if is_hovered {
                draw_activity_caps(
                    frame,
                    button_bounds,
                    outer_top,
                    outer_bottom,
                    palette().activity_hover_bg,
                );
            }
        }
        let gear_text = format!(" {} ", gear_icon(self.theme));
        let gear_w = Span::raw(gear_text.as_str()).width() as u16;
        let gear_x = area.x + area.width.saturating_sub(gear_w);
        self.zones.gear = Rect::new(gear_x, outer_top, gear_w, 3);
        let gear_hovered = self
            .mouse_pos
            .is_some_and(|(x, y)| hits(self.zones.gear, x, y));
        let gear = Span::styled(gear_text, activity_button_style(false, gear_hovered));
        if gear_hovered {
            draw_activity_caps(
                frame,
                (gear_x, gear_x + gear_w),
                outer_top,
                outer_bottom,
                palette().activity_hover_bg,
            );
        }

        let pad = usize::from(area.width)
            .saturating_sub(spans.iter().map(Span::width).sum::<usize>() + usize::from(gear_w));
        let mut line = spans.to_vec();
        line.push(Span::raw(" ".repeat(pad)));
        line.push(gear);
        frame.render_widget(Paragraph::new(Line::from(line)), area);
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        // A single repo titles the panel with its NAME, like VS Code's repo
        // rows (the sections inside are already Changes/Staged Changes);
        // several repos fall back to a neutral "Source Control".
        let title = match self.active_repo() {
            Some(repo) if self.repos.len() == 1 => repo.name.clone(),
            _ => "Source Control".to_string(),
        };
        let left = Span::styled(format!(" ▾ {title}"), Style::default().bold());
        // With several repos visible, the header names the one the commit box
        // and sync act on; a single repo shows branch + ahead/behind arrows.
        let right_text = match self.active_repo() {
            Some(repo) if self.repos.len() > 1 => {
                format!("{} · {} ", repo.name, repo.status.branch)
            }
            Some(repo) => {
                let s = &repo.status;
                let counts = if s.ahead + s.behind > 0 {
                    format!(" {}↑ {}↓", s.ahead, s.behind)
                } else {
                    String::new()
                };
                format!("{}{} ", s.branch, counts)
            }
            None => String::new(),
        };
        // In unified mode the ⚙ lives in the activity bar; standalone puts it
        // at the header's right edge.
        let gear = if self.merged() {
            None
        } else {
            Some(Span::styled(
                format!("{} ", gear_icon(self.theme)),
                Style::default().dim(),
            ))
        };
        let gear_w = gear.as_ref().map(Span::width).unwrap_or(0);
        // The hover title-action buttons sit just left of the gear.
        self.title_zones.clear();
        let (action_spans, actions_w) = if title_actions_visible(self.last_mouse) {
            let view_action = if self.tree_view {
                TitleAction::ViewAsList
            } else {
                TitleAction::ViewAsTree
            };
            let actions = [view_action, TitleAction::Refresh, TitleAction::CollapseAll];
            let w = title_actions_width(self.theme, &actions);
            let ax = area.x + area.width.saturating_sub(gear_w as u16 + w);
            let (spans, zones) =
                title_action_spans(self.theme, &actions, ax, area.y, self.mouse_pos);
            self.title_zones = zones;
            (spans, usize::from(w))
        } else {
            (Vec::new(), 0)
        };
        // The branch text yields to the buttons and gear in narrow panes.
        let avail = (area.width as usize)
            .saturating_sub(left.width() + actions_w + gear_w)
            .saturating_sub(1);
        let branch_text = truncate_to(right_text, avail);
        let branch_width = Span::raw(branch_text.as_str()).width();
        let pad = (area.width as usize)
            .saturating_sub(left.width() + branch_width + actions_w + gear_w)
            .max(1);
        let branch_x = area.x + left.width() as u16 + pad as u16;
        self.zones.header_branch = Rect::new(branch_x, area.y, branch_width as u16, 1);
        let branch_hovered = self
            .mouse_pos
            .is_some_and(|(x, y)| hits(self.zones.header_branch, x, y));
        let branch = Span::styled(
            branch_text,
            if branch_hovered {
                hover_style()
            } else {
                Style::default().dim()
            },
        );
        let mut spans = vec![left, Span::raw(" ".repeat(pad)), branch];
        spans.extend(action_spans);
        if let Some(gear) = gear {
            let gx = area.x + area.width.saturating_sub(gear_w as u16);
            self.zones.gear = Rect::new(gx, area.y, gear_w as u16, 1);
            spans.push(gear);
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_message(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Message;
        let border = if focused {
            Style::default().fg(palette().accent)
        } else {
            Style::default().dim()
        };
        let boxed = Block::bordered().border_style(border);
        let inner = boxed.inner(area);
        frame.render_widget(boxed, area);
        self.zones.message = area;

        // The suggest button lives at the right end of the input line — a
        // monochrome OUTLINE of the ✨ sparkles shape (MDI "creation" in the
        // material theme) in the normal foreground, never the colored emoji.
        let sparkle_glyph = if self.suggesting.is_some() || self.ai_commit.is_some() {
            "…"
        } else {
            sparkle_icon(self.theme)
        };
        // The icon's width even while the "…" spinner shows, so the box
        // height computed in draw() always matches.
        let sparkle_w = Span::raw(sparkle_icon(self.theme)).width() as u16 + 1;
        let [text_area, sparkle_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(sparkle_w)]).areas(inner);
        frame.render_widget(Paragraph::new(sparkle_glyph), sparkle_area);
        self.zones.sparkle = sparkle_area;

        let (message, cursor, selected, branch) = match self.active_repo() {
            Some(r) => (
                r.message.clone(),
                r.cursor,
                r.select_all.armed() && focused,
                r.status.branch.clone(),
            ),
            None => (Vec::new(), 0, false, String::new()),
        };
        if message.is_empty() && !focused {
            let placeholder = message_placeholder(&branch, usize::from(text_area.width));
            frame.render_widget(Paragraph::new(placeholder).dim().italic(), text_area);
            return;
        }

        // Wrapped input: the box grows with the message (draw() sizes it) up
        // to MESSAGE_MAX_ROWS rows, then scrolls to keep the cursor visible.
        let field = text_area.width.max(1) as usize;
        let (rows, cursor_row, cursor_col) = wrap_message(&message, cursor, field);
        let (top, visible) = message_window(rows.len(), cursor_row, focused);
        // An armed Ctrl+A draws the whole message as selected: the next edit
        // replaces all of it.
        let text_style = if selected {
            selected_text_style()
        } else {
            Style::default()
        };
        let text: Vec<Line> = rows
            .iter()
            .skip(top)
            .take(visible)
            .map(|row| Line::from(Span::styled(row.clone(), text_style)))
            .collect();
        frame.render_widget(Paragraph::new(text), text_area);
        if focused {
            frame.set_cursor_position(Position::new(
                text_area.x + cursor_col as u16,
                text_area.y + (cursor_row - top) as u16,
            ));
        }
    }

    fn draw_button(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Commit;
        let bg = if focused {
            palette().button_focus_bg
        } else {
            palette().button_bg
        };
        let mut style = Style::default().bg(bg).fg(palette().button_fg);
        if focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        // A breathing row above and below, like the inline variant.
        let inner = if area.height >= 3 {
            Rect::new(
                area.x.saturating_add(1),
                area.y + 1,
                area.width.saturating_sub(2),
                1,
            )
        } else {
            area
        };
        let ai = self.ai_state(self.active);
        let syncing = self
            .syncing
            .as_ref()
            .is_some_and(|(repo, _)| *repo == self.active);
        let label = match self.active_repo() {
            Some(repo) => button_label(&repo.status, syncing, ai),
            None => "✓ Commit".to_string(),
        };
        draw_activity_caps(
            frame,
            (inner.x, inner.x + inner.width),
            area.y,
            area.y + area.height.saturating_sub(1),
            bg,
        );
        match ai {
            Some((_, elapsed)) => {
                let spans = shimmer_spans(
                    &label,
                    inner.width as usize,
                    elapsed,
                    style,
                    palette().button_focus_bg,
                );
                frame.render_widget(Paragraph::new(Line::from(spans)), inner);
            }
            None => frame.render_widget(Paragraph::new(label).centered().style(style), inner),
        }
        self.zones.button = Rect::new(inner.x, area.y, inner.width, area.height);
    }

    /// The Sync Changes label, or `None` while there is nothing to sync
    /// (which hides the row entirely).
    fn sync_label(&self) -> Option<String> {
        let status = &self.active_repo()?.status;
        let syncing = self
            .syncing
            .as_ref()
            .is_some_and(|(repo, _)| *repo == self.active);
        sync_label_for_status(status, syncing)
    }

    /// A secondary button below Commit, VS Code's Sync Changes: pull + push
    /// with the outgoing↑ / incoming↓ counts.
    fn draw_sync(&mut self, frame: &mut Frame, area: Rect) {
        let Some(label) = self.sync_label() else {
            self.zones.sync = Rect::default();
            return;
        };
        let style = if self.syncing.is_some() {
            Style::default()
                .bg(palette().sync_busy_bg)
                .fg(palette().muted_button_fg)
        } else {
            Style::default().bg(palette().sync_bg).fg(palette().sync_fg)
        };
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        self.zones.sync = inner;
        frame.render_widget(Paragraph::new(label).centered().style(style), inner);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let theme = self.theme;
        let mouse_pos = self.mouse_pos;
        let active = self.active;
        let syncing_repo = self.syncing.as_ref().map(|(repo, _)| *repo);

        let visible = self.prepare_list(area);
        let hovered = self.hovered;
        let selected = self.selected;
        let list_focused = self.focus == Focus::List;

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .map(|(i, row)| {
                let row_hovered = hovered == Some(i);
                let item = match *row {
                    Row::RepoHeader(r) => {
                        let branch_hovered = row_hovered
                            && mouse_pos.is_some_and(|(x, _)| {
                                within(
                                    x,
                                    repo_header_branch_zone(&self.repos[r], theme, width as u16),
                                )
                            });
                        repo_header_item(&self.repos[r], r == active, theme, width, branch_hovered)
                    }
                    Row::Message(r) => message_box_item(
                        &self.repos[r],
                        r == active && self.focus == Focus::Message,
                        theme,
                        width,
                    ),
                    Row::Commit(r) => commit_button_item(
                        &self.repos[r].status,
                        syncing_repo == Some(r),
                        self.ai_state(r),
                        r == active,
                        r == active && self.focus == Focus::Commit,
                        width,
                    ),
                    Row::StagedHeader(r) => section_item(
                        "Staged Changes",
                        self.repos[r].staged_collapsed,
                        Some(self.repos[r].status.staged.len()),
                        width,
                        row_hovered.then_some('−'),
                    ),
                    Row::ChangesHeader(r) => {
                        let count = self.repos[r].status.unstaged.len();
                        let hovered_action = row_hovered
                            .then(|| {
                                mouse_pos.and_then(|(x, _)| {
                                    changes_header_action_at(x, width as u16, count)
                                })
                            })
                            .flatten();
                        changes_header_item(
                            self.repos[r].changes_collapsed,
                            count,
                            width,
                            row_hovered,
                            hovered_action,
                        )
                    }
                    Row::DrawerHeader(kind) => {
                        let mut item = section_item(
                            kind.title(),
                            !self.drawers[kind.index()].expanded,
                            None,
                            width,
                            None,
                        );
                        if kind == Drawer::FileHistory
                            && let Some(target) = &self.history_target
                        {
                            let name = target.rsplit('/').next().unwrap_or(target);
                            item = file_history_header(!self.drawers[kind.index()].expanded, name);
                        }
                        item
                    }
                    Row::DrawerLine(kind, i) => {
                        drawer_line(kind, &self.drawers[kind.index()].lines[i])
                    }
                    Row::StagedDir(r, d) => {
                        let dir = &self.repos[r].staged_dirs[d];
                        let hovered_action = row_hovered
                            .then(|| {
                                mouse_pos.and_then(|(x, _)| {
                                    dir_hover_action_at(x, width as u16, true, dir.count)
                                })
                            })
                            .flatten();
                        tree_dir_item(dir, width, theme, true, row_hovered, hovered_action)
                    }
                    Row::ChangesDir(r, d) => {
                        let dir = &self.repos[r].changes_dirs[d];
                        let hovered_action = row_hovered
                            .then(|| {
                                mouse_pos.and_then(|(x, _)| {
                                    dir_hover_action_at(x, width as u16, false, dir.count)
                                })
                            })
                            .flatten();
                        tree_dir_item(dir, width, theme, false, row_hovered, hovered_action)
                    }
                    Row::Staged(r, i) => {
                        let entry = &self.repos[r].status.staged[i];
                        let conflicts = self.conflicts_for(r, entry);
                        let marker_width = status_marker(entry.letter, conflicts).0.chars().count();
                        let hovered_action = row_hovered
                            .then(|| {
                                mouse_pos.and_then(|(x, _)| {
                                    file_hover_action_at(x, width as u16, true, marker_width as u16)
                                })
                            })
                            .flatten();
                        if self.tree_view {
                            let depth = self.repos[r]
                                .staged_file_depths
                                .get(i)
                                .copied()
                                .unwrap_or(0);
                            tree_file_item(
                                entry,
                                depth,
                                width,
                                theme,
                                row_hovered,
                                hovered_action,
                                RowLook {
                                    staged: true,
                                    conflicts,
                                },
                            )
                        } else {
                            file_item(
                                entry,
                                width,
                                theme,
                                row_hovered,
                                hovered_action,
                                RowLook {
                                    staged: true,
                                    conflicts,
                                },
                            )
                        }
                    }
                    Row::Unstaged(r, i) => {
                        let entry = &self.repos[r].status.unstaged[i];
                        let conflicts = self.conflicts_for(r, entry);
                        let marker_width = status_marker(entry.letter, conflicts).0.chars().count();
                        let hovered_action = row_hovered
                            .then(|| {
                                mouse_pos.and_then(|(x, _)| {
                                    file_hover_action_at(
                                        x,
                                        width as u16,
                                        false,
                                        marker_width as u16,
                                    )
                                })
                            })
                            .flatten();
                        if self.tree_view {
                            let depth = self.repos[r]
                                .changes_file_depths
                                .get(i)
                                .copied()
                                .unwrap_or(0);
                            tree_file_item(
                                entry,
                                depth,
                                width,
                                theme,
                                row_hovered,
                                hovered_action,
                                RowLook {
                                    staged: false,
                                    conflicts,
                                },
                            )
                        } else {
                            file_item(
                                entry,
                                width,
                                theme,
                                row_hovered,
                                hovered_action,
                                RowLook {
                                    staged: false,
                                    conflicts,
                                },
                            )
                        }
                    }
                };
                if selected == Some(i) {
                    let style = if list_focused {
                        selection_style(true)
                    } else {
                        selection_style(false)
                    };
                    item.style(style)
                } else if hovered == Some(i) {
                    item.style(hover_style())
                } else {
                    item
                }
            })
            .collect();
        frame.render_widget(List::new(items), area);
        // Measure in screen lines, not rows: Message and Commit rows are taller.
        let heights: Vec<usize> = self
            .rows
            .iter()
            .map(|row| self.row_height(*row) as usize)
            .collect();
        let total: usize = heights.iter().sum();
        let viewport = area.height as usize;
        let pos: usize = heights[..self.scroll.min(heights.len())].iter().sum();
        draw_scrollbar(
            frame,
            area,
            total,
            viewport,
            pos.min(total.saturating_sub(viewport)),
        );

        // Terminal cursor inside the focused INLINE message box (multi-repo).
        if self.multi() && self.focus == Focus::Message {
            let target = self
                .rows
                .iter()
                .position(|row| matches!(row, Row::Message(r) if *r == self.active));
            if let Some(index) = target
                && let Some(y) = self.row_y(index)
                && y + 1 < area.y + area.height
                && let Some(repo) = self.active_repo()
            {
                let field = usize::from(inline_field_width(self.last_width));
                let (rows, cursor_row, cursor_col) =
                    wrap_message(&repo.message, repo.cursor, field);
                let (top, _) = message_window(rows.len(), cursor_row, true);
                let cy = y + 1 + (cursor_row - top) as u16;
                if cy + 1 < area.y + area.height {
                    frame.set_cursor_position(Position::new(area.x + 1 + cursor_col as u16, cy));
                }
            }
        }
    }

    fn prepare_list(&mut self, area: Rect) -> usize {
        // Clamp the scroll and (keyboard nav only) walk it forward until the
        // selection fits — rows have variable heights.
        let h = (area.height as usize).max(1);
        self.scroll = self.scroll.min(self.max_scroll(h));
        if self.snap {
            if let Some(sel) = self.selected {
                if sel < self.scroll {
                    self.scroll = sel;
                } else {
                    while self.scroll < sel {
                        let used: usize = (self.scroll..=sel)
                            .map(|i| self.row_height(self.rows[i]) as usize)
                            .sum();
                        if used <= h {
                            break;
                        }
                        self.scroll += 1;
                    }
                }
            }
            self.snap = false;
            // The snap never leaves the selection above `scroll`, so this
            // clamp only trims a window that overshoots the bottom.
            self.scroll = self.scroll.min(self.max_scroll(h));
        }
        // Visible slice: everything from `scroll` until the viewport is
        // spent (plus one partially-clipped row).
        let mut end = self.scroll;
        let mut used = 0usize;
        while end < self.rows.len() && used < h {
            used += self.row_height(self.rows[end]) as usize;
            end += 1;
        }
        let visible = end - self.scroll;
        self.body = BodyGeom {
            top: area.y,
            height: area.height,
            offset: self.scroll,
        };
        self.hovered = self
            .mouse_pos
            .and_then(|(_, mouse_row)| self.row_at(mouse_row));
        visible
    }

    /// Footer content: a flash message (WRAPPED — the one-line assumption used
    /// to clip them in narrow panes), or the hotkey hints. The y/N confirms
    /// are dialogs over the body (see `draw_confirm`), not footer lines.
    fn footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let message: Option<(String, Color)> = self.flash.as_ref().map(|(text, is_error)| {
            let color = if *is_error {
                palette().deleted
            } else {
                palette().untracked
            };
            let prefix = if *is_error { "" } else { "✓ " };
            (format!("{prefix}{text}"), color)
        });
        if let Some((msg, color)) = message {
            return wrap_footer_message(&msg, width, 4)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(color)))
                .collect();
        }
        if !self.show_hotkeys() {
            return Vec::new();
        }
        wrap_hints(&self.hints(), width, 3)
    }

    /// The hotkey hints, shown in Settings (and optionally the footer).
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints: Vec<(&'static str, &'static str)> = vec![
            ("⏎", "stage"),
            ("t", if self.tree_view { "list" } else { "tree" }),
            ("a", "all"),
            ("u", "none"),
            ("c", "msg"),
            ("A", "suggest"),
            ("o", "diff"),
            ("m", "menu"),
            ("b", "hide"),
            ("S", "sync"),
            ("s", "settings"),
            ("r", "refresh"),
            ("q", "quit"),
        ];
        if self.merged() {
            hints.extend([("1", "files"), ("2", "search"), ("3", "git")]);
        }
        hints
    }

    /// Switch icon themes and REMEMBER it (see the explorer's twin).
    fn set_theme(&mut self, theme: IconTheme) {
        self.theme = theme;
        self.sidebar_state = sidebar::update_state(|state| state.icons = Some(theme));
    }

    /// The persisted "show hotkeys in the footer" setting.
    fn show_hotkeys(&self) -> bool {
        self.sidebar_state.show_hotkeys
    }

    /// Esc: close the preview pane beside us, if one is open.
    fn close_preview(&mut self) {
        if let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone())
            && let Some(notice) = herdr_sidebar::viewer::close_in_tab(&pane_id)
        {
            self.flash = Some((notice, true));
        }
    }

    /// Render the context-menu popup near its anchor, clamped inside the pane,
    /// and remember its rect for mouse hit-testing.
    fn draw_menu(&mut self, frame: &mut Frame) {
        let Some(Overlay::Menu {
            x,
            y,
            entries,
            selected,
            rect,
            ..
        }) = self.overlay.as_mut()
        else {
            return;
        };
        let area = frame.area();
        let label_width = entries
            .iter()
            .map(|e| match e {
                MenuEntry::Action(_, label) => label.chars().count(),
                MenuEntry::Separator => 0,
            })
            .max()
            .unwrap_or(0) as u16;
        let width = (label_width + 4).min(area.width);
        let height = (entries.len() as u16 + 2).min(area.height);
        let px = (*x).min(area.width.saturating_sub(width));
        let py = (*y + 1).min(area.height.saturating_sub(height));
        let popup = Rect::new(px, py, width, height);
        *rect = popup;

        let items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| match entry {
                MenuEntry::Separator => {
                    ListItem::new(Line::from("─".repeat(usize::from(width - 2)).dim()))
                }
                MenuEntry::Action(_, label) => {
                    let line = Line::raw(format!(" {label}"));
                    if i == *selected {
                        ListItem::new(line).style(selection_style(true))
                    } else {
                        ListItem::new(line)
                    }
                }
            })
            .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            List::new(items).block(Block::bordered().border_style(Style::default().dim())),
            popup,
        );
    }
}

/// The menu's first, non-forcing `git branch -d <name>`. Matched structurally
/// so a forced retry — or any other confirmed command — runs unchanged.
fn safe_branch_delete(args: &[String]) -> Option<&str> {
    (args.len() == 4 && args[0] == "branch" && args[1] == "-d" && args[2] == "--")
        .then(|| args[3].as_str())
}

/// Next selectable (non-separator) menu index in `direction`, staying put at
/// the ends.
fn step_menu(entries: &[MenuEntry], from: usize, direction: isize) -> usize {
    let mut index = from as isize;
    loop {
        index += direction;
        if index < 0 || index >= entries.len() as isize {
            return from;
        }
        if matches!(entries[index as usize], MenuEntry::Action(..)) {
            return index as usize;
        }
    }
}

/// A repository row, matching VS Code's multi-repo Source Control: disclosure
/// arrow, repo icon and name on the left; branch (starred when dirty) and the
/// ⟳ sync / ✓ commit action icons on the right. The right-edge icon columns
/// are FIXED (last 6 cells) — left_click's hit zones rely on that.
fn repo_header_item(
    repo: &Repo,
    active: bool,
    theme: IconTheme,
    width: usize,
    branch_hovered: bool,
) -> ListItem<'static> {
    let arrow = if repo.collapsed { "▸" } else { "▾" };
    let repo_icon = icon(theme, "", true, false);
    let name_style = if active {
        Style::default().bold()
    } else {
        Style::default().dim().bold()
    };
    let s = &repo.status;
    let counts = if s.ahead + s.behind > 0 {
        format!(" {}↑ {}↓", s.ahead, s.behind)
    } else {
        String::new()
    };
    let branch_text = format!("{} {}{}", branch_icon(theme), repo.branch_decor(), counts);
    let full_icons_width = Span::raw(REPO_HEADER_ACTIONS).width();
    let icons_width = if width >= full_icons_width {
        full_icons_width
    } else {
        0
    };
    let content_width = width.saturating_sub(icons_width);
    let branch_width = Span::raw(branch_text.as_str())
        .width()
        .min(content_width / 2);
    let left_width = content_width.saturating_sub(branch_width);
    let left = truncate_to(
        format!(" {arrow} {} {}", repo_icon.glyph, repo.name),
        left_width,
    );
    let branch = truncate_to(branch_text, branch_width);
    let left_pad = left_width.saturating_sub(Span::raw(left.as_str()).width());
    let branch_pad = branch_width.saturating_sub(Span::raw(branch.as_str()).width());
    ListItem::new(Line::from(vec![
        Span::styled(left, name_style),
        Span::raw(" ".repeat(left_pad + branch_pad)),
        Span::styled(
            branch,
            if branch_hovered {
                hover_style()
            } else {
                Style::default().dim()
            },
        ),
        Span::styled(
            truncate_to(REPO_HEADER_ACTIONS.to_string(), icons_width),
            Style::default().dim(),
        ),
    ]))
}

fn repo_header_branch_zone(repo: &Repo, theme: IconTheme, width: u16) -> (u16, u16) {
    let counts = if repo.status.ahead + repo.status.behind > 0 {
        format!(" {}↑ {}↓", repo.status.ahead, repo.status.behind)
    } else {
        String::new()
    };
    let full_icons_width = Span::raw(REPO_HEADER_ACTIONS).width();
    let icons_width = if usize::from(width) >= full_icons_width {
        full_icons_width
    } else {
        0
    };
    let content_width = usize::from(width).saturating_sub(icons_width);
    let branch_width = Span::raw(format!(
        "{} {}{}",
        branch_icon(theme),
        repo.branch_decor(),
        counts
    ))
    .width()
    .min(content_width / 2);
    let start = content_width.saturating_sub(branch_width) as u16;
    (start, start + branch_width as u16)
}

fn repo_header_action_zones(width: u16) -> ((u16, u16), (u16, u16)) {
    let actions_width = Span::raw(REPO_HEADER_ACTIONS).width() as u16;
    if width < actions_width {
        return ((0, 0), (0, 0));
    }
    let start = width - actions_width;
    let midpoint = start + actions_width / 2;
    ((start, midpoint), (midpoint, width))
}

fn changes_header_action_zones(
    width: u16,
    count: usize,
) -> Option<[(ChangesHeaderAction, (u16, u16)); 3]> {
    const ACTION_WIDTH: u16 = 3;
    let left_width = Span::raw(" ▾ Changes").width() as u16;
    let badge_width = Span::raw(format!(" {count} ")).width() as u16;
    let actions_width = ACTION_WIDTH * 3;
    let reserved = badge_width + 1;
    if width < left_width + 1 + actions_width + reserved {
        return None;
    }
    let start = width - reserved - actions_width;
    Some([
        (ChangesHeaderAction::Discard, (start, start + ACTION_WIDTH)),
        (
            ChangesHeaderAction::Stash,
            (start + ACTION_WIDTH, start + ACTION_WIDTH * 2),
        ),
        (
            ChangesHeaderAction::Stage,
            (start + ACTION_WIDTH * 2, start + actions_width),
        ),
    ])
}

fn changes_header_action_at(x: u16, width: u16, count: usize) -> Option<ChangesHeaderAction> {
    changes_header_action_zones(width, count)?
        .into_iter()
        .find_map(|(action, zone)| within(x, zone).then_some(action))
}

fn file_hover_actions(staged: bool) -> &'static [FileHoverAction] {
    const STAGED: &[FileHoverAction] = &[FileHoverAction::Open, FileHoverAction::Unstage];
    const UNSTAGED: &[FileHoverAction] = &[
        FileHoverAction::Open,
        FileHoverAction::Discard,
        FileHoverAction::Stage,
    ];
    if staged { STAGED } else { UNSTAGED }
}

/// First column of the row's hover actions. The right-aligned tail is
/// `marker` + one space, and the marker can be two cells wide (`!3`).
fn file_hover_action_start(width: u16, staged: bool, marker_width: u16) -> Option<u16> {
    const ACTION_WIDTH: u16 = 3;
    const MIN_FILE_CONTENT: u16 = 8;
    let actions_width = file_hover_actions(staged).len() as u16 * ACTION_WIDTH;
    (width >= MIN_FILE_CONTENT + actions_width + 1 + marker_width)
        .then_some(width - actions_width - 1 - marker_width)
}

fn file_hover_action_at(
    x: u16,
    width: u16,
    staged: bool,
    marker_width: u16,
) -> Option<FileHoverAction> {
    const ACTION_WIDTH: u16 = 3;
    let start = file_hover_action_start(width, staged, marker_width)?;
    if x < start {
        return None;
    }
    let index = usize::from((x - start) / ACTION_WIDTH);
    file_hover_actions(staged).get(index).copied()
}

/// Columns the inline message box's input field spans (between the left
/// border and the ✧ button).
fn inline_field_width(pane_width: u16) -> u16 {
    pane_width.saturating_sub(2 + 3)
}

/// Width-aware commit placeholder: drop detail rather than clipping
/// mid-word when the pane is narrow.
fn message_placeholder(branch: &str, width: usize) -> String {
    for text in [
        format!("Message (⏎ to commit on \"{branch}\")"),
        "Message (⏎ to commit)".to_string(),
        "Message".to_string(),
    ] {
        if Span::raw(text.as_str()).width() <= width {
            return text;
        }
    }
    String::new()
}

/// Most content rows a message box shows before it scrolls instead.
const MESSAGE_MAX_ROWS: usize = 4;

/// Wrap `message` into `field`-wide rows plus the cursor's (row, col). The
/// cursor may sit one past the end, opening a fresh row when that lands on a
/// wrap boundary.
fn wrap_message(message: &[char], cursor: usize, field: usize) -> (Vec<String>, usize, usize) {
    let field = field.max(1);
    let mut rows: Vec<String> = message.chunks(field).map(|c| c.iter().collect()).collect();
    if rows.is_empty() {
        rows.push(String::new());
    }
    if !message.is_empty() && message.len().is_multiple_of(field) && cursor == message.len() {
        rows.push(String::new());
    }
    (rows, cursor / field, cursor % field)
}

/// The `(top, count)` window of wrapped rows to show: everything when it
/// fits, else the slice keeping the cursor visible (or the start, unfocused).
fn message_window(rows: usize, cursor_row: usize, focused: bool) -> (usize, usize) {
    let visible = rows.min(MESSAGE_MAX_ROWS);
    let top = if focused {
        (cursor_row + 1).saturating_sub(visible)
    } else {
        0
    };
    (top, visible)
}

/// A repo's inline message box, VS Code style: a bordered input that grows
/// with its wrapped message, with the ✧ suggest button at its right end.
fn message_box_item(
    repo: &Repo,
    focused: bool,
    theme: IconTheme,
    width: usize,
) -> ListItem<'static> {
    let border = if focused {
        Style::default().fg(palette().accent)
    } else {
        Style::default().dim()
    };
    let horizontal = "─".repeat(width.saturating_sub(2));
    let field = usize::from(inline_field_width(width as u16));
    // The ✧ button owns a fixed 3-column tail; pad the glyph to that width so
    // its rendered width can't shove the closing border off the box corners.
    let suggest = sparkle_icon(theme);
    let suggest_tail = format!(
        "{suggest}{}",
        " ".repeat(3usize.saturating_sub(Span::raw(suggest).width()))
    );

    let mut lines = vec![Line::from(Span::styled(format!("┌{horizontal}┐"), border))];
    if repo.message.is_empty() && !focused {
        let placeholder = message_placeholder(&repo.status.branch, field);
        let pad = field.saturating_sub(Span::raw(placeholder.as_str()).width());
        lines.push(Line::from(vec![
            Span::styled("│", border),
            Span::styled(placeholder, Style::default().dim().italic()),
            Span::raw(" ".repeat(pad)),
            Span::raw(suggest_tail.clone()),
            Span::styled("│", border),
        ]));
    } else {
        let (rows, cursor_row, _) = wrap_message(&repo.message, repo.cursor, field);
        let (top, visible) = message_window(rows.len(), cursor_row, focused);
        // An armed Ctrl+A draws the whole message as selected: the next edit
        // replaces all of it.
        let text_style = if repo.select_all.armed() && focused {
            selected_text_style()
        } else {
            Style::default()
        };
        for (i, row) in rows.iter().skip(top).take(visible).enumerate() {
            let pad = field.saturating_sub(Span::raw(row.as_str()).width());
            // The ✧ button owns the 3-column tail of the FIRST line only.
            let tail = if i == 0 {
                Span::raw(suggest_tail.clone())
            } else {
                Span::raw("   ".to_string())
            };
            lines.push(Line::from(vec![
                Span::styled("│", border),
                Span::styled(row.clone(), text_style),
                Span::raw(" ".repeat(pad)),
                tail,
                Span::styled("│", border),
            ]));
        }
    }
    lines.push(Line::from(Span::styled(format!("└{horizontal}┘"), border)));
    ListItem::new(lines)
}

/// A repo's inline ✓ Commit button; only the active repo's button is fully lit.
fn commit_button_item(
    status: &Status,
    syncing: bool,
    ai: Option<(AiPhase, std::time::Duration)>,
    active: bool,
    focused: bool,
    width: usize,
) -> ListItem<'static> {
    let (bg, fg) = match (active, focused) {
        (true, true) => (palette().button_focus_bg, palette().button_fg),
        (true, false) => (palette().button_bg, palette().button_fg),
        (false, _) => (palette().muted_button_bg, palette().muted_button_fg),
    };
    let label = button_label(status, syncing, ai);
    let button_width = width.saturating_sub(2);
    let body_width = button_width;
    let mut style = Style::default().bg(bg).fg(fg);
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    let body: Vec<Span<'static>> = match ai {
        Some((_, elapsed)) => shimmer_spans(
            &label,
            body_width,
            elapsed,
            style,
            palette().button_focus_bg,
        ),
        None => {
            let label = truncate_to(label, body_width);
            let label_width = Span::raw(label.as_str()).width();
            let left_pad = body_width.saturating_sub(label_width) / 2;
            let right_pad = body_width.saturating_sub(left_pad + label_width);
            vec![Span::styled(
                format!("{}{label}{}", " ".repeat(left_pad), " ".repeat(right_pad)),
                style,
            )]
        }
    };
    let cap_style = Style::default().fg(bg);
    let mut middle = vec![Span::raw(" ")];
    middle.extend(body);
    middle.push(Span::raw(" "));
    ListItem::new(vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled("▄".repeat(button_width), cap_style),
            Span::raw(" "),
        ]),
        Line::from(middle),
        Line::from(vec![
            Span::raw(" "),
            Span::styled("▀".repeat(button_width), cap_style),
            Span::raw(" "),
        ]),
    ])
}

/// A collapsible section header; `count` renders as a right-aligned badge
/// (the drawers have no badge, like Git Graph's).
fn section_item(
    title: &str,
    collapsed: bool,
    count: Option<usize>,
    width: usize,
    action: Option<char>,
) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    let left = Span::styled(format!(" {arrow} {title}"), Style::default().bold());
    let Some(count) = count else {
        return ListItem::new(Line::from(left));
    };
    let badge = Span::styled(
        format!(" {count} "),
        Style::default()
            .bg(palette().button_bg)
            .fg(palette().button_fg),
    );
    // Hovering shows the section-wide stage/unstage glyph before the badge.
    let action_span = action.map(|a| Span::styled(format!("{a} "), Style::default().bold()));
    let aw = action_span.as_ref().map(Span::width).unwrap_or(0);
    let pad = width
        .saturating_sub(left.width() + badge.width() + 1 + aw)
        .max(1);
    let mut spans = vec![left, Span::raw(" ".repeat(pad))];
    if let Some(a) = action_span {
        spans.push(a);
    }
    spans.push(badge);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

fn changes_header_item(
    collapsed: bool,
    count: usize,
    width: usize,
    hovered: bool,
    hovered_action: Option<ChangesHeaderAction>,
) -> ListItem<'static> {
    let Some(actions) = hovered
        .then(|| changes_header_action_zones(width as u16, count))
        .flatten()
    else {
        return section_item("Changes", collapsed, Some(count), width, None);
    };
    let arrow = if collapsed { "▸" } else { "▾" };
    let left = Span::styled(format!(" {arrow} Changes"), Style::default().bold());
    let badge = Span::styled(
        format!(" {count} "),
        Style::default()
            .bg(palette().button_bg)
            .fg(palette().button_fg),
    );
    let actions_width = usize::from(actions[2].1.1 - actions[0].1.0);
    let pad = width
        .saturating_sub(left.width() + actions_width + badge.width() + 1)
        .max(1);
    let mut spans = vec![left, Span::raw(" ".repeat(pad))];
    for (action, _) in actions {
        let glyph = match action {
            ChangesHeaderAction::Discard => "↶",
            ChangesHeaderAction::Stash => "⇩",
            ChangesHeaderAction::Stage => "+",
        };
        spans.push(Span::styled(
            format!(" {glyph} "),
            chrome_button_style(hovered_action == Some(action)),
        ));
    }
    spans.push(badge);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

/// The FILE HISTORY header with the followed file's name appended, dimmed.
fn file_history_header(collapsed: bool, file: &str) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {arrow} File History"), Style::default().bold()),
        Span::styled(format!("  {file}"), Style::default().dim()),
    ]))
}

/// One content line inside an expanded drawer. Branch lines highlight the
/// current branch (git's `%(HEAD)` renders it as `* name`).
fn drawer_line(kind: Drawer, text: &str) -> ListItem<'static> {
    let style = match kind {
        Drawer::Branches if text.starts_with('*') => {
            Style::default().fg(palette().untracked).bold()
        }
        _ => Style::default(),
    };
    ListItem::new(Line::from(Span::styled(format!("   {text}"), style)))
}

/// A file row: icon, name colored by status, dimmed parent directory, and a
/// The right-aligned marker and its color: git's letter, except on a conflicted
/// file — `!N` while N conflict blocks remain, `✓` once none do (the file stays
/// unmerged until it is staged, so git's own status cannot say "resolved").
fn status_marker(letter: char, conflicts: Option<usize>) -> (String, Color) {
    match (letter, conflicts) {
        ('!', Some(0)) => ('✓'.to_string(), status_color('✓')),
        ('!', Some(blocks)) => (format!("!{blocks}"), status_color('!')),
        _ => (letter.to_string(), status_color(letter)),
    }
}

/// The two per-row facts a file row draws from beyond its entry: which list it
/// came from (hover actions differ) and how many conflict blocks are left.
#[derive(Clone, Copy)]
struct RowLook {
    staged: bool,
    conflicts: Option<usize>,
}

/// A file row: icon, name colored by status, dimmed parent directory, and a
/// right-aligned status marker — VS Code Source Control's row anatomy.
fn file_item(
    entry: &FileEntry,
    width: usize,
    theme: IconTheme,
    hovered: bool,
    hovered_action: Option<FileHoverAction>,
    look: RowLook,
) -> ListItem<'static> {
    let staged = look.staged;
    let (dir, name) = match entry.path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, entry.path.as_str()),
    };
    let (marker, color) = status_marker(entry.letter, look.conflicts);
    let marker_width = marker.chars().count();
    let file_icon = icon(theme, name, false, false);
    let icon_style = ui_icon_style(file_icon.rgb);
    let mut spans = vec![
        Span::raw("   "),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
    ];
    let actions = file_hover_actions(staged);
    let show_actions =
        hovered && file_hover_action_start(width as u16, staged, marker_width as u16).is_some();
    let actions_width = usize::from(show_actions) * actions.len() * 3;
    let tail = 1 + marker_width + actions_width;
    let prefix_width: usize = spans.iter().map(Span::width).sum();
    let content_width = width.saturating_sub(prefix_width + tail);
    let visible_name = truncate_to(name.to_string(), content_width);
    spans.push(Span::styled(visible_name, Style::default().fg(color)));
    if let Some(dir) = dir {
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let used: usize = spans.iter().map(Span::width).sum();
        let avail = width.saturating_sub(used + tail);
        let text = truncate_to(format!(" {}", dir.replace('/', &sep)), avail);
        if !text.is_empty() {
            spans.push(Span::styled(text, Style::default().dim()));
        }
    }
    let marker_span = Span::styled(marker, Style::default().fg(color).bold());
    let left_width: usize = spans.iter().map(Span::width).sum();
    let pad = width.saturating_sub(left_width + tail);
    spans.push(Span::raw(" ".repeat(pad)));
    if show_actions {
        for action in actions {
            spans.push(Span::styled(
                format!(" {} ", action.glyph()),
                chrome_button_style(hovered_action == Some(*action)),
            ));
        }
    }
    spans.push(marker_span);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

fn dir_hover_action_at(x: u16, width: u16, staged: bool, count: usize) -> Option<FileHoverAction> {
    if width < 10 {
        return None;
    }
    let badge_len = format!(" ({count}) ").len() as u16;
    let action_end = width.saturating_sub(badge_len);
    let action_start = action_end.saturating_sub(3);
    if x >= action_start && x < action_end {
        Some(if staged {
            FileHoverAction::Unstage
        } else {
            FileHoverAction::Stage
        })
    } else {
        None
    }
}

fn tree_file_item(
    entry: &FileEntry,
    depth: usize,
    width: usize,
    theme: IconTheme,
    hovered: bool,
    hovered_action: Option<FileHoverAction>,
    look: RowLook,
) -> ListItem<'static> {
    let staged = look.staged;
    let name = match entry.path.rsplit_once('/') {
        Some((_, name)) => name,
        None => entry.path.as_str(),
    };
    let (marker, color) = status_marker(entry.letter, look.conflicts);
    let marker_width = marker.chars().count();
    let file_icon = icon(theme, name, false, false);
    let icon_style = ui_icon_style(file_icon.rgb);
    let indent = "  ".repeat(depth);
    let mut spans = vec![
        Span::raw(format!(" {indent}  ")),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
    ];
    let actions = file_hover_actions(staged);
    let show_actions =
        hovered && file_hover_action_start(width as u16, staged, marker_width as u16).is_some();
    let actions_width = usize::from(show_actions) * actions.len() * 3;
    let tail = 1 + marker_width + actions_width;
    let prefix_width: usize = spans.iter().map(Span::width).sum();
    let content_width = width.saturating_sub(prefix_width + tail);
    let visible_name = truncate_to(name.to_string(), content_width);
    spans.push(Span::styled(visible_name, Style::default().fg(color)));
    let marker_span = Span::styled(marker, Style::default().fg(color).bold());
    let left_width: usize = spans.iter().map(Span::width).sum();
    let pad = width.saturating_sub(left_width + tail);
    spans.push(Span::raw(" ".repeat(pad)));
    if show_actions {
        for action in actions {
            spans.push(Span::styled(
                format!(" {} ", action.glyph()),
                chrome_button_style(hovered_action == Some(*action)),
            ));
        }
    }
    spans.push(marker_span);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

fn tree_dir_item(
    dir: &DirEntry,
    width: usize,
    theme: IconTheme,
    staged: bool,
    hovered: bool,
    hovered_action: Option<FileHoverAction>,
) -> ListItem<'static> {
    let indent = "  ".repeat(dir.depth);
    let arrow = if dir.expanded { "▾ " } else { "▸ " };
    let folder_icon = icon(theme, &dir.name, true, dir.expanded);
    let icon_style = ui_icon_style(folder_icon.rgb);
    let mut spans = vec![
        Span::styled(format!(" {indent}{arrow}"), Style::default().dim()),
        Span::styled(format!("{} ", folder_icon.glyph), icon_style),
    ];
    let action = if staged {
        FileHoverAction::Unstage
    } else {
        FileHoverAction::Stage
    };
    let show_action = hovered && width >= 10;
    let action_width = if show_action { 3 } else { 0 };
    let badge_str = format!(" ({})", dir.count);
    let tail = 1 + action_width + badge_str.len();
    let prefix_width: usize = spans.iter().map(Span::width).sum();
    let content_width = width.saturating_sub(prefix_width + tail);
    let visible_name = truncate_to(dir.name.clone(), content_width);
    spans.push(Span::styled(visible_name, Style::default().bold()));
    let left_width: usize = spans.iter().map(Span::width).sum();
    let pad = width.saturating_sub(left_width + tail);
    spans.push(Span::raw(" ".repeat(pad)));
    if show_action {
        spans.push(Span::styled(
            format!(" {} ", action.glyph()),
            chrome_button_style(hovered_action == Some(action)),
        ));
    }
    spans.push(Span::styled(badge_str, Style::default().dim()));
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

fn no_repo_layout(area: Rect, merged: bool) -> [Rect; 2] {
    Layout::vertical([
        Constraint::Length(if merged { 3 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(area)
}

/// Stage everything the repository has (nested-repo safe), then ask OpenCommit
/// for a message describing the staged changes. Runs on a worker thread.
fn ai_commit_outcome(git: &Git) -> AiOutcome {
    if let Err(error) = git.stage_under(git.root()) {
        return AiOutcome::Failed(error);
    }
    match git.has_staged() {
        Ok(true) => {}
        Ok(false) => return AiOutcome::NothingToCommit,
        Err(error) => return AiOutcome::Failed(error),
    }
    match herdr_sidebar::ai_commit::generate(git.root()) {
        Ok(generated) => AiOutcome::Message(generated),
        Err(error) => AiOutcome::Failed(error),
    }
}

/// Equal after canonicalizing when both exist, else compared as given.
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_existing_file_opens_in_the_editor_and_a_deletion_keeps_its_diff() {
        let conflicted = FileEntry {
            path: "src/merge.rs".into(),
            orig: None,
            letter: '!',
        };
        let modified = FileEntry {
            path: "src/lib.rs".into(),
            orig: None,
            letter: 'M',
        };
        let untracked = FileEntry {
            path: "new.rs".into(),
            orig: None,
            letter: 'U',
        };
        let deleted = FileEntry {
            path: "src/gone.rs".into(),
            orig: None,
            letter: 'D',
        };
        assert!(opens_in_editor(&conflicted));
        assert!(
            opens_in_editor(&modified),
            "a changed file goes to the editor"
        );
        assert!(opens_in_editor(&untracked));
        assert!(!opens_in_editor(&deleted), "nothing to open: keep the diff");
    }

    #[test]
    fn max_scroll_is_zero_when_everything_fits() {
        assert_eq!(max_scroll_for(&[1, 1, 1], 3), 0);
        assert_eq!(max_scroll_for(&[1, 3, 1], 5), 0);
        assert_eq!(max_scroll_for(&[], 5), 0);
    }

    #[test]
    fn max_scroll_stops_with_last_row_at_bottom() {
        // Ten 1-line rows in a 4-line viewport: the window starts at row 6.
        assert_eq!(max_scroll_for(&[1; 10], 4), 6);
        // Tail [1, 1, 3, 1] (6 lines) overflows 5, so the window starts at 2 ([1, 3, 1]).
        assert_eq!(max_scroll_for(&[3, 1, 1, 3, 1], 5), 2);
    }

    #[test]
    fn max_scroll_handles_variable_heights() {
        // Commit-style 3-line rows mixed with 1-line rows.
        assert_eq!(max_scroll_for(&[1, 3, 1, 3], 6), 2);
        // The 3-line tail plus two 1-line rows fill 5 lines exactly.
        assert_eq!(max_scroll_for(&[1, 1, 1, 3], 5), 1);
    }

    #[test]
    fn max_scroll_returns_last_index_for_row_taller_than_viewport() {
        assert_eq!(max_scroll_for(&[1, 1, 5], 3), 2);
        assert_eq!(max_scroll_for(&[1], 1), 0);
    }

    #[test]
    fn clean_diverged_repo_uses_sync_as_the_primary_action() {
        let status = Status {
            ahead: 43,
            has_upstream: true,
            ..Status::default()
        };
        assert!(sync_is_primary(&status));

        let dirty = Status {
            unstaged: vec![FileEntry {
                path: "README.md".into(),
                orig: None,
                letter: 'M',
            }],
            ..status
        };
        assert!(!sync_is_primary(&dirty));
    }

    #[test]
    fn sync_label_omits_zero_counts() {
        let status = Status {
            ahead: 43,
            has_upstream: true,
            ..Status::default()
        };
        assert_eq!(
            sync_label_for_status(&status, false).as_deref(),
            Some("⟳ Sync Changes 43↑")
        );
    }

    /// A branch created here has no upstream, so git reports no ahead/behind and
    /// the Sync row used to disappear — exactly the case that needs a push
    /// (user-reported). It offers Publish instead; `Git::push` sets the upstream.
    #[test]
    fn a_branch_without_an_upstream_offers_publish() {
        let fresh = Status::default(); // no upstream, nothing ahead/behind
        assert_eq!(
            sync_label_for_status(&fresh, false).as_deref(),
            Some("⟳ Publish Branch")
        );
        // The busy label spins its glyph, so only the words are asserted.
        let publishing = sync_label_for_status(&fresh, true).unwrap();
        assert!(publishing.ends_with(" Publishing…"), "{publishing}");
        // Tracking an upstream with nothing to send keeps the row hidden.
        let tracked = Status {
            has_upstream: true,
            ..Status::default()
        };
        assert_eq!(sync_label_for_status(&tracked, false), None);
    }

    #[test]
    fn narrow_repo_headers_disable_hidden_action_zones() {
        assert_eq!(repo_header_action_zones(4), ((0, 0), (0, 0)));
        assert_eq!(repo_header_action_zones(6), ((0, 3), (3, 6)));
    }

    #[test]
    fn changes_header_actions_are_ordered_and_hide_when_narrow() {
        assert_eq!(changes_header_action_zones(24, 12), None);
        assert_eq!(
            changes_header_action_zones(30, 12),
            Some([
                (ChangesHeaderAction::Discard, (16, 19)),
                (ChangesHeaderAction::Stash, (19, 22)),
                (ChangesHeaderAction::Stage, (22, 25)),
            ])
        );
        assert_eq!(
            changes_header_action_at(16, 30, 12),
            Some(ChangesHeaderAction::Discard)
        );
        assert_eq!(
            changes_header_action_at(21, 30, 12),
            Some(ChangesHeaderAction::Stash)
        );
        assert_eq!(
            changes_header_action_at(24, 30, 12),
            Some(ChangesHeaderAction::Stage)
        );
        assert_eq!(changes_header_action_at(25, 30, 12), None);
    }

    #[test]
    fn changes_header_actions_describe_their_footer_tooltips() {
        assert_eq!(
            ChangesHeaderAction::Discard.footer_hint(),
            "↶ Discard All Changes"
        );
        assert_eq!(ChangesHeaderAction::Stash.footer_hint(), "⇩ Stash Changes");
        assert_eq!(
            ChangesHeaderAction::Stage.footer_hint(),
            "+ Stage All Changes"
        );
    }

    #[test]
    fn file_hover_actions_match_their_rendered_columns() {
        assert_eq!(file_hover_action_start(18, false, 1), None);
        assert_eq!(file_hover_action_start(30, false, 1), Some(19));
        assert_eq!(
            file_hover_action_at(19, 30, false, 1),
            Some(FileHoverAction::Open)
        );
        assert_eq!(
            file_hover_action_at(23, 30, false, 1),
            Some(FileHoverAction::Discard)
        );
        assert_eq!(
            file_hover_action_at(27, 30, false, 1),
            Some(FileHoverAction::Stage)
        );
        assert_eq!(file_hover_action_at(28, 30, false, 1), None);

        assert_eq!(file_hover_action_start(30, true, 1), Some(22));
        assert_eq!(
            file_hover_action_at(22, 30, true, 1),
            Some(FileHoverAction::Open)
        );
        assert_eq!(
            file_hover_action_at(26, 30, true, 1),
            Some(FileHoverAction::Unstage)
        );

        // A conflicted row's marker is two cells wide (`!2`), so the buttons
        // shift one column left with it.
        assert_eq!(file_hover_action_start(30, false, 2), Some(18));
        assert_eq!(
            file_hover_action_at(18, 30, false, 2),
            Some(FileHoverAction::Open)
        );
    }

    #[test]
    fn a_conflicted_row_shows_the_remaining_blocks_then_a_check() {
        assert_eq!(status_marker('!', Some(3)).0, "!3");
        assert_eq!(status_marker('!', Some(0)).0, "✓");
        // An unreadable file keeps git's plain letter: never claim "resolved".
        assert_eq!(status_marker('!', None).0, "!");
        assert_eq!(status_marker('M', None).0, "M");
    }

    #[test]
    fn a_remote_tracking_branch_offers_no_delete_or_rename() {
        fn labels(entries: &[MenuEntry]) -> Vec<&'static str> {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    MenuEntry::Action(_, label) => Some(*label),
                    MenuEntry::Separator => None,
                })
                .collect()
        }

        // The checked-out local branch: rename and undo, never delete.
        let current = labels(&branch_menu_entries(true, true));
        assert!(
            !current.iter().any(|l| l.starts_with("Delete")),
            "{current:?}"
        );
        assert!(
            current.iter().any(|l| l.starts_with("Rename")),
            "{current:?}"
        );
        assert!(current.iter().any(|l| l.starts_with("Undo")), "{current:?}");

        // Another local branch: delete + rename, no undo (that one is HEAD's).
        let other = labels(&branch_menu_entries(false, true));
        assert!(other.iter().any(|l| l.starts_with("Delete")), "{other:?}");
        assert!(other.iter().any(|l| l.starts_with("Rename")), "{other:?}");
        assert!(!other.iter().any(|l| l.starts_with("Undo")), "{other:?}");

        // A remote-tracking ref (`origin/main`): neither, which is the bug that
        // reported "branch not found" after choosing Delete.
        let remote = labels(&branch_menu_entries(false, false));
        assert!(
            !remote.iter().any(|l| l.starts_with("Delete")),
            "{remote:?}"
        );
        assert!(
            !remote.iter().any(|l| l.starts_with("Rename")),
            "{remote:?}"
        );
        assert!(remote.contains(&"Copy Branch Name"), "{remote:?}");
    }

    /// Ctrl+C in a rename field copies it and leaves the dialog alone. It used
    /// to fall through to the `Char(c)` arm and TYPE a "c" — with Ctrl+A armed,
    /// that replaced the whole branch name (user-reported).
    #[test]
    fn ctrl_c_in_a_name_field_copies_instead_of_typing_c() {
        let key = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
        let mut input = "feature/x".to_string();
        let mut select_all = SelectAll::default();

        // Ctrl+A arms the selection, Ctrl+C copies and keeps it armed (the next
        // character still replaces the value, like a real selection).
        assert_eq!(
            field_key(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &mut input,
                &mut select_all
            ),
            FieldKey::Nothing
        );
        assert!(select_all.armed());
        assert_eq!(
            field_key(
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut input,
                &mut select_all
            ),
            FieldKey::Copy
        );
        assert_eq!(input, "feature/x", "Ctrl+C must not touch the field");
        assert!(select_all.armed(), "the selection survives a copy");

        // AltGr arrives as CONTROL|ALT: it must keep typing a literal "c".
        let mut altgr = "ab".to_string();
        let mut no_selection = SelectAll::default();
        assert_eq!(
            field_key(
                key(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                ),
                &mut altgr,
                &mut no_selection
            ),
            FieldKey::Nothing
        );
        assert_eq!(altgr, "abc");
    }

    #[test]
    fn file_hover_actions_describe_their_footer_tooltips() {
        assert_eq!(FileHoverAction::Open.footer_hint(), "↗ Open in Editor");
        assert_eq!(FileHoverAction::Discard.footer_hint(), "↶ Discard Changes");
        assert_eq!(FileHoverAction::Stage.footer_hint(), "+ Stage Changes");
        assert_eq!(FileHoverAction::Unstage.footer_hint(), "− Unstage Changes");
    }

    #[test]
    fn stationary_hover_uses_the_latest_list_geometry() {
        let mouse_row = 12;
        let before = BodyGeom {
            top: 10,
            height: 5,
            offset: 2,
        };
        assert_eq!(
            row_hit_with_heights(before, mouse_row, [(2, 1), (3, 3), (4, 1)]),
            Some((3, 1))
        );

        let after = BodyGeom {
            top: 11,
            height: 3,
            offset: 0,
        };
        assert_eq!(
            row_hit_with_heights(after, mouse_row, [(0, 1), (1, 1), (2, 1)]),
            Some((1, 0))
        );
    }

    #[test]
    fn any_commit_message_pauses_cwd_follow() {
        let empty: &[char] = &[];
        let draft = ['d', 'r', 'a', 'f', 't'];
        assert!(!commit_draft_present([empty, empty]));
        assert!(commit_draft_present([empty, draft.as_slice()]));
    }

    #[test]
    fn background_refresh_is_opt_in() {
        assert!(!sidebar::background_refresh_enabled(None));
        assert!(!sidebar::background_refresh_enabled(Some("")));
        assert!(!sidebar::background_refresh_enabled(Some("0")));
        assert!(!sidebar::background_refresh_enabled(Some("true")));
        assert!(sidebar::background_refresh_enabled(Some("1")));
        assert!(sidebar::background_refresh_enabled(Some(" 1 ")));
    }

    #[test]
    fn drawer_lines_parse_into_actionable_refs() {
        assert_eq!(
            parse_drawer_ref(Drawer::Commits, "a1b2c3d Add telemetry module"),
            DrawerRef::Commit("a1b2c3d".into())
        );
        // Graph edge-only lines carry no commit; subject words never match
        // (uppercase or non-hex letters, or too short).
        assert_eq!(parse_drawer_ref(Drawer::Graph, "| \\"), DrawerRef::None);
        assert_eq!(
            parse_drawer_ref(
                Drawer::Graph,
                "* deadbee (HEAD -> main) Added decoded fallback"
            ),
            DrawerRef::Commit("deadbee".into())
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Branches, "* main"),
            DrawerRef::Branch {
                name: "main".into(),
                current: true
            }
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Branches, "  origin/main"),
            DrawerRef::Branch {
                name: "origin/main".into(),
                current: false
            }
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Stashes, "stash@{2}: WIP on main: 1a2b3c4 x"),
            DrawerRef::Stash(2)
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Remotes, "origin  https://github.com/a/b.git"),
            DrawerRef::Remote {
                name: "origin".into(),
                url: "https://github.com/a/b.git".into()
            }
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Tags, "v0.1.0"),
            DrawerRef::Tag("v0.1.0".into())
        );
        assert_eq!(parse_drawer_ref(Drawer::Tags, "(none)"), DrawerRef::None);
        assert_eq!(
            parse_drawer_ref(Drawer::Worktrees, "C:/Users/x/proj  a1b2c3d [main]"),
            DrawerRef::Worktree("C:/Users/x/proj".into())
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Worktrees, "(none)"),
            DrawerRef::None
        );
    }

    #[test]
    fn drawer_git_specs_preserve_branch_kind_and_qualify_tags() {
        assert_eq!(
            drawer_git_spec(&DrawerRef::Branch {
                name: "--upload-pack=bad".into(),
                current: false,
            })
            .as_deref(),
            Some("--upload-pack=bad")
        );
        assert_eq!(
            drawer_git_spec(&DrawerRef::Tag("--output=pwned.txt".into())).as_deref(),
            Some("refs/tags/--output=pwned.txt")
        );
        assert_eq!(
            drawer_git_spec(&DrawerRef::Commit("deadbee".into())).as_deref(),
            Some("deadbee")
        );
    }

    #[test]
    fn drawer_checkout_and_reset_arguments_work_in_a_real_repo() {
        let root = std::env::temp_dir().join(format!(
            "herdr-drawer-git-actions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        for message in ["one", "two"] {
            assert!(
                git(&[
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "user.name=Test",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    message,
                ])
                .status
                .success()
            );
        }
        let second = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();
        let first = String::from_utf8(git(&["rev-parse", "HEAD^"]).stdout)
            .unwrap()
            .trim()
            .to_string();

        let reset = drawer_reset_args(&first);
        let reset = reset.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(git(&reset).status.success());
        assert_eq!(
            String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim(),
            first
        );

        assert!(
            git(&["update-ref", "refs/remotes/origin/topic", &second])
                .status
                .success()
        );
        let remote = DrawerRef::Branch {
            name: "origin/topic".into(),
            current: false,
        };
        let checkout = drawer_checkout_args(&remote, "origin/topic", "origin/topic").unwrap();
        let checkout = checkout.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(git(&checkout).status.success());
        assert_eq!(
            String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
                .unwrap()
                .trim(),
            second
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unified_no_repo_layout_keeps_the_activity_bar_visible() {
        let area = Rect::new(4, 7, 40, 20);
        let [activity, body] = no_repo_layout(area, true);
        assert_eq!(activity, Rect::new(4, 7, 40, 3));
        assert_eq!(body, Rect::new(4, 10, 40, 17));

        let [activity, body] = no_repo_layout(area, false);
        assert_eq!(activity.height, 0);
        assert_eq!(body, area);
    }

    #[test]
    fn drawer_lines_prettify_for_display() {
        assert_eq!(
            pretty_worktree_line("C:/Users/x/Projects/herdr  7c12b6d [main]"),
            "herdr  ⎇ main"
        );
        assert_eq!(
            pretty_worktree_line("C:/x/wt-fix  1a2b3c4 (detached HEAD)"),
            "wt-fix  (detached)"
        );
        assert_eq!(pretty_worktree_line("(none)"), "(none)");
        assert_eq!(
            pretty_remote_line("origin  https://github.com/alexarthurs/herdr-sidebar.git"),
            "origin  alexarthurs/herdr-sidebar"
        );
        assert_eq!(
            pretty_remote_line("up  git@github.com:me/repo.git"),
            "up  me/repo"
        );
        assert_eq!(
            pretty_remote_line("origin  C:/Users/x/Projects/.demo-origin.git"),
            "origin  .demo-origin"
        );
        assert_eq!(pretty_remote_line("(none)"), "(none)");
    }

    #[test]
    fn menu_navigation_skips_separators_and_clamps() {
        let entries = [
            MenuEntry::Action(MenuAction::StageOrUnstage, "Stage Changes"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
        ];
        assert_eq!(step_menu(&entries, 0, -1), 0);
        assert_eq!(step_menu(&entries, 0, 1), 2, "skips the separator");
        assert_eq!(step_menu(&entries, 2, 1), 2);
    }

    #[test]
    fn drawer_titles_are_title_case_and_include_worktrees() {
        let titles: Vec<&str> = Drawer::ALL.iter().map(|d| d.title()).collect();
        assert_eq!(
            titles,
            [
                "Graph",
                "Commits",
                "File History",
                "Branches",
                "Worktrees",
                "Remotes",
                "Stashes",
                "Tags"
            ]
        );
    }

    #[test]
    fn build_tree_model_compacts_single_child_directories() {
        let files = vec![
            FileEntry {
                path: "deep/nested/subfolder/file1.rs".into(),
                orig: None,
                letter: 'M',
            },
            FileEntry {
                path: "deep/nested/subfolder/file2.rs".into(),
                orig: None,
                letter: 'A',
            },
            FileEntry {
                path: "other/single.txt".into(),
                orig: None,
                letter: '?',
            },
        ];

        let collapsed = std::collections::BTreeSet::new();
        let (dirs, file_depths, tree_rows) = build_tree_model(&files, &collapsed);

        // "deep/nested/subfolder" has 1 child ("nested"), which has 1 child ("subfolder"),
        // which has 2 files. So "deep/nested/subfolder" is compacted into one DirEntry.
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].name, "deep/nested/subfolder");
        assert_eq!(dirs[0].path, "deep/nested/subfolder");
        assert_eq!(dirs[0].count, 2);
        assert_eq!(dirs[0].depth, 0);

        assert_eq!(dirs[1].name, "other");
        assert_eq!(dirs[1].path, "other");
        assert_eq!(dirs[1].count, 1);
        assert_eq!(dirs[1].depth, 0);

        // Rows when nothing is collapsed:
        // Dir(0) "deep/nested/subfolder"
        // File(0) "file1.rs" (depth 1)
        // File(1) "file2.rs" (depth 1)
        // Dir(1) "other"
        // File(2) "single.txt" (depth 1)
        assert_eq!(tree_rows.len(), 5);
        assert_eq!(tree_rows[0], TreeItem::Dir(0));
        assert_eq!(tree_rows[1], TreeItem::File(0));
        assert_eq!(tree_rows[2], TreeItem::File(1));
        assert_eq!(tree_rows[3], TreeItem::Dir(1));
        assert_eq!(tree_rows[4], TreeItem::File(2));

        assert_eq!(file_depths[0], 1);
        assert_eq!(file_depths[1], 1);
        assert_eq!(file_depths[2], 1);
    }

    #[test]
    fn collapsed_folders_are_pruned_only_against_loaded_content() {
        let files = vec![FileEntry {
            path: "src/a.rs".into(),
            orig: None,
            letter: 'M',
        }];
        let mut collapsed: std::collections::BTreeSet<String> =
            ["src".to_string(), "gone".to_string()]
                .into_iter()
                .collect();
        prune_collapsed(&mut collapsed, &[]);
        assert_eq!(collapsed.len(), 2, "an empty status never wipes state");
        prune_collapsed(&mut collapsed, &files);
        assert_eq!(
            collapsed.into_iter().collect::<Vec<_>>(),
            vec!["src".to_string()]
        );
    }

    /// Collapse a nested folder, then its parent: the child is no longer a
    /// visible row, but it still exists and must keep its collapsed state
    /// through a rebuild, so expanding the parent shows it still folded.
    #[test]
    fn a_hidden_child_keeps_its_collapse_through_a_parent_collapse() {
        let files = vec![
            FileEntry {
                path: "src/api/routes.rs".into(),
                orig: None,
                letter: 'M',
            },
            FileEntry {
                path: "src/main.rs".into(),
                orig: None,
                letter: 'M',
            },
        ];
        let mut collapsed: std::collections::BTreeSet<String> =
            ["src/api".to_string()].into_iter().collect();
        collapsed.insert("src".to_string());
        // Rebuild while the parent hides the child, as `build_trees` does.
        let (visible, _, _) = build_tree_model(&files, &collapsed);
        assert!(visible.iter().all(|dir| dir.path != "src/api"));
        prune_collapsed(&mut collapsed, &files);
        assert!(collapsed.contains("src/api"), "hidden child state survives");

        collapsed.remove("src");
        let (dirs, _, rows) = build_tree_model(&files, &collapsed);
        let api = dirs.iter().position(|dir| dir.path == "src/api").unwrap();
        assert!(!dirs[api].expanded, "the child reappears still collapsed");
        assert!(
            !rows.contains(&TreeItem::File(0)),
            "its file stays hidden until the child is expanded"
        );
        assert!(rows.contains(&TreeItem::File(1)));
    }

    #[test]
    fn build_tree_model_respects_collapsed_directories() {
        let files = vec![
            FileEntry {
                path: "src/a.rs".into(),
                orig: None,
                letter: 'M',
            },
            FileEntry {
                path: "src/b.rs".into(),
                orig: None,
                letter: 'M',
            },
            FileEntry {
                path: "root.txt".into(),
                orig: None,
                letter: 'A',
            },
        ];

        let mut collapsed = std::collections::BTreeSet::new();
        collapsed.insert("src".to_string());

        let (dirs, file_depths, tree_rows) = build_tree_model(&files, &collapsed);

        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name, "src");
        assert_eq!(dirs[0].count, 2);

        // Children of "src" should be hidden since "src" is collapsed
        // Expected tree_rows: Dir(0) "src", File(2) "root.txt"
        assert_eq!(tree_rows.len(), 2);
        assert_eq!(tree_rows[0], TreeItem::Dir(0));
        assert_eq!(tree_rows[1], TreeItem::File(2));
        assert_eq!(file_depths[2], 0);
    }

    #[test]
    fn dir_hover_action_hit_testing() {
        // In width 40 with count 3:
        // Columns 32..35 are the stage/unstage button, 35..39 is the count badge " (3)", and 39 is trailing space.
        let action = dir_hover_action_at(33, 40, true, 3);
        assert_eq!(action, Some(FileHoverAction::Unstage));

        let action_unstaged = dir_hover_action_at(33, 40, false, 3);
        assert_eq!(action_unstaged, Some(FileHoverAction::Stage));

        // Click on count badge or directory name should return None
        let action_badge = dir_hover_action_at(38, 40, true, 3);
        assert_eq!(action_badge, None);

        let action_left = dir_hover_action_at(10, 40, true, 3);
        assert_eq!(action_left, None);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// A repo whose commit message holds `text`. `Repo` wraps a `Git`, so this
    /// makes a throwaway one; the box's input rules are what is under test.
    fn message_repo(tag: &str, text: &str) -> Repo {
        let root = std::env::temp_dir().join(format!("herdr-scm-msg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Git::init(&root).unwrap();
        let mut repo = Repo::new(Git::discover(&root).unwrap());
        repo.message = text.chars().collect();
        repo.cursor = repo.message.len();
        repo
    }

    fn message_text(repo: &Repo) -> String {
        repo.message.iter().collect()
    }

    #[test]
    fn ctrl_a_then_typing_replaces_the_whole_commit_message() {
        let mut repo = message_repo("typing", "héllo");
        repo.message_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(repo.select_all.armed(), "ctrl+a selects the message");
        // The next typed character replaces everything, not just the caret.
        repo.message_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(message_text(&repo), "x");
        assert_eq!(repo.cursor, 1);
        assert!(!repo.select_all.armed());

        // An empty box has nothing to select.
        repo.message_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        repo.message_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!repo.select_all.armed());
    }

    #[test]
    fn ctrl_alt_a_is_text_not_select_all() {
        let mut repo = message_repo("altgr", "ab");
        repo.message_key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(!repo.select_all.armed(), "ctrl+alt is a typed character");
        assert_eq!(message_text(&repo), "aba");
    }

    #[test]
    fn ctrl_a_then_backspace_clears_the_whole_commit_message() {
        let mut repo = message_repo("backspace", "héllo");
        repo.message_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        repo.message_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(message_text(&repo), "", "one backspace takes the selection");
        assert_eq!(repo.cursor, 0);
        assert!(!repo.select_all.armed());
    }

    #[test]
    fn ctrl_a_is_dropped_by_a_caret_move() {
        let mut repo = message_repo("caret", "abc");
        repo.message_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        repo.message_key(key(KeyCode::Left, KeyModifiers::NONE));
        assert!(!repo.select_all.armed(), "moving the caret drops it");
        // A plain backspace after the move deletes one character, not all.
        repo.message_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(message_text(&repo), "ac");
        assert_eq!(repo.cursor, 1);
    }

    #[test]
    fn ctrl_a_is_dropped_by_esc_before_esc_leaves_the_box() {
        let mut repo = message_repo("esc", "abc");
        repo.message_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(repo.esc_message(), "the first Esc only drops the selection");
        assert!(!repo.select_all.armed());
        assert!(
            !repo.esc_message(),
            "the next Esc is the one that leaves the box"
        );
    }
}

#[cfg(test)]
mod ai_commit_button_tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    use std::time::Duration;

    const BASE_BG: Color = Color::Rgb(1, 1, 1);
    const BAND_BG: Color = Color::Rgb(2, 2, 2);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Per-cell background, expanding each span by its display width.
    fn cell_bgs(spans: &[Span<'static>]) -> Vec<Option<Color>> {
        let mut out = Vec::new();
        for span in spans {
            for c in span.content.chars() {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                for _ in 0..w {
                    out.push(span.style.bg);
                }
            }
        }
        out
    }

    fn band_cells(spans: &[Span<'static>]) -> Vec<usize> {
        cell_bgs(spans)
            .iter()
            .enumerate()
            .filter(|(_, bg)| **bg == Some(BAND_BG))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn shimmer_spans_always_fill_exactly_width_cells() {
        let labels = [
            "",
            "✓ Commit",
            "⠋ AI generating…",
            "漢字テスト",
            "a very long label indeed",
        ];
        for width in [0usize, 1, 2, 5, 6, 7, 10, 30] {
            for label in labels {
                for elapsed in [0u64, 120, 500, 9_999] {
                    let spans = shimmer_spans(
                        label,
                        width,
                        ms(elapsed),
                        Style::default().bg(BASE_BG),
                        BAND_BG,
                    );
                    let total: usize = spans.iter().map(|s| s.width()).sum();
                    assert_eq!(total, width, "label {label:?} width {width} at {elapsed}ms");
                }
            }
        }
    }

    #[test]
    fn shimmer_band_starts_at_cell_zero_and_advances_two_cells_per_frame() {
        let base = Style::default().bg(BASE_BG);
        let at = |elapsed| band_cells(&shimmer_spans("", 10, ms(elapsed), base, BAND_BG));
        // Band of 6 cells, clipped to the 10-cell button.
        assert_eq!(at(0), (0..6).collect::<Vec<_>>());
        assert_eq!(at(119), (0..6).collect::<Vec<_>>());
        assert_eq!(at(120), (2..8).collect::<Vec<_>>());
        assert_eq!(at(240), (4..10).collect::<Vec<_>>());
        // period = width + band = 16: after 8 frames the band wraps to cell 0.
        assert_eq!(at(960), (0..6).collect::<Vec<_>>());
    }

    #[test]
    fn shimmer_keeps_label_foreground_and_centers_it() {
        let fg = Color::Rgb(9, 9, 9);
        let base = Style::default().bg(BASE_BG).fg(fg);
        let spans = shimmer_spans("ab", 10, ms(0), base, BAND_BG);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "    ab    ");
        assert!(spans.iter().all(|s| s.style.fg == Some(fg)));
    }

    #[test]
    fn spinner_frame_index_cycles_every_ten_steps() {
        let frame = |elapsed| ai_button_label(AiPhase::Generating, ms(elapsed));
        assert!(frame(0).starts_with("⠋ AI generating"));
        assert!(frame(120).starts_with("⠙ AI generating"));
        assert!(frame(1_080).starts_with("⠏ AI generating"));
        assert!(frame(1_200).starts_with("⠋ AI generating"));
    }

    #[test]
    fn ai_label_overrides_sync_label() {
        let ahead = Status {
            ahead: 43,
            has_upstream: true,
            ..Status::default()
        };
        assert!(sync_is_primary(&ahead));
        let ai = Some((AiPhase::Generating, ms(0)));
        assert_eq!(button_label(&ahead, false, ai), "⠋ AI generating…");
        assert_eq!(button_label(&ahead, false, None), "⟳ Sync Changes 43↑");
        assert_eq!(button_label(&Status::default(), false, None), "✓ Commit");
    }

    /// Behind the upstream AND with changes staged (so the button is a Commit,
    /// not a Sync): the label says a pull is on offer.
    #[test]
    fn commit_button_says_how_far_behind_when_it_offers_a_pull() {
        let behind = Status {
            behind: 2,
            has_upstream: true,
            // Staged work is what makes this a Commit button rather than Sync.
            staged: vec![FileEntry {
                path: "a.txt".into(),
                orig: None,
                letter: 'M',
            }],
            ..Status::default()
        };
        assert!(!sync_is_primary(&behind));
        assert_eq!(button_label(&behind, false, None), "✓ Commit · 2↓");

        // Nothing staged: still Sync Changes, which pulls AND pushes.
        let clean = Status {
            behind: 2,
            has_upstream: true,
            ..Status::default()
        };
        assert_eq!(button_label(&clean, false, None), "⟳ Sync Changes 2↓");

        // Not behind: the plain label, unchanged.
        let ahead_only = Status {
            ahead: 1,
            behind: 0,
            has_upstream: true,
            staged: vec![FileEntry {
                path: "a.txt".into(),
                orig: None,
                letter: 'M',
            }],
            ..Status::default()
        };
        assert_eq!(button_label(&ahead_only, false, None), "✓ Commit");
    }

    #[test]
    fn commit_button_row_shows_ai_label_when_sync_is_primary() {
        let ahead = Status {
            ahead: 3,
            has_upstream: true,
            ..Status::default()
        };
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        let item = commit_button_item(
            &ahead,
            false,
            Some((AiPhase::Generating, ms(240))),
            true,
            false,
            30,
        );
        List::new(vec![item]).render(area, &mut buf);
        let row: String = (0..30).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(row.contains("⠹ AI generating"), "row was {row:?}");
        assert!(!row.contains("Sync"), "row was {row:?}");
    }
}
