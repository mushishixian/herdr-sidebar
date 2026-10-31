//! The file context menu's model and effects: which entries a target offers,
//! and the filesystem/clipboard/shell operations behind them. UI-free so it is
//! unit-testable; `app.rs` owns the popup rendering and input routing.

use std::io;
use std::path::{Path, PathBuf};

use crate::pane_menu::{self, PaneAction};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    NewFile,
    NewFolder,
    CopyPath,
    CopyRelativePath,
    Rename,
    Delete,
    /// Hand the file to the configured custom editor (fork).
    OpenInEditor,
    /// The Explorer's own file clipboard (fork): cut / copy, then paste.
    Cut,
    Copy,
    Paste,
    Duplicate,
    /// `git add` the target — the Explorer's staging entry (issue #20).
    Stage,
    /// Append the target to the owning repository's `.gitignore`.
    AddToGitignore,
    OpenExternal,
    Reveal,
    ChangeFolder,
    ChangeFolderTyped,
    /// herdr's own pane-menu item, offered at the end of the menu.
    Herdr(PaneAction),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

/// VS Code-style context menu for a tree row (`target` = `Some(is_dir)` for a
/// row; `None` for a right-click on empty space, which targets the workspace
/// root: creation only). "Open with Default App" is offered for files only —
/// a directory's shell association is the file manager, which is what
/// "Reveal in File Explorer" already does. `in_repo` adds "Stage Changes"
/// (issue #20) — it stays out of the menu entirely when the target is not
/// inside a git repository, so the entry never offers an action that can only
/// fail.
pub fn menu_entries(target: Option<bool>, in_repo: bool) -> Vec<MenuEntry> {
    menu_entries_with(target, in_repo, false, false)
}

/// [`menu_entries`] plus the fork's entries: "Open in Editor" when a custom
/// editor is configured, Cut / Copy / Duplicate for a row, and Paste while
/// the Explorer clipboard holds something.
pub fn menu_entries_with(
    target: Option<bool>,
    in_repo: bool,
    editor: bool,
    can_paste: bool,
) -> Vec<MenuEntry> {
    let mut entries = vec![
        MenuEntry::Action(MenuAction::NewFile, "New File…"),
        MenuEntry::Action(MenuAction::NewFolder, "New Folder…"),
    ];
    if target == Some(false) {
        entries.push(MenuEntry::Separator);
        if editor {
            entries.push(MenuEntry::Action(
                MenuAction::OpenInEditor,
                "Open in Editor",
            ));
        }
        entries.push(MenuEntry::Action(
            MenuAction::OpenExternal,
            "Open with Default App",
        ));
    }
    if target.is_some() || can_paste {
        entries.push(MenuEntry::Separator);
    }
    if target.is_some() {
        entries.extend([
            MenuEntry::Action(MenuAction::Cut, "Cut"),
            MenuEntry::Action(MenuAction::Copy, "Copy"),
        ]);
    }
    if can_paste {
        entries.push(MenuEntry::Action(MenuAction::Paste, "Paste"));
    }
    if target.is_some() {
        entries.push(MenuEntry::Action(MenuAction::Duplicate, "Duplicate"));
    }
    if target.is_some() && in_repo {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Stage, "Stage Changes"),
            MenuEntry::Action(MenuAction::AddToGitignore, "Add to .gitignore"),
        ]);
    }
    if target.is_some() {
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
            MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Rename, "Rename…"),
            MenuEntry::Action(MenuAction::Delete, "Delete"),
        ]);
    }
    entries.extend([
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
        MenuEntry::Separator,
        MenuEntry::Action(MenuAction::ChangeFolder, "Change Folder…"),
        MenuEntry::Action(MenuAction::ChangeFolderTyped, "Change Folder (Type Path)…"),
    ]);
    entries
}

/// herdr's pane-menu section, appended after the file entries when the
/// sidebar knows its own pane (`known`). Empty otherwise.
pub fn pane_menu_entries(known: bool, labelled: bool) -> Vec<MenuEntry> {
    if !known {
        return Vec::new();
    }
    let mut entries = vec![MenuEntry::Separator];
    entries.extend(
        pane_menu::items(labelled)
            .into_iter()
            .map(|item| MenuEntry::Action(MenuAction::Herdr(item), item.label())),
    );
    entries
}

/// A usable file name from prompt input: trimmed, non-empty, no path
/// separators or drive colons (a name, not a path).
pub fn validate_name(input: &str) -> Option<&str> {
    let name = input.trim();
    (!name.is_empty() && !name.contains(['/', '\\', ':']) && name != "." && name != "..")
        .then_some(name)
}

fn fresh_path(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = dir.join(name);
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{name} already exists"),
        ));
    }
    Ok(path)
}

pub fn create_file(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = fresh_path(dir, name)?;
    std::fs::write(&path, b"")?;
    Ok(path)
}

pub fn create_folder(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = fresh_path(dir, name)?;
    std::fs::create_dir(&path)?;
    Ok(path)
}

pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    let target = fresh_path(parent, new_name)?;
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// What the Explorer clipboard holds: a path and whether paste moves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileClip {
    pub path: PathBuf,
    pub cut: bool,
}

fn file_clip_path() -> Option<PathBuf> {
    Some(crate::state::state_dir()?.join("file-clipboard.json"))
}

