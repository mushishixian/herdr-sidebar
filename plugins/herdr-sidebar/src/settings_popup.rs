//! The ⚙ Settings modal as a herdr floating popup (`--settings-popup`).
//!
//! The sidebar opens this popup from `⚙` / `s` on Linux and macOS, instead of
//! drawing the page inside its own narrow pane. The popup is a separate
//! process, so it cannot reach the sidebar's state. Settings that persist are
//! written to `state.json` by the page itself, and every running sidebar
//! re-reads them. The two actions that act on the sidebar's own process or tab
//! (unified mode and the change-folder dialog) travel as a request file keyed
//! by the caller pane; the sidebar claims it on its next loop turn.
//!
//! The in-pane modal stays the fallback: Windows (popup panes are linux/macos
//! only), no herdr pane, or a refused popup request.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::icons::IconTheme;
use crate::settings_page::{Outcome, Setting, SettingsHost, SettingsPage};
use crate::state::{self, State};
use crate::ui;

/// The `[[panes]]` id in `herdr-plugin.toml`; the popup is opened by it.
pub const ENTRYPOINT: &str = "settings";
/// `"1"` when the other view's executable exists (unified mode can be set).
pub const ENV_UNIFIED_OK: &str = "HERDR_SIDEBAR_SETTINGS_UNIFIED_OK";
/// The folder name the sidebar is rooted at.
pub const ENV_ROOT: &str = "HERDR_SIDEBAR_SETTINGS_ROOT";
/// `"emoji"` or `"material"`: the sidebar's icon theme when state.json has none.
pub const ENV_ICONS: &str = "HERDR_SIDEBAR_SETTINGS_ICONS";
/// JSON array of `[key, label]` pairs: the hotkey chips the sidebar shows.
pub const ENV_HINTS: &str = "HERDR_SIDEBAR_SETTINGS_HINTS";

/// Idle redraw interval. Keys and mouse events wake the loop at once.
const POLL: Duration = Duration::from_millis(250);
/// A request older than this is dropped instead of applied late.
const REQUEST_MAX_AGE_SECS: u64 = 30;

/// What the sidebar hands the popup through the environment: the values the
/// popup cannot read from disk.
pub struct CallerContext {
    pub unified_available: bool,
    pub root_name: String,
    pub icon_theme: IconTheme,
    pub hints: Vec<(String, String)>,
}

/// Ask herdr for the popup. Errors (for example `ui_busy`, since herdr allows
/// one popup per session) let the caller fall back to the in-pane modal.
pub fn open(ctx: &CallerContext) -> io::Result<()> {
    let unified = if ctx.unified_available { "1" } else { "0" };
    let icons = match ctx.icon_theme {
        IconTheme::Emoji => "emoji",
        IconTheme::Material => "material",
    };
    let hints = serde_json::to_string(&ctx.hints).unwrap_or_else(|_| "[]".to_string());
    crate::file_popup::request_popup_pane_with_env(
        ENTRYPOINT,
        "44%",
        "80%",
        &[
            (ENV_UNIFIED_OK, unified),
            (ENV_ROOT, ctx.root_name.as_str()),
            (ENV_ICONS, icons),
            (ENV_HINTS, hints.as_str()),
        ],
    )
}

// ── Requests from the popup to the sidebar ───────────────────────────────

/// An action only the sidebar process can perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Turn the unified sidebar on (`true`) or off.
    Unified(bool),
    /// Open the change-folder dialog.
    ChangeFolder,
}

impl Request {
    /// The request's text form: the first line of the request file.
    pub fn to_text(self) -> &'static str {
        match self {
            Self::Unified(true) => "unified on",
            Self::Unified(false) => "unified off",
            Self::ChangeFolder => "folder",
        }
    }

    fn from_text(text: &str) -> Option<Self> {
        match text.trim() {
            "unified on" => Some(Self::Unified(true)),
            "unified off" => Some(Self::Unified(false)),
            "folder" => Some(Self::ChangeFolder),
            _ => None,
        }
    }
}

