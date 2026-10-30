//! Git plumbing: repo discovery, `status --porcelain -z` parsing, and the
//! stage / unstage / commit operations, all via the `git` CLI (no libgit2).
//! Parsing is pure and unit-tested; commands run with the repo toplevel as cwd
//! so the repo-relative paths porcelain reports resolve even when the pane's
//! cwd is a subdirectory.

use std::cmp::Reverse;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const IGNORED_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// One file in the staged or unstaged list.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    /// Repo-relative path (the new path, for renames), `/`-separated as git reports it.
    pub path: String,
    /// Rename/copy source, when there is one — unstaging a rename must reset both.
    pub orig: Option<String>,
    /// The VS Code-style status letter to display: M, A, D, R, C, U (untracked),
    /// or `!` for merge conflicts.
    pub letter: char,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub branch: String,
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    /// Commits ahead of / behind the upstream, from the porcelain `##` header.
    pub ahead: usize,
    pub behind: usize,
    /// The branch has an upstream at all (the header carries `...remote`).
    pub has_upstream: bool,
}

#[derive(Clone)]
pub struct Git {
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub current: bool,
    pub remote: bool,
}

/// One commit in a line-range history (`git log -L`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineCommit {
    pub hash: String,
    pub short: String,
    pub author: String,
    /// `YYYY-MM-DD` (`--date=short`).
    pub date: String,
    pub subject: String,
    /// The unified patch restricted to the requested lines (may be empty).
    pub patch: String,
}

/// Why a non-forcing branch delete did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteRefused {
    /// The branch holds commits reachable from nowhere else; deleting it loses
    /// them. The only case worth re-asking about.
    NotMerged,
    /// Anything else — checked out in another worktree, no such branch, git
    /// missing. Carries git's own message.
    Failed(String),
}

/// How a [`Git::merge_branch`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merged (or fast-forwarded, or already up to date); git's summary.
    Done(String),
    /// git stopped with conflicts in these paths. The merge is in progress:
    /// resolve and commit, or [`Git::merge_abort`].
    Conflicts(Vec<String>),
}

/// What one [`Git::stage_under`] call did: how many paths it staged, and how
/// many it deliberately left alone because they live at or inside a NESTED
/// repository. The second number is what lets the UI explain a stage that
/// looks like it did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Staged {
    pub count: usize,
    pub skipped_nested: usize,
}

impl Git {
    /// Locate the repository containing `dir`; Err with git's message when there
    /// is none (or git itself is missing).
    pub fn discover(dir: &Path) -> Result<Git, String> {
        let out = run_in(dir, &["rev-parse", "--show-toplevel"])?;
        let root = out.trim();
        if root.is_empty() {
            return Err("not inside a git repository".to_string());
        }
        Ok(Git {
            root: PathBuf::from(root),
        })
    }