/// Shared by every sidebar (all tabs), so copy in one and paste in another.
pub fn save_file_clip(clip: Option<&FileClip>) {
    let Some(path) = file_clip_path() else { return };
    match clip {
        Some(clip) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let json = serde_json::json!({ "path": clip.path, "cut": clip.cut });
            let _ = std::fs::write(path, json.to_string());
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The clipboard entry, if its source still exists.
pub fn load_file_clip() -> Option<FileClip> {
    let text = std::fs::read_to_string(file_clip_path()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let path = PathBuf::from(value.get("path")?.as_str()?);
    let cut = value.get("cut").and_then(|v| v.as_bool()).unwrap_or(false);
    path.symlink_metadata().ok()?;
    Some(FileClip { path, cut })
}

/// A free name in `dir` for a copy of `name`: the name itself if free, else
/// `stem copy.ext`, `stem copy 2.ext`, … (VS Code's scheme).
pub fn copy_name(dir: &Path, name: &str, force_copy: bool) -> PathBuf {
    let plain = dir.join(name);
    if !force_copy && plain.symlink_metadata().is_err() {
        return plain;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| {
            let suffix = if n == 1 {
                " copy".to_string()
            } else {
                format!(" copy {n}")
            };
            dir.join(format!("{stem}{suffix}{ext}"))
        })
        .find(|candidate| candidate.symlink_metadata().is_err())
        .expect("an unbounded range always yields a free name")
}

fn copy_recursive(from: &Path, to: &Path) -> io::Result<()> {
    let meta = from.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(from)?, to);
        #[cfg(not(unix))]
        return std::fs::copy(from, to).map(drop);
    }
    if meta.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

/// Paste `clip` into `dir`; returns where it landed. A copy never overwrites
/// (it picks a `copy` name); a cut refuses when the name is taken, and moving
/// a folder into itself is refused either way.
pub fn paste(clip: &FileClip, dir: &Path) -> io::Result<PathBuf> {
    let name = clip
        .path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "nothing to paste"))?
        .to_string_lossy()
        .into_owned();
    let source = clip.path.canonicalize()?;
    let dir_real = dir.canonicalize()?;
    if dir_real.starts_with(&source) && source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "can't paste a folder into itself",
        ));
    }
    if clip.cut {
        if source.parent() == Some(dir_real.as_path()) {
            return Ok(clip.path.clone()); // cut + paste in place: nothing to do
        }
        let target = fresh_path(dir, &name)?;
        if std::fs::rename(&clip.path, &target).is_err() {
            // Another filesystem: copy, then remove the original.
            copy_recursive(&clip.path, &target)?;
            delete(&clip.path, source.is_dir())?;
        }
        Ok(target)
    } else {
        let target = copy_name(dir, &name, false);
        copy_recursive(&clip.path, &target)?;
        Ok(target)
    }
}

/// A copy next to the original, named `… copy`.
pub fn duplicate(path: &Path) -> io::Result<PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    let target = copy_name(dir, &name, true);
    copy_recursive(path, &target)?;
    Ok(target)
}

pub fn delete(path: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Which path [`copy_to_clipboard`] took, so a caller can word its notice
/// accordingly: a native tool's exit status is a real confirmation, while an
/// OSC 52 write is not (the terminal never acknowledges it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardWrite {
    Native,
    Osc52Unacknowledged,
}

/// herdr's libghostty-vt OSC parser caps a captured OSC 52 payload at 8 MiB
/// (`Parser.MAX_ALLOCATING_BUF` in `terminal/osc.zig`). The captured span
/// starts right after the "52;" prefix (which is consumed by the parser's
/// state machine and never itself counted) and holds the kind + separator
/// ("c;", 2 bytes), the base64 payload, and a trailing NUL byte the parser
/// appends once the sequence ends (`terminal/osc/parsers/clipboard_operation.zig`,
/// `cap.writeByte(0)`). A larger sequence never reaches the clipboard, so
/// reject it up front rather than writing a truncated payload the client
/// can't decode.
const OSC52_MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;

/// Copy text to the system clipboard.
///
/// Over SSH there is often no local clipboard tool to shell out to, so this
/// emits an OSC 52 sequence on stdout instead: herdr's own terminal emulator
/// parses it out of the pane's PTY stream and forwards it to the attached
/// client, which writes it to the real (possibly remote) clipboard — the
/// same path core herdr's own selection-copy uses. Otherwise this first pipes
/// to the platform's clipboard tool (a console child of the TUI's own pty —
/// no window is created), then falls back to OSC 52 when stdout is a terminal.
pub fn copy_to_clipboard(text: &str) -> io::Result<ClipboardWrite> {
    if osc52_preferred() {
        return write_osc52_stdout(text);
    }
    match copy_native(text) {
        Ok(()) => Ok(ClipboardWrite::Native),
        Err(_) if osc52_fallback_available() => write_osc52_stdout(text),
        Err(err) => Err(err),
    }
}

/// Over SSH there is no local clipboard tool to trust, so OSC 52 goes first.
/// Mirrors core herdr's SSH presence check; deliberately narrower than its
/// `should_prefer_osc52` (no WSL/VS Code detection — those aren't "we have
/// no local clipboard tool" cases the way SSH is).
pub fn osc52_preferred() -> bool {
    #[cfg(not(windows))]
    {
        (std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some())
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
    }
    #[cfg(windows)]
    {
        false
    }
}

