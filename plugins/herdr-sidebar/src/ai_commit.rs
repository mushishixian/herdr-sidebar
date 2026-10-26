//! AI commit handoff between the `git-commit-ai` action and the Source Control
//! sidebar.
//!
//! The action process does not commit anything. It writes a per-tab request
//! file (`<rundir>/ai_commit/tab-<tab>.req`) and activates the tab's Source
//! Control view. The sidebar TUI claims the request, stages the repository,
//! builds a curated copy of the staged diff, asks the AI for a message through
//! the OpenAI-compatible endpoint saved in `ai.json` (see [`crate::ai_client`]),
//! fills the Message box and then commits it through the ordinary ✓ Commit path
//! after a short review window.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ai_config::AiConfig;

/// A request older than this is discarded instead of committing something the
/// user no longer expects.
const REQUEST_MAX_AGE_SECS: u64 = 30;

/// How long the model may take to answer before the request is abandoned.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Patch bytes one file may contribute before the AI sees a shortened version.
/// A huge file would crowd out the rest of the change in the request.
const PER_FILE_BYTES: usize = 12 * 1024;

/// Patch bytes across all files the AI sees; the largest files are shortened
/// until the total fits.
const TOTAL_BYTES: usize = 48 * 1024;

/// Lines kept from the start of a shortened new file (and of a modified file
/// whose patch cannot be cut down to whole hunks).
const KEEP_LINES: usize = 40;

/// Most diff text sent to the model, in bytes. The rest is cut off with a
/// marker saying so.
const MAX_DIFF_BYTES: usize = 64 * 1024;

/// Path substrings left out of the diff the AI reads. Files matching these
/// never reach the model, so they cost nothing against the budgets.
const EXCLUDED: &[&str] = &[
    ".lock", "-lock.", ".svg", ".png", ".jpg", ".jpeg", ".webp", ".gif",
];

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

/// Ask the sidebar of `tab_id` to generate a message for `repo_root`.
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

/// A generated message, plus the files the AI saw only in shortened form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub message: String,
    /// `"<path> (shortened)"` or `"<path> (not shown)"`, one per curated file.
    /// The commit itself always contains the full change.
    pub summarised: Vec<String>,
}

