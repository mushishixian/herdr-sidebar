//! OS file-change notifications for the Explorer and Source Control views.
//!
//! The views used to learn about changes only from polling (the focused
//! Explorer's 2 s decoration read, the focused SCM's 2 s `git status`). This
//! module runs a background thread that watches the directories a view shows
//! and publishes two coalesced flags: `tree` (a listing changed) and `git`
//! (something git reports may have changed). The owner polls [`FsWatch::take`]
//! from its tick and does the actual refresh there, so no view state is ever
//! touched from the watcher thread.
//!
//! What is watched:
//! - `tree_dirs`: non-recursively (the Explorer's root plus every expanded
//!   directory, exactly what the tree lists).
//! - `repo_roots`: work trees, so an edit anywhere in a repository updates git
//!   status. On Linux (inotify, one watch per directory) each root is walked
//!   with git ignore rules, so `target/` and `node_modules/` cost nothing; the
//!   total is capped at [`MAX_WALKED_DIRS`]. Elsewhere the root is watched
//!   recursively and events under `.git`, `target` and `node_modules` are
//!   dropped.
//! - `git_dirs`: the repository's real git dir (for a linked worktree, where
//!   its `.git` file points), non-recursively, plus `refs/` recursively.
//!
//! Everything here is best effort: [`FsWatch::start`] returns `None` when the
//! OS watcher cannot be created, and a directory that cannot be watched is
//! skipped. Callers keep their polling fallback for filesystems that emit no
//! events (WSL `/mnt/c`, network shares).

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period after the last relevant event before the flags are published.
const QUIET: Duration = Duration::from_millis(150);
/// Longest a storm of events may hold back a publication, which also bounds
/// publications to about one per second.
const MAX_LATENCY: Duration = Duration::from_secs(1);
/// Directories watched under repository roots (Linux inotify).
const MAX_WALKED_DIRS: usize = 4000;

/// The directories one [`FsWatch`] should observe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchSet {
    pub tree_dirs: Vec<PathBuf>,
    pub repo_roots: Vec<PathBuf>,
    pub git_dirs: Vec<PathBuf>,
}

/// Coalesced changes since the last [`FsWatch::take`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changes {
    /// Something a directory listing shows may have changed.
    pub tree: bool,
    /// Something git reports (status, refs, HEAD, index) may have changed.
    pub git: bool,
    /// Something inside a git dir changed (HEAD, index, refs): branches,
    /// commits and stashes, which `git status` alone does not show.
    pub refs: bool,
}

#[derive(Default)]
struct Flags {
    tree: AtomicBool,
    git: AtomicBool,
    refs: AtomicBool,
}

enum Msg {
    Fs(notify::Result<notify::Event>),
    Update(WatchSet),
    Stop,
}

