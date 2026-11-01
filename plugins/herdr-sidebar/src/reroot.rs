//! "Change Folder" across a project boundary → rebuild the whole herdr space.
//!
//! A manual change to a folder under a DIFFERENT project re-points more than
//! the sidebar: the space's editor, terminals and agents still run in the old
//! project, and herdr derives a workspace's rail identity (name, branch, git
//! status) from the live cwd of its FIRST tab's root pane
//! (`Workspace::resolved_identity_cwd_from`), so the rail keeps describing the
//! old project. The dotfiles IDE layer owns that layout, so the sidebar only
//! hands over: after a y/N confirm it spawns that plugin's
//! `bin/ide-reroot <dir> <workspace_id> <tab_id>`, which closes the stale
//! panes and rebuilds via `herdr-ide <dir>`. The rail then follows on its own
//! (identity cwd moves with the rebuilt panes; `$agents`/`$pad` follow on the
//! next heartbeat); the sidebar reports nothing extra.
//!
//! The IDE layer is optional per machine: everything here degrades to "no
//! space refresh" when `dotfiles-ide` is not installed, and the sidebar's own
//! re-root (already applied before this runs) is never affected.

use std::path::{Path, PathBuf};

/// The project root that owns `dir`: the nearest enclosing `.git` — a
/// directory, or a worktree's `.git` FILE — walking up. No git process, the
/// same convention as `Git::is_initialized`. Without a repository the
/// directory itself is the root. `dir` must be absolute (callers pass the
/// canonicalized sidebar root).
pub fn project_root_of(dir: &Path) -> PathBuf {
    let mut current = dir;
    loop {
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return dir.to_path_buf(),
        }
    }
}

/// A manual folder change that crosses a project boundary refreshes the whole
/// space; a change within the same project (e.g. into a sibling folder of the
/// repo the IDE is already built around) only re-roots the sidebar.
pub fn should_refresh_space(old_root: &Path, new_root: &Path) -> bool {
    project_root_of(old_root) != project_root_of(new_root)
}

/// `plugin_root` of the `dotfiles-ide` plugin from a
/// `herdr plugin list --json` reply. `None` when the IDE layer is not
/// installed (or the reply is not what we expect).
fn ide_plugin_root(list_json: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(list_json).ok()?;
    let plugins = value.get("result")?.get("plugins")?.as_array()?;
    for plugin in plugins {
        if plugin.get("plugin_id").and_then(|id| id.as_str()) == Some("dotfiles-ide") {
            let root = plugin.get("plugin_root")?.as_str()?;
            if !root.is_empty() {
                return Some(PathBuf::from(root));
            }
        }
    }
    None
}

/// The reroot hand-over command: `bash <script> <dir> <workspace> <tab>`.
fn reroot_command(
    script: &Path,
    root: &Path,
    workspace_id: &str,
    tab_id: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new("bash");
    command.arg(script).arg(root).arg(workspace_id).arg(tab_id);
    command
}

/// Ask the dotfiles-ide plugin to rebuild this space in `root`. Runs the
/// plugin lookup and the script to completion — callers invoke this from a
/// worker thread. False when the IDE layer is not installed, the pane has no
/// workspace/tab identity, or the script could not be run; none of these
/// affect the sidebar's own re-root, which has already happened.
pub fn spawn_refresh(root: &Path) -> bool {
    #[cfg(unix)]
    {
        unix_spawn_refresh(root)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        false
    }
}

#[cfg(unix)]
fn unix_spawn_refresh(root: &Path) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let workspace_id = std::env::var("HERDR_WORKSPACE_ID").unwrap_or_default();
    let tab_id = std::env::var("HERDR_TAB_ID").unwrap_or_default();
    if workspace_id.is_empty() || tab_id.is_empty() {
        return false;
    }
    // HERDR_BIN_PATH is injected for actions/hooks but not panes (AGENTS.md):
    // a sidebar TUI falls back to the CLI on PATH.
    let herdr = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let Ok(list) = std::process::Command::new(herdr)
        .arg("plugin")
        .arg("list")
        .arg("--json")
        .output()
    else {
        return false;
    };
    if !list.status.success() {
        return false;
    }
    let Some(plugin_root) = ide_plugin_root(&String::from_utf8_lossy(&list.stdout)) else {
        return false;
    };
    let script = plugin_root.join("bin").join("ide-reroot");
    if !script.is_file() {
        return false;
    }

    // The script closes and re-splits panes; its output is the only trace of
    // what happened, so it lands in a private runtime log next to the other
    // scratch files.
    let log_dir = crate::rundir::dir("ide-reroot");
    if crate::rundir::ensure_private(&log_dir).is_err() {
        return false;
    }
    let Ok(log) = std::fs::File::create(log_dir.join("last.log")) else {
        return false;
    };
    let Ok(log_err) = log.try_clone() else {
        return false;
    };

    let mut command = reroot_command(&script, root, &workspace_id, &tab_id);
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // Our own process group, so a future cleanup can stop the whole chain —
    // the script spawns herdr CLI calls for every pane it closes.
    command.process_group(0);
    command.status().map(|status| status.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn project_root_walks_up_to_the_git_dir_or_file() {
        let base = std::env::temp_dir().join(format!(
            "herdr-reroot-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = base.join("repo");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(project_root_of(&nested), repo);
        // A worktree's `.git` FILE counts like a `.git` directory.
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(project_root_of(&nested), repo);

        // Outside any repository the directory itself is the root.
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_root_of(&plain), plain);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_change_across_project_roots_refreshes_the_space() {
        let base = std::env::temp_dir().join(format!(
            "herdr-reroot-cmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo_a = base.join("alpha");
        let repo_b = base.join("beta");
        std::fs::create_dir_all(repo_a.join("src")).unwrap();
        std::fs::create_dir_all(repo_b.join("src")).unwrap();
        std::fs::create_dir_all(repo_a.join(".git")).unwrap();
        std::fs::create_dir_all(repo_b.join(".git")).unwrap();

        // A subfolder of the same repo: sidebar-only re-root.
        assert!(!should_refresh_space(&repo_a, &repo_a.join("src")));
        // The same directory again: nothing to do.
        assert!(!should_refresh_space(&repo_a, &repo_a));
        // Another repo (or a plain folder): the space follows.
        assert!(should_refresh_space(&repo_a, &repo_b));
        assert!(should_refresh_space(&repo_a, &base));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ide_plugin_root_reads_the_plugin_list_reply() {
        let reply = r#"{"result":{"plugins":[
            {"plugin_id":"dotfiles-ide","plugin_root":"/ide/dir"},
            {"plugin_id":"herdr-sidebar","plugin_root":"/sb/dir"}
        ]}}"#;
        assert_eq!(
            ide_plugin_root(reply),
            Some(PathBuf::from("/ide/dir"))
        );
        // Missing plugin, broken JSON, or an empty root all mean "optional
        // feature absent", never an error.
        assert_eq!(ide_plugin_root(r#"{"result":{"plugins":[]}}"#), None);
        assert_eq!(ide_plugin_root("not json"), None);
        assert_eq!(
            ide_plugin_root(r#"{"result":{"plugins":[{"plugin_id":"dotfiles-ide","plugin_root":""}]}}"#),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn reroot_command_targets_the_script_with_dir_workspace_and_tab() {
        let command = reroot_command(
            Path::new("/ide/bin/ide-reroot"),
            Path::new("/new/root"),
            "wV",
            "wV:t1",
        );
        assert_eq!(command.get_program(), "bash");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["/ide/bin/ide-reroot", "/new/root", "wV", "wV:t1"]);
    }
}