    /// All repositories visible from `dir`, VS Code style: the repository
    /// containing `dir` (if any) plus child repositories up to two directory
    /// levels down (a `.git` dir, or a `.git` FILE — worktrees/submodules).
    /// Deduped by root; the containing repo sorts first, children by path.
    pub fn discover_all(dir: &Path) -> Vec<Git> {
        let mut repos: Vec<Git> = Vec::new();
        let mut push = |git: Git| {
            if !repos.iter().any(|r| r.root == git.root) {
                repos.push(git);
            }
        };
        if let Ok(git) = Git::discover(dir) {
            push(git);
        }
        for child in child_dirs(dir, 2) {
            if child.join(".git").exists()
                && let Ok(git) = Git::discover(&child)
            {
                push(git);
            }
        }
        repos
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Display name for repo headers: the root directory's name.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// `git init` in `dir`: the Explorer offers this as a button when the
    /// folder is not in a repository yet. Cheap and local, but callers may run
    /// it on a worker anyway (a network share can stall).
    pub fn init(dir: &Path) -> Result<String, String> {
        run_in(dir, &["init", "-q"])?;
        Ok("Initialized a git repository".to_string())
    }

    /// Is `dir` (or any ancestor) inside a git repository? A pure filesystem
    /// walk — no git process — so the Explorer can ask on every tick. A
    /// worktree's or submodule's `.git` is a FILE and counts too.
    pub fn is_initialized(dir: &Path) -> bool {
        repo_root(dir).is_some()
    }

    /// VS Code's Sync Changes: pull then push. Returns a short human summary;
    /// the caller runs this on a background thread.
    ///
    /// The pull goes through `PULL_ARGS`, i.e. it MERGES (`--no-rebase`). A
    /// `pull.rebase=true` machine is not worth the dirty-index refusal, and the
    /// panel must not rebase behind the user's back (user-requested).
    pub fn sync(&self) -> Result<String, String> {
        // A branch created here has no upstream, so there is nothing to pull
        // from and `git pull` refuses — publish it instead (see `push`).
        if !self.has_upstream() {
            return self.push();
        }
        run_in(&self.root, PULL_ARGS)?;
        run_in(&self.root, &["push"])?;
        Ok("synced with remote".to_string())
    }

    /// Whether the current branch tracks an upstream (i.e. `@{u}` resolves).
    pub fn has_upstream(&self) -> bool {
        run_in(
            &self.root,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .is_ok()
    }

    /// The remote a branch with no upstream should be published to: whatever
    /// `branch.<name>.remote` names (it is normally unset before the first
    /// push), else `origin`, else the repo's first remote.
    fn publish_remote(&self, branch: &str) -> Result<String, String> {
        if let Ok(name) = run_in(
            &self.root,
            &["config", "--get", &format!("branch.{branch}.remote")],
        ) {
            let name = name.trim();
            if !name.is_empty() && name != "." {
                return Ok(name.to_string());
            }
        }
        let names = lines(run_in(&self.root, &["remote"])?)?;
        if names.iter().any(|n| n == "origin") {
            return Ok("origin".to_string());
        }
        names
            .into_iter()
            .next()
            .ok_or_else(|| "no remote to push to".to_string())
    }

    /// `git push`. A branch with no upstream — created here and never pushed —
    /// is PUBLISHED instead of refused: `--set-upstream` to the remote this
    /// repo already uses, which is VS Code's "Publish Branch". Plain
    /// `git push` answers "fatal: The current branch X has no upstream
    /// branch", and nothing in the panel could get past that (user-reported).
    pub fn push(&self) -> Result<String, String> {
        let (extra, summary) = self.push_plan()?;
        let mut args = vec!["push"];
        args.extend(extra.iter().map(String::as_str));
        run_in(&self.root, &args)?;
        Ok(summary)
    }

    /// What a push owes beyond `push` itself, plus the summary to report: no
    /// extra args for a branch that already tracks an upstream, `--set-upstream
    /// <remote> <branch>` for one that has never been pushed. Shared with the
    /// streaming `git-push` action so the two cannot disagree.
    pub fn push_plan(&self) -> Result<(Vec<String>, String), String> {
        if self.has_upstream() {
            return Ok((Vec::new(), "Pushed.".to_string()));
        }
        let branch = self
            .current_branch()
            .ok_or_else(|| "detached HEAD: nothing to publish".to_string())?;
        let remote = self.publish_remote(&branch)?;
        Ok((
            vec!["--set-upstream".to_string(), remote.clone(), branch.clone()],
            format!("Published {branch} to {remote}."),
        ))
    }

    /// The commit button's "pull first". Returns the footer summary on success.
    /// Fails fast instead of waiting on a credential prompt — this runs on a
    /// worker, so a hang would leave a stray process behind.
    ///
    /// `PULL_ARGS` always merges (`--no-rebase`): a `pull.rebase=true` machine
    /// would otherwise refuse a pull with a dirty index, and the panel must not
    /// rebase behind the user's back. `--no-edit` keeps a merge from opening an
    /// editor in a TUI with no stdin.
    pub fn pull(&self) -> Result<String, String> {
        let out = git_command(&self.root, PULL_ARGS)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("git: {e}"))?;
        let text = command_result(out.status, out.stdout, out.stderr)?;
        Ok(if text.contains("Already up to date") {
            "Already up to date.".to_string()
        } else {
            "Pulled.".to_string()
        })
    }

    /// Refresh the remote-tracking refs so the next [`Git::status`] can report
    /// how far the branch is behind. Best effort, quiet and non-interactive:
    /// this runs on a UI-owned worker, never on the event loop, and a dead
    /// network must not turn the refresh that follows into an error.
    pub fn fetch(&self) -> Result<(), String> {
        let out = git_command(&self.root, &["fetch", "--quiet"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("git: {e}"))?;
        command_result(out.status, out.stdout, out.stderr).map(drop)
    }

    pub fn status(&self) -> Result<Status, String> {
        let out = run_read_in(
            &self.root,
            &[
                "status",
                "--porcelain",
                "-z",
                "--branch",
                "--renames",
                "--untracked-files=all",
            ],
        )?;
        Ok(parse_status(&out))
    }

    /// Stage one entry: `add -A` records modifications, additions, and deletions alike.
    pub fn stage(&self, entry: &FileEntry) -> Result<(), String> {
        let mut args = vec!["add", "-A", "--", entry.path.as_str()];
        if let Some(original) = entry.orig.as_deref() {
            args.push(original);
        }
        run_in(&self.root, &args).map(drop)
    }

    pub fn stage_all(&self) -> Result<(), String> {
        run_in(&self.root, &["add", "-A"]).map(drop)
    }

    /// Whether HEAD resolves to a real commit — false only on an unborn
    /// branch (a repo with no commits yet), where `git reset` has nothing to
    /// reset against.
    fn has_head(&self) -> bool {
        run_in(&self.root, &["rev-parse", "--verify", "HEAD"]).is_ok()
    }

    /// Unstage one entry. `reset` needs a HEAD to reset against; on an unborn
    /// branch (no commits yet) fall back to dropping the path from the index.
    /// The fallback is destructive on a repo WITH a HEAD (`rm --cached` stages
    /// a deletion instead of unstaging), so it only runs for the unborn-branch
    /// case — any other `reset` failure is propagated instead of swallowed.
    pub fn unstage(&self, entry: &FileEntry) -> Result<(), String> {
        let mut args = vec!["reset", "-q", "--", entry.path.as_str()];
        if let Some(orig) = &entry.orig {
            args.push(orig);
        }
        match run_in(&self.root, &args) {
            Ok(_) => Ok(()),
            Err(e) if self.has_head() => Err(e),
            Err(_) => run_in(
                &self.root,
                &["rm", "--cached", "-r", "-q", "--", &entry.path],
            )
            .map(drop),
        }
    }

    pub fn unstage_all(&self) -> Result<(), String> {
        match run_in(&self.root, &["reset", "-q"]) {
            Ok(_) => Ok(()),
            Err(e) if self.has_head() => Err(e),
            Err(_) => run_in(&self.root, &["rm", "--cached", "-r", "-q", "--", "."]).map(drop),
        }
    }

    /// Commit the staged changes; returns git's summary line ("[branch abc1234] …").
    pub fn commit(&self, message: &str) -> Result<String, String> {
        let out = run_in(&self.root, &["commit", "-m", message])?;
        Ok(out.lines().next().unwrap_or("committed").to_string())
    }

    /// Whether anything is in the index. Used before an AI commit so an empty
    /// index reports "nothing to commit" instead of asking the AI.
    pub fn has_staged(&self) -> Result<bool, String> {
        let out = run_in(&self.root, &["diff", "--cached", "--name-only"])?;
        Ok(!out.trim().is_empty())
    }

    /// Throw away a file's working-tree changes: untracked files are deleted,
    /// tracked ones restored from HEAD (the caller confirms first).
    pub fn discard(&self, entry: &FileEntry) -> Result<(), String> {
        if entry.letter == 'U' {
            return run_in(&self.root, &["clean", "-fd", "--", &entry.path]).map(drop);
        }
        if let Some(original) = entry.orig.as_deref() {
            if entry.letter == 'R' {
                run_in(&self.root, &["checkout", "--", original])?;
            }
            run_in(&self.root, &["reset", "-q", "--", &entry.path])?;
            return run_in(&self.root, &["clean", "-fd", "--", &entry.path]).map(drop);
        }
        run_in(&self.root, &["checkout", "--", &entry.path]).map(drop)
    }

    /// The diff a commit-message suggestion should describe: the staged diff
    /// when something is staged (that is what would be committed), else the
    /// working-tree diff. Untracked files only appear as names, so they ride
    /// along in the returned path list either way.
    pub fn diff_for_message(&self) -> Result<(String, Vec<String>), String> {
        let staged = run_in(&self.root, &["diff", "--cached", "--stat", "--patch"])?;
        let (diff, names_args): (String, &[&str]) = if staged.trim().is_empty() {
            let unstaged = run_in(&self.root, &["diff", "--stat", "--patch"])?;
            (unstaged, &["diff", "--name-only"])
        } else {
            (staged, &["diff", "--cached", "--name-only"])
        };
        let mut files: Vec<String> = run_in(&self.root, names_args)?
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect();
        if files.is_empty() {
            // Nothing tracked changed: describe the untracked files instead.
            files = run_in(&self.root, &["ls-files", "--others", "--exclude-standard"])?
                .lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect();
        }
        Ok((diff, files))
    }

    /// Repo-relative roots of ignored paths, for the Explorer's `Ignored`
    /// decoration (issue #19). This deliberately uses a separate, bounded
    /// `ls-files` query: `--directory` keeps wholly ignored directories
    /// collapsed, and a shared non-blocking lock prevents preview sidebars
    /// from multiplying an expensive scan of the same repository.
    pub fn ignored(&self) -> Result<Vec<String>, String> {
        let _scan = IgnoredScanLock::acquire(&self.root)?;
        let out = run_read_in_with_timeout(
            &self.root,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ],
            IGNORED_SCAN_TIMEOUT,
        )?;
        Ok(parse_ignored(&out))
    }

    /// The repository that OWNS `path`: the NEAREST enclosing repo, found by
    /// git's own upward walk. A path inside a nested repository therefore
    /// belongs to the nested repo, never to the parent — the boundary rule
    /// issue #20 asks for.
    pub fn owner_of(path: &Path) -> Result<Git, String> {
        // Directory symlinks expand like folders in the Explorer, but the Git
        // object is the link at its parent path—not the target directory.
        let is_symlink =
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        let dir = if path.is_dir() && !is_symlink {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        Git::discover(dir)
    }

    /// Stage everything under `target` that belongs to THIS repository —
    /// additions, modifications and deletions alike.
    ///
    /// Rather than handing git the directory (`git add -A -- dir`), the paths
    /// come from this repo's own `status`, filtered to the target subtree and
    /// then stripped of anything at or inside a NESTED repository root. A bare
    /// `git add` on a directory holding an unregistered inner repo records it
    /// as a gitlink ("adding embedded git repository"); enumerating instead
    /// makes the boundary explicit, and a nested repo's changes can only ever
    /// be staged by selecting something inside it (which `owner_of` then
    /// routes to the nested repo).
    pub fn stage_under(&self, target: &Path) -> Result<Staged, String> {
        let prefix = self.rel_of(target)?;
        let status = self.status()?;
        let candidates = paths_under(&status, prefix.as_deref());
        let before = candidates.len();
        let nested = self.nested_roots_for(&candidates);
        let paths = drop_nested(candidates, &nested);
        let skipped_nested = before - paths.len();
        if paths.is_empty() {
            return Ok(Staged {
                count: 0,
                skipped_nested,
            });
        }
        // Windows caps a command line at ~32k: stage in batches so a huge
        // untracked tree does not blow past it.
        for chunk in paths.chunks(64) {
            let mut args = vec!["add", "-A", "--"];
            args.extend(chunk.iter().map(String::as_str));
            run_in(&self.root, &args)?;
        }
        Ok(Staged {
            count: paths.len(),
            skipped_nested,
        })
    }

    /// Unstage everything staged at or under the repo-relative `prefix`
    /// (`/`-separated) with a handful of batched `reset` calls instead of one
    /// process per file. A rename whose source or destination lies under the
    /// prefix is unstaged as a pair, mirroring [`Self::stage_under`].
    pub fn unstage_under(&self, prefix: &str) -> Result<usize, String> {
        let status = self.status()?;
        let mut paths: Vec<String> = Vec::new();
        for entry in &status.staged {
            let touches = under(&entry.path, Some(prefix))
                || entry
                    .orig
                    .as_deref()
                    .is_some_and(|original| under(original, Some(prefix)));
            if !touches {
                continue;
            }
            for path in std::iter::once(&entry.path).chain(entry.orig.iter()) {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
        }
        if paths.is_empty() {
            return Ok(0);
        }
        // Same unborn-branch rule as `unstage`: without a HEAD there is
        // nothing to reset against, and `rm --cached` is only safe then.
        let has_head = self.has_head();
        for chunk in paths.chunks(64) {
            let mut args = if has_head {
                vec!["reset", "-q", "--"]
            } else {
                vec!["rm", "--cached", "-r", "-q", "--"]
            };
            args.extend(chunk.iter().map(String::as_str));
            run_in(&self.root, &args)?;
        }
        Ok(paths.len())
    }

    /// `target` as a `/`-separated repo-relative path; `None` when it IS the
    /// repo root (no prefix = the whole repo). Refuse an outside or
    /// differently-normalized path rather than accidentally staging the whole
    /// repository.
    fn rel_of(&self, target: &Path) -> Result<Option<String>, String> {
        let rel = target.strip_prefix(&self.root).map_err(|_| {
            format!(
                "{} is outside repository {}",
                target.display(),
                self.root.display()
            )
        })?;
        let joined = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        Ok((!joined.is_empty()).then_some(joined))
    }

    /// The nested-repository roots that lie on the way to any of `paths`:
    /// every ancestor prefix (and the path itself) that carries its own
    /// `.git`, excluding this repo's root.
    fn nested_roots_for(&self, paths: &[String]) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut roots: Vec<String> = Vec::new();
        for path in paths {
            let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
            for end in 1..=parts.len() {
                let prefix = parts[..end].join("/");
                if !seen.insert(prefix.clone()) {
                    continue;
                }
                let mut abs = self.root.clone();
                for part in &parts[..end] {
                    abs.push(part);
                }
                if abs.join(".git").exists() {
                    roots.push(prefix);
                }
            }
        }
        roots
    }

    // ---- Drawer queries (display-only lists, VS Code Git-Graph style) ----

    pub fn graph(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(
            &self.root,
            &["log", "--graph", "--oneline", "--decorate=short", &n],
        )?)
    }

    pub fn commits(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(
            &self.root,
            &["log", "--oneline", "--decorate=short", "--date=short", &n],
        )?)
    }

    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        lines(run_in(
            &self.root,
            &["log", "--oneline", "--follow", &n, "--", path],
        )?)
    }

    /// Commits that touched lines `start..=end` (1-based, inclusive) of
    /// `rel_path`, newest first, each with its patch restricted to those
    /// lines (`git log -L`, which also follows the lines through renames).
    pub fn line_history(
        &self,
        rel_path: &str,
        start: usize,
        end: usize,
    ) -> Result<Vec<LineCommit>, String> {
        let range = format!("-L{start},{end}:{rel_path}");
        let out = run_in(
            &self.root,
            &[
                "log",
                &range,
                "-n",
                "200",
                "--date=short",
                "--format=%x1e%H%x1f%h%x1f%an%x1f%ad%x1f%s",
            ],
        )?;
        Ok(parse_line_history(&out))
    }

    /// Local + remote branches, the current one first and starred.
    /// Whether `name` is a LOCAL branch.
    ///
    /// `git branch -a` (what the Branches drawer lists) also yields
    /// remote-tracking refs such as `origin/main`, and `git branch -d origin/main`
    /// fails with "branch not found" — the drawer has to tell them apart before
    /// offering a delete or a rename.
    pub fn is_local_branch(&self, name: &str) -> bool {
        run_in(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .is_ok()
    }

    pub fn branches(&self) -> Result<Vec<String>, String> {
        let out = run_in(
            &self.root,
            &[
                "branch",
                "-a",
                "--sort=-committerdate",
                "--format=%(HEAD)%00%(refname:short)%00%(committerdate:unix)",
            ],
        )?;
        // Rebuild the `"* name"` / `"  name"` lines the drawer parses, in
        // creation order (see `ref_created_at` and `sort_stamp`).
        let mut entries: Vec<(String, String, i64)> = out
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\0');
                let head = fields.next()?.trim().to_string();
                let name = fields.next()?.trim().to_string();
                let commit = fields
                    .next()
                    .unwrap_or("")
                    .trim()
                    .parse()
                    .unwrap_or(i64::MIN);
                Some((head, name, commit))
            })
            .filter(|(_, name, _)| !name.is_empty())
            .collect();
        let created = ref_created_at(&self.root);
        entries.sort_by_key(|(_, name, commit)| Reverse(sort_stamp(&created, name, *commit)));
        Ok(entries
            .into_iter()
            .map(|(head, name, _)| format!("{} {name}", if head == "*" { "*" } else { " " }))
            .collect())
    }

    /// Branches suitable for an interactive checkout picker. Symbolic remote
    /// HEAD aliases are omitted because selecting one would create a detached
    /// or misleading checkout target.
    pub fn branch_choices(&self) -> Result<Vec<Branch>, String> {
        let out = run_in(
            &self.root,
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(HEAD)%00%(refname:short)%00%(refname)%00%(symref)%00%(committerdate:unix)",
                "refs/heads",
                "refs/remotes",
            ],
        )?;
        // Newest-CREATED first: git's committerdate sort puts a branch made from
        // an old commit near the bottom, which is not the order anybody picks
        // from (user-requested). `parse_branch_choices` then keeps locals before
        // remotes, stably — it does NOT hoist the current branch, because that
        // pushed the branch you just created off row 0.
        let created = ref_created_at(&self.root);
        let mut rows: Vec<&str> = out.lines().collect();
        rows.sort_by_key(|line| {
            let mut fields = line.split('\0');
            let name = fields.nth(1).unwrap_or("").trim();
            let commit = fields.next().and_then(|c| c.trim().parse::<i64>().ok());
            Reverse(sort_stamp(&created, name, commit.unwrap_or(i64::MIN)))
        });
        Ok(parse_branch_choices(&rows.join("\n")))
    }

    /// Delete a branch without forcing. `NotMerged` is reported separately so
    /// the caller can ask a second, informed question instead of destroying
    /// commits behind a prompt that could not know they existed.
    pub fn delete_branch(&self, name: &str) -> Result<(), DeleteRefused> {
        let branch_ref = format!("refs/heads/{name}");
        let worktrees = run_in(&self.root, &["worktree", "list", "--porcelain"])
            .map_err(DeleteRefused::Failed)?;
        if worktrees
            .lines()
            .any(|line| line.strip_prefix("branch ") == Some(&branch_ref))
        {
            return Err(DeleteRefused::Failed(format!(
                "branch '{name}' is checked out in a worktree"
            )));
        }
        match run_in(&self.root, &["branch", "-d", "--", name]) {
            Ok(_) => Ok(()),
            // git's refusal text is translated under a non-English locale, and
            // nothing here pins one, so classify the failure structurally
            // rather than by matching the message.
            Err(message) => match self.branch_is_merged(name) {
                Some(false) => Err(DeleteRefused::NotMerged),
                _ => Err(DeleteRefused::Failed(message)),
            },
        }
    }

    /// `branch -D`: only for a caller that has confirmed the loss explicitly.
    pub fn force_delete_branch(&self, name: &str) -> Result<(), String> {
        run_in(&self.root, &["branch", "-D", "--", name]).map(drop)
    }

    /// Whether `name`'s tip is contained in its upstream (or HEAD). `None` when the ref
    /// does not resolve, which keeps a missing branch — or any other failure —
    /// from being mistaken for unmerged work.
    fn branch_is_merged(&self, name: &str) -> Option<bool> {
        let branch_ref = format!("refs/heads/{name}");
        run_in(
            &self.root,
            &["rev-parse", "--verify", "--quiet", &branch_ref],
        )
        .ok()?;
        let upstream = run_in(
            &self.root,
            &["for-each-ref", "--format=%(upstream)", &branch_ref],
        )
        .ok()?;
        let destination = if upstream.trim().is_empty() {
            "HEAD"
        } else {
            upstream.trim()
        };
        let status = git_command(
            &self.root,
            &["merge-base", "--is-ancestor", &branch_ref, destination],
        )
        .output()
        .ok()?
        .status;
        match status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        }
    }

    /// Create a branch at HEAD and switch to it. `switch -c` refuses to
    /// clobber an existing branch, so an in-use name is reported rather than
    /// silently reset, and a dirty worktree carries over exactly as it does
    /// for [`Self::checkout_branch`].
    pub fn create_branch(&self, name: &str) -> Result<(), String> {
        let name = self.valid_new_branch_name(name)?;
        run_in(&self.root, &["switch", "-c", name]).map(drop)
    }

    /// `name` trimmed, if git accepts it as a new branch name.
    fn valid_new_branch_name<'a>(&self, name: &'a str) -> Result<&'a str, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("branch name is empty".to_string());
        }
        // Let git judge the name: its rules cover far more than a hand-rolled
        // check (trailing `.lock`, `@{`, control characters, leading dashes).
        let validated = run_in(&self.root, &["check-ref-format", "--branch", name])
            .map_err(|_| format!("`{name}` is not a valid branch name"))?;
        if validated.trim() != name {
            return Err("enter a literal new branch name".into());
        }
        // `check-ref-format --branch` accepts `refs/heads/x`, which would
        // create `refs/heads/refs/heads/x` — ambiguous everywhere after.
        if name.starts_with("refs/") {
            return Err("enter the branch name without a refs/ prefix".into());
        }
        Ok(name)
    }

    /// Cheap identity of the checked-out branch: the content of the git dir's
    /// `HEAD` (`ref: refs/heads/<branch>`, or a detached hash). A checkout,
    /// branch creation or rename rewrites it, so comparing two readings shows
    /// that the branch moved WITHOUT starting a git process — which is what lets
    /// the views react within a few hundred ms to a popup (another process) or a
    /// `git switch` typed in a terminal.
    pub fn head_signature(&self) -> Option<String> {
        let dir = self.git_dir()?;
        let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
        let head = head.trim().to_string();
        let Some(refname) = head.strip_prefix("ref: ") else {
            // Detached: HEAD itself holds the commit.
            return Some(head);
        };
        let tip = std::fs::read_to_string(dir.join(refname))
            .ok()
            .map(|text| text.trim().to_string())
            .or_else(|| packed_ref_tip(&dir.join("packed-refs"), refname));
        // A merge that stopped on conflicts does NOT move the branch, so the
        // ref alone would make it (and its abort) invisible to the views.
        let merge = std::fs::read_to_string(dir.join("MERGE_HEAD"))
            .ok()
            .map(|text| text.trim().to_string());
        Some(format!(
            "{head} {}{}",
            tip.unwrap_or_default(),
            merge
                .map(|head| format!(" merge:{head}"))
                .unwrap_or_default()
        ))
    }

    /// The repository's git directory. In a worktree or a submodule `.git` is a
    /// FILE holding `gitdir: <path>`, not a directory.
    pub fn git_dir(&self) -> Option<PathBuf> {
        git_dir_at(&self.root)
    }

    /// The checked-out branch; `None` on a detached HEAD.
    pub fn current_branch(&self) -> Option<String> {
        run_in(&self.root, &["symbolic-ref", "--short", "-q", "HEAD"])
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    /// `branch -m old new`. Refuses to overwrite an existing branch (no -M);
    /// git moves the upstream and reflog along with it.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<(), String> {
        let new = self.valid_new_branch_name(new)?;
        if new == old {
            return Ok(());
        }
        run_in(&self.root, &["branch", "-m", old, new]).map(drop)
    }

    /// Full hash of `commit` (any rev that names a commit).
    fn resolve_commit(&self, commit: &str) -> Result<String, String> {
        run_in(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{commit}^{{commit}}"),
            ],
        )
        .map(|hash| hash.trim().to_string())
    }

    /// The raw message of `commit`: everything after the header/message separator.
    pub fn commit_message(&self, commit: &str) -> Result<String, String> {
        let full = self.resolve_commit(commit)?;
        let raw = run_in(&self.root, &["cat-file", "commit", &full])?;
        Ok(raw
            .split_once("\n\n")
            .map(|(_, message)| message.to_string())
            .unwrap_or_default())
    }

    /// Whether `commit` is reachable from HEAD. Any failure counts as no.
    pub fn is_ancestor_of_head(&self, commit: &str) -> bool {
        let Ok(full) = self.resolve_commit(commit) else {
            return false;
        };
        run_in(&self.root, &["merge-base", "--is-ancestor", &full, "HEAD"]).is_ok()
    }

    /// A merge, cherry-pick, revert or rebase is paused in this repository. Such
    /// a state must be finished before history is rewritten.
    pub fn operation_in_progress(&self) -> bool {
        let Some(dir) = self.git_dir() else {
            return false;
        };
        [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
        ]
        .iter()
        .any(|name| dir.join(name).exists())
    }

    /// Whether `commit` names the current HEAD commit.
    pub fn is_head(&self, commit: &str) -> bool {
        let Ok(full) = self.resolve_commit(commit) else {
            return false;
        };
        run_in(&self.root, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .map(|head| head.trim() == full)
            .unwrap_or(false)
    }

    /// HEAD's short hash and subject; `None` when there are no commits yet.
    pub fn head_summary(&self) -> Option<(String, String)> {
        let line = run_in(&self.root, &["log", "-1", "--format=%h%x00%s", "HEAD"]).ok()?;
        let (hash, subject) = line.trim_end_matches('\n').split_once('\0')?;
        Some((hash.to_string(), subject.to_string()))
    }

    /// Undo the last commit with `reset --soft HEAD~1`: the commit leaves the
    /// branch and its changes stay staged. This is the one implementation every
    /// entry point (Branches drawer, commit row menu, `U`, the global action)
    /// shares. Refuses during an operation, without commits, and on a root commit.
    pub fn undo_last_commit(&self) -> Result<Undone, String> {
        if self.operation_in_progress() {
            return Err("finish the merge/rebase in progress first".to_string());
        }
        if run_in(&self.root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
            return Err("nothing to undo: no commits yet".to_string());
        }
        if run_in(&self.root, &["rev-parse", "--verify", "--quiet", "HEAD~1"]).is_err() {
            return Err("the first commit cannot be undone".to_string());
        }
        let line = run_in(&self.root, &["log", "-1", "--format=%H%x00%s", "HEAD"])?;
        let (full, subject) = line
            .trim_end_matches('\n')
            .split_once('\0')
            .ok_or_else(|| "could not read HEAD".to_string())?;
        let (full, subject) = (full.to_string(), subject.to_string());
        let pushed = run_in(&self.root, &["branch", "-r", "--contains", &full])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        run_in(&self.root, &["reset", "--soft", "HEAD~1"])?;
        Ok(Undone {
            hash: full.chars().take(7).collect(),
            subject,
            pushed,
        })
    }

    /// Replace `commit`'s message and rewrite its descendants up to HEAD with
    /// plumbing (`commit-tree` + one guarded `update-ref`). Trees, authors and
    /// the index / work tree are untouched, so nothing can conflict.
    pub fn reword_commit(&self, commit: &str, message: &str) -> Result<Reworded, String> {
        if message.trim().is_empty() {
            return Err("Commit message is empty.".to_string());
        }
        if self.operation_in_progress() {
            return Err("finish the merge/rebase in progress first".to_string());
        }
        let full = self.resolve_commit(commit)?;
        if !self.is_ancestor_of_head(&full) {
            return Err("that commit is not on the current branch".to_string());
        }
        let old_head = run_in(&self.root, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        let old_message = self.commit_message(&full)?;
        let new_message = format!("{}\n", message.trim_end());
        let short = |hash: &str| hash.get(..7).unwrap_or(hash).to_string();
        let pushed = run_in(&self.root, &["branch", "-r", "--contains", &full])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false);
        if new_message == old_message {
            return Ok(Reworded {
                old: short(&full),
                new: short(&full),
                pushed,
            });
        }

        let listing = run_in(
            &self.root,
            &[
                "rev-list",
                "--topo-order",
                "--reverse",
                "--ancestry-path",
                &format!("{full}..HEAD"),
            ],
        )?;
        let mut map = std::collections::HashMap::new();
        let new_full = self.write_rewritten_commit(&full, &new_message, &map)?;
        map.insert(full.clone(), new_full.clone());
        for descendant in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let raw = self.commit_message(descendant)?;
            let rewritten = self.write_rewritten_commit(descendant, &raw, &map)?;
            map.insert(descendant.to_string(), rewritten);
        }

        let new_head = map
            .get(&old_head)
            .cloned()
            .unwrap_or_else(|| old_head.clone());
        let subject = message.trim().lines().next().unwrap_or("");
        run_in(
            &self.root,
            &[
                "update-ref",
                "-m",
                &format!("reword: {subject}"),
                "HEAD",
                &new_head,
                &old_head,
            ],
        )?;
        Ok(Reworded {
            old: short(&full),
            new: short(&new_full),
            pushed,
        })
    }

    /// A copy of `commit` with `message`, parents remapped through `map` and the
    /// author (name, email, raw date) preserved. Returns the new full hash.
    fn write_rewritten_commit(
        &self,
        commit: &str,
        message: &str,
        map: &std::collections::HashMap<String, String>,
    ) -> Result<String, String> {
        let tree = run_in(&self.root, &["rev-parse", &format!("{commit}^{{tree}}")])?
            .trim()
            .to_string();
        let parents = run_in(&self.root, &["log", "-1", "--format=%P", commit])?;
        let mut args: Vec<String> = vec!["commit-tree".to_string(), tree];
        for parent in parents.split_whitespace() {
            args.push("-p".to_string());
            args.push(
                map.get(parent)
                    .cloned()
                    .unwrap_or_else(|| parent.to_string()),
            );
        }
        args.extend(["-F".to_string(), "-".to_string()]);
        let author = run_in(
            &self.root,
            &[
                "log",
                "-1",
                "--format=%an%x00%ae%x00%ad",
                "--date=raw",
                commit,
            ],
        )?;
        let fields: Vec<&str> = author.trim_end_matches('\n').split('\0').collect();
        let [name, email, date] = fields[..] else {
            return Err(format!("could not read the author of {commit}"));
        };

        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut child = git_command(&self.root, &refs)
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_AUTHOR_DATE", date)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("git: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(message.as_bytes())
                .map_err(|e| format!("git stdin: {e}"))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("git: {e}"))?;
        command_result(out.status, out.stdout, out.stderr).map(|hash| hash.trim().to_string())
    }

    /// Merge `name` into the current branch, committing with git's default
    /// message when it is not a fast-forward. A dirty worktree that the merge
    /// would touch is refused by git and reported as Err, with nothing done.
    pub fn merge_branch(&self, name: &str) -> Result<MergeOutcome, String> {
        let merge = git_command(
            &self.root,
            &["merge", "--no-edit", "--end-of-options", name],
        )
        .output()
        .map_err(|e| format!("git: {e}"))?;
        if merge.status.success() {
            let summary = String::from_utf8_lossy(&merge.stdout).trim().to_string();
            return Ok(MergeOutcome::Done(summary));
        }
        if self.merge_in_progress() {
            let conflicted =
                run_in(&self.root, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
            return Ok(MergeOutcome::Conflicts(
                conflicted.lines().map(str::to_string).collect(),
            ));
        }
        command_result(merge.status, merge.stdout, merge.stderr)
            .map(|_| MergeOutcome::Done(String::new()))
    }

    pub fn merge_in_progress(&self) -> bool {
        run_in(&self.root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_ok()
    }

    pub fn merge_abort(&self) -> Result<(), String> {
        run_in(&self.root, &["merge", "--abort"]).map(drop)
    }

    /// Repo-relative paths git still lists as unmerged (`--diff-filter=U`),
    /// sorted and deduped. `-z` keeps paths with spaces or odd bytes intact.
    pub fn conflicted_paths(&self) -> Result<Vec<String>, String> {
        let out = run_in(
            &self.root,
            &["diff", "--name-only", "--diff-filter=U", "-z", "--"],
        )?;
        let mut paths: Vec<String> = out
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Which sides of an unmerged path exist, read from its index stages. The
    /// merge editor uses this to say what happened instead of showing an
    /// empty side. Any git failure reads as [`ConflictKind::Other`].
    pub fn conflict_kind(&self, rel: &str) -> ConflictKind {
        let Ok(out) = run_in(&self.root, &["ls-files", "-u", "-z", "--", rel]) else {
            return ConflictKind::Other;
        };
        let mut stages = [false; 4];
        for entry in out.split('\0').filter(|entry| !entry.is_empty()) {
            // `<mode> <object> <stage>\t<path>`.
            let stage = entry
                .split('\t')
                .next()
                .and_then(|meta| meta.split(' ').nth(2))
                .and_then(|stage| stage.parse::<usize>().ok());
            if let Some(slot) = stage.and_then(|stage| stages.get_mut(stage)) {
                *slot = true;
            }
        }
        match (stages[1], stages[2], stages[3]) {
            (true, true, true) => ConflictKind::BothModified,
            (false, true, true) => ConflictKind::BothAdded,
            (true, false, true) => ConflictKind::DeletedByUs,
            (true, true, false) => ConflictKind::DeletedByThem,
            _ => ConflictKind::Other,
        }
    }

    /// Stage a resolved path (`git add --`). Git then drops its unmerged
    /// entries for it, which is how a resolution is recorded.
    pub fn mark_resolved(&self, rel: &str) -> Result<(), String> {
        run_in(&self.root, &["add", "--", rel]).map(drop)
    }

    /// Resolve a modify/delete conflict by removing the file (`git rm --`).
    pub fn remove_conflicted(&self, rel: &str) -> Result<(), String> {
        run_in(&self.root, &["rm", "--quiet", "--", rel]).map(drop)
    }

    /// Checkout one picker entry. Remote refs become ordinary local tracking
    /// branches, matching editor branch pickers rather than leaving a detached
    /// HEAD. Dirty-worktree failures are returned unchanged; nothing is forced.
    pub fn checkout_branch(&self, branch: &Branch) -> Result<(), String> {
        if branch.current {
            return Ok(());
        }
        let args = if branch.remote {
            vec![
                "checkout",
                "--track",
                "--end-of-options",
                branch.name.as_str(),
            ]
        } else {
            vec!["checkout", "--end-of-options", branch.name.as_str()]
        };
        run_in(&self.root, &args).map(drop)
    }

    /// Checkout that survives a dirty worktree the way the pickers offer it:
    /// stash the local modifications (untracked files included), switch, then
    /// put them back with `stash pop`. Nothing is discarded — when the pop
    /// conflicts the stash entry stays on the list. Returns the line to show.
    pub fn checkout_branch_stashing(&self, branch: &Branch) -> Result<String, String> {
        let stashed = !run_in(
            &self.root,
            &[
                "stash",
                "push",
                "--include-untracked",
                "--message",
                "herdr-sidebar: local changes across a branch switch",
            ],
        )?
        .contains("No local changes to save");
        let args = if branch.remote {
            vec![
                "checkout",
                "--track",
                "--end-of-options",
                branch.name.as_str(),
            ]
        } else {
            vec!["checkout", "--end-of-options", branch.name.as_str()]
        };
        if let Err(error) = run_in(&self.root, &args) {
            // The switch never happened: put the work back rather than leave it
            // parked in a stash the user did not ask for.
            if stashed {
                let _ = run_in(&self.root, &["stash", "pop"]);
            }
            return Err(error);
        }
        if !stashed {
            return Ok(format!("switched to {}", branch.name));
        }
        Ok(match run_in(&self.root, &["stash", "pop"]) {
            Ok(_) => format!("switched to {} · local changes restored", branch.name),
            Err(_) => format!(
                "switched to {} · local changes kept in the stash (pop conflicted)",
                branch.name
            ),
        })
    }

    pub fn remotes(&self) -> Result<Vec<String>, String> {
        let out = run_in(&self.root, &["remote", "-v"])?;
        // `remote -v` lists fetch and push separately; one line per remote reads better.
        let mut seen = Vec::new();
        for line in out.lines() {
            if let Some(rest) = line.strip_suffix(" (fetch)") {
                seen.push(rest.replace('\t', "  "));
            }
        }
        Ok(seen)
    }

    pub fn stashes(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["stash", "list"])?)
    }

    pub fn tags(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["tag", "--sort=-creatordate"])?)
    }

    /// One line per worktree (`git worktree list`): path, short head,
    /// [branch] — the primary checkout first.
    pub fn worktrees(&self) -> Result<Vec<String>, String> {
        lines(run_in(&self.root, &["worktree", "list"])?)
    }

    /// Run an arbitrary git command in this repo — the escape hatch the
    /// drawer context menus use (checkout / merge / cherry-pick / …).
    pub fn raw(&self, args: &[&str]) -> Result<String, String> {
        run_in(&self.root, args)
    }
}