/// A running watcher. Dropping it stops and joins its thread.
pub struct FsWatch {
    flags: Arc<Flags>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl FsWatch {
    /// Start watching `set`. `None` when the OS watcher cannot be created.
    pub fn start(set: WatchSet) -> Option<FsWatch> {
        let (tx, rx) = mpsc::channel();
        let fs_tx = tx.clone();
        let watcher = notify::recommended_watcher(move |res| {
            let _ = fs_tx.send(Msg::Fs(res));
        })
        .ok()?;
        let flags = Arc::new(Flags::default());
        let worker_flags = Arc::clone(&flags);
        let thread = std::thread::Builder::new()
            .name("fswatch".into())
            .spawn(move || {
                let mut worker = Worker::new(watcher, worker_flags);
                worker.apply(set);
                worker.run(rx);
            })
            .ok()?;
        Some(FsWatch {
            flags,
            tx,
            thread: Some(thread),
        })
    }

    /// Replace the watched directories. Cheap to call; the watcher thread
    /// adds and removes only what changed.
    pub fn update(&self, set: WatchSet) {
        let _ = self.tx.send(Msg::Update(set));
    }

    /// Return and clear the flags accumulated since the last call.
    pub fn take(&self) -> Changes {
        Changes {
            tree: self.flags.tree.swap(false, Ordering::AcqRel),
            git: self.flags.git.swap(false, Ordering::AcqRel),
            refs: self.flags.refs.swap(false, Ordering::AcqRel),
        }
    }
}

impl Drop for FsWatch {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Events waiting for the quiet period.
struct Pending {
    first: Instant,
    last: Instant,
    tree: bool,
    git: bool,
    refs: bool,
}

impl Pending {
    fn deadline(&self) -> Instant {
        (self.last + QUIET).min(self.first + MAX_LATENCY)
    }
}

enum Class {
    /// A work-tree path in a directory the tree shows: listing and status.
    Shown,
    /// A work-tree path elsewhere: status only.
    Tree,
    /// Inside a git dir: status and refs.
    Git,
    Ignore,
}

struct Worker {
    watcher: RecommendedWatcher,
    flags: Arc<Flags>,
    set: WatchSet,
    /// What is currently registered with the OS watcher.
    watched: BTreeMap<PathBuf, RecursiveMode>,
    /// Linux: directories under repo roots watched one by one.
    #[cfg(target_os = "linux")]
    walked: BTreeSet<PathBuf>,
    /// Linux: the roots `walked` was computed for.
    #[cfg(target_os = "linux")]
    walked_roots: Vec<PathBuf>,
    pending: Option<Pending>,
}

impl Worker {
    fn new(watcher: RecommendedWatcher, flags: Arc<Flags>) -> Worker {
        Worker {
            watcher,
            flags,
            set: WatchSet::default(),
            watched: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            walked: BTreeSet::new(),
            #[cfg(target_os = "linux")]
            walked_roots: Vec::new(),
            pending: None,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            if let Some(pending) = &self.pending
                && Instant::now() >= pending.deadline()
            {
                self.publish();
            }
            let msg = match self.pending.as_ref().map(Pending::deadline) {
                None => match rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                },
                Some(deadline) => {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            };
            match msg {
                Msg::Fs(res) => self.on_event(res, Instant::now()),
                Msg::Update(set) => self.apply(set),
                Msg::Stop => break,
            }
        }
    }

    fn publish(&mut self) {
        if let Some(pending) = self.pending.take() {
            if pending.tree {
                self.flags.tree.store(true, Ordering::Release);
            }
            if pending.git {
                self.flags.git.store(true, Ordering::Release);
            }
            if pending.refs {
                self.flags.refs.store(true, Ordering::Release);
            }
        }
    }

    fn mark(&mut self, tree: bool, git: bool, refs: bool, now: Instant) {
        let pending = self.pending.get_or_insert(Pending {
            first: now,
            last: now,
            tree: false,
            git: false,
            refs: false,
        });
        pending.last = now;
        pending.tree |= tree;
        pending.git |= git;
        pending.refs |= refs;
    }

    fn on_event(&mut self, res: notify::Result<notify::Event>, now: Instant) {
        let event = match res {
            Ok(event) => event,
            // The backend lost track (queue overflow and the like): rescan.
            Err(_) => {
                self.mark(true, true, true, now);
                return;
            }
        };
        if !relevant(&event.kind) {
            return;
        }
        for path in &event.paths {
            if matches!(event.kind, EventKind::Create(_)) && path.is_dir() {
                self.adopt_new_dir(path);
            }
            match self.classify(path) {
                Class::Shown => self.mark(true, true, false, now),
                Class::Tree => self.mark(false, true, false, now),
                Class::Git => self.mark(false, true, true, now),
                Class::Ignore => {}
            }
        }
    }