/// Whether a failed native copy may fall back to OSC 52 (Unix, on a TTY).
pub fn osc52_fallback_available() -> bool {
    cfg!(not(windows)) && std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// Copy through the platform's clipboard tool (a console child of the TUI's
/// own pty — no window is created). Blocks until the tool exits, so callers
/// on a UI thread should run it on a worker.
pub fn copy_native(text: &str) -> io::Result<()> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&["clip"]];
    #[cfg(not(windows))]
    let candidates: &[&[&str]] = &[
        &["pbcopy"],
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
    ];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match copy_with(argv, text) {
            Ok(()) => return Ok(()),
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// Emit OSC 52 on stdout. Must run on the thread that draws the TUI, or the
/// escape sequence could interleave with a frame being written.
pub fn write_osc52_stdout(text: &str) -> io::Result<ClipboardWrite> {
    let stdout = io::stdout();
    write_osc52(stdout.lock(), text)
}

fn write_osc52(mut out: impl std::io::Write, text: &str) -> io::Result<ClipboardWrite> {
    if text.is_empty() {
        // An empty OSC 52 payload clears the terminal's clipboard instead of
        // leaving it untouched.
        return Err(io::Error::other("nothing to copy"));
    }
    if !osc52_sequence_fits(text.len()) {
        return Err(io::Error::other(
            "selection too large for terminal clipboard (OSC 52 limit)",
        ));
    }
    let encoded = base64_encode(text.as_bytes());
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(ClipboardWrite::Osc52Unacknowledged)
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard (RFC 4648, padded) base64 encoder. Hand-rolled so the OSC 52
/// path doesn't need a new dependency for something this small.
fn base64_encode(bytes: &[u8]) -> String {
    // A precise capacity is just a preallocation hint here (the caller has
    // already validated the length via `osc52_sequence_fits`); 0 is always a
    // safe fallback if it were ever to overflow, not a claim about the real
    // encoded length.
    let capacity = checked_base64_encoded_len(bytes.len()).unwrap_or(0);
    let mut out = String::with_capacity(capacity);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(BASE64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(match b1 {
            Some(b1) => {
                BASE64_ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char
            }
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => BASE64_ALPHABET[(b2 & 0x3f) as usize] as char,
            None => '=',
        });
    }
    out
}

/// The base64-encoded length of `input_len` raw bytes (RFC 4648 padded:
/// `ceil(input_len / 3) * 4`), or `None` if computing it would overflow
/// `usize`.
fn checked_base64_encoded_len(input_len: usize) -> Option<usize> {
    input_len.checked_add(2)?.checked_div(3)?.checked_mul(4)
}

/// Whether `input_len` raw bytes, once base64-encoded, fit inside herdr's
/// libghostty-vt OSC 52 capture limit (see [`OSC52_MAX_CAPTURE_BYTES`])
/// alongside the "c;" kind prefix and the trailing NUL the parser appends.
/// Factored out (and worked on the byte count, before encoding) so the
/// boundary can be tested without allocating or emitting real escape
/// sequences, and so a pathological length can't be encoded just to be
/// rejected. Uses checked arithmetic throughout rather than a sentinel
/// fallback, so an overflow is reported as "doesn't fit" instead of being
/// silently misrepresented as a real length.
fn osc52_sequence_fits(input_len: usize) -> bool {
    // The capture holds "c;" (2 bytes) then the base64 payload, then the
    // parser appends one trailing NUL once the OSC sequence ends.
    const KIND_PREFIX_BYTES: usize = 2;
    const TRAILING_NUL_BYTES: usize = 1;
    let Some(encoded_len) = checked_base64_encoded_len(input_len) else {
        return false;
    };
    let Some(total) = encoded_len
        .checked_add(KIND_PREFIX_BYTES)
        .and_then(|t| t.checked_add(TRAILING_NUL_BYTES))
    else {
        return false;
    };
    total <= OSC52_MAX_CAPTURE_BYTES
}

/// How long a clipboard helper may take before it is killed. `clip` and
/// friends exit in milliseconds; one that hangs must not wedge its caller.
const CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn copy_with(argv: &[&str], text: &str) -> io::Result<()> {
    use std::io::Write;

    let mut child = clipboard_command(argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{} opened without stdin", argv[0]),
        ));
    };
    // Write on a helper thread: a helper that never reads would block the
    // write forever once the pipe buffer fills. One deadline covers BOTH the
    // delivery and the exit, and the copy only counts when every byte was
    // delivered AND the helper succeeded — callers such as the editor's cut
    // delete text on success, and there is no undo.
    let bytes = text.as_bytes().to_vec();
    let (sent, delivery) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = stdin.write_all(&bytes);
        drop(stdin);
        let _ = sent.send(result);
    });
    let deadline = std::time::Instant::now() + CLIPBOARD_TIMEOUT;
    let mut delivered: Option<io::Result<()>> = None;
    let mut exited = None;
    loop {
        if delivered.is_none() {
            match delivery.try_recv() {
                Ok(result) => delivered = Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    delivered = Some(Err(io::Error::other("clipboard writer failed")));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        if let (Some(_), Some(_)) = (&delivered, &exited) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // A descendant that inherited the pipe can outlive the helper;
            // the writer thread is then abandoned rather than awaited.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} did not finish in time", argv[0]),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let (Some(delivered), Some(status)) = (delivered, exited) else {
        unreachable!("the loop only exits with both outcomes");
    };
    delivered.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", argv[0])))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            argv[0]
        )))
    }
}

/// A clipboard helper process. Windows PowerShell children must not inherit
/// a PowerShell 7 `PSModulePath` (issue #96): 5.1 would autoload PS7's
/// CoreCLR modules and lose `Get-Clipboard`. Without the variable, 5.1 uses
/// its own defaults.
fn clipboard_command(argv: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(argv[0]);
    command.args(&argv[1..]);
    if argv[0].eq_ignore_ascii_case("powershell") {
        command.env_remove("PSModulePath");
    }
    command
}

/// Read text from the system clipboard when a platform clipboard command is
/// available. Keeping this best-effort matches [`copy_to_clipboard`]: the
/// editor remains useful over SSH/headless sessions where no clipboard exists.
pub fn paste_from_clipboard() -> io::Result<String> {
    #[cfg(windows)]
    let candidates: &[&[&str]] = &[&[
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); Get-Clipboard -Raw",
    ]];
    #[cfg(target_os = "macos")]
    let candidates: &[&[&str]] = &[&["pbpaste"]];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates: &[&[&str]] = &[
        &["wl-paste", "--no-newline"],
        &["xclip", "-selection", "clipboard", "-o"],
    ];

    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for argv in candidates {
        match clipboard_command(argv).output() {
            Ok(output) if output.status.success() => {
                return String::from_utf8(output.stdout)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            Ok(output) => {
                last_err = io::Error::other(format!("{} exited with {}", argv[0], output.status));
            }
            Err(err) => last_err = err,
        }
    }
    Err(last_err)
}

/// Open a directory itself, or open its parent with a file selected
/// (best-effort). Revealing a folder in its parent made the Explorer action
/// look like it had ignored the clicked tree row.
pub fn reveal(path: &Path, directory: bool) {
    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("explorer");
        if directory {
            command.arg(path);
        } else {
            command.arg(format!("/select,{}", path.display()));
        }
        let _ = command.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = std::process::Command::new("open");
        if !directory {
            command.arg("-R");
        }
        let _ = command.arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if directory { Some(path) } else { path.parent() };
        if let Some(target) = target {
            let _ = std::process::Command::new("xdg-open").arg(target).spawn();
        }
    }
}

/// Open a path with the OS-associated application (VS Code's "Open with
/// Default App" / a double click in the file manager).
///
/// Windows goes through `explorer.exe <path>` rather than `cmd /c start`:
/// explorer is a GUI-subsystem process, so no console is created for it and
/// Windows 11 doesn't flash a Windows Terminal window (the same reason the
/// [[events]] hooks use the windowless sidecar). It resolves the shell
/// association exactly like a double click. Its exit code is unreliable
/// (explorer routinely returns 1 on success), so only the spawn is checked.
pub fn open_external(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    std::process::Command::new(program)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

const EDITOR_COMMAND_ENV: &str = "HERDR_SIDEBAR_EDITOR";
const EDITOR_FILE_ENV: &str = "HERDR_SIDEBAR_EDITOR_FILE";
const EDITOR_FILE_TOKEN_ENV: &str = "HERDR_SIDEBAR_EDITOR_FILE_TOKEN";
const EDITOR_METADATA_SOURCE: &str = "herdr-sidebar-editor";
const EDITOR_PATH_TOKEN: &str = "hs-editor-path";
const EDITOR_HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

pub fn configured_editor() -> Option<String> {
    crate::state::load_editor_command().or_else(|| {
        [EDITOR_COMMAND_ENV, "VISUAL", "EDITOR"]
            .into_iter()
            .find_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
    })
}

fn editor_argv(command: &str, file: &Path) -> io::Result<Vec<String>> {
    let argv = split_editor_command(command)?;
    if argv.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "editor command is empty",
        ));
    }
    Ok(fill_editor_argv(argv, &file.display().to_string(), None))
}