/// Parse `git log -L … --format=%x1e%H%x1f%h%x1f%an%x1f%ad%x1f%s` output:
/// each record starts with `\x1e`, its header is one line of `\x1f`-separated
/// fields, and the patch follows after a blank line.
pub fn parse_line_history(out: &str) -> Vec<LineCommit> {
    out.split('\x1e')
        .skip(1)
        .filter_map(|record| {
            let (header, patch) = record.split_once('\n').unwrap_or((record, ""));
            let mut fields = header.split('\x1f');
            let hash = fields.next()?.trim().to_string();
            if hash.is_empty() {
                return None;
            }
            Some(LineCommit {
                hash,
                short: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
                subject: fields.next()?.to_string(),
                patch: patch.trim_start_matches('\n').trim_end().to_string(),
            })
        })
        .collect()
}

fn lines(out: String) -> Result<Vec<String>, String> {
    Ok(out
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect())
}

/// What [`Git::reword_commit`] did; `old`/`new` are 7-character hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reworded {
    pub old: String,
    pub new: String,
    /// The original commit is on a remote-tracking branch, so a remote needs a
    /// force-push to match the rewritten history.
    pub pushed: bool,
}

/// How a path is unmerged, from the index stages git keeps for it. The merge
/// editor shows a different message for a deletion than for a text conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Both sides changed the file (stages 1, 2 and 3).
    BothModified,
    /// Both sides added the file with no common ancestor (stages 2 and 3).
    BothAdded,
    /// Ours deleted it while theirs changed it (stages 1 and 3).
    DeletedByUs,
    /// Theirs deleted it while ours changed it (stages 1 and 2).
    DeletedByThem,
    Other,
}

/// What [`Git::undo_last_commit`] removed; `hash` is 7 characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Undone {
    pub hash: String,
    pub subject: String,
    /// The commit is on a remote-tracking branch, so the remote still has it.
    pub pushed: bool,
}

fn run_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command(dir, args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    command_result(out.status, out.stdout, out.stderr)
}

/// The repository root above `dir`: the nearest ancestor holding a `.git`
/// entry (a directory, or the FILE a worktree/submodule uses).
fn repo_root(dir: &Path) -> Option<PathBuf> {
    let mut path = dir;
    loop {
        if path.join(".git").exists() {
            return Some(path.to_path_buf());
        }
        path = path.parent()?;
    }
}