    fn classify(&self, path: &Path) -> Class {
        if let Some(git_dir) = self.set.git_dirs.iter().find(|g| path.starts_with(g)) {
            let is_lock = path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".lock"));
            let churn = path
                .strip_prefix(git_dir)
                .map(|rest| {
                    rest.components()
                        .any(|c| c.as_os_str() == "objects" || c.as_os_str() == "logs")
                })
                .unwrap_or(false);
            return if is_lock || churn {
                Class::Ignore
            } else {
                Class::Git
            };
        }
        if path.components().any(|c| c.as_os_str() == ".git") {
            return Class::Ignore;
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Recursive repo watches also see build output. An expanded tree
            // directory still shows its own children, so those stay visible.
            let build_dir = path
                .components()
                .any(|c| c.as_os_str() == "target" || c.as_os_str() == "node_modules");
            let parent_shown = path
                .parent()
                .is_some_and(|parent| self.set.tree_dirs.iter().any(|d| d == parent));
            if build_dir && !parent_shown {
                return Class::Ignore;
            }
        }
        // Only a change in a directory the tree lists (or to such a directory
        // itself) changes what the tree shows; any other edit is status only.
        let shown = path
            .parent()
            .is_some_and(|parent| self.set.tree_dirs.iter().any(|d| d == parent))
            || self.set.tree_dirs.iter().any(|d| d == path);
        if shown { Class::Shown } else { Class::Tree }
    }

