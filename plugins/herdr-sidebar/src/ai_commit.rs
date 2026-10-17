//! AI commit handoff between the `git-commit-ai` action and the Source Control
//! sidebar.
//!
//! The action process does not commit anything. It writes a per-tab request
//! file (`<rundir>/ai_commit/tab-<tab>.req`) and activates the tab's Source
//! Control view. The sidebar TUI claims the request, stages the repository,
//! asks OpenCommit for a message, fills the Message box and then commits
//! through its own ✓ Commit path.
//!
//! Message generation uses OpenCommit's `prepare-commit-msg` hook mode. oco
//! treats itself as the hook when `process.argv[1]` equals
//! `<git hooks path>/prepare-commit-msg`, writes the message into the file it
//! is given and never commits. The hooks path is pointed at a private temp
//! directory through `GIT_CONFIG_*` env. The hook must be a SYMLINK to the real
//! `cli.cjs`, because oco loads `tiktoken_bg.wasm` from next to itself.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::git_action::summarize_oco_failure;

/// A request older than this is discarded instead of committing something the
/// user no longer expects.
const REQUEST_MAX_AGE_SECS: u64 = 30;

/// How long OpenCommit may run before it is killed.
const OCO_TIMEOUT: Duration = Duration::from_secs(120);

const OPEN_COMMIT_MARKER: &str = "# ---------- [OpenCommit]";
const FAILED_HEADER: &str = "# Failed to generate the commit message";
const CANCEL_LINE: &str = "# To cancel";
const INSTALL_HINT: &str =
    "OpenCommit (oco) not found on PATH. Install it with: npm install -g opencommit";

