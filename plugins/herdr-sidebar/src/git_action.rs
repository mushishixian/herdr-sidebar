//! Asynchronous Git operations (Pull, Push and AI Commit) with progress notifications.
//!
//! Invoked as plugin actions (e.g. `ctrl+alt+p` -> `git-pull`, `ctrl+alt+k` -> `git-push`,
//! `git-commit-ai` -> `commit`, `ctrl+alt+z` -> `git-undo-commit`). Resolves the focused
//! pane's repository and performs git pull/push. AI Commit does not commit here: it hands the
//! repository to the tab's Source Control sidebar (see [`crate::ai_commit`]), which generates
//! the message into its Message box for the user to edit and commit. Undo is instant: it runs
//! [`Git::undo_last_commit`] right away (`reset --soft HEAD~1`) and reports the hash to redo it.
//! Live toast notifications:
//! 1. Initial notification when started (branch name & repo).
//! 2. Live progress notification updates (throttled) as git reports progress.
//! 3. Completion notification with human-readable summary or error.

use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::git::Git;
use crate::shortcut_tip;

pub fn run(op: &str) -> io::Result<()> {
    if op != "pull" && op != "push" && op != "commit" && op != "undo" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected 'pull', 'push', 'commit' or 'undo'",
        ));
    }

    let op_label = match op {
        "pull" => "Git Pull",
        "push" => "Git Push",
        "undo" => "Undo Commit",
        _ => "AI Commit",
    };
    // Key reminder on the FIRST notification only (plugin-action runs alone).
    let action_id = match op {
        "pull" => "git-pull",
        "push" => "git-push",
        "undo" => "git-undo-commit",
        _ => "git-commit-ai",
    };
    let tip = shortcut_tip::suffix_for(action_id);

    let cwd = match focused_cwd() {
        Some(d) => d,
        None => {
            notify(
                &format!("{op_label}{tip}"),
                "Could not determine working directory.",
            );
            return Ok(());
        }
    };

    let git = match Git::discover(&cwd) {
        Ok(git) => git,
        Err(_) => {
            let dir_name = cwd.file_name().and_then(|n| n.to_str()).unwrap_or("");
            notify(
                &format!("{op_label}{tip}"),
                &format!("Not inside a git repository ({dir_name})."),
            );
            return Ok(());
        }
    };

    let repo_name = git.name();
    if op == "commit" {
        request_ai_commit(&git, &repo_name, &tip);
        return Ok(());
    }
    if op == "undo" {
        undo_commit(&git, &repo_name, &tip);
        return Ok(());
    }
    let branch = git.current_branch().unwrap_or_else(|| "HEAD".to_string());

    // Single-flight lock per repository to prevent concurrent conflicting operations
    let lock_dir = crate::rundir::dir("git_action");
    let _ = crate::rundir::ensure_private(&lock_dir);
    let repo_hash = {
        let mut s = std::collections::hash_map::DefaultHasher::new();
        git.root().hash(&mut s);
        s.finish()
    };
    let lock_path = lock_dir.join(format!("lock-{repo_hash:016x}"));
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    if lock_file.try_lock().is_err() {
        notify(
            &format!("{op_label} [{repo_name}]{tip}"),
            "Another git operation is already running for this repository.",
        );
        return Ok(());
    }

    // 1. Initial notification
    // A push to a branch that has no upstream PUBLISHES it (`--set-upstream`):
    // plain `git push` refuses with "no upstream branch", which left a new
    // branch unpushable from any entry point. Same plan the panel uses.
    let push_extra = if op == "push" {
        match git.push_plan() {
            Ok((extra, _)) => extra,
            Err(e) => {
                notify(&format!("{op_label}{tip}"), &e);
                return Ok(());
            }
        }
    } else {
        Vec::new()
    };
    let start_msg = if op == "pull" {
        format!("Pulling {branch} from remote…")
    } else if push_extra.is_empty() {
        format!("Pushing {branch} to remote…")
    } else {
        format!("Publishing {branch} to remote…")
    };
    notify(&format!("{op_label} [{repo_name}]{tip}"), &start_msg);

    // 2. Spawn git command with piped stdout and stderr
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg("color.ui=false")
        .arg(op)
        .arg("--progress")
        .current_dir(git.root())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Pulls MERGE, like every other pull this plugin runs: a machine-wide
    // `pull.rebase=true` would fail on a dirty index and rewrite history
    // unasked (user-requested).
    if op == "pull" {
        cmd.arg("--no-rebase");
    }
    // Publishing: `--set-upstream <remote> <branch>` (empty for a plain push).
    for arg in &push_extra {
        cmd.arg(arg);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let mut child = cmd.spawn()?;

    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("failed to capture stderr"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture stdout"))?;

    let (tx, rx) = mpsc::channel();
    let stderr_handle = thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut all_bytes = Vec::new();
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            all_bytes.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = tx.send(text);
        }
        all_bytes
    });

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    // 3. Monitor live progress output
    let mut last_notify = Instant::now();
    let mut last_stage = String::new();

    while let Ok(chunk) = rx.recv() {
        for line in chunk.split(['\r', '\n']) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let stage = match () {
                _ if trimmed.contains("Counting objects:") => "Counting objects",
                _ if trimmed.contains("Compressing objects:") => "Compressing objects",
                _ if trimmed.contains("Receiving objects:") => "Receiving objects",
                _ if trimmed.contains("Resolving deltas:") => "Resolving deltas",
                _ if trimmed.contains("Writing objects:") => "Writing objects",
                _ => "",
            };

            let elapsed = last_notify.elapsed();
            if !stage.is_empty()
                && ((stage != last_stage && elapsed >= Duration::from_millis(1000))
                    || elapsed >= Duration::from_millis(2000))
            {
                last_stage = stage.to_string();
                last_notify = Instant::now();
                let detail = trimmed.strip_prefix("remote: ").unwrap_or(trimmed);
                notify(&format!("{op_label} [{repo_name}]"), detail);
            }
        }
    }

    let status = child.wait()?;
    let stderr_bytes = stderr_handle
        .join()
        .map_err(|_| io::Error::other("stderr thread panic"))?;
    let stdout_bytes = stdout_handle
        .join()
        .map_err(|_| io::Error::other("stdout thread panic"))?;

    let stdout_str = String::from_utf8_lossy(&stdout_bytes);
    let stderr_str = String::from_utf8_lossy(&stderr_bytes);

    // 4. Final notification
    if status.success() {
        let summary = summarize_success(op, &stdout_str, &stderr_str);
        notify(&format!("{op_label} Succeeded [{repo_name}]"), &summary);
    } else {
        let error = summarize_error(&stderr_str, &stdout_str);
        notify(&format!("{op_label} Failed [{repo_name}]"), &error);
    }

    // Explicitly drop lock_file to keep the lock held until operation finishes
    drop(lock_file);

    Ok(())
}