/// Generate a commit message for the STAGED changes in `root` without
/// committing. Err carries a one-line, user-facing reason.
pub fn generate(root: &Path) -> Result<Generated, String> {
    let config = crate::ai_config::load();
    if !config.is_configured() {
        return Err(
            "AI commit is not set up: open ⚙ Settings → AI commit and enter an API key and model."
                .to_string(),
        );
    }
    let base = crate::rundir::dir("ai_commit");
    crate::rundir::ensure_private(&base).map_err(|e| format!("ai_commit directory: {e}"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let work = base.join(format!("gen-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&work).map_err(|e| format!("ai_commit temp directory: {e}"))?;
    let result = run_curated(&work, &config, root);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn run_curated(work: &Path, config: &AiConfig, root: &Path) -> Result<Generated, String> {
    let curated = curate(root, work);
    let mut diff = staged_diff(root, curated.index.as_deref())?;
    if diff.trim().is_empty() {
        // Everything staged is excluded (a lock file bump, images): the file
        // list is still worth describing.
        let stat = git(
            root,
            None,
            &["diff", "--cached", "--no-ext-diff", "--no-color", "--stat"],
            None,
        )?;
        diff = String::from_utf8_lossy(&stat).into_owned();
    }
    if diff.trim().is_empty() {
        return Err("no staged changes to describe".to_string());
    }
    let message = crate::ai_client::commit_message(
        config,
        &cap_diff(&diff, MAX_DIFF_BYTES),
        REQUEST_TIMEOUT,
    )?;
    Ok(Generated {
        message,
        summarised: curated.notes,
    })
}

/// The staged diff the AI reads, minus the lock and generated files named in
/// [`EXCLUDED`]. `index` selects the curated index when there is one.
fn staged_diff(root: &Path, index: Option<&Path>) -> Result<String, String> {
    let specs: Vec<String> = EXCLUDED
        .iter()
        .map(|s| format!(":(exclude)*{s}*"))
        .collect();
    let mut args = vec![
        "-c",
        "core.quotePath=false",
        "diff",
        "--cached",
        "--no-ext-diff",
        "--no-color",
        "-M",
        "--",
        ".",
    ];
    args.extend(specs.iter().map(String::as_str));
    let out = git(root, index, &args, None)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `text` cut to at most `max` bytes on a character boundary, with a marker
/// when anything was cut.
fn cap_diff(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[diff truncated]", &text[..end])
}

/// What the AI should read instead of the real index.
struct Curated {
    /// Temp index under the work dir, or `None` to read the real index.
    index: Option<PathBuf>,
    notes: Vec<String>,
}

impl Curated {
    fn real() -> Self {
        Curated {
            index: None,
            notes: Vec::new(),
        }
    }
}

/// Build the curated index for the message request. Never fails the commit:
/// any problem falls back to the real index, the behaviour before curation.
fn curate(root: &Path, work: &Path) -> Curated {
    curate_index(root, work).unwrap_or_else(|_| Curated::real())
}

fn curate_index(root: &Path, work: &Path) -> Result<Curated, String> {
    if !work.is_absolute() {
        return Err("work directory is not absolute".to_string());
    }
    let patch = git(
        root,
        None,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-color",
            "-M",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ],
        None,
    )?;
    let files = split_patch(&String::from_utf8_lossy(&patch));
    let chosen = choose_shrinks(&files);
    if chosen.is_empty() {
        return Ok(Curated::real());
    }

    let real = git(root, None, &["rev-parse", "--git-path", "index"], None)?;
    let real = PathBuf::from(String::from_utf8_lossy(&real).trim());
    let real = if real.is_absolute() {
        real
    } else {
        root.join(real)
    };
    let temp = work.join("index");
    std::fs::copy(&real, &temp).map_err(|e| format!("copy index: {e}"))?;

    let mut shrunk: Vec<Shrunk> = Vec::with_capacity(chosen.len());
    for &i in &chosen {
        let file = &files[i];
        let shortened = match file.status {
            Status::Added => {
                stage_shortened(root, &temp, &file.path)?;
                true
            }
            Status::Modified if file.hunks.len() >= 2 => {
                reset_to_head(root, &temp, &file.path)?;
                match truncate_hunks(&file.header, &file.hunks, PER_FILE_BYTES) {
                    // A failed apply leaves the entry at HEAD: the file is hidden.
                    Some(patch) => git(
                        root,
                        Some(&temp),
                        &["apply", "--cached"],
                        Some(patch.as_bytes()),
                    )
                    .is_ok(),
                    None => false,
                }
            }
            Status::Renamed | Status::Copied => {
                // The new path is not in HEAD: drop it. A rename also puts the
                // old path back at its HEAD entry, so the pair disappears.
                remove_from_index(root, &temp, &file.path)?;
                if let (Status::Renamed, Some(old)) = (file.status, &file.old_path) {
                    reset_to_head(root, &temp, old)?;
                }
                false
            }
            Status::Modified | Status::Deleted => {
                reset_to_head(root, &temp, &file.path)?;
                false
            }
        };
        shrunk.push(Shrunk { file: i, shortened });
    }

    // The AI must never see an empty diff: the message would be a guess.
    if !has_diff(root, &temp)? {
        let pick = shrunk
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.shortened && files[s.file].status == Status::Modified)
            .max_by_key(|(_, s)| files[s.file].bytes)
            .map(|(k, _)| k);
        if let Some(k) = pick {
            stage_shortened(root, &temp, &files[shrunk[k].file].path)?;
            shrunk[k].shortened = true;
        }
        if !has_diff(root, &temp)? {
            // Nothing usable for the AI: it reads the real index, so nothing
            // was summarised and no note may claim otherwise.
            return Ok(Curated::real());
        }
    }
    Ok(Curated {
        index: Some(temp),
        notes: notes(&files, &shrunk),
    })
}

/// One file the curated index changed.
struct Shrunk {
    file: usize,
    /// `true`: the AI sees a shortened version. `false`: the file is hidden.
    shortened: bool,
}

fn notes(files: &[FilePatch], shrunk: &[Shrunk]) -> Vec<String> {
    shrunk
        .iter()
        .map(|s| {
            let what = if s.shortened {
                "shortened"
            } else {
                "not shown"
            };
            format!("{} ({what})", files[s.file].path)
        })
        .collect()
}

/// Stage a shortened copy of the real staged content of `path` into the temp
/// index. The blob is a loose object: harmless, `git gc` collects it.
fn stage_shortened(root: &Path, index: &Path, path: &str) -> Result<(), String> {
    let entry = git(root, None, &["ls-files", "-s", "--", path], None)?;
    let entry = String::from_utf8_lossy(&entry).into_owned();
    let mode = entry
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("{path} is not in the index"))?;
    let content = git(root, None, &["cat-file", "blob", &format!(":{path}")], None)?;
    let shortened = shorten_blob(&content, KEEP_LINES, PER_FILE_BYTES);
    let sha = git(
        root,
        None,
        &["hash-object", "-w", "--stdin"],
        Some(&shortened),
    )?;
    let sha = String::from_utf8_lossy(&sha).trim().to_string();
    let line = format!("{mode} {sha}\t{path}\n");
    git(
        root,
        Some(index),
        &["update-index", "--index-info"],
        Some(line.as_bytes()),
    )?;
    Ok(())
}

/// Put the HEAD entry of `path` back into the temp index (or drop it when HEAD
/// has none).
fn reset_to_head(root: &Path, index: &Path, path: &str) -> Result<(), String> {
    let listing = git(root, None, &["ls-tree", "HEAD", "--", path], None)?;
    if listing.is_empty() {
        remove_from_index(root, index, path)
    } else {
        git(
            root,
            Some(index),
            &["update-index", "--index-info"],
            Some(&listing),
        )?;
        Ok(())
    }
}

fn remove_from_index(root: &Path, index: &Path, path: &str) -> Result<(), String> {
    git(
        root,
        Some(index),
        &["update-index", "--force-remove", "--", path],
        None,
    )?;
    Ok(())
}

/// `true` when the temp index differs from HEAD.
fn has_diff(root: &Path, index: &Path) -> Result<bool, String> {
    let out = run_git(
        root,
        Some(index),
        &["diff", "--cached", "--quiet", "--no-ext-diff"],
        None,
    )?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err("git diff failed on the curated index".to_string()),
    }
}

/// Run git in `root` and return stdout; a non-zero exit is an error.
fn git(
    root: &Path,
    index: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let out = run_git(root, index, args, stdin)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run git with a null (or piped) stdin, no credential prompts and no
/// optional index locks. `index` selects an alternate `GIT_INDEX_FILE`.
fn run_git(
    root: &Path,
    index: Option<&Path>,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not run git: {e}"))?;
    let writer = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => {
            let bytes = bytes.to_vec();
            Some(thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        _ => None,
    };
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git did not finish: {e}"))?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(out)
}

/// One file of a `git diff --cached -M` patch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FilePatch {
    /// The new path (the deleted path for a deletion).
    path: String,
    /// The source path of a rename or copy.
    old_path: Option<String>,
    status: Status,
    binary: bool,
    /// A quoted path we do not decode. Such a file is never shrunk.
    unparsable: bool,
    /// Byte length of the whole per-file patch.
    bytes: usize,
    /// Everything before the first `@@`, including the `diff --git` line.
    header: String,
    /// Each hunk from its `@@` line up to the next one (or the end).
    hunks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

/// Split a full `git diff` patch into per-file patches.
fn split_patch(text: &str) -> Vec<FilePatch> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let end = starts.get(k + 1).copied().unwrap_or(text.len());
            parse_file(&text[start..end])
        })
        .collect()
}

fn parse_file(chunk: &str) -> FilePatch {
    let mut hunk_starts = Vec::new();
    let mut offset = 0;
    for line in chunk.split_inclusive('\n') {
        if line.starts_with("@@") {
            hunk_starts.push(offset);
        }
        offset += line.len();
    }
    let header_end = hunk_starts.first().copied().unwrap_or(chunk.len());
    let header = chunk[..header_end].to_string();
    let hunks = hunk_starts
        .iter()
        .enumerate()
        .map(|(k, &start)| {
            let end = hunk_starts.get(k + 1).copied().unwrap_or(chunk.len());
            chunk[start..end].to_string()
        })
        .collect();

    let mut status = Status::Modified;
    let mut binary = false;
    let mut rename_from = None;
    let mut rename_to = None;
    let mut minus = None;
    let mut plus = None;
    for line in header.lines() {
        if line.starts_with("new file mode") {
            status = Status::Added;
        } else if line.starts_with("deleted file mode") {
            status = Status::Deleted;
        } else if let Some(v) = line.strip_prefix("rename from ") {
            status = Status::Renamed;
            rename_from = Some(v);
        } else if let Some(v) = line.strip_prefix("rename to ") {
            status = Status::Renamed;
            rename_to = Some(v);
        } else if let Some(v) = line.strip_prefix("copy from ") {
            status = Status::Copied;
            rename_from = Some(v);
        } else if let Some(v) = line.strip_prefix("copy to ") {
            status = Status::Copied;
            rename_to = Some(v);
        } else if let Some(v) = line.strip_prefix("--- ") {
            minus = Some(v);
        } else if let Some(v) = line.strip_prefix("+++ ") {
            plus = Some(v);
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            binary = true;
        }
    }

    let git_line = header
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("diff --git "))
        .unwrap_or("");
    let quoted = git_line.starts_with('"')
        || [rename_from, rename_to, minus, plus]
            .iter()
            .flatten()
            .any(|v| v.starts_with('"'));

    let real = |v: Option<&str>| v.filter(|v| *v != "/dev/null").map(|v| side(v, ""));
    let path = rename_to
        .map(|v| side(v, ""))
        .or_else(|| real(plus).map(|v| side(&v, "b/")))
        .or_else(|| real(minus).map(|v| side(&v, "a/")))
        .or_else(|| path_from_git_line(git_line));
    let unparsable = quoted || path.is_none();
    let path = path.unwrap_or_else(|| git_line.to_string());

    FilePatch {
        path,
        old_path: rename_from.map(|v| side(v, "")),
        status,
        binary,
        unparsable,
        bytes: chunk.len(),
        header,
        hunks,
    }
}