    /// Linux only: a directory created inside a repo work tree gets its own
    /// watch, so files later written into it are seen.
    #[cfg(target_os = "linux")]
    fn adopt_new_dir(&mut self, dir: &Path) {
        let in_repo = self.set.repo_roots.iter().any(|r| dir.starts_with(r));
        let in_git = self.set.git_dirs.iter().any(|g| dir.starts_with(g));
        if !in_repo || in_git {
            return;
        }
        let budget = MAX_WALKED_DIRS.saturating_sub(self.walked.len());
        let mut found = BTreeSet::new();
        walk_into(dir, &mut found, budget);
        for new in found {
            if !self.walked.contains(&new) {
                self.walked.insert(new.clone());
                if !self.watched.contains_key(&new)
                    && self
                        .watcher
                        .watch(&new, RecursiveMode::NonRecursive)
                        .is_ok()
                {
                    self.watched.insert(new, RecursiveMode::NonRecursive);
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn adopt_new_dir(&mut self, _dir: &Path) {}

    /// The registrations a set needs, given the directories already walked.
    fn plan(&mut self, set: &WatchSet) -> BTreeMap<PathBuf, RecursiveMode> {
        let mut want = BTreeMap::new();
        #[cfg(target_os = "linux")]
        {
            if self.walked_roots != set.repo_roots {
                self.walked.clear();
                let budget = MAX_WALKED_DIRS;
                for root in &set.repo_roots {
                    let room = budget.saturating_sub(self.walked.len());
                    walk_into(root, &mut self.walked, room);
                }
                self.walked_roots = set.repo_roots.clone();
            }
            for dir in &self.walked {
                want.insert(dir.clone(), RecursiveMode::NonRecursive);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            for root in &set.repo_roots {
                want.insert(root.clone(), RecursiveMode::Recursive);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let covered = |p: &Path| set.repo_roots.iter().any(|r| p.starts_with(r));
        #[cfg(target_os = "linux")]
        let covered = |_: &Path| false;

        for dir in &set.tree_dirs {
            if !covered(dir) {
                want.entry(dir.clone())
                    .or_insert(RecursiveMode::NonRecursive);
            }
        }
        for git_dir in &set.git_dirs {
            if !covered(git_dir) {
                want.entry(git_dir.clone())
                    .or_insert(RecursiveMode::NonRecursive);
            }
            let refs = git_dir.join("refs");
            if !covered(&refs) {
                want.insert(refs, RecursiveMode::Recursive);
            }
        }
        want
    }

    fn apply(&mut self, set: WatchSet) {
        let want = self.plan(&set);
        self.set = set;
        let stale: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|path| !want.contains_key(*path))
            .cloned()
            .collect();
        for path in stale {
            let _ = self.watcher.unwatch(&path);
            self.watched.remove(&path);
        }
        for (path, mode) in want {
            if self.watched.get(&path) == Some(&mode) {
                continue;
            }
            if self.watched.contains_key(&path) {
                let _ = self.watcher.unwatch(&path);
                self.watched.remove(&path);
            }
            if self.watcher.watch(&path, mode).is_ok() {
                self.watched.insert(path, mode);
            }
        }
    }
}

fn relevant(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(_)
        | EventKind::Other
        | EventKind::Any => true,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
    }
}

/// Every directory under `root` that git would not ignore, `root` included,
/// up to `cap` entries. `.git` is skipped and symlinks are never followed.
#[cfg(target_os = "linux")]
fn walk_into(root: &Path, into: &mut BTreeSet<PathBuf>, cap: usize) {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if into.len() >= cap {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            into.insert(entry.into_path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("fswatch-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake repository: `.git` with HEAD and refs/heads, a `.gitignore`
    /// ignoring `target/`, and an existing `target/` directory.
    fn fixture(name: &str) -> (PathBuf, WatchSet) {
        let root = scratch(name);
        let git = root.join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("refs/heads/main"), "0123\n").unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        let set = WatchSet {
            tree_dirs: vec![root.clone()],
            repo_roots: vec![root.clone()],
            git_dirs: vec![git],
        };
        (root, set)
    }

    /// Accumulate flags for up to `for_`, stopping early once `done` holds.
    fn collect(watch: &FsWatch, for_: Duration, done: impl Fn(Changes) -> bool) -> Changes {
        let deadline = Instant::now() + for_;
        let mut acc = Changes::default();
        loop {
            let c = watch.take();
            acc.tree |= c.tree;
            acc.git |= c.git;
            acc.refs |= c.refs;
            if done(acc) || Instant::now() >= deadline {
                return acc;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Give the watcher thread time to register its watches after `start`.
    fn start(set: WatchSet) -> FsWatch {
        let watch = FsWatch::start(set).expect("watcher should start");
        std::thread::sleep(Duration::from_millis(300));
        watch
    }

    #[test]
    fn file_creation_reports_tree_and_git() {
        let (root, set) = fixture("create");
        let watch = start(set);
        fs::write(root.join("new.txt"), "hi").unwrap();
        let c = collect(&watch, Duration::from_secs(3), |c| c.tree && c.git);
        assert!(c.tree && c.git, "got {c:?}");
        assert!(!c.refs, "a work-tree edit must not reload refs");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn head_write_reports_git_only() {
        let (root, set) = fixture("head");
        let watch = start(set);
        collect(&watch, Duration::from_millis(500), |_| false);
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        let c = collect(&watch, Duration::from_secs(3), |c| c.git);
        assert!(c.git, "git change not seen");
        assert!(!c.tree, "HEAD write must not refresh the tree");
        assert!(c.refs, "HEAD write must reload refs");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn index_lock_reports_nothing() {
        let (root, set) = fixture("lock");
        let watch = start(set);
        collect(&watch, Duration::from_millis(500), |_| false);
        fs::write(root.join(".git/index.lock"), "").unwrap();
        let c = collect(&watch, Duration::from_millis(800), |_| false);
        assert_eq!(c, Changes::default());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn ignored_directory_reports_nothing() {
        let (root, set) = fixture("ignored");
        let watch = start(set);
        collect(&watch, Duration::from_millis(500), |_| false);
        fs::write(root.join("target/x"), "built").unwrap();
        let c = collect(&watch, Duration::from_millis(800), |_| false);
        assert_eq!(c, Changes::default());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn new_subdirectory_is_watched() {
        let (root, set) = fixture("subdir");
        let watch = start(set);
        fs::create_dir(root.join("sub")).unwrap();
        let first = collect(&watch, Duration::from_secs(3), |c| c.tree);
        assert!(first.tree, "directory creation not seen");
        collect(&watch, Duration::from_millis(500), |_| false);
        fs::write(root.join("sub/file"), "x").unwrap();
        let c = collect(&watch, Duration::from_secs(3), |c| c.git);
        assert!(c.git, "file inside new directory not seen");
        // `sub/` is not expanded in the tree, so only status changes.
        assert!(
            !c.tree,
            "a change outside the shown directories must not reload the tree"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn dropping_stops_the_thread() {
        let (root, set) = fixture("drop");
        let watch = start(set);
        // References: the watch, the worker thread, and this test's clone.
        let flags = Arc::clone(&watch.flags);
        assert_eq!(Arc::strong_count(&flags), 3);
        drop(watch);
        // Joining the worker released its reference; only the test's remains.
        assert_eq!(Arc::strong_count(&flags), 1);
        let _ = fs::remove_dir_all(&root);
    }
}