/// Undo the last commit now (no confirmation: the toast names the hash and the
/// `git reset --soft <hash>` that redoes it).
fn undo_commit(git: &Git, repo_name: &str, tip: &str) {
    let title = format!("Undo Commit [{repo_name}]{tip}");
    let body = match git.undo_last_commit() {
        Ok(undone) => {
            let mut body = format!(
                "Undid {} \"{}\" — its changes are staged. Redo: git reset --soft {}",
                undone.hash, undone.subject, undone.hash
            );
            if undone.pushed {
                body.push_str(" It was already pushed.");
            }
            body
        }
        Err(error) => error,
    };
    notify(&title, &body);
}

/// Hand the repository to this tab's Source Control sidebar: it stages, asks
/// OpenCommit for a message, fills the Message box and commits it shortly after.
fn request_ai_commit(git: &Git, repo_name: &str, tip: &str) {
    let title = format!("AI Commit [{repo_name}]");
    let fail_title = format!("AI Commit Failed [{repo_name}]");
    let Some(tab) = target_tab() else {
        notify(
            &format!("{fail_title}{tip}"),
            "Could not find this pane's tab.",
        );
        return;
    };
    if let Err(e) = crate::ai_commit::write_request(&tab, git.root()) {
        notify(&format!("{fail_title}{tip}"), &e.to_string());
        return;
    }
    // Activating the view may focus its pane (and docking a NEW pane always
    // does). The request is a file on disk and `take_ai_request` has no focus
    // gate, so an unfocused sidebar still does the work — put the user back
    // where they were instead of yanking them into Source Control mid-edit.
    let before = focused_pane_id();
    let target = crate::ensure::Mode::Activate(crate::ensure::Target::SourceControl);
    if let Err(e) = crate::ensure::run(target) {
        notify(&format!("{fail_title}{tip}"), &e.to_string());
        return;
    }
    restore_focus(before);
    notify(
        &format!("{title}{tip}"),
        "Generating the message in Source Control…",
    );
}

/// The server's focused pane, or `None` when the snapshot is unavailable.
fn focused_pane_id() -> Option<String> {
    let list = crate::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    let pane = crate::launch::server_focused_pane_id(&list);
    (!pane.is_empty()).then_some(pane)
}