/// A header path without its `a/`/`b/` prefix and without git's trailing tab.
fn side(value: &str, prefix: &str) -> String {
    let value = value.trim_end_matches('\t');
    value.strip_prefix(prefix).unwrap_or(value).to_string()
}

/// Fallback for a file with neither `+++` nor `rename to` (an empty or binary
/// file): `diff --git a/P b/P` names the same path twice.
fn path_from_git_line(rest: &str) -> Option<String> {
    let a = rest.strip_prefix("a/")?;
    let mut search = 0;
    while let Some(off) = a[search..].find(" b/") {
        let pos = search + off;
        let candidate = &a[pos + 3..];
        if &a[..pos] == candidate {
            return Some(candidate.to_string());
        }
        search = pos + 1;
    }
    None
}

fn excluded(path: &str) -> bool {
    EXCLUDED.iter().any(|s| path.contains(s))
}

/// Paths that are generated or vendored, whatever their size.
fn is_generated(path: &str) -> bool {
    const SUFFIXES: &[&str] = &[".min.js", ".min.css", ".map", ".snap", ".pb.go"];
    const PARTS: &[&str] = &["_generated.", ".generated."];
    const DIRS: &[&str] = &["dist", "build", "vendor", "node_modules"];
    SUFFIXES.iter().any(|s| path.ends_with(s))
        || PARTS.iter().any(|s| path.contains(s))
        || path.split('/').any(|c| DIRS.contains(&c))
}