fn sanitize(pane_id: &str) -> String {
    pane_id
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

/// Where the request for `pane_id` lives.
pub fn request_path(pane_id: &str) -> PathBuf {
    crate::rundir::dir("settings").join(format!("pane-{}.req", sanitize(pane_id)))
}

/// Leave a request for the sidebar that owns `pane_id`. Written to a temp
/// file and renamed, so the sidebar never reads half a request.
pub fn write_request(pane_id: &str, request: Request) -> io::Result<()> {
    let path = request_path(pane_id);
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("settings request directory unavailable"))?;
    crate::rundir::ensure_private(dir)?;
    let tmp = dir.join(format!(
        "pane-{}.req.tmp-{}",
        sanitize(pane_id),
        std::process::id()
    ));
    let body = format!("{}\n{}\n", request.to_text(), crate::state::unix_now());
    std::fs::write(&tmp, body)?;
    if let Err(error) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Claim the pending request for `pane_id`, if any. The file is renamed away
/// before it is read, so it is applied at most once. A missing file costs one
/// failed rename.
pub fn take_request(pane_id: &str) -> Option<Request> {
    let path = request_path(pane_id);
    let claimed = path.with_file_name(format!(
        "pane-{}.req.claimed-{}",
        sanitize(pane_id),
        std::process::id()
    ));
    std::fs::rename(&path, &claimed).ok()?;
    let text = std::fs::read_to_string(&claimed);
    let _ = std::fs::remove_file(&claimed);
    parse_request(&text.ok()?, crate::state::unix_now())
}

/// Pure half of [`take_request`]: `"<command>\n<unix secs>\n"` → the request
/// when it is recognised and fresh.
fn parse_request(text: &str, now: u64) -> Option<Request> {
    let mut lines = text.lines();
    let request = Request::from_text(lines.next()?)?;
    let stamp: u64 = lines.next()?.trim().parse().ok()?;
    (now.saturating_sub(stamp) <= REQUEST_MAX_AGE_SECS).then_some(request)
}

/// The hotkey chips from [`ENV_HINTS`]. Malformed input yields none.
fn parse_hints(json: &str) -> Vec<(String, String)> {
    serde_json::from_str(json).unwrap_or_default()
}

// ── The popup process ────────────────────────────────────────────────────

/// The popup's host. Everything the page changes that lives in a sidebar goes
/// out as a request to the caller pane.
struct PopupHost {
    unified_ok: bool,
    root: String,
    fallback_icons: IconTheme,
    hints: Vec<(&'static str, &'static str)>,
    /// The sidebar that opened the popup (`HERDR_CALLER_PANE_ID`).
    caller: Option<String>,
    notice: Option<(String, bool)>,
    /// A request could not be written. The page then stays open so the
    /// message is visible, even though the page itself asked to close.
    failed: bool,
}

impl PopupHost {
    fn from_env() -> Self {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        // Leaked once per popup run: the process is short-lived and the page
        // wants `&'static str` hints.
        let hints = env(ENV_HINTS)
            .map(|json| parse_hints(&json))
            .unwrap_or_default()
            .into_iter()
            .map(|(key, label)| {
                (
                    &*Box::leak(key.into_boxed_str()),
                    &*Box::leak(label.into_boxed_str()),
                )
            })
            .collect();
        Self {
            unified_ok: env(ENV_UNIFIED_OK).is_some_and(|v| v == "1"),
            root: env(ENV_ROOT).unwrap_or_default(),
            fallback_icons: env(ENV_ICONS)
                .and_then(|name| IconTheme::from_state_name(&name))
                .unwrap_or(IconTheme::Material),
            hints,
            caller: env("HERDR_CALLER_PANE_ID"),
            notice: None,
            failed: false,
        }
    }

    fn send(&mut self, request: Request) {
        let result = match &self.caller {
            Some(pane) => write_request(pane, request),
            None => Err(io::Error::other("no sidebar to apply this to")),
        };
        if let Err(error) = result {
            self.failed = true;
            self.notice = Some((format!("could not reach the sidebar: {error}"), true));
        }
    }

    /// Clear the previous event's message before the next one runs.
    fn begin_event(&mut self) {
        self.notice = None;
        self.failed = false;
    }
}

impl SettingsHost for PopupHost {
    fn unified_available(&self) -> bool {
        self.unified_ok
    }

    fn set_unified(&mut self, on: bool) {
        self.send(Request::Unified(on));
    }

    fn change_folder(&mut self) {
        self.send(Request::ChangeFolder);
    }

    fn root_name(&self) -> String {
        self.root.clone()
    }

    fn icon_theme(&self) -> IconTheme {
        state::load_state().icons.unwrap_or(self.fallback_icons)
    }

    fn set_icon_theme(&mut self, theme: IconTheme) {
        let _ = state::update_state(|s| s.icons = Some(theme));
    }

    fn state_changed(&mut self, _setting: Setting, _state: &State) {
        // state.json is already written. Running sidebars re-read it.
    }

    fn pane_id(&self) -> Option<String> {
        self.caller.clone()
    }

    fn notice(&mut self, text: String, error: bool) {
        self.notice = Some((text, error));
        if error {
            self.failed = true;
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        self.hints.clone()
    }
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn draw(frame: &mut Frame, page: &mut SettingsPage, host: &PopupHost) {
    let area = frame.area();
    let body_height = area.height.saturating_sub(1);
    let body = Rect::new(area.x, area.y, area.width, body_height);
    let footer = Rect::new(
        area.x,
        area.y + body_height,
        area.width,
        area.height - body_height,
    );
    page.draw(frame, body, true, host);
    let line = match &host.notice {
        Some((text, true)) => {
            Span::styled(text.clone(), Style::default().fg(ui::palette().deleted))
        }
        Some((text, false)) => Span::styled(text.clone(), Style::default().dim()),
        None => Span::styled("esc close · click/⏎ change", Style::default().dim()),
    };
    frame.render_widget(Paragraph::new(line), footer);
}

/// The `--settings-popup` process: the Settings page alone, full size, until
/// the page closes or Ctrl+C.
pub fn run() -> io::Result<()> {
    ui::set_color_theme(state::load_state().color_theme);
    let mut host = PopupHost::from_env();
    let mut page = SettingsPage::open();

    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableMouseCapture);
    let result = (|| -> io::Result<()> {
        loop {
            // Redrawn on every turn, idle ones included: the Update row's
            // background check lands without an event.
            terminal.draw(|frame| draw(frame, &mut page, &host))?;
            if !event::poll(POLL)? {
                continue;
            }
            let outcome = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if is_ctrl_c(key) {
                        break;
                    }
                    host.begin_event();
                    page.key(key, &mut host)
                }
                Event::Mouse(mouse) => {
                    host.begin_event();
                    page.mouse(mouse, &mut host)
                }
                Event::Resize(..) => {
                    ui::repaint_after_resize(&mut terminal)?;
                    Outcome::Stay
                }
                _ => Outcome::Stay,
            };
            if outcome == Outcome::Close && !host.failed {
                break;
            }
        }
        Ok(())
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point this thread's state dir (and so the request directory, which
    /// lives under it) at a fresh scratch directory.
    fn isolated_state(test: &str) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "hs-settings-popup-{test}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch state dir");
        state::TEST_STATE_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
    }

    #[test]
    fn request_text_round_trips() {
        for request in [
            Request::Unified(true),
            Request::Unified(false),
            Request::ChangeFolder,
        ] {
            let text = format!("{}\n100\n", request.to_text());
            assert_eq!(parse_request(&text, 100), Some(request));
        }
    }

    #[test]
    fn stale_and_unknown_requests_are_ignored() {
        let now = 1_000;
        assert_eq!(
            parse_request("folder\n1000\n", now),
            Some(Request::ChangeFolder)
        );
        assert_eq!(
            parse_request("folder\n970\n", now),
            Some(Request::ChangeFolder),
            "exactly at the age limit still counts"
        );
        assert_eq!(parse_request("folder\n969\n", now), None);
        assert_eq!(parse_request("explode\n1000\n", now), None);
        assert_eq!(parse_request("folder\nnot-a-number\n", now), None);
        assert_eq!(parse_request("folder\n", now), None);
    }

    #[test]
    fn hints_decode_and_bad_input_is_empty() {
        let json = serde_json::to_string(&vec![("j".to_string(), "down".to_string())]).unwrap();
        assert_eq!(
            parse_hints(&json),
            vec![("j".to_string(), "down".to_string())]
        );
        assert!(parse_hints("nope").is_empty());
    }

    #[test]
    fn take_on_missing_request_is_none() {
        isolated_state("missing");
        assert_eq!(take_request("w1-p1"), None);
    }

    #[test]
    fn write_then_take_applies_once() {
        isolated_state("round-trip");
        write_request("w1:p2", Request::Unified(false)).expect("write");
        assert_eq!(take_request("w1:p2"), Some(Request::Unified(false)));
        assert_eq!(take_request("w1:p2"), None, "a claimed request is gone");
    }

    #[test]
    fn stale_request_on_disk_is_claimed_and_dropped() {
        isolated_state("stale");
        let path = request_path("w1-p9");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        std::fs::write(&path, "folder\n1\n").expect("stale request");
        assert_eq!(take_request("w1-p9"), None);
        assert!(!path.exists(), "a stale request is still consumed");
    }
}