/// [`editor_argv`] for a search hit: the editor is opened at `line` (1-based).
/// Three shapes, in order: a `{line}` placeholder in any argument is
/// substituted; a `fresh` / `fresh-open` program gets `<file>:<line>` as its
/// file argument (Fresh's own `path:line` syntax); anything else gets the plain
/// file, since there is no portable way to address a line.
fn editor_argv_at(command: &str, file: &Path, line: usize) -> io::Result<Vec<String>> {
    let argv = split_editor_command(command)?;
    if argv.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "editor command is empty",
        ));
    }
    let plain = file.display().to_string();
    if argv.iter().any(|arg| arg.contains("{line}")) {
        return Ok(fill_editor_argv(argv, &plain, Some(line)));
    }
    let program = Path::new(&argv[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(program, "fresh" | "fresh-open") {
        return Ok(fill_editor_argv(argv, &format!("{plain}:{line}"), None));
    }
    Ok(fill_editor_argv(argv, &plain, None))
}

/// Substitute `{file}` (and `{line}` when given) in every argument. A command
/// with no `{file}` placeholder gets the file appended, as before.
fn fill_editor_argv(mut argv: Vec<String>, file: &str, line: Option<usize>) -> Vec<String> {
    let mut replaced = false;
    for arg in &mut argv {
        if let Some(line) = line {
            *arg = arg.replace("{line}", &line.to_string());
        }
        if arg.contains("{file}") {
            *arg = arg.replace("{file}", file);
            replaced = true;
        }
    }
    if !replaced {
        argv.push(file.to_string());
    }
    argv
}

fn split_editor_command(command: &str) -> io::Result<Vec<String>> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(active) if ch == active => quote = None,
            Some('\'') => current.push(ch),
            Some('"') if ch == '\\' => {
                if matches!(chars.peek(), Some('"' | '\\')) {
                    current.push(chars.next().unwrap());
                } else {
                    current.push(ch);
                }
            }
            Some(_) => current.push(ch),
            None if matches!(ch, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    argv.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None if ch == '\\' => {
                if chars
                    .peek()
                    .is_some_and(|next| next.is_whitespace() || matches!(next, '\'' | '"' | '\\'))
                {
                    current.push(chars.next().unwrap());
                } else {
                    current.push(ch);
                }
                started = true;
            }
            None => {
                current.push(ch);
                started = true;
            }
        }
    }

    if quote.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid editor command",
        ));
    }
    if started {
        argv.push(current);
    }
    Ok(argv)
}

pub fn run_configured_editor() -> io::Result<()> {
    let command = std::env::var(EDITOR_COMMAND_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "custom editor is not configured"))?;
    let file = std::env::var_os(EDITOR_FILE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "editor file is missing"))?;
    let argv = editor_argv(&command, &file)?;
    let pane_id = std::env::var("HERDR_PANE_ID").unwrap_or_default();
    let file_token = std::env::var(EDITOR_FILE_TOKEN_ENV).unwrap_or_default();
    let heartbeat = if pane_id.is_empty() || file_token.is_empty() {
        None
    } else {
        let _ = report_editor_identity(&pane_id, Some(&file_token));
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let heartbeat_pane = pane_id.clone();
        let heartbeat_token = file_token.clone();
        let handle = std::thread::spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                stop_rx.recv_timeout(EDITOR_HEARTBEAT_EVERY)
            {
                let _ = report_editor_identity(&heartbeat_pane, Some(&heartbeat_token));
            }
        });
        Some((stop_tx, handle))
    };
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status();
    if let Some((stop_tx, handle)) = heartbeat {
        let _ = stop_tx.send(());
        let _ = handle.join();
        let _ = report_editor_identity(&pane_id, None);
    }
    let status = status?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} exited with {status}",
            argv[0]
        )))
    }
}

/// Run the configured editor command right here, in the background, instead
/// of in a new herdr tab. The command inherits this pane's herdr environment
/// (HERDR_SOCKET_PATH, HERDR_TAB_ID, ...), so a client such as a script that
/// forwards the file to an editor pane in the same tab can find it. Nothing
/// on screen changes besides what that command does.
pub fn open_in_editor_inline(root: &Path, file: &Path) -> io::Result<()> {
    open_in_editor_inline_env(root, file, &[])
}

/// [`open_in_editor_inline`] plus extra environment variables for the editor
/// process. Source Control uses `HERDR_SIDEBAR_OPEN_DIFF=1` to ask the editor
/// to show the file's diff against HEAD.
pub fn open_in_editor_inline_env(root: &Path, file: &Path, env: &[(&str, &str)]) -> io::Result<()> {
    spawn_inline_editor(root, file, None, env)
}

/// [`open_in_editor_inline_env`] positioned at `line` (1-based). See
/// [`editor_argv_at`] for how the configured command receives the line.
pub fn open_in_editor_inline_at(
    root: &Path,
    file: &Path,
    line: usize,
    env: &[(&str, &str)],
) -> io::Result<()> {
    spawn_inline_editor(root, file, Some(line), env)
}

fn spawn_inline_editor(
    root: &Path,
    file: &Path,
    line: Option<usize>,
    env: &[(&str, &str)],
) -> io::Result<()> {
    // A viewer covering the caller's host pane (the editor's pane) must step
    // aside first, or the editor would stay hidden behind it.
    let caller = env
        .iter()
        .find(|(key, _)| *key == "HERDR_PANE_ID")
        .map(|(_, value)| value.to_string())
        .or_else(|| std::env::var("HERDR_PANE_ID").ok())
        .unwrap_or_default();
    crate::viewer::uncover_host(&caller);
    // The tab's inline preview is about to be REPLACED by the editor. Leaving it
    // beside the editor made every file click accumulate one more pane once an
    // editor was configured (user-reported: a git diff, then a file, and the tab
    // held both). `close_in_tab` writes the viewer's own close request, so an
    // unsaved preview buffer still asks before anything is discarded.
    let _ = crate::viewer::close_in_tab(&caller);
    let command = configured_editor().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "set HERDR_SIDEBAR_EDITOR, VISUAL, or EDITOR",
        )
    })?;
    let editor_file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    };
    let argv = match line {
        Some(line) => editor_argv_at(&command, &editor_file, line)?,
        None => editor_argv(&command, &editor_file)?,
    };
    let program = argv[0].clone();
    let child = inline_command_from_argv(argv, root, env).spawn()?;
    // Reap it in the background so finished editors don't linger as zombies. The
    // child's stdio is null and this call already reported success, so an editor
    // command that dies on the spot (a helper script whose own tool is missing,
    // a bad path) would otherwise look exactly like a successful open.
    std::thread::spawn(move || report_editor_exit(child, program));
    Ok(())
}