/// Indices (ascending) of the files to shorten for the AI. Binary and
/// unparsable files are never shortened, and excluded files are ignored.
fn choose_shrinks(files: &[FilePatch]) -> Vec<usize> {
    let candidates: Vec<usize> = (0..files.len())
        .filter(|&i| !excluded(&files[i].path))
        .collect();
    let shrinkable = |i: usize| !files[i].binary && !files[i].unparsable;

    let mut shrink: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&i| {
            shrinkable(i) && (is_generated(&files[i].path) || files[i].bytes > PER_FILE_BYTES)
        })
        .collect();
    let mut kept: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|i| !shrink.contains(i))
        .collect();
    let mut total: usize = kept.iter().map(|&i| files[i].bytes).sum();
    while total > TOTAL_BYTES {
        // Largest first; the lower index wins a tie, so the order is stable.
        let largest = kept
            .iter()
            .copied()
            .filter(|&i| shrinkable(i))
            .max_by(|&a, &b| files[a].bytes.cmp(&files[b].bytes).then(b.cmp(&a)));
        let Some(largest) = largest else { break };
        kept.retain(|&i| i != largest);
        total -= files[largest].bytes;
        shrink.push(largest);
    }
    shrink.sort_unstable();
    shrink
}

/// The first `budget` bytes' worth of whole hunks under `header`. The first
/// hunk is always kept, up to twice the budget; `None` when it is larger.
fn truncate_hunks(header: &str, hunks: &[String], budget: usize) -> Option<String> {
    let first = hunks.first()?;
    if first.len() > 2 * budget {
        return None;
    }
    let mut out = String::with_capacity(header.len() + budget);
    out.push_str(header);
    out.push_str(first);
    let mut used = first.len();
    for hunk in &hunks[1..] {
        if used + hunk.len() > budget {
            break;
        }
        out.push_str(hunk);
        used += hunk.len();
    }
    Some(out)
}