/// Map a tab id onto a file-name-safe token.
fn sanitize(tab_id: &str) -> String {
    tab_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn request_file_name(tab_id: &str) -> String {
    format!("tab-{}.req", sanitize(tab_id))
}

/// Where the request for `tab_id` lives.
pub fn request_path(tab_id: &str) -> PathBuf {
    crate::rundir::dir("ai_commit").join(request_file_name(tab_id))
}

/// Ask the sidebar of `tab_id` to generate and commit for `repo_root`.
pub fn write_request(tab_id: &str, repo_root: &Path) -> io::Result<()> {
    let path = request_path(tab_id);
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("ai_commit directory unavailable"))?;
    crate::rundir::ensure_private(dir)?;
    let tmp = dir.join(format!(
        "{}.tmp-{}",
        request_file_name(tab_id),
        std::process::id()
    ));
    let body = format!("{}\n{}\n", repo_root.display(), crate::state::unix_now());
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// Claim the pending request for `tab_id`, if any. The file is renamed away
/// before it is read, so two pollers can never both act on one request.
/// Returns the repository root when the request is fresh.
pub fn take_request(tab_id: &str) -> Option<PathBuf> {
    let path = request_path(tab_id);
    let claimed = path.with_file_name(format!(
        "{}.taken-{}",
        request_file_name(tab_id),
        std::process::id()
    ));
    std::fs::rename(&path, &claimed).ok()?;
    let text = std::fs::read_to_string(&claimed);
    let _ = std::fs::remove_file(&claimed);
    parse_request(&text.ok()?, crate::state::unix_now())
}

/// Pure half of [`take_request`]: `"<root>\n<unix secs>\n"` → root when fresh.
fn parse_request(text: &str, now: u64) -> Option<PathBuf> {
    let mut lines = text.lines();
    let root = lines.next()?;
    let stamp: u64 = lines.next()?.trim().parse().ok()?;
    if root.is_empty() {
        return None;
    }
    (now.saturating_sub(stamp) <= REQUEST_MAX_AGE_SECS).then(|| PathBuf::from(root))
}

/// Generate a commit message for the STAGED changes in `root` without
/// committing. Err carries a one-line, user-facing reason.
pub fn generate(root: &Path) -> Result<String, String> {
    let cli = oco_cli_script()?;
    let base = crate::rundir::dir("ai_commit");
    crate::rundir::ensure_private(&base).map_err(|e| format!("ai_commit directory: {e}"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let work = base.join(format!("gen-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&work).map_err(|e| format!("ai_commit temp directory: {e}"))?;
    let result = run_hook(&work, &cli, root);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn run_hook(work: &Path, cli: &Path, root: &Path) -> Result<String, String> {
    let hooks = work.join("hooks");
    std::fs::create_dir(&hooks).map_err(|e| format!("ai_commit hooks directory: {e}"))?;
    let hook = hooks.join("prepare-commit-msg");
    link_script(cli, &hook)?;
    let msg = work.join("msg");
    std::fs::write(&msg, "").map_err(|e| format!("ai_commit message file: {e}"))?;

    let mut cmd = Command::new("node");
    cmd.arg(&hook)
        .arg(&msg)
        .current_dir(root)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", &hooks)
        .env("OCO_GITPUSH", "false")
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err("Node.js (node) not found on PATH; OpenCommit needs it.".to_string());
        }
        Err(e) => return Err(format!("could not start OpenCommit: {e}")),
    };
    let stdout_handle = child.stdout.take().map(drain);
    let stderr_handle = child.stderr.take().map(drain);

    let deadline = Instant::now() + OCO_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The readers are detached: a grandchild may still hold the pipes.
                    return Err("OpenCommit timed out after 120s.".to_string());
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let content = std::fs::read_to_string(&msg).unwrap_or_default();
    match parse_hook_message(&content)? {
        Some(message) => Ok(message),
        None => {
            let out = String::from_utf8_lossy(&stdout);
            let err = String::from_utf8_lossy(&stderr);
            if out.trim().is_empty() && err.trim().is_empty() {
                Err("OpenCommit produced no message".to_string())
            } else {
                Err(summarize_oco_failure(&out, &err))
            }
        }
    }
}

/// Read what oco wrote into the hook's message file.
///
/// Commented output is `# <message>` followed by the OpenCommit banner; the
/// uncommented form has the message and no banner. A failure carries the
/// `Failed to generate` header, with the error text in the lines after the
/// `To cancel` line. Empty output (nothing staged, or no API key) is `Ok(None)`.
pub fn parse_hook_message(content: &str) -> Result<Option<String>, String> {
    let lines: Vec<&str> = content.lines().collect();
    let marker_at = lines.iter().position(|l| l.starts_with(OPEN_COMMIT_MARKER));
    let (body, banner) = match marker_at {
        Some(i) => (&lines[..i], &lines[i..]),
        None => (&lines[..], &lines[lines.len()..]),
    };

    if banner.iter().any(|l| l.starts_with(FAILED_HEADER)) {
        let after_cancel = banner
            .iter()
            .position(|l| l.starts_with(CANCEL_LINE))
            .map(|i| &banner[i + 1..])
            .unwrap_or(&[]);
        let errors: Vec<&str> = after_cancel
            .iter()
            .filter_map(|l| l.strip_prefix('#'))
            .map(|s| s.strip_prefix(' ').unwrap_or(s).trim())
            .filter(|s| !s.is_empty())
            .collect();
        return Err(if errors.is_empty() {
            "Failed to generate the commit message".to_string()
        } else {
            errors.join(" ")
        });
    }

    let mut text: Vec<String> = body.iter().map(|l| l.to_string()).collect();
    if let Some(first) = text.iter_mut().find(|l| !l.trim().is_empty())
        && let Some(rest) = first.strip_prefix("# ")
    {
        *first = rest.to_string();
    }
    let joined = text.join("\n");
    let trimmed = joined.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The `cli.cjs` behind the `oco` on PATH (npm's symlink resolves to it).
#[cfg(not(windows))]
fn oco_cli_script() -> Result<PathBuf, String> {
    let oco = find_on_path("oco").ok_or_else(|| INSTALL_HINT.to_string())?;
    let real = std::fs::canonicalize(&oco)
        .map_err(|e| format!("could not resolve {}: {e}", oco.display()))?;
    match real.extension().and_then(|e| e.to_str()) {
        Some("cjs" | "js") => Ok(real),
        _ => Err(format!(
            "unsupported OpenCommit install: {} (expected an npm install with a .cjs/.js CLI)",
            real.display()
        )),
    }
}

/// npm installs `oco.cmd` next to `node_modules`. Best effort, untested live.
#[cfg(windows)]
fn oco_cli_script() -> Result<PathBuf, String> {
    let shim = find_on_path("oco.cmd").ok_or_else(|| INSTALL_HINT.to_string())?;
    let cli = shim
        .parent()
        .unwrap_or(Path::new(""))
        .join("node_modules")
        .join("opencommit")
        .join("out")
        .join("cli.cjs");
    if cli.is_file() {
        Ok(cli)
    } else {
        Err(format!(
            "unsupported OpenCommit install: {} not found next to {}",
            cli.display(),
            shim.display()
        ))
    }
}

#[cfg(unix)]
fn link_script(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| format!("could not link the OpenCommit hook: {e}"))
}

#[cfg(windows)]
fn link_script(target: &Path, link: &Path) -> Result<(), String> {
    std::os::windows::fs::symlink_file(target, link).map_err(|e| {
        format!(
            "could not create a symlink for the OpenCommit hook (Windows needs developer mode or symlink permission): {e}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commented_single_line_message() {
        let content = "# feat(x): add thing\n\n# ---------- [OpenCommit] ---------- #\n# Remove the # above to use the message\n# To cancel the commit, delete everything\n\n";
        assert_eq!(
            parse_hook_message(content),
            Ok(Some("feat(x): add thing".to_string()))
        );
    }

    #[test]
    fn commented_message_keeps_body() {
        let content = "# fix: handle empty input\nAdds a guard for empty input.\n\n# ---------- [OpenCommit] ---------- #\n# Remove the # above\n# To cancel the commit\n\n";
        assert_eq!(
            parse_hook_message(content),
            Ok(Some(
                "fix: handle empty input\nAdds a guard for empty input.".to_string()
            ))
        );
    }

    #[test]
    fn uncommented_message() {
        let content = "feat(x): subject\n\nbody line\n\n";
        assert_eq!(
            parse_hook_message(content),
            Ok(Some("feat(x): subject\n\nbody line".to_string()))
        );
    }

    #[test]
    fn failure_with_error_lines() {
        let content = "\n\n# ---------- [OpenCommit] ---------- #\n# Failed to generate the commit message.\n# To cancel the commit, delete everything\n# 401 Incorrect API key provided\n# check your key\n";
        assert_eq!(
            parse_hook_message(content),
            Err("401 Incorrect API key provided check your key".to_string())
        );
    }

    #[test]
    fn failure_without_error_lines() {
        let content = "\n\n# ---------- [OpenCommit] ---------- #\n# Failed to generate the commit message.\n# To cancel the commit, delete everything\n";
        assert_eq!(
            parse_hook_message(content),
            Err("Failed to generate the commit message".to_string())
        );
    }

    #[test]
    fn empty_output_is_none() {
        assert_eq!(parse_hook_message(""), Ok(None));
        assert_eq!(parse_hook_message("  \n\n"), Ok(None));
    }

    #[test]
    fn no_marker_uses_whole_text() {
        assert_eq!(
            parse_hook_message("# docs: update readme\n"),
            Ok(Some("docs: update readme".to_string()))
        );
    }

    #[test]
    fn request_round_trip_is_fresh() {
        let text = "/repo/one\n1000\n";
        assert_eq!(parse_request(text, 1020), Some(PathBuf::from("/repo/one")));
    }

    #[test]
    fn stale_or_malformed_request_is_dropped() {
        assert_eq!(parse_request("/repo/one\n1000\n", 1031), None);
        assert_eq!(parse_request("/repo/one\nnot-a-number\n", 1000), None);
        assert_eq!(parse_request("\n1000\n", 1000), None);
    }

    #[test]
    fn tab_ids_are_sanitized_for_file_names() {
        assert_eq!(sanitize("w1:2/3"), "w1_2_3");
        assert_eq!(sanitize("tab-7_a"), "tab-7_a");
    }

    /// Calls the configured AI provider and must not commit. Run it by hand with
    /// `HS_AI_COMMIT_E2E_REPO=<repo with staged changes> cargo test -- --ignored e2e_generate`.
    #[test]
    #[ignore = "calls the configured OpenCommit provider"]
    fn e2e_generate_does_not_commit() {
        let repo = std::env::var("HS_AI_COMMIT_E2E_REPO").expect("set HS_AI_COMMIT_E2E_REPO");
        let root = PathBuf::from(repo);
        let head = |root: &Path| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["rev-parse", "HEAD"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        let before = head(&root);
        let result = generate(&root);
        println!("GENERATE RESULT: {result:?}");
        assert_eq!(head(&root), before, "generate must not commit");
        assert!(result.is_ok(), "generate failed: {result:?}");
    }
}