/// The git directory for `root`, resolving the `gitdir:` file worktrees and
/// submodules use. Shared by [`Git::git_dir`] and [`branch_label_at`].
fn git_dir_at(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let path = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

/// The checked-out branch read straight from the git dir's `HEAD` — no git
/// process, so a 5 s heartbeat can afford it. A branch keeps its slashes; a
/// detached HEAD yields the short commit hash (7 chars). `None` outside a
/// repository or when `HEAD` is unreadable.
pub fn branch_label_at(dir: &Path) -> Option<String> {
    let root = repo_root(dir)?;
    let head = std::fs::read_to_string(git_dir_at(&root)?.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(refname) = head.strip_prefix("ref: ") {
        let name = refname.strip_prefix("refs/heads/").unwrap_or(refname);
        return (!name.is_empty()).then(|| name.to_string());
    }
    let short: String = head.chars().take(7).collect();
    (!short.is_empty()).then_some(short)
}

/// The tip named by `refname` in a `packed-refs` file. Loose refs win over
/// packed ones, so this is only the fallback; `^<commit>` peel lines and the
/// `# pack-refs with: …` header never match, because both lack a `<hash> <ref>`
/// pair.
fn packed_ref_tip(path: &Path, refname: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let (hash, name) = line.split_once(' ')?;
        (name.trim() == refname).then(|| hash.trim().to_string())
    })
}

fn parse_branch_choices(out: &str) -> Vec<Branch> {
    let branches: Vec<Branch> = out
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let current = fields.next()?.trim() == "*";
            let name = fields.next()?.trim();
            let full = fields.next()?.trim();
            let symref = fields.next().unwrap_or("").trim();
            if name.is_empty() || !symref.is_empty() {
                return None;
            }
            Some(Branch {
                name: name.to_string(),
                current,
                remote: full.starts_with("refs/remotes/"),
            })
        })
        .collect();
    // The current branch is NOT hoisted to row 0. It used to be, and that is
    // what made "newest first" look broken: the branch you had just created sat
    // second whenever an older branch happened to be checked out (user-reported,
    // reproduced in a repo where `main` was older than `feat/themes-i18n`). Row 0
    // is the newest local branch, which is the one people pick; switching to the
    // current branch is a no-op anyway and Merge filters it out.
    //
    // Locals before remotes: `--sort=-committerdate` interleaves them, and a
    // remote is almost never the intended merge/checkout target. The partition
    // is stable, so both groups keep the creation order.
    let mut locals = Vec::with_capacity(branches.len());
    let mut remotes = Vec::new();
    for branch in branches {
        if branch.remote {
            remotes.push(branch);
        } else {
            locals.push(branch);
        }
    }
    locals.extend(remotes);
    locals
}

/// Background status reads must never refresh the index while another pane or
/// tool is trying to write it. `GIT_OPTIONAL_LOCKS=0` asks Git to skip those
/// optional index writes without changing the result of these read-only calls.
fn run_read_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command(dir, args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("git: {e}"))?;
    command_result(out.status, out.stdout, out.stderr)
}

fn run_read_in_with_timeout(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let mut command = git_command(dir, args);
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepare_timed_command(&mut command);
    let mut child = command.spawn().map_err(|e| format!("git: {e}"))?;
    let child_job = match ChildJob::attach(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let stdout = child.stdout.take().ok_or("git stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("git stderr unavailable")?;
    let stdout_reader = std::thread::spawn(move || read_all(stdout));
    let stderr_reader = std::thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("git: {e}"))? {
            break status;
        }
        if Instant::now() >= deadline {
            let tree_terminated = terminate_child(&mut child, child_job.as_ref());
            let _ = child.wait();
            if tree_terminated {
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
            } else {
                drop(stdout_reader);
                drop(stderr_reader);
            }
            return Err(format!(
                "ignored-file scan timed out after {}s",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "git stdout reader panicked".to_string())??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "git stderr reader panicked".to_string())??;
    command_result(status, stdout, stderr)
}

#[cfg(windows)]
fn prepare_timed_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
}

#[cfg(not(windows))]
fn prepare_timed_command(_command: &mut Command) {}

#[cfg(windows)]
struct ChildJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
fn resume_child(process_id: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut found = unsafe { Thread32First(snapshot, &mut entry) != 0 };
    while found {
        if entry.th32OwnerProcessID == process_id {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                let resumed = unsafe { ResumeThread(thread) != u32::MAX };
                unsafe { CloseHandle(thread) };
                unsafe { CloseHandle(snapshot) };
                return resumed;
            }
        }
        found = unsafe { Thread32Next(snapshot, &mut entry) != 0 };
    }
    unsafe { CloseHandle(snapshot) };
    false
}

#[cfg(windows)]
impl ChildJob {
    fn attach(child: &Child) -> Result<Option<Self>, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err("git: could not create timeout job".to_string());
        }
        let assigned = unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) };
        if assigned == 0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err("git: could not assign timeout job".to_string());
        }
        let job = Self { handle };
        if !resume_child(child.id()) {
            job.terminate();
            return Err("git: could not resume timeout child".to_string());
        }
        Ok(Some(job))
    }

    fn terminate(&self) -> bool {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1) != 0 }
    }
}

#[cfg(windows)]
impl Drop for ChildJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
struct ChildJob;

#[cfg(not(windows))]
impl ChildJob {
    fn attach(_child: &Child) -> Result<Option<Self>, String> {
        Ok(None)
    }

    fn terminate(&self) -> bool {
        false
    }
}

fn terminate_child(child: &mut Child, job: Option<&ChildJob>) -> bool {
    if job.is_some_and(ChildJob::terminate) {
        true
    } else {
        let _ = child.kill();
        false
    }
}

/// What a branch list sorts on, newest first: the ref's creation stamp when git
/// has a reflog for it, else the tip commit's committer date.
///
/// The fallback matters: a local branch with no reflog (cloned from a remote,
/// reflog expired, made by an older tool) used to sort LAST — below branches that
/// were demonstrably created before it. A commit date is not a creation date, but
/// it keeps such a branch in the right neighbourhood instead of at the bottom.
fn sort_stamp(created: &std::collections::HashMap<String, i64>, name: &str, commit: i64) -> i64 {
    created.get(name).copied().unwrap_or(commit)
}

/// Creation time (unix seconds) of every ref git keeps a reflog for.
///
/// Git records no branch creation date, so the OLDEST reflog entry is the first
/// time it wrote anything for that ref — that is the closest thing to "when this
/// branch was created", and it is what orders the branch lists. Refs without a
/// reflog (`core.logAllRefUpdates=false`, a bare-ish checkout) are simply absent
/// and sort last. One process, no per-branch calls: `--all` walks every reflog
/// and `%gd` carries the entry's date as `ref@{unix}`.
fn ref_created_at(root: &Path) -> std::collections::HashMap<String, i64> {
    let mut created = std::collections::HashMap::new();
    let Ok(out) = run_in(
        root,
        &["reflog", "show", "--all", "--date=unix", "--format=%gd"],
    ) else {
        return created;
    };
    for line in out.lines() {
        // `main@{1700000000}`, `refs/heads/x@{1700}`, `HEAD@{1700}`, …
        let Some((name, stamp)) = line.trim().rsplit_once("@{") else {
            continue;
        };
        let name = name.trim();
        let name = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/remotes/"))
            .unwrap_or(name);
        let stamp = stamp.trim_end_matches('}');
        let Ok(stamp) = stamp.parse::<i64>() else {
            continue;
        };
        if name.is_empty() || name == "HEAD" {
            continue;
        }
        // Entries arrive newest first per ref, so the smallest stamp wins.
        created
            .entry(name.to_string())
            .and_modify(|known: &mut i64| *known = (*known).min(stamp))
            .or_insert(stamp);
    }
    created
}

/// Every `git pull` this crate runs. `--no-edit` keeps a merge from opening an
/// editor in a TUI with no stdin; `--no-rebase` makes a pull MERGE by default
/// instead of honouring a machine-wide `pull.rebase=true` (user-requested:
/// rebasing a dirty index fails and rewriting history behind their back is
/// worse). A user who wants a rebase can still run it by hand.
const PULL_ARGS: &[&str] = &["pull", "--no-edit", "--no-rebase"];

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("color.ui=false")
        .args(args)
        .current_dir(dir);
    command
}

fn read_all(mut reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| format!("git output: {e}"))?;
    Ok(bytes)
}

/// Whether `error` is git refusing a checkout only because local modifications
/// would be overwritten. `command_result` keeps the first non-empty stderr line,
/// which is that `error:` line for both the tracked and the untracked variant.
pub fn checkout_blocked_by_local_changes(error: &str) -> bool {
    error.contains("would be overwritten by")
        || error.contains("Please commit your changes or stash them")
}

fn command_result(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> Result<String, String> {
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&stderr);
    Err(stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_string())
}

struct IgnoredScanLock {
    _file: File,
}

impl IgnoredScanLock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = ignored_scan_lock_path(root);
        let file = open_ignored_scan_lock(&path).or_else(|_| {
            let fallback = ignored_scan_fallback_path(root);
            open_ignored_scan_lock(&fallback)
        })?;
        file.try_lock()
            .map_err(|e| format!("ignored-file scan already running: {e}"))?;
        Ok(Self { _file: file })
    }
}

fn open_ignored_scan_lock(path: &Path) -> Result<File, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("ignored-file scan lock: {e}"))?;
    }
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("ignored-file scan lock: {e}"))
}

fn ignored_scan_lock_path(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return dot_git.join("herdr-sidebar-ignored.lock");
    }
    if dot_git.is_file()
        && let Ok(pointer) = std::fs::read_to_string(&dot_git)
        && let Some(path) = pointer.trim().strip_prefix("gitdir:")
    {
        let path = PathBuf::from(path.trim());
        return if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
        .join("herdr-sidebar-ignored.lock");
    }
    ignored_scan_fallback_path(root)
}

fn ignored_scan_fallback_path(root: &Path) -> PathBuf {
    let mut normalized = root.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized.make_ascii_lowercase();
    }
    let hash = normalized
        .bytes()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    let name = format!("ignored-scan-{hash:016x}.lock");
    std::env::temp_dir().join(name)
}

/// How many merge-conflict blocks a working-tree file still holds: one per
/// line starting with `<<<<<<<` (git's diff3 marker too). Source Control uses
/// it to show `!N` and to turn the letter into `✓` once the last block is gone
/// — the file stays unmerged (`UU`) until it is staged, so git's own status
/// cannot tell the two states apart.
///
/// At most [`CONFLICT_SCAN_BYTES`] are read, on the caller's worker/refresh
/// path (never per frame). `None` means "cannot tell" (unreadable file, or one
/// far too large to scan): the caller keeps showing plain `!` rather than
/// claiming the conflict is resolved.
pub fn count_conflict_blocks(path: &Path) -> Option<usize> {
    const CONFLICT_SCAN_BYTES: u64 = 4 * 1024 * 1024;
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(CONFLICT_SCAN_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(count_conflict_markers(&String::from_utf8_lossy(&bytes)))
}

/// The line scan behind [`count_conflict_blocks`], split out so it is testable
/// without a filesystem.
pub fn count_conflict_markers(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("<<<<<<<"))
        .count()
}

/// Parse `git status --porcelain -z --branch` output. Entries are NUL-separated
/// `XY path`; a rename/copy is followed by a second NUL-separated field holding
/// the source path. X is the index (staged) state, Y the worktree state.
pub fn parse_status(raw: &str) -> Status {
    let mut status = Status::default();
    let mut parts = raw.split('\0');
    while let Some(entry) = parts.next() {
        if entry.is_empty() {
            continue;
        }
        if let Some(header) = entry.strip_prefix("## ") {
            status.branch = parse_branch(header);
            (status.ahead, status.behind) = parse_ahead_behind(header);
            status.has_upstream = header.contains("...");
            continue;
        }
        let Some((xy, path)) = split_entry(entry) else {
            continue;
        };
        let (x, y) = xy;
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            parts.next().filter(|s| !s.is_empty()).map(str::to_string)
        } else {
            None
        };
        let path = path.to_string();
        if x == '?' && y == '?' {
            status.unstaged.push(FileEntry {
                path,
                orig: None,
                letter: 'U',
            });
            continue;
        }
        if x == '!' {
            continue; // ignored file
        }
        if is_conflict(x, y) {
            status.unstaged.push(FileEntry {
                path,
                orig,
                letter: '!',
            });
            continue;
        }
        if x != ' ' {
            status.staged.push(FileEntry {
                path: path.clone(),
                orig: matches!(x, 'R' | 'C').then(|| orig.clone()).flatten(),
                letter: display_letter(x),
            });
        }
        if y != ' ' {
            status.unstaged.push(FileEntry {
                path,
                orig: matches!(y, 'R' | 'C').then_some(orig).flatten(),
                letter: display_letter(y),
            });
        }
    }
    status
}

/// The repo-relative paths a stage under `prefix` should touch: the
/// WORKING-TREE side of `status` (already-staged entries need no re-adding),
/// deduped and ordered. `None` = the whole repository.
pub fn paths_under(status: &Status, prefix: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in &status.unstaged {
        let rename_touches_prefix = entry
            .orig
            .as_deref()
            .is_some_and(|original| under(original, prefix));
        if !under(&entry.path, prefix) && !rename_touches_prefix {
            continue;
        }
        for path in std::iter::once(&entry.path).chain(entry.orig.iter()) {
            if !out.contains(path) {
                out.push(path.clone());
            }
        }
    }
    out.sort();
    out
}

/// Drop every path that sits AT or inside one of `nested` — the nested
/// repository roots that must not be staged from an outer repo.
pub fn drop_nested(paths: Vec<String>, nested: &[String]) -> Vec<String> {
    paths
        .into_iter()
        .filter(|path| !nested.iter().any(|root| under(path, Some(root))))
        .collect()
}