/// How long after the launch an editor's failure is still worth reporting. A
/// long-lived inline editor that exits non-zero when the user quits it must not
/// pop a notice an hour later.
const EDITOR_REPORT_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// Most failures kept for a view that has not polled yet; the oldest go first.
const EDITOR_FAILURES_MAX: usize = 8;

static EDITOR_FAILURES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Wait for an inline editor and, when it died right away, leave a message for
/// the view to show. Runs on the reaping thread.
fn report_editor_exit(mut child: std::process::Child, program: String) {
    let started = std::time::Instant::now();
    if let Ok(status) = child.wait()
        && !status.success()
        && started.elapsed() < EDITOR_REPORT_WINDOW
    {
        push_editor_failure(format!(
            "custom editor failed: {program} exited with {status}"
        ));
    }
}

fn push_editor_failure(message: String) {
    let mut queue = EDITOR_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if queue.len() >= EDITOR_FAILURES_MAX {
        queue.remove(0);
    }
    queue.push(message);
}

/// Inline editor launches that already failed, oldest first, as messages a view
/// can show. Drains the queue, so each failure is reported once. Both views
/// poll this from their tick.
pub fn take_editor_failures() -> Vec<String> {
    let mut queue = EDITOR_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *queue)
}

/// The inline editor process, unspawned: argv already resolved from the
/// configured command, cwd at the repo root, null stdio, its own process group
/// on Unix, and the extra `env` pairs applied on top of the inherited
/// environment. `argv` is never empty (`editor_argv` rejects that).
fn inline_command_from_argv(
    argv: Vec<String>,
    root: &Path,
    env: &[(&str, &str)],
) -> std::process::Command {
    let mut child = std::process::Command::new(&argv[0]);
    child
        .args(&argv[1..])
        .current_dir(root)
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: a Ctrl+C in the sidebar never reaches it.
        child.process_group(0);
    }
    child
}

pub fn open_in_editor_tab(my_pane_id: &str, root: &Path, file: &Path) -> io::Result<()> {
    let command = configured_editor().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "set HERDR_SIDEBAR_EDITOR, VISUAL, or EDITOR",
        )
    })?;
    let panes = crate::ipc::call_text("pane.list", serde_json::json!({}))?;
    let workspace_id = crate::launch::workspace_of(&panes, my_pane_id);
    if workspace_id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not find the current workspace",
        ));
    }
    let editor_file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    };
    let file_token = editor_file_token(&editor_file);
    if let Some((tab_id, pane_id)) =
        editor_tab_for_file(&panes, &workspace_id, &file_token, crate::state::unix_now())
    {
        crate::viewer::focus_tab_for_client(&tab_id, Some(&pane_id));
        return Ok(());
    }
    let mut env = crate::state::spawn_env()
        .as_object()
        .cloned()
        .unwrap_or_default();
    env.insert(
        EDITOR_COMMAND_ENV.into(),
        serde_json::Value::String(command),
    );
    env.insert(
        EDITOR_FILE_ENV.into(),
        serde_json::Value::String(editor_file.display().to_string()),
    );
    env.insert(
        EDITOR_FILE_TOKEN_ENV.into(),
        serde_json::Value::String(file_token.clone()),
    );
    let label = file
        .file_name()
        .unwrap_or(file.as_os_str())
        .to_string_lossy();
    let response = crate::ipc::call_text(
        "tab.create",
        serde_json::json!({
            "workspace_id": workspace_id,
            "label": format!("{label} · editor"),
            "cwd": root.display().to_string(),
            "focus": false,
            "env": env,
        }),
    )?;
    let (tab_id, pane_id) = tab_create_ids(&response).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "editor tab opened without pane metadata",
        )
    })?;
    if let Err(error) = report_editor_identity(&pane_id, Some(&file_token)) {
        let _ = crate::ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
        return Err(error);
    }
    if let Err(error) = crate::ipc::call_text(
        "pane.send_input",
        serde_json::json!({
            "pane_id": pane_id,
            "text": format!("{} --run-custom-editor", crate::state::EXECUTABLE_NAME),
            "keys": ["Enter"],
        }),
    ) {
        let _ = crate::ipc::call_text("tab.close", serde_json::json!({ "tab_id": tab_id }));
        return Err(error);
    }
    crate::viewer::focus_tab_for_client(&tab_id, Some(&pane_id));
    Ok(())
}

fn editor_file_token(file: &Path) -> String {
    let path = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let key = path.display().to_string();
    #[cfg(windows)]
    let key = key.replace('/', "\\").to_lowercase();
    crate::viewer::document_token(&key)
}

fn editor_tab_for_file(
    panes_json: &str,
    workspace_id: &str,
    file_token: &str,
    now: u64,
) -> Option<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        tab_id: Option<String>,
        workspace_id: Option<String>,
        #[serde(default)]
        tokens: std::collections::BTreeMap<String, serde_json::Value>,
    }

    let msg = serde_json::from_str::<Msg>(crate::launch::strip_bom(panes_json)).ok()?;
    msg.result.panes.into_iter().find_map(|pane| {
        if pane.workspace_id.as_deref() != Some(workspace_id)
            || pane.tokens.get(EDITOR_PATH_TOKEN)?.as_str()? != file_token
        {
            return None;
        }
        let heartbeat = pane
            .tokens
            .get(EDITOR_METADATA_SOURCE)?
            .as_str()?
            .parse::<u64>()
            .ok()?;
        if now.saturating_sub(heartbeat) > crate::launch::HEARTBEAT_STALE_SECS {
            return None;
        }
        Some((pane.tab_id?, pane.pane_id?))
    })
}

fn report_editor_identity(pane_id: &str, file_token: Option<&str>) -> io::Result<()> {
    let heartbeat = file_token.map(|_| crate::state::unix_now().to_string());
    crate::ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": EDITOR_METADATA_SOURCE,
            "tokens": {
                EDITOR_METADATA_SOURCE: heartbeat,
                EDITOR_PATH_TOKEN: file_token,
            },
        }),
    )?;
    Ok(())
}

fn tab_create_ids(response: &str) -> Option<(String, String)> {
    let value: serde_json::Value =
        serde_json::from_str(response.trim_start_matches('\u{feff}')).ok()?;
    let result = value.get("result")?;
    let tab_id = result.get("tab")?.get("tab_id")?.as_str()?.to_string();
    let pane_id = result
        .get("root_pane")?
        .get("pane_id")?
        .as_str()?
        .to_string();
    Some((tab_id, pane_id))
}