/// Which pane to re-focus after a quiet activation, if any.
///
/// Focus is only given back when the activation itself took it: the user may
/// have moved on in the meantime, and a pane that is not ours must never be
/// overridden. Pure, so the decision is testable without a herdr server.
pub fn focus_to_restore(
    before: Option<&str>,
    now: &str,
    now_is_sidebar: bool,
) -> Option<String> {
    let before = before.filter(|pane| !pane.is_empty())?;
    if now.is_empty() || now == before || !now_is_sidebar {
        return None;
    }
    Some(before.to_string())
}

/// Give focus back to `before` when our activation stole it.
fn restore_focus(before: Option<String>) {
    let Ok(list) = crate::ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    let now = crate::launch::server_focused_pane_id(&list);
    let Some(pane) =
        focus_to_restore(before.as_deref(), &now, crate::launch::pane_has_token(&list, &now))
    else {
        return;
    };
    let _ = crate::ipc::call_text("pane.focus", serde_json::json!({ "pane_id": pane }));
}

/// The tab the commit belongs to: the action's own tab, else the focused pane's.
fn target_tab() -> Option<String> {
    if let Some(tab) = std::env::var("HERDR_TAB_ID").ok().filter(|t| !t.is_empty()) {
        return Some(tab);
    }
    let list = crate::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    let pane = crate::launch::server_focused_pane_id(&list);
    let tab = crate::launch::tab_of(&list, &pane);
    (!tab.is_empty()).then_some(tab)
}

/// Remove CSI (`ESC [ … final`) and OSC (`ESC ] … BEL` or `ESC ] … ESC \`) sequences.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Turn OpenCommit's output into one line for a toast.
pub fn summarize_oco_failure(stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");
    if combined.contains("Welcome to OpenCommit") || combined.contains("ERR_TTY_INIT_FAILED") {
        return "OpenCommit is not configured. Run `oco setup` in a terminal to choose an AI provider. Changes are left staged.".to_string();
    }
    if combined.contains("No changes detected") {
        return "Nothing to commit.".to_string();
    }

    let cleaned = strip_ansi(&combined);
    let last = cleaned
        .lines()
        .map(|line| {
            line.trim_start_matches(|c: char| c.is_whitespace() || "│┌└├◇◆●○✖".contains(c))
                .trim_end()
        })
        .rfind(|line| !line.is_empty());
    match last {
        Some(line) => format!("{line} Changes are left staged."),
        None => "OpenCommit failed. Changes are left staged.".to_string(),
    }
}

fn focused_cwd() -> Option<PathBuf> {
    let pane_id = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(|| {
            let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
            let context: serde_json::Value = serde_json::from_str(&context).ok()?;
            ["focused_pane_id", "pane_id"]
                .iter()
                .find_map(|key| context.get(*key)?.as_str().map(str::to_string))
        })
        .or_else(|| {
            let list = crate::ipc::call_text("pane.list", serde_json::json!({})).ok()?;
            let id = crate::launch::server_focused_pane_id(&list);
            if id.is_empty() { None } else { Some(id) }
        });

    let cwd = pane_id.and_then(|id| {
        let reply = crate::ipc::call_text("pane.get", serde_json::json!({ "pane_id": id })).ok()?;
        let reply: serde_json::Value =
            serde_json::from_str(crate::launch::strip_bom(&reply)).ok()?;
        let pane = &reply["result"]["pane"];
        pane["foreground_cwd"]
            .as_str()
            .or(pane["cwd"].as_str())
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
    });

    cwd.or_else(|| std::env::current_dir().ok())
}

pub fn notify(title: &str, body: &str) {
    let _ = crate::ipc::call_text(
        "notification.show",
        serde_json::json!({ "title": title, "body": body }),
    );
}

pub fn summarize_success(op: &str, stdout: &str, stderr: &str) -> String {
    if op == "pull" {
        if stdout.contains("Already up to date") || stderr.contains("Already up to date") {
            return "Already up to date.".to_string();
        }
        let mut changes = String::new();
        let mut is_fast_forward = false;
        for line in stdout.lines().chain(stderr.lines()) {
            let trimmed = line.trim();
            if trimmed.starts_with("Fast-forward") {
                is_fast_forward = true;
            }
            if trimmed.contains("files changed") || trimmed.contains("file changed") {
                changes = trimmed.to_string();
            }
        }
        if !changes.is_empty() {
            if is_fast_forward {
                return format!("Fast-forward: {changes}");
            }
            return changes;
        }
        stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("Pull completed successfully.")
            .trim()
            .to_string()
    } else {
        if stderr.contains("Everything up-to-date") || stdout.contains("Everything up-to-date") {
            return "Everything up-to-date.".to_string();
        }
        for line in stderr.lines().chain(stdout.lines()) {
            let trimmed = line.trim();
            if trimmed.contains("->") && !trimmed.starts_with("To ") {
                return trimmed.to_string();
            }
        }
        "Pushed to remote successfully.".to_string()
    }
}