/// The first `keep` lines of `content`, then a marker saying how many lines
/// were left out. Content of `keep` lines or fewer is returned unchanged.
/// The first `keep` lines of `content`, but never more than `max_bytes`: a
/// minified file is a few enormous lines, so a line count alone would let it
/// through whole. A cut line counts as not shown.
fn shorten_blob(content: &[u8], keep: usize, max_bytes: usize) -> Vec<u8> {
    let lines: Vec<&[u8]> = content.split_inclusive(|&b| b == b'\n').collect();
    if lines.len() <= keep && content.len() <= max_bytes {
        return content.to_vec();
    }
    let mut out = Vec::new();
    let mut shown = 0;
    for line in lines.iter().take(keep) {
        if out.len() + line.len() > max_bytes {
            break;
        }
        out.extend_from_slice(line);
        shown += 1;
    }
    let hidden = lines.len() - shown;
    out.extend_from_slice(
        format!("\n… [herdr-sidebar: {hidden} more lines not shown to the AI]\n").as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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

    const SAMPLE_PATCH: &str = r#"diff --git a/new.rs b/new.rs
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn a() {}
+fn b() {}
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index 2222222..0000000
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-old
diff --git a/src/m.rs b/src/m.rs
index 3333333..4444444 100644
--- a/src/m.rs
+++ b/src/m.rs
@@ -1,2 +1,2 @@
-a
+b
 c
@@ -10,2 +10,2 @@
-x
+y
 z
diff --git a/old.txt b/moved.txt
similarity index 100%
rename from old.txt
rename to moved.txt
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
diff --git "a/tab\there" "b/tab\there"
--- "a/tab\there"
+++ "b/tab\there"
@@ -1 +1 @@
-a
+b
"#;

    fn fp(path: &str, bytes: usize) -> FilePatch {
        FilePatch {
            path: path.to_string(),
            old_path: None,
            status: Status::Modified,
            binary: false,
            unparsable: false,
            bytes,
            header: String::new(),
            hunks: Vec::new(),
        }
    }

    #[test]
    fn split_patch_classifies_each_file() {
        let files = split_patch(SAMPLE_PATCH);
        assert_eq!(files.len(), 6);

        assert_eq!(files[0].path, "new.rs");
        assert_eq!(files[0].status, Status::Added);
        assert_eq!(files[0].hunks.len(), 1);
        assert!(
            files[0]
                .header
                .starts_with("diff --git a/new.rs b/new.rs\n")
        );
        assert!(files[0].header.ends_with("+++ b/new.rs\n"));

        assert_eq!(files[1].path, "gone.rs");
        assert_eq!(files[1].status, Status::Deleted);

        assert_eq!(files[2].path, "src/m.rs");
        assert_eq!(files[2].status, Status::Modified);
        assert_eq!(files[2].hunks.len(), 2);
        assert!(files[2].hunks[0].starts_with("@@ -1,2"));
        assert!(files[2].hunks[1].starts_with("@@ -10,2"));
        assert!(files[2].hunks[1].contains("-x\n"));
        assert!(!files[2].unparsable && !files[2].binary);

        assert_eq!(files[3].path, "moved.txt");
        assert_eq!(files[3].old_path.as_deref(), Some("old.txt"));
        assert_eq!(files[3].status, Status::Renamed);
        assert!(files[3].hunks.is_empty());

        assert_eq!(files[4].path, "img.png");
        assert!(files[4].binary);
        assert!(!files[4].unparsable);
    }

    #[test]
    fn split_patch_byte_lengths_add_up() {
        let files = split_patch(SAMPLE_PATCH);
        let total: usize = files.iter().map(|f| f.bytes).sum();
        assert_eq!(total, SAMPLE_PATCH.len());
    }

    #[test]
    fn quoted_path_is_unparsable() {
        let files = split_patch(SAMPLE_PATCH);
        let quoted = files.last().expect("quoted file");
        assert!(quoted.unparsable);
        assert_eq!(quoted.status, Status::Modified);
    }

    #[test]
    fn git_line_fallback_for_empty_file() {
        let patch =
            "diff --git a/empty.txt b/empty.txt\nnew file mode 100644\nindex 0000000..e69de29\n";
        let files = split_patch(patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "empty.txt");
        assert!(!files[0].unparsable);
        assert_eq!(files[0].status, Status::Added);
    }

    #[test]
    fn choose_shrinks_picks_generated_paths() {
        let files = vec![
            fp("src/app.rs", 100),
            fp("web/app.min.js", 100),
            fp("dist/bundle.js", 100),
            fp("api/types.pb.go", 100),
            fp("schema_generated.ts", 100),
            fp("Cargo.toml", 100),
        ];
        assert_eq!(choose_shrinks(&files), vec![1, 2, 3, 4]);
    }

    #[test]
    fn choose_shrinks_per_file_limit() {
        let files = vec![
            fp("big.rs", PER_FILE_BYTES + 1),
            fp("small.rs", PER_FILE_BYTES),
        ];
        assert_eq!(choose_shrinks(&files), vec![0]);
    }

    #[test]
    fn choose_shrinks_total_budget_takes_largest_first() {
        let size = 10 * 1024;
        let files: Vec<FilePatch> = (0..5).map(|i| fp(&format!("f{i}.rs"), size)).collect();
        // 5 x 10 KiB = 50 KiB > 48 KiB: one shrink (the tie goes to index 0).
        assert_eq!(choose_shrinks(&files), vec![0]);

        // 55 KiB, each under the per-file limit: shrinking the first leaves 44 KiB.
        let eleven: Vec<FilePatch> = (0..5).map(|i| fp(&format!("g{i}.rs"), 11 * 1024)).collect();
        assert_eq!(choose_shrinks(&eleven), vec![0]);
    }

    #[test]
    fn choose_shrinks_ignores_excluded_files() {
        let files = vec![
            fp("Cargo.lock", 1_000_000),
            fp("package-lock.json", 1_000_000),
            fp("logo.svg", 1_000_000),
            fp("icon.png", 1_000_000),
            fp("a.rs", 10 * 1024),
        ];
        // The huge excluded files are invisible to the AI and cost no budget.
        assert!(choose_shrinks(&files).is_empty());
    }

    #[test]
    fn choose_shrinks_never_touches_binary_or_unparsable() {
        let mut binary = fp("dist/blob.bin", PER_FILE_BYTES * 4);
        binary.binary = true;
        let mut quoted = fp("odd.rs", PER_FILE_BYTES * 4);
        quoted.unparsable = true;
        assert!(choose_shrinks(&[binary, quoted]).is_empty());
    }

    #[test]
    fn truncate_hunks_keeps_whole_hunks_within_budget() {
        let header = "H\n";
        let hunks = vec!["@@a".repeat(3), "@@b".repeat(3)]; // 12 bytes each
        assert_eq!(
            truncate_hunks(header, &hunks, 15).as_deref(),
            Some("H\n@@a@@a@@a")
        );
        assert_eq!(
            truncate_hunks(header, &hunks, 24).as_deref(),
            Some("H\n@@a@@a@@a@@b@@b@@b")
        );
    }

    #[test]
    fn truncate_hunks_first_hunk_limit() {
        let header = "H\n";
        let fits = vec!["x".repeat(2 * 100)];
        assert_eq!(
            truncate_hunks(header, &fits, 100).map(|p| p.len()),
            Some(2 + 200)
        );
        let too_big = vec!["x".repeat(2 * 100 + 1)];
        assert_eq!(truncate_hunks(header, &too_big, 100), None);
        assert_eq!(truncate_hunks(header, &[], 100), None);
    }

    #[test]
    fn shorten_blob_keeps_lines_and_states_the_rest() {
        let content: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let out = String::from_utf8(shorten_blob(content.as_bytes(), KEEP_LINES, PER_FILE_BYTES))
            .unwrap();
        let expected_head: String = (0..KEEP_LINES).map(|i| format!("line {i}\n")).collect();
        assert!(out.starts_with(&expected_head));
        assert!(out.ends_with("\n… [herdr-sidebar: 60 more lines not shown to the AI]\n"));
        assert_eq!(out.lines().count(), KEEP_LINES + 2);
    }

    #[test]
    fn shorten_blob_caps_bytes_for_long_lines() {
        // A minified file: three lines, each far over the byte budget.
        let line = format!("{}\n", "x".repeat(PER_FILE_BYTES * 2));
        let content = line.repeat(3);
        let out = String::from_utf8(shorten_blob(content.as_bytes(), KEEP_LINES, PER_FILE_BYTES))
            .unwrap();
        assert!(out.len() < PER_FILE_BYTES);
        assert!(out.ends_with("\n… [herdr-sidebar: 3 more lines not shown to the AI]\n"));
    }

    #[test]
    fn shorten_blob_leaves_short_content_alone() {
        let content = b"one\ntwo\n";
        assert_eq!(
            shorten_blob(content, KEEP_LINES, PER_FILE_BYTES),
            content.to_vec()
        );
    }

    /// Calls the configured AI provider and must not commit. It needs `ai.json`
    /// (⚙ Settings → AI commit) with a key and model. Run it by hand with
    /// `HS_AI_COMMIT_E2E_REPO=<repo with staged changes> cargo test -- --ignored e2e_generate`.
    #[test]
    #[ignore = "calls the configured AI provider"]
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
        let staged = |root: &Path| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["ls-files", "-s"])
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default()
        };
        let before = head(&root);
        let index_before = staged(&root);
        let result = generate(&root);
        println!("GENERATE RESULT: {result:?}");
        assert_eq!(head(&root), before, "generate must not commit");
        assert_eq!(
            staged(&root),
            index_before,
            "generate must not touch the real index"
        );
        let generated = result.expect("generate failed");
        println!("MESSAGE: {}", generated.message);
        println!("SUMMARISED: {:?}", generated.summarised);
    }
}