/// Path-prefix containment on `/`-separated repo-relative paths: equal, or a
/// real descendant. `None` contains everything.
pub fn under(path: &str, prefix: Option<&str>) -> bool {
    let Some(prefix) = prefix else { return true };
    let prefix = prefix.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

/// NUL-delimited paths from `ls-files --others --ignored --directory`, with
/// the collapsed-directory trailing slash removed for normal path matching.
pub fn parse_ignored(raw: &str) -> Vec<String> {
    raw.split('\0')
        .map(|path| path.trim_end_matches('/').to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

/// `("XY", path)` from one porcelain entry; the XY columns are always ASCII.
fn split_entry(entry: &str) -> Option<((char, char), &str)> {
    let bytes = entry.as_bytes();
    if bytes.len() < 4 || bytes[2] != b' ' {
        return None;
    }
    Some(((bytes[0] as char, bytes[1] as char), &entry[3..]))
}

fn is_conflict(x: char, y: char) -> bool {
    matches!(
        (x, y),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

/// Type changes (T) read as plain modifications, matching VS Code.
fn display_letter(c: char) -> char {
    if c == 'T' { 'M' } else { c }
}

/// Branch from the `## …` header: `main...origin/main [ahead 1]`, bare `main`,
/// `No commits yet on main`, or `HEAD (no branch)` when detached.
fn parse_branch(header: &str) -> String {
    let head = header.split("...").next().unwrap_or(header);
    head.strip_prefix("No commits yet on ")
        .unwrap_or(head)
        .to_string()
}

/// `(ahead, behind)` from the header's `[ahead 1, behind 2]` suffix (either
/// half may be absent; `[gone]` and no-bracket headers give zeros).
fn parse_ahead_behind(header: &str) -> (usize, usize) {
    let Some(bracket) = header
        .rsplit_once('[')
        .map(|(_, b)| b.trim_end_matches(']'))
    else {
        return (0, 0);
    };
    let count_after = |tag: &str| {
        bracket
            .split(',')
            .map(str::trim)
            .find_map(|part| part.strip_prefix(tag))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0)
    };
    (count_after("ahead "), count_after("behind "))
}

/// Directories under `dir`, `depth` levels deep, skipping build/VCS internals
/// (the repo scan visits each).
fn child_dirs(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if depth == 0 {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        // Never follow a symlink while scanning (issue #87): a link into a
        // cloud or network mount (OneDrive's File Provider trash, an offline
        // SMB share) can block `stat` forever and the sidebar never draws.
        // `file_type()` does not traverse links; it usually comes from
        // readdir's d_type, falling back to a non-following lstat where the
        // filesystem gives none. Quick Open's walker already skips links. This
        // does not make enumerating a stalled mount itself non-blocking.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git" | "target" | "node_modules" | ".claude"
        ) {
            continue;
        }
        out.push(path.clone());
        out.extend(child_dirs(&path, depth - 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_branch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-git-branch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The tab label reads `HEAD` directly: no git process, worktrees included.
    #[test]
    fn branch_label_reads_head_without_git() {
        let base = scratch_branch("head");
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feat/tab-name\n").unwrap();
        assert_eq!(
            branch_label_at(&root.join("src/deep")).as_deref(),
            Some("feat/tab-name"),
            "a nested cwd still finds the repository root"
        );

        // Detached HEAD: the short commit, which is still better than "main".
        std::fs::write(
            root.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(
            branch_label_at(&root).as_deref(),
            Some("0123456"),
            "detached HEAD renders the short hash"
        );

        // No repository anywhere above the directory.
        std::fs::create_dir_all(base.join("plain")).unwrap();
        assert_eq!(branch_label_at(&base.join("plain")), None);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A worktree/submodule `.git` is a FILE pointing at a shared git dir; the
    /// label must come from the worktree's own `HEAD` there.
    #[test]
    fn branch_label_follows_a_worktree_gitdir_file() {
        let base = scratch_branch("worktree");
        let worktree = base.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let gitdir = base.join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        assert_eq!(branch_label_at(&worktree).as_deref(), Some("topic"));

        // An absolute gitdir is what `git worktree add` writes on some hosts.
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        assert_eq!(branch_label_at(&worktree).as_deref(), Some("topic"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn conflict_blocks_count_only_their_own_markers() {
        let diff3 = "keep\n<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> other\n<<<<<<<\n";
        assert_eq!(count_conflict_markers(diff3), 2);
        // The closing markers and diff3's base section must not count.
        assert_eq!(count_conflict_markers("=======\n>>>>>>> other\n"), 0);
        assert_eq!(count_conflict_markers("no conflicts here\n"), 0);
    }

    /// Issue #87: repository discovery must not descend through directory
    /// symlinks (a link into a stalled cloud mount hung the sidebar).
    #[cfg(unix)]
    #[test]
    fn child_dirs_does_not_follow_directory_symlinks() {
        let base = std::env::temp_dir().join(format!("aa-git-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/inner")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let dirs = child_dirs(&base, 2);
        assert!(dirs.contains(&base.join("real/inner")));
        assert!(!dirs.iter().any(|dir| dir.starts_with(base.join("link"))));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn entry(path: &str, letter: char, orig: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            orig: orig.map(str::to_string),
            letter,
        }
    }

    #[test]
    fn parses_branch_variants() {
        assert_eq!(
            parse_status("## main...origin/main [ahead 1]\0").branch,
            "main"
        );
        assert_eq!(parse_status("## git-panel\0").branch, "git-panel");
        assert_eq!(parse_status("## No commits yet on trunk\0").branch, "trunk");
        assert_eq!(
            parse_status("## HEAD (no branch)\0").branch,
            "HEAD (no branch)"
        );
    }

    #[test]
    fn branch_choices_keep_listing_order_and_drop_symbolic_remote_head() {
        // The input is already in creation order (newest first). The current
        // branch is NOT hoisted: doing that pushed a freshly created branch off
        // row 0 whenever an older branch was checked out (user-reported).
        let choices = parse_branch_choices(concat!(
            " \0feature/x\0refs/heads/feature/x\0\n",
            "*\0main\0refs/heads/main\0\n",
            " \0origin/main\0refs/remotes/origin/main\0\n",
            " \0origin/HEAD\0refs/remotes/origin/HEAD\0refs/remotes/origin/main\n",
        ));
        // Locals first in the order given, remotes last, the symbolic remote
        // HEAD alias dropped — even though git listed the remote before feature/x.
        assert_eq!(
            choices,
            [
                Branch {
                    name: "feature/x".into(),
                    current: false,
                    remote: false,
                },
                Branch {
                    name: "main".into(),
                    current: true,
                    remote: false,
                },
                Branch {
                    name: "origin/main".into(),
                    current: false,
                    remote: true,
                },
            ]
        );
    }

    #[test]
    fn head_signature_moves_with_a_commit() {
        let git = repo_with_head("head-signature-commit");
        let before = git.head_signature().expect("HEAD readable");
        run_in(
            &git.root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.dev",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "next",
            ],
        )
        .unwrap();
        let after = git.head_signature().expect("HEAD readable");
        assert_ne!(after, before, "a commit moves the branch tip");
        assert!(after.starts_with(&before[..before.find(' ').unwrap()])); // same HEAD ref line
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn head_signature_sees_a_paused_merge_and_its_abort() {
        let git = repo_with_head("head-signature-merge");
        let before = git.head_signature().expect("HEAD readable");
        // A conflicted merge writes MERGE_HEAD and does NOT move the branch,
        // so without this the `!` rows would wait for the 2 s poll.
        std::fs::write(git.root.join(".git/MERGE_HEAD"), "0123456789abcdef\n").unwrap();
        let during = git.head_signature().expect("HEAD readable");
        assert_ne!(during, before);
        assert!(during.contains("0123456789abcdef"), "{during}");
        std::fs::remove_file(git.root.join(".git/MERGE_HEAD")).unwrap();
        assert_eq!(git.head_signature().expect("HEAD readable"), before);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn head_signature_reads_a_packed_ref() {
        let git = repo_with_head("head-signature-packed");
        let before = git.head_signature().expect("HEAD readable");
        run_in(&git.root, &["pack-refs", "--all"]).unwrap();
        // Whatever git kept loose, drop it: the tip must come from packed-refs.
        let head = std::fs::read_to_string(git.root.join(".git/HEAD")).unwrap();
        let refname = head.trim().strip_prefix("ref: ").unwrap();
        let _ = std::fs::remove_file(git.root.join(".git").join(refname));
        assert_eq!(git.head_signature().expect("HEAD readable"), before);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn parses_ahead_behind_and_upstream() {
        let s = parse_status("## main...origin/main [ahead 3, behind 2]\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (3, 2, true));
        let s = parse_status("## main...origin/main [behind 4]\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (0, 4, true));
        let s = parse_status("## main...origin/main\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (0, 0, true));
        let s = parse_status("## local-only\0");
        assert_eq!((s.ahead, s.behind, s.has_upstream), (0, 0, false));
        let s = parse_status("## main...origin/main [gone]\0");
        assert_eq!((s.ahead, s.behind), (0, 0));
    }

    #[test]
    fn discover_all_finds_child_repos_and_dedupes() {
        let base = std::env::temp_dir().join(format!("aa-git-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // base is NOT a repo; two children are (one nested two levels down),
        // and a `.git`-less child is ignored.
        for sub in ["a", "group/b", "plain"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        for repo in ["a", "group/b"] {
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(base.join(repo))
                .output()
                .unwrap();
        }
        let repos = Git::discover_all(&base);
        let mut names: Vec<String> = repos.iter().map(|r| r.name()).collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fresh repo with one commit on `main`, so HEAD resolves.
    fn repo_with_head(name: &str) -> Git {
        let root =
            std::env::temp_dir().join(format!("aa-git-unstage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "-q"][..],
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ][..],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
        }
        Git { root }
    }

    /// `status` compares against the LAST known upstream tip, so a branch whose
    /// remote moved on still reads `behind 0` until something fetches. The
    /// Source Control button relies on `fetch` to become Sync Changes.
    #[test]
    fn fetch_makes_a_behind_branch_visible_to_status() {
        let base = std::env::temp_dir().join(format!(
            "aa-git-fetch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let remote = base.join("remote.git");
        let one = base.join("one");
        let two = base.join("two");
        let run = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.email=t@t.dev", "-c", "user.name=t"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        run(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                one.to_str().unwrap(),
            ],
        );
        run(&one, &["commit", "-q", "--allow-empty", "-m", "one"]);
        run(&one, &["push", "-q", "-u", "origin", "HEAD"]);
        run(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                two.to_str().unwrap(),
            ],
        );
        run(&two, &["commit", "-q", "--allow-empty", "-m", "two"]);
        run(&two, &["push", "-q"]);

        let git = Git::discover(&one).unwrap();
        assert_eq!(
            git.status().unwrap().behind,
            0,
            "without a fetch the remote-tracking ref is stale"
        );
        git.fetch().unwrap();
        assert_eq!(git.status().unwrap().behind, 1, "fetch updated the refs");
        std::fs::remove_dir_all(&base).ok();
    }

    /// `is_initialized` is a filesystem walk, so it must see a fresh `git init`
    /// (and not claim a repository above a nested, unrelated folder).
    #[test]
    fn is_initialized_walks_up_and_sees_a_fresh_init() {
        let base = std::env::temp_dir().join(format!("aa-git-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        let nested = project.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(!Git::is_initialized(&nested));

        Git::init(&project).unwrap();
        assert!(Git::is_initialized(&project));
        assert!(
            Git::is_initialized(&nested),
            "a subdirectory of an initialized project counts"
        );

        // A worktree/submodule `.git` is a FILE, not a directory.
        let linked = base.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /nowhere\n").unwrap();
        assert!(Git::is_initialized(&linked));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The views poll this instead of running git, so it must change exactly when
    /// the branch does.
    #[test]
    fn head_signature_tracks_create_switch_and_rename() {
        let git = repo_with_head("head-signature");
        let initial = git.head_signature().expect("HEAD readable");
        assert!(initial.starts_with("ref: refs/heads/"), "{initial}");

        git.create_branch("feature/x").unwrap();
        let created = git.head_signature().expect("HEAD readable");
        assert!(created.contains("refs/heads/feature/x"), "{created}");
        assert_ne!(created, initial);

        git.rename_branch("feature/x", "feature/y").unwrap();
        let renamed = git.head_signature().expect("HEAD readable");
        assert!(renamed.contains("refs/heads/feature/y"), "{renamed}");
        assert_ne!(renamed, created);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    /// A worktree's `.git` is a FILE holding `gitdir: <path>`; a plain
    /// `root/.git/HEAD` read would find nothing there.
    #[test]
    fn head_signature_resolves_a_worktree_git_file() {
        let git = repo_with_head("head-signature-worktree");
        let worktree = std::env::temp_dir().join(format!("aa-git-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&worktree);
        let path = worktree.to_str().unwrap().to_string();
        run_in(&git.root, &["worktree", "add", "-q", &path]).unwrap();

        let linked = Git {
            root: worktree.clone(),
        };
        let signature = linked.head_signature().expect("worktree HEAD");
        assert!(signature.starts_with("ref: refs/heads/"), "{signature}");

        let _ = run_in(&git.root, &["worktree", "remove", "--force", &path]);
        let _ = std::fs::remove_dir_all(&worktree);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    /// The commit button's pull is a real upstream merge, so this uses a bare
    /// origin and two clones rather than the single-repo fixture.
    #[test]
    fn pull_brings_in_an_upstream_commit_and_says_so() {
        let base = std::env::temp_dir().join(format!("aa-git-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin = base.join("origin.git");
        let one = base.join("one");
        let two = base.join("two");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare"]);
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::create_dir_all(&one).unwrap();
        git(&one, &["init", "-q"]);
        git(&one, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(&one, &["remote", "add", "origin", origin.to_str().unwrap()]);
        std::fs::write(one.join("a.txt"), "a\n").unwrap();
        git(&one, &["add", "."]);
        git(
            &one,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "a",
            ],
        );
        git(&one, &["push", "-q", "-u", "origin", "HEAD"]);
        git(
            &base,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                two.to_str().unwrap(),
            ],
        );

        // A second commit lands upstream after the clone.
        std::fs::write(one.join("b.txt"), "b\n").unwrap();
        git(&one, &["add", "."]);
        git(
            &one,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "b",
            ],
        );
        git(&one, &["push", "-q"]);

        // Even with a rebase config, our pull MERGES: the staged index would
        // make `git pull --rebase` refuse, and rewriting history behind the
        // user is worse than a merge commit (user-requested).
        git(&two, &["config", "pull.rebase", "true"]);
        std::fs::write(two.join("c.txt"), "c\n").unwrap();
        git(&two, &["add", "c.txt"]);

        let clone = Git { root: two.clone() };
        assert_eq!(clone.pull(), Ok("Pulled.".to_string()));
        assert!(two.join("b.txt").is_file(), "the upstream commit landed");
        // Second pull: nothing new.
        assert_eq!(clone.pull(), Ok("Already up to date.".to_string()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A branch created here has no upstream, and plain `git push` refuses that
    /// ("fatal: The current branch X has no upstream branch"), which left the
    /// panel with no way to push a new branch at all (user-reported). `push`
    /// publishes it with `--set-upstream`, and `sync` publishes instead of
    /// running the pull that could never succeed.
    #[test]
    fn push_publishes_a_branch_that_has_no_upstream() {
        let base = std::env::temp_dir().join(format!("aa-git-publish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin = base.join("origin.git");
        let work = base.join("work");
        let run = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "-q", "--bare"]);
        run(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "-q"]);
        run(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        run(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        run(&work, &["add", "."]);
        run(
            &work,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "a",
            ],
        );
        let repo = Git { root: work.clone() };

        // A brand-new branch: no upstream, so publish on the first push.
        run(&work, &["switch", "-q", "-c", "feature/publish"]);
        assert!(!repo.has_upstream());
        assert_eq!(
            repo.push(),
            Ok("Published feature/publish to origin.".to_string())
        );
        assert!(repo.has_upstream(), "the upstream was set");
        assert!(
            run_in(&origin, &["branch", "--list", "feature/publish"])
                .unwrap()
                .contains("feature/publish"),
            "the branch really landed on the remote"
        );
        // Once it tracks, the same call is an ordinary push.
        assert_eq!(repo.push(), Ok("Pushed.".to_string()));

        // Sync on another fresh branch publishes too: its pull could never work.
        run(&work, &["switch", "-q", "-c", "feature/sync"]);
        assert!(!repo.has_upstream());
        assert_eq!(
            repo.sync(),
            Ok("Published feature/sync to origin.".to_string())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn branch_choices_and_the_drawer_are_newest_created_first() {
        let git = repo_with_head("branch-creation-order");
        // Distinct reflog stamps without sleeping: `update-ref --create-reflog`
        // dates the entry from GIT_COMMITTER_DATE. Their COMMIT dates are all
        // HEAD's, and their names sort the wrong way round, so a list ordered by
        // committerdate or alphabetically cannot pass this.
        for (name, stamp) in [
            ("alpha", "1000000000"),
            ("beta", "1000100000"),
            ("gamma", "1000200000"),
        ] {
            let out = git_command(
                &git.root,
                &[
                    "update-ref",
                    "--create-reflog",
                    "-m",
                    "created",
                    &format!("refs/heads/{name}"),
                    "HEAD",
                ],
            )
            .env("GIT_COMMITTER_DATE", format!("{stamp} +0000"))
            .output()
            .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let choices: Vec<String> = git
            .branch_choices()
            .unwrap()
            .into_iter()
            .filter(|branch| !branch.current)
            .map(|branch| branch.name)
            .collect();
        assert_eq!(choices, vec!["gamma", "beta", "alpha"]);

        // The drawer reads the same thing, in the same order.
        let drawer = git.branches().unwrap();
        let position = |name: &str| {
            drawer
                .iter()
                .position(|line| line.trim_start_matches(['*', ' ']).trim() == name)
                .unwrap_or(usize::MAX)
        };
        assert!(
            position("gamma") < position("beta") && position("beta") < position("alpha"),
            "drawer order: {drawer:?}"
        );
    }

    #[test]
    fn a_branch_without_a_reflog_sorts_by_its_commit_date_not_last() {
        // A clone-made branch has no reflog here; before the fallback it sorted
        // below every branch that did have one, however old.
        let git = repo_with_head("branch-no-reflog-order");
        for name in ["logged-old", "logged-new"] {
            let out = git_command(&git.root, &["branch", name, "HEAD"])
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        // `logged-new` gets the newest reflog stamp; `unlogged` is a plain ref
        // (no reflog line) whose commit is HEAD, i.e. newer than both.
        let out = git_command(
            &git.root,
            &["update-ref", "--no-deref", "refs/heads/unlogged", "HEAD"],
        )
        .output()
        .unwrap();
        assert!(out.status.success());
        std::fs::remove_file(git.root.join(".git/logs/refs/heads/unlogged")).ok();
        let created = ref_created_at(&git.root);
        assert!(!created.contains_key("unlogged"), "no reflog for it");
        let commit = 5_000_000_000; // far newer than any real reflog stamp here
        assert_eq!(sort_stamp(&created, "unlogged", commit), commit);
        assert!(sort_stamp(&created, "logged-new", commit) < commit);
    }

    #[test]
    fn a_pull_merges_instead_of_rebasing() {
        // Defaulting to merge is user-requested: a machine-wide
        // `pull.rebase=true` makes a pull with a dirty index fail, and a rebase
        // run by the panel rewrites history without being asked.
        assert!(PULL_ARGS.contains(&"--no-rebase"), "{PULL_ARGS:?}");
        for arg in PULL_ARGS {
            assert!(
                !arg.contains("ff") && *arg != "--rebase" && *arg != "--merge",
                "PULL_ARGS must not defer to the config: {PULL_ARGS:?}"
            );
        }
        assert!(PULL_ARGS.contains(&"pull"));
    }

    #[test]
    fn checkout_branch_switches_without_forcing_dirty_work() {
        let git = repo_with_head("checkout-branch");
        let original = git.status().unwrap().branch;
        run_in(&git.root, &["branch", "topic"]).unwrap();
        git.checkout_branch(&Branch {
            name: "topic".into(),
            current: false,
            remote: false,
        })
        .unwrap();
        assert_eq!(git.status().unwrap().branch, "topic");
        git.checkout_branch(&Branch {
            name: original.clone(),
            current: false,
            remote: false,
        })
        .unwrap();
        assert_eq!(git.status().unwrap().branch, original);
        let topic = run_in(&git.root, &["rev-parse", "topic"])
            .unwrap()
            .trim()
            .to_string();
        run_in(&git.root, &["remote", "add", "origin", "."]).unwrap();
        run_in(
            &git.root,
            &["update-ref", "refs/remotes/origin/remote-topic", &topic],
        )
        .unwrap();
        git.checkout_branch(&Branch {
            name: "origin/remote-topic".into(),
            current: false,
            remote: true,
        })
        .unwrap();
        assert_eq!(git.status().unwrap().branch, "remote-topic");
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn checkout_refusals_for_local_changes_are_recognised() {
        assert!(checkout_blocked_by_local_changes(
            "error: Your local changes to the following files would be overwritten by checkout:"
        ));
        assert!(checkout_blocked_by_local_changes(
            "error: The following untracked working tree files would be overwritten by checkout:"
        ));
        assert!(checkout_blocked_by_local_changes(
            "error: Your local changes to the following files would be overwritten by merge:"
        ));
        assert!(!checkout_blocked_by_local_changes(
            "error: pathspec 'nope' did not match any file(s) known to git"
        ));
    }

    #[test]
    fn checkout_stashing_switches_and_keeps_the_local_edit() {
        let git = repo_with_head("checkout-stashing");
        let original = git.status().unwrap().branch;
        // The same file with a different body on each branch: a local edit of it
        // is exactly what makes a plain checkout refuse.
        commit_file(&git, "shared.txt", "base\n", "base");
        run_in(&git.root, &["checkout", "-q", "-b", "topic"]).unwrap();
        commit_file(&git, "shared.txt", "topic\n", "topic");
        run_in(&git.root, &["checkout", "-q", &original]).unwrap();
        std::fs::write(git.root.join("shared.txt"), "local edit\n").unwrap();

        let topic = Branch {
            name: "topic".into(),
            current: false,
            remote: false,
        };
        let refused = git.checkout_branch(&topic).unwrap_err();
        assert!(checkout_blocked_by_local_changes(&refused), "{refused}");

        let message = git.checkout_branch_stashing(&topic).unwrap();
        assert_eq!(git.status().unwrap().branch, "topic");
        assert!(message.starts_with("switched to topic"), "{message}");
        // The pop conflicts here (the file differs between the branches), so the
        // edit has to survive in the stash instead of vanishing.
        let file = std::fs::read_to_string(git.root.join("shared.txt")).unwrap();
        let stash = run_in(&git.root, &["stash", "list"]).unwrap();
        assert!(
            file == "local edit\n" || stash.contains("herdr-sidebar"),
            "the edit must survive in the file or in the stash: file={file:?} stash={stash:?}"
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn unstage_under_batches_a_large_directory_and_keeps_renames_paired() {
        let git = repo_with_head("unstage-under");
        std::fs::create_dir_all(git.root.join("big")).unwrap();
        for index in 0..70 {
            std::fs::write(git.root.join(format!("big/f{index}.txt")), "x").unwrap();
        }
        std::fs::write(git.root.join("keep.txt"), "keep").unwrap();
        std::fs::write(git.root.join("old.txt"), "moved").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();
        for index in 0..70 {
            std::fs::write(git.root.join(format!("big/f{index}.txt")), "changed").unwrap();
        }
        std::fs::write(git.root.join("keep.txt"), "changed").unwrap();
        run_in(&git.root, &["mv", "old.txt", "big/moved.txt"]).unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();

        let count = git.unstage_under("big").unwrap();
        assert_eq!(count, 72, "70 files plus both sides of the rename");
        let status = git.status().unwrap();
        assert_eq!(
            status
                .staged
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["keep.txt"],
            "only the path outside the prefix stays staged"
        );
        assert_eq!(git.unstage_under("big").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    fn commit_file(git: &Git, file: &str, body: &str, message: &str) {
        std::fs::write(git.root.join(file), body).unwrap();
        run_in(&git.root, &["add", file]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                message,
            ],
        )
        .unwrap();
    }

    /// The merge/switch popups open on row 0, so a remote must never be there
    /// while a local branch exists (user-requested: locals first).
    #[test]
    fn branch_choices_put_every_local_branch_before_a_remote() {
        let git = repo_with_head("branch-choices-locals-first");
        // A remote-tracking ref that is NEWER than the local branches, which is
        // what made git's committerdate sort put it on top.
        run_in(
            &git.root,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        )
        .unwrap();
        git.create_branch("a-local").unwrap();
        let choices = git.branch_choices().unwrap();
        let first_remote = choices.iter().position(|b| b.remote);
        let last_local = choices.iter().rposition(|b| !b.remote);
        match (first_remote, last_local) {
            (Some(remote), Some(local)) => assert!(local < remote, "{choices:?}"),
            (Some(_), None) => panic!("locals vanished: {choices:?}"),
            _ => {}
        }
        assert!(choices.iter().any(|b| b.name == "origin/main" && b.remote));
        assert!(choices.iter().any(|b| b.name == "a-local" && !b.remote));
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn rename_branch_moves_the_current_branch_and_refuses_clobbering() {
        let git = repo_with_head("rename-branch");
        let original = git.current_branch().unwrap();
        run_in(&git.root, &["branch", "taken"]).unwrap();
        assert!(
            git.rename_branch(&original, "taken").is_err(),
            "no -M overwrite"
        );
        assert!(git.rename_branch(&original, "bad..name").is_err());
        git.rename_branch(&original, " renamed ").unwrap();
        assert_eq!(git.current_branch().as_deref(), Some("renamed"));
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn undo_last_commit_drops_the_commit_and_keeps_its_changes_staged() {
        let git = repo_with_identity("undo-last");
        commit_file(&git, "a.txt", "a\n", "subject a");
        commit_file(&git, "b.txt", "b\n", "subject b");
        let before = subjects(&git);
        let undone = git.undo_last_commit().unwrap();
        assert_eq!(undone.subject, "subject b");
        assert_eq!(undone.hash.len(), 7);
        assert!(!undone.pushed);
        assert_eq!(subjects(&git), before[1..].to_vec());
        let staged = run_in(&git.root, &["diff", "--cached", "--name-only"]).unwrap();
        assert_eq!(staged.trim(), "b.txt");
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn undo_last_commit_refuses_the_root_commit() {
        let git = repo_with_identity("undo-root");
        let err = git.undo_last_commit().unwrap_err();
        assert_eq!(err, "the first commit cannot be undone");
        assert_eq!(subjects(&git).len(), 1);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn undo_last_commit_refuses_during_a_merge() {
        let git = repo_with_identity("undo-merge");
        commit_file(&git, "a.txt", "a\n", "subject a");
        commit_file(&git, "b.txt", "b\n", "subject b");
        std::fs::write(git.root.join(".git/MERGE_HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(
            git.undo_last_commit().unwrap_err(),
            "finish the merge/rebase in progress first"
        );
        assert_eq!(subjects(&git).len(), 3);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    /// `repo_with_head` plus a committer identity, so `commit-tree` works without a
    /// global git identity.
    fn repo_with_identity(name: &str) -> Git {
        let git = repo_with_head(name);
        run_in(&git.root, &["config", "user.name", "t"]).unwrap();
        run_in(&git.root, &["config", "user.email", "t@t.dev"]).unwrap();
        git
    }

    /// A real merge that conflicts on `a.txt` (both modified) and on `b.txt`
    /// (ours edited it, theirs deleted it).
    fn repo_with_conflicts(name: &str) -> Git {
        let git = repo_with_identity(name);
        commit_file(&git, "a.txt", "base\n", "base a");
        commit_file(&git, "b.txt", "base\n", "base b");
        run_in(&git.root, &["checkout", "-q", "-b", "side"]).unwrap();
        commit_file(&git, "a.txt", "theirs\n", "side a");
        run_in(&git.root, &["rm", "-q", "b.txt"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "side b",
            ],
        )
        .unwrap();
        run_in(&git.root, &["checkout", "-q", "-"]).unwrap();
        commit_file(&git, "a.txt", "ours\n", "main a");
        commit_file(&git, "b.txt", "ours edit\n", "main b");
        // The merge is expected to stop with conflicts, so its exit is ignored.
        let _ = run_in(&git.root, &["merge", "-q", "--no-edit", "side"]);
        git
    }

    /// Unmerged paths come from git itself, and the stage numbers classify them.
    #[test]
    fn conflicted_paths_and_kinds_come_from_index_stages() {
        let git = repo_with_conflicts("conflicts");
        assert!(git.merge_in_progress());
        assert_eq!(git.conflicted_paths().unwrap(), ["a.txt", "b.txt"]);
        assert_eq!(git.conflict_kind("a.txt"), ConflictKind::BothModified);
        assert_eq!(git.conflict_kind("b.txt"), ConflictKind::DeletedByThem);
        assert_eq!(git.conflict_kind("untouched.txt"), ConflictKind::Other);

        git.mark_resolved("a.txt").unwrap();
        assert_eq!(git.conflicted_paths().unwrap(), ["b.txt"]);
        assert_eq!(git.conflict_kind("a.txt"), ConflictKind::Other);

        git.remove_conflicted("b.txt").unwrap();
        assert!(git.conflicted_paths().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&git.root);
    }

    /// Like `commit_file`, but with a fixed foreign author and date, so a test can
    /// check that a reword keeps both.
    fn commit_as_ada(git: &Git, file: &str, message: &str) {
        std::fs::write(git.root.join(file), format!("{file}\n")).unwrap();
        run_in(&git.root, &["add", file]).unwrap();
        let out = git_command(&git.root, &["commit", "-q", "-m", message])
            .env("GIT_AUTHOR_NAME", "Ada Author")
            .env("GIT_AUTHOR_EMAIL", "ada@example.dev")
            .env("GIT_AUTHOR_DATE", "1600000000 +0200")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.dev")
            .output()
            .unwrap();
        assert!(out.status.success(), "commit failed");
    }

    fn subjects(git: &Git) -> Vec<String> {
        run_in(&git.root, &["log", "--format=%s"])
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn reword_commit_rewrites_the_middle_commit_and_keeps_the_index() {
        let git = repo_with_identity("reword-middle");
        let branch = git.current_branch().unwrap();
        commit_file(&git, "a.txt", "a\n", "subject a");
        commit_as_ada(&git, "b.txt", "subject b\n\nold body\n");
        commit_file(&git, "c.txt", "c\n", "subject c");

        // A staged change that must stay staged, and an unstaged one that must stay put.
        std::fs::write(git.root.join("staged.txt"), "s\n").unwrap();
        run_in(&git.root, &["add", "staged.txt"]).unwrap();
        std::fs::write(git.root.join("a.txt"), "dirty\n").unwrap();

        let tree_before = run_in(&git.root, &["rev-parse", "HEAD^{tree}"]).unwrap();
        let b_hash = run_in(&git.root, &["rev-parse", "HEAD~1"])
            .unwrap()
            .trim()
            .to_string();
        let author_before = run_in(
            &git.root,
            &["log", "-1", "--format=%an%n%ae%n%ad", "--date=raw", &b_hash],
        )
        .unwrap();

        let reworded = git
            .reword_commit(&b_hash, "new subject\n\nold body\n")
            .unwrap();
        assert_eq!(reworded.old, b_hash[..7]);
        assert_ne!(reworded.new, reworded.old);
        assert!(!reworded.pushed);

        assert_eq!(
            subjects(&git),
            ["subject c", "new subject", "subject a", "init"]
        );
        assert_eq!(
            run_in(&git.root, &["rev-parse", "HEAD^{tree}"]).unwrap(),
            tree_before,
            "the tree of HEAD is unchanged"
        );
        assert_eq!(
            run_in(
                &git.root,
                &[
                    "log",
                    "-1",
                    "--format=%an%n%ae%n%ad",
                    "--date=raw",
                    "HEAD~1"
                ],
            )
            .unwrap(),
            author_before,
            "author name, email and date survive"
        );
        assert_eq!(
            git.commit_message("HEAD~1").unwrap(),
            "new subject\n\nold body\n"
        );
        assert!(
            run_in(&git.root, &["diff", "--cached", "--name-only"])
                .unwrap()
                .contains("staged.txt")
        );
        assert!(
            run_in(&git.root, &["diff", "--name-only"])
                .unwrap()
                .contains("a.txt")
        );
        assert_eq!(git.current_branch().as_deref(), Some(branch.as_str()));
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn reword_commit_on_head_keeps_the_body_and_the_commit_count() {
        let git = repo_with_identity("reword-head");
        commit_file(&git, "a.txt", "a\n", "old subject\n\nkeep this body");
        let count = run_in(&git.root, &["rev-list", "--count", "HEAD"]).unwrap();

        let reworded = git
            .reword_commit("HEAD", "renamed\n\nkeep this body")
            .unwrap();
        assert_ne!(reworded.new, reworded.old);
        assert_eq!(
            run_in(&git.root, &["rev-list", "--count", "HEAD"]).unwrap(),
            count
        );
        assert_eq!(
            git.commit_message("HEAD").unwrap(),
            "renamed\n\nkeep this body\n"
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn reword_commit_refuses_a_commit_off_the_current_branch_and_empty_messages() {
        let git = repo_with_identity("reword-refuse");
        let main = git.current_branch().unwrap();
        run_in(&git.root, &["switch", "-q", "-c", "side"]).unwrap();
        commit_file(&git, "side.txt", "s\n", "side commit");
        let side = run_in(&git.root, &["rev-parse", "HEAD"]).unwrap();
        run_in(&git.root, &["switch", "-q", &main]).unwrap();

        assert!(!git.is_ancestor_of_head(side.trim()));
        assert_eq!(
            git.reword_commit(side.trim(), "x").unwrap_err(),
            "that commit is not on the current branch"
        );
        assert_eq!(
            git.reword_commit("HEAD", "  \n").unwrap_err(),
            "Commit message is empty."
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn merge_branch_reports_done_and_conflicts_and_can_abort() {
        let git = repo_with_head("merge-branch");
        let main = git.current_branch().unwrap();
        commit_file(&git, "f.txt", "base\n", "base");
        run_in(&git.root, &["switch", "-q", "-c", "clean"]).unwrap();
        commit_file(&git, "other.txt", "x\n", "clean change");
        run_in(&git.root, &["switch", "-q", &main]).unwrap();
        assert!(matches!(
            git.merge_branch("clean"),
            Ok(MergeOutcome::Done(_))
        ));
        assert!(git.root.join("other.txt").exists());

        run_in(&git.root, &["switch", "-q", "-c", "clash"]).unwrap();
        commit_file(&git, "f.txt", "theirs\n", "theirs");
        run_in(&git.root, &["switch", "-q", &main]).unwrap();
        commit_file(&git, "f.txt", "ours\n", "ours");
        assert_eq!(
            git.merge_branch("clash"),
            Ok(MergeOutcome::Conflicts(vec!["f.txt".to_string()]))
        );
        assert!(git.merge_in_progress());
        git.merge_abort().unwrap();
        assert!(!git.merge_in_progress());
        assert!(git.merge_branch("no-such-branch").is_err());
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn create_branch_makes_and_switches_to_the_new_branch() {
        let git = repo_with_head("create-branch");
        git.create_branch("feature/login").unwrap();
        assert_eq!(git.status().unwrap().branch, "feature/login");
        assert!(
            git.branch_choices()
                .unwrap()
                .iter()
                .any(|branch| branch.name == "feature/login" && branch.current)
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn create_branch_refuses_a_name_already_in_use() {
        let git = repo_with_head("create-branch-existing");
        let original = git.status().unwrap().branch;
        git.create_branch("topic").unwrap();
        git.checkout_branch(&Branch {
            name: original.clone(),
            current: false,
            remote: false,
        })
        .unwrap();
        assert!(git.create_branch("topic").is_err());
        assert_eq!(git.status().unwrap().branch, original);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn create_branch_rejects_a_malformed_name_before_running_switch() {
        let git = repo_with_head("create-branch-invalid");
        let original = git.status().unwrap().branch;
        for bad in [
            "",
            "   ",
            "has space",
            "bad..name",
            "-leading-dash",
            "ends/",
            "@{-1}",
            "refs/heads/topic",
        ] {
            assert!(git.create_branch(bad).is_err(), "accepted {bad:?}");
        }
        assert_eq!(git.status().unwrap().branch, original);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn delete_branch_removes_a_merged_branch_without_forcing() {
        let git = repo_with_head("delete-merged");
        run_in(&git.root, &["branch", "merged"]).unwrap();
        assert_eq!(git.delete_branch("merged"), Ok(()));
        assert!(
            !git.branch_choices()
                .unwrap()
                .iter()
                .any(|b| b.name == "merged")
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn delete_branch_reports_unmerged_work_instead_of_destroying_it() {
        let git = repo_with_head("delete-unmerged");
        let original = git.status().unwrap().branch;
        run_in(&git.root, &["checkout", "-q", "-b", "wip"]).unwrap();
        std::fs::write(git.root.join("w.txt"), "work").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "wip",
            ],
        )
        .unwrap();
        run_in(&git.root, &["checkout", "-q", &original]).unwrap();

        assert_eq!(git.delete_branch("wip"), Err(DeleteRefused::NotMerged));
        assert!(
            git.branch_choices()
                .unwrap()
                .iter()
                .any(|b| b.name == "wip"),
            "a refused delete must leave the branch alone"
        );
        git.force_delete_branch("wip").unwrap();
        assert!(
            !git.branch_choices()
                .unwrap()
                .iter()
                .any(|b| b.name == "wip")
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    /// A branch that does not exist must not be reported as unmerged work —
    /// that would offer to force-delete nothing and hide git's real message.
    #[test]
    fn delete_branch_separates_other_failures_from_unmerged() {
        let git = repo_with_head("delete-missing");
        match git.delete_branch("no-such-branch") {
            Err(DeleteRefused::Failed(message)) => assert!(!message.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn delete_branch_does_not_offer_force_for_a_checked_out_branch() {
        let git = repo_with_head("delete-checked-out");
        let name = run_in(&git.root, &["branch", "--show-current"]).unwrap();
        assert!(matches!(
            git.delete_branch(name.trim()),
            Err(DeleteRefused::Failed(_))
        ));
        assert!(run_in(&git.root, &["rev-parse", "--verify", "HEAD"]).is_ok());
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn unstage_all_propagates_reset_failure_instead_of_destructive_fallback_when_head_exists() {
        let git = repo_with_head("unstage-all");
        std::fs::write(git.root.join("file.txt"), "v1").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        // Force `git reset` to fail deterministically without touching HEAD.
        std::fs::write(git.root.join(".git/index.lock"), "").unwrap();
        let result = git.unstage_all();
        std::fs::remove_file(git.root.join(".git/index.lock")).unwrap();
        assert!(
            result.is_err(),
            "a real reset failure must not report success"
        );
        let status = git.status().unwrap();
        assert_eq!(
            status.staged.len(),
            1,
            "file must still be staged, untouched"
        );
        assert_eq!(
            status.staged[0].letter, 'A',
            "must still be a staged add, not a staged deletion"
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn unstage_all_falls_back_on_a_genuinely_unborn_branch() {
        let root =
            std::env::temp_dir().join(format!("aa-git-unstage-unborn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::fs::write(root.join("file.txt"), "v1").unwrap();
        let git = Git { root: root.clone() };
        run_in(&git.root, &["add", "-A"]).unwrap();
        // No commits yet: `git reset` has no HEAD to reset against.
        assert!(!git.has_head());
        assert!(git.unstage_all().is_ok());
        let status = git.status().unwrap();
        assert_eq!(status.staged, vec![]);
        assert_eq!(status.unstaged, vec![entry("file.txt", 'U', None)]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn splits_staged_and_unstaged_sides() {
        let s = parse_status("## main\0MM src/app.rs\0A  new.rs\0 D gone.rs\0");
        assert_eq!(
            s.staged,
            vec![entry("src/app.rs", 'M', None), entry("new.rs", 'A', None)]
        );
        assert_eq!(
            s.unstaged,
            vec![entry("src/app.rs", 'M', None), entry("gone.rs", 'D', None)]
        );
    }

    #[test]
    fn untracked_shows_as_u() {
        let s = parse_status("?? docs/notes.md\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(s.unstaged, vec![entry("docs/notes.md", 'U', None)]);
    }

    #[test]
    fn rename_consumes_the_source_field() {
        let s = parse_status("R  new_name.rs\0old_name.rs\0?? after.txt\0");
        assert_eq!(
            s.staged,
            vec![entry("new_name.rs", 'R', Some("old_name.rs"))]
        );
        assert_eq!(s.unstaged, vec![entry("after.txt", 'U', None)]);
    }

    #[test]
    fn each_status_side_only_keeps_its_own_rename_source() {
        let s = parse_status("RM new_name.rs\0old_name.rs\0");
        assert_eq!(
            s.staged,
            vec![entry("new_name.rs", 'R', Some("old_name.rs"))]
        );
        assert_eq!(s.unstaged, vec![entry("new_name.rs", 'M', None)]);
    }

    #[test]
    fn type_change_reads_as_modified() {
        let s = parse_status("T  link.sh\0 T other.sh\0");
        assert_eq!(s.staged, vec![entry("link.sh", 'M', None)]);
        assert_eq!(s.unstaged, vec![entry("other.sh", 'M', None)]);
    }

    #[test]
    fn conflicts_land_unstaged_with_bang() {
        let s = parse_status("UU merge.rs\0AA both.rs\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(
            s.unstaged,
            vec![entry("merge.rs", '!', None), entry("both.rs", '!', None)]
        );
    }

    #[test]
    fn garbage_and_ignored_entries_are_skipped() {
        let s = parse_status("!! target\0x\0\0 M ok.rs\0");
        assert_eq!(s.staged, vec![]);
        assert_eq!(s.unstaged, vec![entry("ok.rs", 'M', None)]);
    }

    #[test]
    fn ignored_entries_parse_into_roots() {
        // `ls-files --directory` collapses an ignored directory to one entry;
        // the trailing slash goes so it compares like any other path.
        let raw = "target/\0build.log\0";
        assert_eq!(parse_ignored(raw), ["target", "build.log"]);
        assert_eq!(parse_ignored(""), Vec::<String>::new());
    }

    #[test]
    fn ignored_scan_lock_is_repository_scoped() {
        let first = std::env::temp_dir().join("ignored-lock-a");
        let second = std::env::temp_dir().join("ignored-lock-b");
        assert_eq!(
            ignored_scan_lock_path(&first),
            ignored_scan_lock_path(&first)
        );
        assert_ne!(
            ignored_scan_lock_path(&first),
            ignored_scan_lock_path(&second)
        );
    }

    #[test]
    fn ignored_scan_lock_is_single_flight() {
        let root =
            std::env::temp_dir().join(format!("ignored-lock-single-flight-{}", std::process::id()));
        let first = IgnoredScanLock::acquire(&root).unwrap();
        assert!(
            IgnoredScanLock::acquire(&root).is_err(),
            "a second process must skip an outstanding scan"
        );
        drop(first);
        // A git child spawned by a parallel test can briefly hold an inherited
        // copy of the lock's file descriptor until it execs, so the release is
        // only guaranteed shortly after the drop.
        let deadline = Instant::now() + Duration::from_secs(2);
        while IgnoredScanLock::acquire(&root).is_err() {
            assert!(
                Instant::now() < deadline,
                "the lock must be free after the drop"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn prefix_containment_is_path_aware() {
        assert!(under("src/app.rs", None), "no prefix contains everything");
        assert!(under("src/app.rs", Some("src")));
        assert!(under("src/app.rs", Some("src/app.rs")), "the path itself");
        assert!(under("src/api/routes.rs", Some("src")));
        assert!(
            !under("srcfoo/app.rs", Some("src")),
            "not a component boundary"
        );
        assert!(
            !under("src", Some("src/api")),
            "an ancestor is not under it"
        );
        assert!(!under("docs/x.md", Some("src")));
    }

    #[test]
    fn stage_candidates_are_the_working_tree_side_under_the_prefix() {
        let status = parse_status(
            "## main\0M  already-staged.rs\0 M src/app.rs\0?? src/new.rs\0 D src/gone.rs\0 M docs/x.md\0",
        );
        assert_eq!(
            paths_under(&status, Some("src")),
            ["src/app.rs", "src/gone.rs", "src/new.rs"],
            "modifications, deletions and untracked files alike"
        );
        assert_eq!(
            paths_under(&status, None),
            ["docs/x.md", "src/app.rs", "src/gone.rs", "src/new.rs"],
            "the whole repo, minus what is already staged"
        );
        assert_eq!(paths_under(&status, Some("nothing")), Vec::<String>::new());
    }

    #[test]
    fn stage_candidates_keep_both_sides_of_an_unstaged_rename() {
        let status = parse_status(" R src/new.rs\0src/old.rs\0 M docs/x.md\0");
        assert_eq!(
            paths_under(&status, Some("src")),
            ["src/new.rs", "src/old.rs"]
        );
        assert_eq!(
            paths_under(&status, Some("src/new.rs")),
            ["src/new.rs", "src/old.rs"],
            "staging the rename row must include its deleted source"
        );
    }

    #[test]
    fn nested_repo_paths_are_dropped_from_a_parent_stage() {
        let paths = vec![
            "src/app.rs".to_string(),
            "vendor/lib".to_string(),
            "vendor/lib/inner.rs".to_string(),
            "vendor/other.rs".to_string(),
        ];
        assert_eq!(
            drop_nested(paths, &["vendor/lib".to_string()]),
            ["src/app.rs", "vendor/other.rs"],
            "the nested root itself and everything inside it"
        );
    }

    /// A repo with an inner repo under `vendor/lib`, both dirty.
    fn nested_repo_fixture(tag: &str) -> (Git, Git) {
        let root = std::env::temp_dir().join(format!("aa-git-nested-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("vendor/lib")).unwrap();
        for dir in [root.clone(), root.join("vendor/lib")] {
            for args in [
                &["init", "-q"][..],
                &[
                    "-c",
                    "user.email=t@t.dev",
                    "-c",
                    "user.name=t",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    "init",
                ][..],
            ] {
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .output()
                    .unwrap();
            }
        }
        std::fs::write(root.join("src/app.rs"), "outer").unwrap();
        std::fs::write(root.join("vendor/note.txt"), "sibling").unwrap();
        std::fs::write(root.join("vendor/lib/inner.rs"), "inner").unwrap();
        (
            Git { root: root.clone() },
            Git {
                root: root.join("vendor/lib"),
            },
        )
    }

    #[test]
    fn staging_a_directory_stops_at_a_nested_repository_boundary() {
        let (outer, inner) = nested_repo_fixture("boundary");
        // Staging the whole outer repo must not reach into vendor/lib — not
        // even as the gitlink a bare `git add vendor` would record.
        let staged = outer.stage_under(&outer.root).unwrap();
        assert_eq!(staged.count, 2, "src/app.rs and vendor/note.txt only");
        assert_eq!(
            staged.skipped_nested, 1,
            "vendor/lib was skipped, not staged"
        );
        let status = outer.status().unwrap();
        let names: Vec<&str> = status.staged.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, ["src/app.rs", "vendor/note.txt"]);
        assert!(
            !status
                .staged
                .iter()
                .any(|e| e.path.starts_with("vendor/lib")),
            "no gitlink for the embedded repo: {names:?}"
        );
        // The inner repo is untouched, and stages independently.
        assert!(inner.status().unwrap().staged.is_empty());
        assert_eq!(
            inner.stage_under(&inner.root).unwrap(),
            Staged {
                count: 1,
                skipped_nested: 0
            }
        );
        assert_eq!(inner.status().unwrap().staged[0].path, "inner.rs");
        let _ = std::fs::remove_dir_all(&outer.root);
    }

    #[test]
    fn owner_of_resolves_to_the_nearest_enclosing_repository() {
        let (outer, inner) = nested_repo_fixture("owner");
        let outer_root = std::fs::canonicalize(&outer.root).unwrap();
        let inner_root = std::fs::canonicalize(&inner.root).unwrap();
        assert_eq!(
            std::fs::canonicalize(Git::owner_of(&outer.root.join("src/app.rs")).unwrap().root)
                .unwrap(),
            outer_root
        );
        assert_eq!(
            std::fs::canonicalize(Git::owner_of(&outer.root.join("src")).unwrap().root).unwrap(),
            outer_root
        );
        // Inside the nested checkout the INNER repo owns the path.
        assert_eq!(
            std::fs::canonicalize(Git::owner_of(&inner.root.join("inner.rs")).unwrap().root)
                .unwrap(),
            inner_root
        );
        assert_eq!(
            std::fs::canonicalize(Git::owner_of(&inner.root).unwrap().root).unwrap(),
            inner_root
        );
        let _ = std::fs::remove_dir_all(&outer.root);
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_is_owned_and_staged_as_the_link_not_its_target() {
        use std::os::unix::fs::symlink;

        let git = repo_with_head("directory-symlink-owner");
        let target = std::env::temp_dir().join(format!(
            "herdr-directory-symlink-target-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("outside.txt"), "outside").unwrap();
        let link = git.root.join("linked");
        symlink(&target, &link).unwrap();

        assert_eq!(
            std::fs::canonicalize(Git::owner_of(&link).unwrap().root).unwrap(),
            std::fs::canonicalize(&git.root).unwrap()
        );
        assert_eq!(git.stage_under(&link).unwrap().count, 1);
        assert_eq!(git.status().unwrap().staged[0].path, "linked");

        let _ = std::fs::remove_dir_all(&git.root);
        let _ = std::fs::remove_dir_all(target);
    }

    #[test]
    fn staging_covers_additions_modifications_and_deletions() {
        let git = repo_with_head("stage-kinds");
        std::fs::create_dir_all(git.root.join("src")).unwrap();
        std::fs::write(git.root.join("src/tracked.rs"), "v1").unwrap();
        std::fs::write(git.root.join("src/removed.rs"), "v1").unwrap();
        std::fs::write(git.root.join("other.rs"), "v1").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();
        std::fs::write(git.root.join("src/tracked.rs"), "v2").unwrap();
        std::fs::remove_file(git.root.join("src/removed.rs")).unwrap();
        std::fs::write(git.root.join("src/added.rs"), "new").unwrap();
        std::fs::write(git.root.join("other.rs"), "v2").unwrap();

        assert_eq!(git.stage_under(&git.root.join("src")).unwrap().count, 3);
        let status = git.status().unwrap();
        let staged: Vec<(&str, char)> = status
            .staged
            .iter()
            .map(|e| (e.path.as_str(), e.letter))
            .collect();
        assert_eq!(
            staged,
            [
                ("src/added.rs", 'A'),
                ("src/removed.rs", 'D'),
                ("src/tracked.rs", 'M')
            ]
        );
        assert_eq!(
            status
                .unstaged
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["other.rs"],
            "a sibling outside the staged directory stays untouched"
        );
        // Nothing left to stage there.
        assert_eq!(git.stage_under(&git.root.join("src")).unwrap().count, 0);
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn staging_a_directory_records_a_rename_not_only_its_destination() {
        let git = repo_with_head("stage-rename");
        std::fs::create_dir_all(git.root.join("src")).unwrap();
        std::fs::write(git.root.join("src/old.rs"), "tracked").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();
        run_in(&git.root, &["config", "status.renames", "true"]).unwrap();
        std::fs::rename(git.root.join("src/old.rs"), git.root.join("src/new.rs")).unwrap();
        run_in(&git.root, &["add", "-N", "src/new.rs"]).unwrap();

        let before = git.status().unwrap();
        assert_eq!(before.unstaged[0].orig.as_deref(), Some("src/old.rs"));
        assert_eq!(git.stage_under(&git.root.join("src")).unwrap().count, 2);

        let staged = git.status().unwrap();
        assert!(staged.unstaged.is_empty(), "both rename sides were staged");
        assert_eq!(staged.staged[0].path, "src/new.rs");
        assert_eq!(staged.staged[0].orig.as_deref(), Some("src/old.rs"));
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn staging_a_rename_row_records_both_sides() {
        let git = repo_with_head("stage-rename-row");
        std::fs::create_dir_all(git.root.join("src")).unwrap();
        std::fs::write(git.root.join("src/old.rs"), "tracked").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();
        std::fs::rename(git.root.join("src/old.rs"), git.root.join("src/new.rs")).unwrap();
        run_in(&git.root, &["add", "-N", "src/new.rs"]).unwrap();

        let entry = git
            .status()
            .unwrap()
            .unstaged
            .into_iter()
            .find(|entry| entry.orig.is_some())
            .unwrap();
        git.stage(&entry).unwrap();

        let status = git.status().unwrap();
        assert!(status.unstaged.is_empty(), "both rename sides were staged");
        assert_eq!(status.staged[0].path, "src/new.rs");
        assert_eq!(status.staged[0].orig.as_deref(), Some("src/old.rs"));
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn discarding_an_unstaged_rename_restores_only_its_snapshotted_paths() {
        let git = repo_with_head("discard-rename");
        std::fs::create_dir_all(git.root.join("src")).unwrap();
        std::fs::write(git.root.join("src/old.rs"), "tracked").unwrap();
        run_in(&git.root, &["add", "-A"]).unwrap();
        run_in(
            &git.root,
            &[
                "-c",
                "user.email=t@t.dev",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        )
        .unwrap();
        std::fs::rename(git.root.join("src/old.rs"), git.root.join("src/new.rs")).unwrap();
        run_in(&git.root, &["add", "-N", "src/new.rs"]).unwrap();

        let entry = git
            .status()
            .unwrap()
            .unstaged
            .into_iter()
            .find(|entry| entry.orig.is_some())
            .unwrap();
        git.discard(&entry).unwrap();

        assert_eq!(
            std::fs::read_to_string(git.root.join("src/old.rs")).unwrap(),
            "tracked"
        );
        assert!(!git.root.join("src/new.rs").exists());
        let status = git.status().unwrap();
        assert!(status.staged.is_empty());
        assert!(status.unstaged.is_empty());
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn staging_a_single_file_stages_only_that_file() {
        let git = repo_with_head("stage-one");
        std::fs::create_dir_all(git.root.join("src")).unwrap();
        std::fs::write(git.root.join("src/a.rs"), "a").unwrap();
        std::fs::write(git.root.join("src/b.rs"), "b").unwrap();
        assert_eq!(
            git.stage_under(&git.root.join("src/a.rs")).unwrap().count,
            1
        );
        let status = git.status().unwrap();
        assert_eq!(
            status
                .staged
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["src/a.rs"]
        );
        assert_eq!(
            status
                .unstaged
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            ["src/b.rs"]
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn staging_refuses_a_target_outside_the_repository() {
        let git = Git {
            root: std::env::temp_dir().join("stage-boundary-repo"),
        };
        let outside = std::env::temp_dir().join("stage-boundary-other");
        let error = git.stage_under(&outside).unwrap_err();
        assert!(error.contains("outside repository"), "{error}");
    }

    #[test]
    fn ignored_lists_the_repos_ignored_roots() {
        let git = repo_with_head("ignored");
        assert_eq!(
            ignored_scan_lock_path(&git.root),
            git.root.join(".git/herdr-sidebar-ignored.lock")
        );
        std::fs::write(
            git.root.join(".gitignore"),
            "target/\nempty-cache/\n*.log\n",
        )
        .unwrap();
        std::fs::create_dir_all(git.root.join("target/debug")).unwrap();
        std::fs::create_dir_all(git.root.join("empty-cache")).unwrap();
        std::fs::write(git.root.join("target/debug/app.exe"), "bin").unwrap();
        std::fs::write(git.root.join("build.log"), "noise").unwrap();
        let mut ignored = git.ignored().unwrap();
        ignored.sort();
        assert_eq!(
            ignored,
            ["build.log", "empty-cache", "target"],
            "the ignored dir stays collapsed"
        );
        // Ignored paths are never stage candidates.
        assert_eq!(
            paths_under(&git.status().unwrap(), None),
            [".gitignore"],
            "only the non-ignored file is a candidate"
        );
        let _ = std::fs::remove_dir_all(&git.root);
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = parse_status("M  my docs/read me.md\0");
        assert_eq!(s.staged, vec![entry("my docs/read me.md", 'M', None)]);
    }

    /// Captured from `git log -L2,3:f.txt` on a file whose second and third
    /// lines were changed by two later commits (git 2.53).
    const LINE_LOG: &str = concat!(
        "\u{1e}5e4e31bc2d7df6a3840a3acd5e719e7382eb7c09\u{1f}5e4e31b\u{1f}Bob\u{1f}2026-10-16\u{1f}change c\n",
        "\n",
        "diff --git a/f.txt b/f.txt\n",
        "--- a/f.txt\n",
        "+++ b/f.txt\n",
        "@@ -2,2 +2,2 @@\n",
        " B\n",
        "-c\n",
        "+C\n",
        "\u{1e}368d2af2c147da1fa6dd8b0d0e5f87605a85c6f4\u{1f}368d2af\u{1f}Ann\u{1f}2026-10-16\u{1f}change b\n",
        "\n",
        "diff --git a/f.txt b/f.txt\n",
        "--- a/f.txt\n",
        "+++ b/f.txt\n",
        "@@ -2,2 +2,2 @@\n",
        "-b\n",
        "+B\n",
        " c\n",
        "\u{1e}c49a6eb8332c76350347578f98510d578a29c6e2\u{1f}c49a6eb\u{1f}Ann\u{1f}2026-10-16\u{1f}first\n",
        "\n",
        "diff --git a/f.txt b/f.txt\n",
        "--- /dev/null\n",
        "+++ b/f.txt\n",
        "@@ -0,0 +2,2 @@\n",
        "+b\n",
        "+c\n",
    );

    #[test]
    fn line_history_parser_splits_records_and_keeps_each_patch() {
        let commits = parse_line_history(LINE_LOG);
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].short, "5e4e31b");
        assert_eq!(commits[0].author, "Bob");
        assert_eq!(commits[0].date, "2026-10-16");
        assert_eq!(commits[0].subject, "change c");
        assert!(commits[0].patch.starts_with("diff --git a/f.txt"));
        assert!(commits[0].patch.contains("+C"));
        assert!(
            !commits[0].patch.contains("368d2af"),
            "a patch must not run into the next record"
        );
        assert_eq!(commits[1].hash, "368d2af2c147da1fa6dd8b0d0e5f87605a85c6f4");
        assert!(commits[1].patch.ends_with("+B\n c"));
        assert!(commits[2].patch.contains("--- /dev/null"));
        assert!(parse_line_history("").is_empty());
    }

    #[test]
    fn line_history_lists_commits_touching_the_lines_newest_first() {
        let git = repo_with_head("line-history");
        commit_file(&git, "f.txt", "a\nb\nc\nd\n", "first");
        commit_file(&git, "f.txt", "a\nB\nc\nd\n", "change b");
        commit_file(&git, "f.txt", "a\nB\nC\nd\n", "change c");
        commit_file(&git, "other.txt", "x\n", "unrelated");
        let commits = git.line_history("f.txt", 2, 3).unwrap();
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["change c", "change b", "first"]);
        assert!(commits[0].patch.contains("+C"));
        assert!(commits[1].patch.contains("+B"));
        // Lines past the end of the committed file are a git error, not a list.
        assert!(git.line_history("f.txt", 9, 9).is_err());
        let _ = std::fs::remove_dir_all(&git.root);
    }
}