pub fn summarize_error(stderr: &str, stdout: &str) -> String {
    let mut err_lines = Vec::new();
    for line in stderr.lines().chain(stdout.lines()) {
        let trimmed = line.trim();
        if (trimmed.starts_with("fatal:")
            || trimmed.starts_with("error:")
            || trimmed.starts_with("CONFLICT"))
            && !err_lines.contains(&trimmed)
        {
            err_lines.push(trimmed);
        }
    }
    if !err_lines.is_empty() {
        return err_lines.join(" | ");
    }
    stderr
        .lines()
        .chain(stdout.lines())
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Operation failed.")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_summarize_up_to_date() {
        let stdout = "Already up to date.\n";
        assert_eq!(summarize_success("pull", stdout, ""), "Already up to date.");
    }

    #[test]
    fn pull_summarize_fast_forward() {
        let stdout = "Updating 01dea4e..22756b9\nFast-forward\n src/main.rs | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n";
        assert_eq!(
            summarize_success("pull", stdout, ""),
            "Fast-forward: 1 file changed, 1 insertion(+), 1 deletion(-)"
        );
    }

    #[test]
    fn push_summarize_everything_up_to_date() {
        let stderr = "Everything up-to-date\n";
        assert_eq!(
            summarize_success("push", "", stderr),
            "Everything up-to-date."
        );
    }

    #[test]
    fn push_summarize_branch_update() {
        let stderr = "To github.com:ogulcancelik/herdr.git\n   01dea4e..22756b9  fresh-inline-editor -> fresh-inline-editor\n";
        assert_eq!(
            summarize_success("push", "", stderr),
            "01dea4e..22756b9  fresh-inline-editor -> fresh-inline-editor"
        );
    }

    #[test]
    fn strip_ansi_removes_csi_color_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m text"), "red text");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        assert_eq!(
            strip_ansi("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{1b}\\"),
            "link"
        );
    }

    #[test]
    fn oco_unconfigured_is_explained() {
        let stdout =
            "┌   Welcome to OpenCommit! \nnode:tty:97\n    throw new ERR_TTY_INIT_FAILED(ctx);\n";
        assert_eq!(
            summarize_oco_failure(stdout, ""),
            "OpenCommit is not configured. Run `oco setup` in a terminal to choose an AI provider. Changes are left staged."
        );
    }

    #[test]
    fn ai_commit_gives_focus_back_only_when_the_activation_took_it() {
        // The activation focused our sidebar pane: put the user back.
        assert_eq!(
            focus_to_restore(Some("w1:p1"), "w1:p9", true),
            Some("w1:p1".to_string())
        );
        // Focus never moved (the sidebar was already focused, or the view was).
        assert_eq!(focus_to_restore(Some("w1:p1"), "w1:p1", true), None);
        // Nothing to restore / no snapshot.
        assert_eq!(focus_to_restore(None, "w1:p9", true), None);
        assert_eq!(focus_to_restore(Some(""), "w1:p9", true), None);
        assert_eq!(focus_to_restore(Some("w1:p1"), "", true), None);
        // The user moved to a pane that is not ours: never override that.
        assert_eq!(focus_to_restore(Some("w1:p1"), "w1:p5", false), None);
    }

    #[test]
    fn oco_no_changes_is_nothing_to_commit() {
        assert_eq!(
            summarize_oco_failure("No changes detected\n", ""),
            "Nothing to commit."
        );
    }

    #[test]
    fn oco_generic_error_uses_last_meaningful_line() {
        let stderr = "│\n└  ✖ 401 Incorrect API key provided\n";
        assert_eq!(
            summarize_oco_failure("", stderr),
            "401 Incorrect API key provided Changes are left staged."
        );
    }

    #[test]
    fn oco_empty_output_has_fallback() {
        assert_eq!(
            summarize_oco_failure("│\n", ""),
            "OpenCommit failed. Changes are left staged."
        );
    }

    #[test]
    fn summarize_error_picks_fatal_or_error() {
        let stderr = "warning: redirecting to https://github.com/...\nfatal: unable to access 'https://github.com/...': Could not resolve host\n";
        assert_eq!(
            summarize_error(stderr, ""),
            "fatal: unable to access 'https://github.com/...': Could not resolve host"
        );
    }
}