/// Quote text embedded in a double-quoted AppleScript string literal.
#[cfg(any(test, target_os = "macos"))]
fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Parse a completed osascript picker invocation. Keeping this separate from
/// process launch makes cancel/error and path normalization testable on every
/// platform, including CI hosts that do not provide osascript.
#[cfg(any(test, target_os = "macos"))]
fn parse_osascript_folder(success: bool, stdout: &[u8]) -> Option<PathBuf> {
    if !success {
        return None;
    }
    let picked = String::from_utf8_lossy(stdout).trim().to_string();
    if picked.is_empty() {
        return None;
    }
    let trimmed = picked.trim_end_matches('/');
    Some(if trimmed.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(trimmed)
    })
}

/// Native "choose a folder" dialog. The apps call this from a worker thread
/// so their heartbeat continues while the dialog is open.
///
/// macOS deliberately uses an osascript subprocess: rfd's Cocoa backend needs
/// the main thread or a running NSApplication and aborts a terminal TUI when
/// invoked from the worker. Windows keeps its existing rfd/IFileDialog path.
#[cfg(any(windows, target_os = "macos"))]
pub fn pick_folder(start: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // An invalid default location makes choose-folder fail instead of
        // opening, so omit it if the former root has disappeared.
        let location = if start.is_dir() {
            format!(
                " default location POSIX file \"{}\"",
                applescript_escape(&start.display().to_string())
            )
        } else {
            String::new()
        };
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!(
                "POSIX path of (choose folder with prompt \"Open Folder\"{location})"
            ))
            .output()
            .ok()?;
        parse_osascript_folder(output.status.success(), &output.stdout)
    }
    #[cfg(windows)]
    {
        rfd::FileDialog::new()
            .set_title("Open Folder")
            .set_directory(start)
            .pick_folder()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_editor_command_applies_extra_env_and_repo_cwd() {
        let root = Path::new("/repo");
        let file = Path::new("/repo/src/main.rs");
        let cmd = inline_command_from_argv(
            editor_argv("fresh --wait", file).expect("argv"),
            root,
            &[("HERDR_SIDEBAR_OPEN_DIFF", "1")],
        );
        let envs: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        assert!(envs.contains(&(
            "HERDR_SIDEBAR_OPEN_DIFF".into(),
            Some(std::ffi::OsString::from("1"))
        )));
        assert_eq!(cmd.get_program(), "fresh");
        assert_eq!(cmd.get_current_dir(), Some(root));
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["--wait", "/repo/src/main.rs"]);
    }

    /// The click path only reports a successful SPAWN, and the child's stdio is
    /// null: an editor command that dies immediately (a helper script whose own
    /// tool is missing) used to look exactly like a successful open.
    ///
    /// ONE test for the whole (process-global) failure queue: the cases share it,
    /// so separate `#[test]`s race each other under the parallel test runner and
    /// fail intermittently.
    #[cfg(unix)]
    #[test]
    fn inline_editor_failures_are_reported_once_and_the_queue_is_bounded() {
        let _ = take_editor_failures();
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sh");
        report_editor_exit(child, "fresh-open".into());
        let failures = take_editor_failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("fresh-open") && failures[0].contains("exited with"),
            "{failures:?}"
        );
        // Drained: the view shows each failure once.
        assert!(take_editor_failures().is_empty());

        // Bounded and newest-kept: a view that never polls cannot grow it
        // forever while the failure the user just caused survives.
        for n in 0..(EDITOR_FAILURES_MAX + 3) {
            push_editor_failure(format!("fail {n}"));
        }
        let failures = take_editor_failures();
        assert_eq!(failures.len(), EDITOR_FAILURES_MAX);
        assert_eq!(
            failures.last().unwrap(),
            &format!("fail {}", EDITOR_FAILURES_MAX + 2)
        );
        assert!(take_editor_failures().is_empty());
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-ft-actions-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn has(entries: &[MenuEntry], action: MenuAction) -> bool {
        entries
            .iter()
            .any(|e| matches!(e, MenuEntry::Action(a, _) if *a == action))
    }

    #[test]
    fn clipboard_entries_follow_target_editor_and_clip() {
        let file = menu_entries_with(Some(false), false, true, false);
        for action in [
            MenuAction::OpenInEditor,
            MenuAction::Cut,
            MenuAction::Copy,
            MenuAction::Duplicate,
        ] {
            assert!(has(&file, action), "{action:?} on a file row");
        }
        assert!(!has(&file, MenuAction::Paste), "nothing copied yet");
        assert!(!has(
            &menu_entries_with(Some(true), false, true, true),
            MenuAction::OpenInEditor
        ));
        assert!(has(
            &menu_entries_with(Some(true), false, false, true),
            MenuAction::Paste
        ));
        let root = menu_entries_with(None, false, true, true);
        assert!(has(&root, MenuAction::Paste) && !has(&root, MenuAction::Cut));
        assert!(!has(&menu_entries(Some(false), true), MenuAction::Paste));
    }

    #[test]
    fn copy_name_follows_the_vscode_scheme() {
        let dir = tmp("copy-name");
        assert_eq!(copy_name(&dir, "a.rs", false), dir.join("a.rs"));
        std::fs::write(dir.join("a.rs"), "").unwrap();
        assert_eq!(copy_name(&dir, "a.rs", false), dir.join("a copy.rs"));
        std::fs::write(dir.join("a copy.rs"), "").unwrap();
        assert_eq!(copy_name(&dir, "a.rs", true), dir.join("a copy 2.rs"));
        assert_eq!(copy_name(&dir, ".env", true), dir.join(".env copy"));
        assert_eq!(copy_name(&dir, "Makefile", true), dir.join("Makefile copy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paste_copies_moves_and_refuses_folder_into_itself() {
        let dir = tmp("paste");
        std::fs::create_dir_all(dir.join("src/inner")).unwrap();
        std::fs::write(dir.join("src/inner/x.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("dest")).unwrap();

        let copy = FileClip {
            path: dir.join("src"),
            cut: false,
        };
        assert_eq!(
            paste(&copy, &dir.join("dest")).unwrap(),
            dir.join("dest/src")
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("dest/src/inner/x.txt")).unwrap(),
            "x"
        );
        assert_eq!(
            paste(&copy, &dir.join("dest")).unwrap(),
            dir.join("dest/src copy")
        );
        assert!(
            paste(&copy, &dir.join("src/inner")).is_err(),
            "folder into itself"
        );
        assert!(dir.join("src").exists(), "copy keeps the source");

        let cut = FileClip {
            path: dir.join("src/inner/x.txt"),
            cut: true,
        };
        assert_eq!(
            paste(&cut, &dir.join("dest")).unwrap(),
            dir.join("dest/x.txt")
        );
        assert!(!dir.join("src/inner/x.txt").exists(), "cut moves it");
        std::fs::write(dir.join("src/x.txt"), "again").unwrap();
        let clash = FileClip {
            path: dir.join("src/x.txt"),
            cut: true,
        };
        assert!(
            paste(&clash, &dir.join("dest")).is_err(),
            "a cut never overwrites"
        );

        assert_eq!(
            duplicate(&dir.join("dest/x.txt")).unwrap(),
            dir.join("dest/x copy.txt")
        );
        assert_eq!(duplicate(&dir.join("src")).unwrap(), dir.join("src copy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn menu_shape_for_rows_and_root() {
        let row = menu_entries(Some(false), true);
        assert!(matches!(row[0], MenuEntry::Action(MenuAction::NewFile, _)));
        assert!(has(&row, MenuAction::Delete));
        let root = menu_entries(None, true);
        assert!(!has(&root, MenuAction::Rename));
        assert!(has(&root, MenuAction::Reveal));
    }

    #[test]
    fn open_external_is_offered_for_files_only() {
        assert!(
            has(&menu_entries(Some(false), true), MenuAction::OpenExternal),
            "file row"
        );
        assert!(
            !has(&menu_entries(Some(true), true), MenuAction::OpenExternal),
            "directory row"
        );
        assert!(
            !has(&menu_entries(None, true), MenuAction::OpenExternal),
            "empty space"
        );
        // Directories keep everything else they had.
        assert!(has(&menu_entries(Some(true), true), MenuAction::Rename));
    }

    #[test]
    fn stage_is_offered_for_rows_inside_a_repo_only() {
        assert!(
            has(&menu_entries(Some(false), true), MenuAction::Stage),
            "file row"
        );
        assert!(
            has(&menu_entries(Some(true), true), MenuAction::Stage),
            "directory row"
        );
        assert!(
            !has(&menu_entries(None, true), MenuAction::Stage),
            "empty space"
        );
        assert!(
            !has(&menu_entries(Some(false), false), MenuAction::Stage),
            "outside a repo"
        );
        assert!(!has(&menu_entries(Some(true), false), MenuAction::Stage));
    }

    #[test]
    fn add_to_gitignore_is_offered_wherever_staging_is() {
        for (target, in_repo) in [
            (Some(false), true),
            (Some(true), true),
            (None, true),
            (Some(false), false),
            (Some(true), false),
            (None, false),
        ] {
            assert_eq!(
                has(&menu_entries(target, in_repo), MenuAction::AddToGitignore),
                target.is_some() && in_repo,
                "{target:?} in_repo={in_repo}"
            );
        }
    }

    #[test]
    fn name_validation_rejects_paths_and_blanks() {
        assert_eq!(validate_name("  notes.md "), Some("notes.md"));
        assert_eq!(validate_name(""), None);
        assert_eq!(validate_name("   "), None);
        assert_eq!(validate_name("a/b"), None);
        assert_eq!(validate_name("a\\b"), None);
        assert_eq!(validate_name("C:"), None);
        assert_eq!(validate_name(".."), None);
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_commands_must_accept_input_and_exit_successfully() {
        assert!(copy_with(&["sh", "-c", "cat >/dev/null"], "copied").is_ok());
        let error = copy_with(&["sh", "-c", "cat >/dev/null; exit 7"], "not copied")
            .unwrap_err()
            .to_string();
        assert!(error.contains("exited with"), "{error}");
    }

    #[test]
    fn line_aware_editor_argv_has_three_shapes() {
        let file = Path::new("/repo/src/main.rs");
        // 1. An explicit {line} placeholder wins and is substituted.
        assert_eq!(
            editor_argv_at("code --goto {file}:{line}", file, 12).unwrap(),
            vec!["code", "--goto", "/repo/src/main.rs:12"]
        );
        // 2. Fresh (by basename) gets path:line as its file argument.
        assert_eq!(
            editor_argv_at("/home/u/bin/fresh-open {file}", file, 7).unwrap(),
            vec!["/home/u/bin/fresh-open", "/repo/src/main.rs:7"]
        );
        assert_eq!(
            editor_argv_at("fresh --wait", file, 7).unwrap(),
            vec!["fresh", "--wait", "/repo/src/main.rs:7"]
        );
        // 3. Anything else gets the plain file; the line is dropped.
        assert_eq!(
            editor_argv_at("nvim -f", file, 7).unwrap(),
            vec!["nvim", "-f", "/repo/src/main.rs"]
        );
        assert!(editor_argv_at("   ", file, 7).is_err());
    }

    #[test]
    fn editor_command_is_split_without_a_shell_and_substitutes_the_file() {
        let file = Path::new("C:/work/my project/main.rs");
        assert_eq!(
            editor_argv("nvim -f", file).unwrap(),
            vec!["nvim", "-f", "C:/work/my project/main.rs"]
        );
        assert_eq!(
            editor_argv("code --goto \"{file}:12\"", file).unwrap(),
            vec!["code", "--goto", "C:/work/my project/main.rs:12"]
        );
        assert_eq!(
            editor_argv(r#"C:\Tools\Code\code.exe --wait"#, file).unwrap(),
            vec![
                r#"C:\Tools\Code\code.exe"#,
                "--wait",
                "C:/work/my project/main.rs"
            ]
        );
        assert_eq!(
            editor_argv(r#""C:\Program Files\Code\code.exe" --wait"#, file).unwrap(),
            vec![
                r#"C:\Program Files\Code\code.exe"#,
                "--wait",
                "C:/work/my project/main.rs"
            ]
        );
        assert!(editor_argv("\"unterminated", file).is_err());
    }

    #[test]
    fn tab_create_response_exposes_editor_root_pane() {
        let response = r#"{"result":{"tab":{"tab_id":"w1:t9"},"root_pane":{"pane_id":"w1:p8"}}}"#;
        assert_eq!(
            tab_create_ids(response),
            Some(("w1:t9".into(), "w1:p8".into()))
        );
        assert_eq!(tab_create_ids("garbage"), None);
    }

    #[test]
    fn editor_tabs_reuse_only_the_live_matching_path_in_the_same_workspace() {
        let first = editor_file_token(Path::new("/one/README.md"));
        let second = editor_file_token(Path::new("/two/README.md"));
        assert_ne!(first, second, "same-named files keep distinct identities");
        let panes = serde_json::json!({
            "result": { "panes": [
                {
                    "pane_id": "w1:p1",
                    "tab_id": "w1:t1",
                    "workspace_id": "w1",
                    "tokens": {
                        EDITOR_METADATA_SOURCE: "100",
                        EDITOR_PATH_TOKEN: first,
                    }
                },
                {
                    "pane_id": "w2:p1",
                    "tab_id": "w2:t1",
                    "workspace_id": "w2",
                    "tokens": {
                        EDITOR_METADATA_SOURCE: "100",
                        EDITOR_PATH_TOKEN: first,
                    }
                },
                {
                    "pane_id": "w1:p2",
                    "tab_id": "w1:t2",
                    "workspace_id": "w1",
                    "tokens": {
                        EDITOR_METADATA_SOURCE: "1",
                        EDITOR_PATH_TOKEN: second,
                    }
                }
            ] }
        })
        .to_string();
        assert_eq!(
            editor_tab_for_file(&panes, "w1", &first, 100),
            Some(("w1:t1".into(), "w1:p1".into()))
        );
        assert_eq!(editor_tab_for_file(&panes, "w1", &second, 100), None);
        assert_eq!(
            editor_tab_for_file(&panes, "w2", &first, 100),
            Some(("w2:t1".into(), "w2:p1".into()))
        );
    }

    #[test]
    fn applescript_literals_escape_backslashes_before_quotes() {
        assert_eq!(applescript_escape("/tmp/plain"), "/tmp/plain");
        assert_eq!(applescript_escape(r#"/tmp/a"b"#), r#"/tmp/a\"b"#);
        assert_eq!(applescript_escape(r"/tmp/a\b"), r"/tmp/a\\b");
        assert_eq!(applescript_escape(r#"/tmp/a\"b"#), r#"/tmp/a\\\"b"#);
    }

    #[test]
    fn osascript_picker_output_parsing_handles_cancel_root_and_trailing_slash() {
        assert_eq!(parse_osascript_folder(false, b"/ignored/\n"), None);
        assert_eq!(parse_osascript_folder(true, b"\n"), None);
        assert_eq!(
            parse_osascript_folder(true, b"/\n"),
            Some(PathBuf::from("/"))
        );
        assert_eq!(
            parse_osascript_folder(true, b"/Users/alex/My Folder/\n"),
            Some(PathBuf::from("/Users/alex/My Folder"))
        );
    }

    #[test]
    fn create_rename_delete_roundtrip() {
        let dir = tmp("roundtrip");
        let file = create_file(&dir, "a.txt").unwrap();
        assert!(file.exists());
        assert!(create_file(&dir, "a.txt").is_err(), "no overwrite");
        let folder = create_folder(&dir, "sub").unwrap();
        assert!(folder.is_dir());
        let renamed = rename(&file, "b.txt").unwrap();
        assert!(renamed.exists() && !file.exists());
        assert!(rename(&renamed, "sub").is_err(), "no clobbering existing");
        delete(&renamed, false).unwrap();
        delete(&folder, true).unwrap();
        assert!(!renamed.exists() && !folder.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Astra round 3 HIGH: a helper that exits 0 without reading its input
    /// did NOT copy. Reporting success would let the editor's cut delete the
    /// selection with nothing on the clipboard.
    #[test]
    fn a_helper_that_ignores_its_input_is_not_a_successful_copy() {
        let big = "x".repeat(4 * 1024 * 1024);
        #[cfg(unix)]
        let argv: &[&str] = &["sh", "-c", "exit 0"];
        #[cfg(windows)]
        let argv: &[&str] = &["cmd", "/c", "exit 0"];
        assert!(
            copy_with(argv, &big).is_err(),
            "undelivered input must fail"
        );
    }

    #[test]
    fn powershell_clipboard_helpers_drop_an_inherited_module_path() {
        let command = clipboard_command(&["powershell", "-NoProfile"]);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "PSModulePath" && value.is_none()),
            "PSModulePath must be removed for Windows PowerShell children"
        );
        assert!(clipboard_command(&["clip"]).get_envs().next().is_none());
    }

    #[test]
    fn base64_matches_rfc4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_unicode_and_embedded_nul_bytes() {
        // Cross-checked against the shell: `printf '%s' 'héllo→✓' | base64`.
        assert_eq!(base64_encode("héllo→✓".as_bytes()), "aMOpbGxv4oaS4pyT");
        // `printf 'a\x00b' | base64`.
        assert_eq!(base64_encode(b"a\x00b"), "YQBi");
    }

    #[test]
    fn osc52_size_check_accepts_up_to_the_limit_and_rejects_past_it() {
        // libghostty-vt's capture holds "c;" (2 bytes) + the base64 payload
        // + a trailing NUL (1 byte) within OSC52_MAX_CAPTURE_BYTES
        // (8_388_608), so the payload budget is 8_388_608 - 3 == 8_388_605
        // bytes. Base64 output length is always a multiple of 4, so the
        // largest encoded length that fits is
        // floor(8_388_605 / 4) * 4 == 2_097_151 * 4 == 8_388_604, i.e.
        // ceil(n / 3) == 2_097_151; the largest raw n giving that is
        // 3 * 2_097_151 == 6_291_453.
        const MAX_FITTING_LEN: usize = 6_291_453;
        assert!(osc52_sequence_fits(MAX_FITTING_LEN));
        assert!(!osc52_sequence_fits(MAX_FITTING_LEN + 1));
    }

    #[test]
    fn osc52_size_check_rejects_usize_max_without_panicking() {
        assert!(!osc52_sequence_fits(usize::MAX));
    }

    #[test]
    fn osc52_writer_emits_the_complete_terminal_sequence() {
        let mut bytes = Vec::new();
        assert_eq!(
            write_osc52(&mut bytes, "hello").unwrap(),
            ClipboardWrite::Osc52Unacknowledged
        );
        assert_eq!(bytes, b"\x1b]52;c;aGVsbG8=\x07");
        assert!(write_osc52(Vec::new(), "").is_err());
    }

    #[test]
    fn pane_section_needs_a_known_pane_and_ends_the_menu() {
        assert!(pane_menu_entries(false, true).is_empty());
        let section = pane_menu_entries(true, true);
        assert!(matches!(section[0], MenuEntry::Separator));
        let labels: Vec<&str> = section
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Action(MenuAction::Herdr(_), label) => Some(*label),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Rename pane…",
                "Clear pane name",
                "Split right",
                "Split down",
                "Zoom",
                "Use Herdr right-click menu",
                "Close pane",
            ]
        );
        let unlabelled = pane_menu_entries(true, false);
        assert!(!has(&unlabelled, MenuAction::Herdr(PaneAction::ClearName)));
        // Appended after the file menu's last entry: the file menu is intact.
        let mut menu = menu_entries_with(None, false, false, false);
        menu.extend(section);
        assert!(matches!(
            menu.last(),
            Some(MenuEntry::Action(MenuAction::Herdr(PaneAction::Close), _))
        ));
    }
}
